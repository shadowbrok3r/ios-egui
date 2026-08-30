//! Android backend for the shared [`egui_mobile_core`] runtime. Implement [`EguiApp`] and invoke
//! [`app!`]; the macro emits `android_main`. The render loop is driven by `eframe` (winit + wgpu,
//! Vulkan/GL) which handles the Android surface-recreation-on-resume dance and input/IME; the
//! `Host` capability bridge is threaded through and (in the JNI layer) drained to Android APIs.

pub use android_activity::AndroidApp;
pub use egui;
pub use egui_mobile_core::{CreateContext, EguiApp, Haptic, Host, Insets, Permission};

/// Adapts an [`EguiApp`] + [`Host`] to `eframe::App`. Each frame it opens a central panel, hands
/// the root `ui` to the app, then drains queued host requests (JNI dispatch lives in `host`).
struct Adapter {
    app: Box<dyn EguiApp>,
    host: Host,
    started: bool,
    /// Events queued by the text-actions bar, injected into the next frame's input.
    pending_events: Vec<egui::Event>,
    frame: u64,
    /// Cached "clipboard has text" plus the frame at which to re-poll it.
    has_clip: bool,
    next_clip_poll: u64,
    /// Most recent focused widget; restored after a bar tap surrenders focus.
    last_focus: Option<egui::Id>,
    /// Text-actions bar rect from the previous frame.
    bar_rect: Option<egui::Rect>,
    /// A pointer press that began inside the bar (cleared only after the frame that handles release).
    bar_touch: bool,
    /// Extra frames to pin focus + soft keyboard after a bar action (avoids IME flicker).
    ime_hold_frames: u8,
    /// Soft keyboard was requested via the EditText bridge (rising-edge show / debounced hide).
    ime_bridge_hot: bool,
    /// Consecutive frames where IME was not wanted (hide only after this exceeds a threshold).
    ime_hide_arm: u8,
    /// Consecutive frames with `want_ime` but no keyboard inset, once the keyboard has actually
    /// been seen open this "hot" session — used to detect a genuine external hide (as opposed to
    /// the normal open animation's low-inset frames right after we request a show).
    ime_recover_arm: u16,
    /// The keyboard inset has reached [`Self::IME_OPEN_PT`] since [`Self::ime_bridge_hot`] went
    /// true. Recovery is gated on this so it never fights the keyboard's own opening animation
    /// (whose inset legitimately stays near zero for several frames while it ramps up).
    ime_seen_open: bool,
    /// Frames left before another forced re-show is allowed, after one just fired.
    ime_recover_cooldown: u16,
    /// Last egui IME rect; re-emitted when a frame drops `PlatformOutput::ime` so winit does not
    /// call `set_ime_allowed(false)` (hide) while the EditText bridge still owns the keyboard.
    last_ime: Option<egui::output::IMEOutput>,
    /// Focus id we last pushed into the hidden EditText (one-shot sync on focus/show/bar).
    ime_synced_focus: Option<egui::Id>,
    /// Force one egui→EditText sync after a text-actions bar edit (paste/cut/select-all).
    ime_force_sync: bool,
    /// The next successful seed restarts the IME session (field switch / out-of-band edit).
    ime_seed_restart: bool,
    /// Input type last pushed to the EditText, from egui's `IMEOutput::purpose`.
    ime_password: bool,
    /// Points subtracted from `screen_rect.max.y` this frame for the soft keyboard.
    ime_inset_pt: f32,
}

impl Adapter {
    /// Keyboard inset (pt) above which we consider the soft keyboard genuinely open.
    const IME_OPEN_PT: f32 = 60.0;
    /// Keyboard inset (pt) below which we consider it hidden.
    const IME_HIDDEN_PT: f32 = 16.0;
    /// Consecutive frames hidden (after having been open) before a forced re-show.
    /// Long on purpose — winit's implicit hide is patched out; this is only a safety net.
    const IME_RECOVER_FRAMES: u16 = 120;
    /// Open->hidden frames before a pre-API-30 device (no insets dismissal edge) treats the
    /// hide as an external dismissal. Short enough to feel instant at the 100ms drain cadence,
    /// long enough to absorb IME-switch / rotation inset dips.
    const IME_SYNTH_DISMISS_FRAMES: u16 = 6;
    /// Frames to wait after a forced re-show before trying again.
    const IME_RECOVER_COOLDOWN_FRAMES: u16 = 300;
}

impl eframe::App for Adapter {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if !self.started {
            self.started = true;
            crate::host::init_documents_dir(&self.host);
            crate::host::register_lifecycle_natives();
            self.app.on_start(ui.ctx(), &self.host);
        }
        // The activity's onPause/onResume arrive on the Android UI thread and are picked up here;
        // the JNI side asks for a frame, so a pause still reaches the app before the OS may reap
        // the process. `drv_set_active` returns the previous value, so a repeated event is not a
        // second callback.
        if let Some(active) = crate::host::take_active_change() {
            let was = self.host.drv_set_active(active);
            if was != active {
                if active {
                    self.app.on_resume(&self.host);
                } else {
                    self.app.on_pause(&self.host);
                }
            }
        }
        self.frame += 1;
        // While a tap is on the bar, disable click-away focus surrender so the focused text
        // field (or plugin viewport) still has focus when the queued event lands.
        // Do NOT clear `bar_touch` on pointer-up here: the click is processed on the release
        // frame, and clearing early would re-enable surrender and collapse the keyboard.
        let pressed_in_bar = self.bar_rect.is_some_and(|r| {
            ui.ctx().input(|i| {
                i.events.iter().any(|e| {
                    matches!(e, egui::Event::PointerButton { pos, pressed: true, .. } if r.contains(*pos))
                })
            })
        });
        // Back button / gesture dismissed the keyboard: no egui input event says so, focus stays,
        // and the recovery path below would re-show it (and the actions bar would never hide).
        // Treat it as leaving the field. Before `hold`/focus are read so it takes effect now.
        if self.ime_bridge_hot && crate::ime_bridge::take_dismissed() {
            self.ime_teardown(ui.ctx());
        }
        self.bar_touch |= pressed_in_bar;
        let hold = self.bar_touch || self.ime_hold_frames > 0;
        // The focused field was not laid out last frame — the app switched tab/page or closed the
        // section holding it mid-edit. egui's focus dead-man switch drops it, but `pin_text_focus`
        // re-requests it every frame, so the keyboard and the actions bar outlive the widget they
        // belong to. Same treatment as a back-gesture dismissal. Only follows egui's own verdict
        // (focus already dropped, read before `pin_text_focus` resurrects it): egui spares a
        // just-requested focus for one pass, so `request_focus` on a field that appears next frame
        // still works. `used_ids` covers scroll-culled and clipped fields.
        if self.ime_bridge_hot
            && !hold
            && let Some(id) = self.last_focus
            && ui.ctx().memory(|m| m.focused()) != Some(id)
            && !ui.ctx().viewport(|v| v.prev_pass.used_ids.contains_key(&id))
        {
            self.ime_teardown(ui.ctx());
        }
        // egui surrenders focus on the frame a full CLICK lands (SurrenderFocusOn::Clicks checks
        // any_click during the widget's interact). allow_blur must be true on that same frame or
        // last_focus survives and pin_text_focus re-focuses the field. Keyed to any_click, not
        // primary_pressed: with the IME wake running the loop, press and release land in
        // different frames, and a press-keyed flag is false again by the surrender frame.
        let clicked = ui.ctx().input(|i| i.pointer.any_click());
        let click_in_bar = clicked
            && self.bar_rect.is_some_and(|r| {
                ui.ctx().input(|i| i.pointer.interact_pos().is_some_and(|p| r.contains(p)))
            });
        let allow_blur = clicked && !click_in_bar && !hold;
        ui.ctx().options_mut(|o| {
            o.input_options.surrender_focus_on = if hold {
                egui::SurrenderFocusOn::Never
            } else {
                egui::SurrenderFocusOn::Clicks
            };
        });
        // Keep egui TextEdit focused while the keyboard is hot so the caret blinks and IME
        // Text events are consumed (otherwise: first letter, then silence until retap).
        if (hold || self.ime_bridge_hot) && !allow_blur {
            self.pin_text_focus(ui.ctx());
        }
        // Drain InputConnection → egui before the app frame. Do NOT show/hide the IME here:
        // that decision needs this frame's focus after `app.update` (pre-update `ime` output
        // flickers with keyboard-inset layout and caused a show/hide loop).
        let mut ime_applied = false;
        if self.ime_bridge_hot || hold {
            let _ = crate::ime_bridge::bind_ime();
            ime_applied = crate::ime_bridge::apply_pending(
                ui.ctx(),
                self.last_focus,
                &mut self.pending_events,
            );
            // Backstop for nativeImeWake (missing JNI symbol / event landing mid-frame): while
            // the keyboard is up, never sleep longer than this between queue drains.
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
        if !self.pending_events.is_empty() {
            let events = std::mem::take(&mut self.pending_events);
            // Extend `raw` too: the plugin viewport forwards guest input from `raw.events`.
            ui.ctx().input_mut(|i| {
                i.raw.events.extend(events.iter().cloned());
                i.events.extend(events);
            });
        }
        let insets = self.host.safe_area_insets();
        // `screen_rect` was shortened by the keyboard in `raw_input_hook`; `rect` is that reduced
        // area (given to the app), `full_rect` restores full height for the text-actions bar.
        let mut rect = ui.max_rect();
        let mut full_rect = rect;
        full_rect.max.y += self.ime_inset_pt;
        for r in [&mut rect, &mut full_rect] {
            r.min.x += insets.left;
            r.min.y += insets.top;
            r.max.x -= insets.right;
            r.max.y -= insets.bottom;
        }
        ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
            self.app.update(ui, &self.host);
        });
        let focused = ui.ctx().memory(|m| m.focused());
        // Text-edit focus only: plugin viewports focus on any press to route keys to the guest, and
        // any-widget focus would raise the soft keyboard for plain taps. Plugins that draw their own
        // text (the terminal) ask via `Host::request_keyboard` -> `guest_kb`.
        let text_focus = is_text_edit(ui.ctx(), focused);
        let prev_focus = self.last_focus;
        if let Some(id) = focused {
            self.last_focus = Some(id);
        } else if allow_blur || !(hold || self.ime_bridge_hot) {
            self.last_focus = None;
            self.ime_synced_focus = None;
        }
        let switched_field = matches!(
            (prev_focus, self.last_focus),
            (Some(a), Some(b)) if a != b
        );
        if switched_field {
            crate::ime_bridge::clear_preedit_tracking();
            // Events deferred against field A's document must not replay into field B, and
            // the seed for B restarts the IME session so the keyboard re-reads the document.
            crate::ime_bridge::clear_carry();
            self.ime_seed_restart = true;
        }
        // A field gaining focus may carry a stuck composition from an earlier tap/dismissal —
        // egui paints no caret while composing, and a stuck purpose never self-heals (its only
        // reset is an IME event). Not gated on ime_bridge_hot: the session-opening tap runs this
        // before the flag turns on, and that tap is exactly when the stale state must clear.
        if let Some(id) = self.last_focus
            && prev_focus != self.last_focus
        {
            // The reset deletes the stored cursor span while a composition is stuck; a
            // programmatic focus gain (no tap to collapse it) must collapse it first.
            if let Some(mut st) = egui::text_edit::TextEditState::load(ui.ctx(), id)
                && let Some(range) = st.cursor.char_range()
            {
                let r = range.as_sorted_char_range();
                if r.start != r.end {
                    st.cursor.set_char_range(Some(egui::text::CCursorRange::one(
                        egui::text::CCursor::new(r.end),
                    )));
                    st.store(ui.ctx(), id);
                }
            }
            self.pending_events.push(egui::Event::Ime(egui::ImeEvent::Preedit {
                text: String::new(),
                active_range_chars: None,
            }));
        }
        // Keep `PlatformOutput::ime` stable while editing. A one-frame `ime: None` makes
        // egui-winit call `set_ime_allowed(false)` → hideSoftInput on the DecorView token,
        // which dismisses our EditText keyboard; its follow-up show on DecorView is ignored
        // ("view is not served").
        let guest_kb = crate::host::keyboard_requested();
        if let Some(ime) = ui.ctx().output(|o| o.ime) {
            self.last_ime = Some(egui::output::IMEOutput {
                purpose: ime.purpose,
                rect: ime.rect,
                cursor_rect: ime.cursor_rect,
                should_interrupt_composition: false,
            });
        } else if hold
            || text_focus
            || guest_kb
            || (self.ime_bridge_hot && is_text_edit(ui.ctx(), self.last_focus) && !allow_blur)
        {
            if let Some(ime) = self.last_ime {
                ui.ctx().output_mut(|o| {
                    o.ime = Some(ime);
                });
            }
        }
        let ime_wanted = ui.ctx().output(|o| o.ime.is_some());
        // egui 0.36 reports the focused field's IME purpose; a password field must not reach the
        // keyboard's suggestion, autocorrect or personalized-learning stores.
        let password = ui
            .ctx()
            .output(|o| o.ime.map(|ime| ime.purpose == egui::IMEPurpose::Password))
            .unwrap_or(self.ime_password);
        if password != self.ime_password {
            self.ime_password = password;
            crate::ime_bridge::set_ime_password(password);
            // The restart drops the EditText's IME session; the mirror must be pushed again.
            crate::ime_bridge::invalidate_last_sync();
            self.ime_seed_restart = true;
            self.ime_force_sync = true;
        }
        // Do not key want_ime off ime_bridge_hot alone — that can never go false and traps the keyboard.
        let want_ime = hold || ime_wanted || text_focus || guest_kb;
        if want_ime {
            self.ime_hide_arm = 0;
            let kb = self.host.keyboard_height();
            if !self.ime_bridge_hot {
                let _ = crate::ime_bridge::set_soft_keyboard(true);
                self.ime_bridge_hot = true;
                self.ime_seen_open = false;
                self.ime_recover_arm = 0;
                self.ime_recover_cooldown = 0;
                // Seed EditText once when the keyboard opens; never every frame while typing.
                self.ime_force_sync = true;
            } else {
                let _ = crate::ime_bridge::bind_ime();
                if kb >= Self::IME_OPEN_PT {
                    self.ime_seen_open = true;
                    self.ime_recover_arm = 0;
                } else if self.ime_seen_open && kb < Self::IME_HIDDEN_PT {
                    // The keyboard was genuinely open and is now gone — winit's DecorView hide
                    // beat us and its follow-up show was ignored. Recover, but slowly: forcing
                    // showSoftInput restarts the IME session (see logcat "Session id mismatch"),
                    // which can corrupt in-flight typing/backspace if fired too eagerly.
                    self.ime_recover_arm = self.ime_recover_arm.saturating_add(1);
                    if !crate::host::ime_inset_reliable()
                        && self.ime_recover_arm >= Self::IME_SYNTH_DISMISS_FRAMES
                    {
                        // Pre-API-30: no insets edge ever reports the keyboard's own hide key,
                        // so a debounced open->hidden edge is the external-dismissal signal.
                        self.ime_teardown(ui.ctx());
                    } else if self.ime_recover_cooldown > 0 {
                        self.ime_recover_cooldown -= 1;
                    } else if self.ime_recover_arm >= Self::IME_RECOVER_FRAMES {
                        let _ = crate::ime_bridge::show_ime_force();
                        self.ime_recover_arm = 0;
                        self.ime_recover_cooldown = Self::IME_RECOVER_COOLDOWN_FRAMES;
                    }
                } else {
                    self.ime_recover_arm = 0;
                }
            }
        } else {
            self.ime_recover_arm = 0;
            self.ime_recover_cooldown = 0;
            self.ime_hide_arm = self.ime_hide_arm.saturating_add(1);
            // ~0.5s at 60fps — absorbs one-frame ime_wanted flickers from keyboard reflow.
            if self.ime_hide_arm >= 30 && self.ime_bridge_hot {
                let _ = crate::ime_bridge::set_soft_keyboard(false);
                crate::ime_bridge::clear_preedit_tracking();
                crate::ime_bridge::clear_carry();
                self.ime_bridge_hot = false;
                self.ime_seen_open = false;
                self.last_ime = None;
                self.ime_synced_focus = None;
                self.ime_force_sync = false;
                self.ime_seed_restart = false;
            }
        }
        // egui → EditText only when opening the keyboard, switching fields, or bar paste/cut —
        // never while typing (setText resets the caret and triggers invalidateInput).
        // Retries until the undoer has a stable snapshot: seeding before that pushed "" into the
        // EditText, and every later IME op then edited against an empty mirror.
        // Latched until the seed actually lands: the seed retries across frames while the
        // undoer settles, and the restart intent must survive those retries.
        self.ime_seed_restart |= crate::ime_bridge::take_reseed_restart();
        let need_sync =
            self.ime_force_sync || switched_field || crate::ime_bridge::take_needs_reseed();
        if need_sync {
            crate::ime_bridge::invalidate_last_sync();
            let seeded = if self.ime_seed_restart {
                crate::ime_bridge::sync_focused_text_edit_restart(ui.ctx(), self.last_focus)
            } else {
                crate::ime_bridge::sync_focused_text_edit(ui.ctx(), self.last_focus)
            };
            if seeded {
                self.ime_synced_focus = self.last_focus;
                self.ime_force_sync = false;
                self.ime_seed_restart = false;
            } else if self.ime_bridge_hot && is_text_edit(ui.ctx(), self.last_focus) {
                self.ime_force_sync = true;
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            } else {
                self.ime_force_sync = false;
                self.ime_seed_restart = false;
            }
        }
        // Mirror egui's caret into the EditText every frame it differs (the call is a no-op when
        // it matches or a composition is active). Not gated on a pointer release: the seed can
        // land frames after the tap that caused it, by which point the release is long gone and
        // the mirror would keep a stale caret for the rest of the session. Skipped on frames
        // that applied IME events — egui's caret is mid-convergence then and pushing it back
        // would plant a stale offset in the EditText.
        if self.ime_bridge_hot
            && !need_sync
            && !ime_applied
            && let Some(id) = self.last_focus
            && let Some(state) = egui::text_edit::TextEditState::load(ui.ctx(), id)
            && state.cursor.char_range().is_some()
        {
            // Text changed outside the IME (app edits, hardware keys): push the whole buffer
            // instead of just the caret, restarting the IME session over the new document.
            if !crate::ime_bridge::resync_out_of_band(ui.ctx(), self.last_focus) {
                let (s, e) = crate::ime_bridge::selection_chars(&state);
                let user_tap =
                    ui.ctx().input(|i| i.pointer.any_pressed() || i.pointer.any_released());
                crate::ime_bridge::sync_caret_to_ime(s, e, user_tap);
            }
        }
        // Mirror this frame's egui copies (host widgets and plugin viewports alike) into the
        // system clipboard; winit has no Android clipboard backend.
        let copied = ui.ctx().output(|o| {
            o.commands.iter().rev().find_map(|c| match c {
                egui::OutputCommand::CopyText(t) if !t.is_empty() => Some(t.clone()),
                _ => None,
            })
        });
        if let Some(text) = copied {
            self.host.copy_text(text);
        }
        self.text_actions_bar(ui, full_rect, text_focus);
        // Clear bar_touch only after the bar has handled this frame's release/click.
        if self.bar_touch && ui.ctx().input(|i| !i.pointer.any_down()) {
            self.bar_touch = false;
        }
        if self.ime_hold_frames > 0 {
            self.ime_hold_frames -= 1;
        }
        // Re-pin after the app frame so reflow cannot leave us unfocused for the next IME char.
        if (hold || self.ime_bridge_hot) && !allow_blur {
            self.pin_text_focus(ui.ctx());
        }
        crate::host::drain(&self.host);
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        // Feed Android WindowInsets (status bar / camera cutout / nav bar / IME) into the host so
        // `host.safe_area_insets()` and `host.keyboard_height()` track the current frame.
        crate::host::update_insets(&self.host, ctx.pixels_per_point());
        // Shrink egui's layout viewport by the keyboard's occlusion (points) so the whole UI —
        // central panel, ctx-level windows and popups — lays out above the soft keyboard. The GL
        // surface stays full-size; only `screen_rect` shrinks. `keyboard_height()` is in points.
        self.ime_inset_pt = 0.0;
        let inset = (self.host.keyboard_height() - self.host.safe_area_insets().bottom).max(0.0);
        if inset <= 0.0 {
            return;
        }
        if let Some(rect) = raw_input.screen_rect.as_mut()
            && inset < rect.height() - 1.0
        {
            rect.max.y -= inset;
            self.ime_inset_pt = inset;
        }
    }
}

/// Whether `id` is a `TextEdit`, as opposed to a widget holding focus for key routing only.
fn is_text_edit(ctx: &egui::Context, id: Option<egui::Id>) -> bool {
    id.is_some_and(|id| egui::text_edit::TextEditState::load(ctx, id).is_some())
}

impl Adapter {
    /// Drop the whole IME session: surrender focus, hide the keyboard, and reset every bridge
    /// latch. Used for external dismissals (back gesture) and their synthesized pre-API-30 twin.
    fn ime_teardown(&mut self, ctx: &egui::Context) {
        if let Some(id) = self.last_focus {
            ctx.memory_mut(|m| m.surrender_focus(id));
        }
        let _ = crate::ime_bridge::set_soft_keyboard(false);
        crate::ime_bridge::clear_preedit_tracking();
        crate::ime_bridge::clear_carry();
        self.ime_bridge_hot = false;
        self.ime_seen_open = false;
        self.ime_recover_arm = 0;
        self.ime_recover_cooldown = 0;
        self.ime_hide_arm = 0;
        self.ime_hold_frames = 0;
        self.bar_touch = false;
        self.ime_force_sync = false;
        self.ime_seed_restart = false;
        self.last_focus = None;
        self.last_ime = None;
        self.ime_synced_focus = None;
        self.pending_events.clear();
    }

    /// Restore text-field focus after a bar tap.
    /// Skips when already focused — `Memory::request_focus` always sets `interrupt_ime`, and
    /// egui-winit then does `set_ime_allowed(false/true)` which hides our keyboard and fails
    /// to re-show on the DecorView.
    fn pin_text_focus(&self, ctx: &egui::Context) {
        let Some(id) = self.last_focus else { return };
        if ctx.memory(|m| m.focused() == Some(id)) {
            return;
        }
        ctx.memory_mut(|m| m.request_focus(id));
    }

    /// Floating Paste/Copy/Cut/Select-all bar shown while a text field is being edited — the
    /// Android equivalent of the selection context menu, since egui draws its own text widgets.
    fn text_actions_bar(&mut self, ui: &egui::Ui, rect: egui::Rect, has_focus: bool) {
        let ctx = ui.ctx().clone();
        let keyboard = self.host.keyboard_height();
        let ime_wanted = ctx.output(|o| o.ime.is_some());
        let guest_kb = crate::host::keyboard_requested();
        let hold = self.bar_touch || self.ime_hold_frames > 0;
        // Hide on click-away: require an active edit signal (IME/focus/guest), not merely a
        // lingering keyboard inset or a sticky flag from a prior bar tap.
        let show = hold
            || guest_kb
            || (ime_wanted && has_focus)
            || (keyboard > 0.0 && (has_focus || guest_kb));
        if !show {
            self.next_clip_poll = 0;
            self.bar_rect = None;
            return;
        }
        if self.frame >= self.next_clip_poll {
            // Presence only — never materialize clipboard text just to enable Paste.
            self.has_clip = crate::host::clipboard_has_text();
            self.next_clip_poll = self.frame + 30;
        }
        // Fixed just above the keyboard and horizontally centred — the same place every frame,
        // whatever field is focused and wherever the caret sits. `rect` is the full-height safe
        // area and `ime_inset_pt` is what this frame's layout was shortened by, so their difference
        // is the keyboard's top edge, in lockstep with the layout the app just drew. Before the
        // first inset lands — and on a window that never resizes for the IME, or a guest viewport
        // that asked for the keyboard itself — assume a typical keyboard fraction rather than
        // parking the bar behind the keys.
        let overlap = if self.ime_inset_pt > 0.0 {
            self.ime_inset_pt
        } else if guest_kb || (self.ime_bridge_hot && !self.ime_seen_open) {
            rect.height() * 0.4
        } else {
            0.0
        };
        let keyboard_top = (rect.bottom() - overlap).max(rect.top() + 1.0);
        let pos = egui::pos2(rect.center().x, keyboard_top - 8.0);
        let mut acted = false;
        let area = egui::Area::new(egui::Id::new("egui-android-text-actions"))
            .order(egui::Order::Foreground)
            .pivot(egui::Align2::CENTER_BOTTOM)
            .fixed_pos(pos)
            // Clamped to the area above the keyboard, so an over-tall bar rides up instead of down
            // over the keys.
            .constrain_to(egui::Rect::from_min_max(rect.min, egui::pos2(rect.max.x, keyboard_top)))
            .show(&ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.spacing_mut().button_padding = egui::vec2(10.0, 8.0);
                    ui.horizontal(|ui| {
                        let icon = |s: &str| egui::RichText::new(s).size(20.0);
                        if ui
                            .add_enabled(self.has_clip, egui::Button::new(icon("📋")))
                            .on_hover_text("Paste")
                            .clicked()
                            && let Some(text) = crate::host::read_clipboard_text()
                        {
                            self.pending_events.push(egui::Event::Paste(text));
                            acted = true;
                        }
                        if ui.button(icon("📄")).on_hover_text("Copy").clicked() {
                            self.pending_events.push(egui::Event::Copy);
                            acted = true;
                        }
                        if ui.button(icon("✂")).on_hover_text("Cut").clicked() {
                            self.pending_events.push(egui::Event::Cut);
                            acted = true;
                        }
                        if ui.button(icon("Aa")).on_hover_text("Select all").clicked() {
                            // Live TextEdit buffer (not the lagged undoer snapshot).
                            self.pending_events.push(egui::Event::Key {
                                key: egui::Key::A,
                                physical_key: None,
                                pressed: true,
                                repeat: false,
                                modifiers: egui::Modifiers::COMMAND,
                            });
                            acted = true;
                        }
                    });
                });
            });
        self.bar_rect = Some(area.response.rect);
        // Pin focus after a bar tap. Rising-edge show only if the IME was already down.
        if acted {
            self.ime_hold_frames = self.ime_hold_frames.max(24);
            self.ime_hide_arm = 0;
            self.ime_force_sync = true;
            self.pin_text_focus(&ctx);
            if !self.ime_bridge_hot {
                let _ = crate::ime_bridge::set_soft_keyboard(true);
                self.ime_bridge_hot = true;
            }
        }
    }
}

/// Entry point invoked by [`app!`]. Boots logging, installs a panic logger, and runs eframe with
/// the Android app handle and the wgpu renderer.
pub fn run(app: AndroidApp, factory: impl FnMut(&CreateContext) -> Box<dyn EguiApp> + 'static) {
    run_with(app, Backend::default(), factory);
}

/// Which renderer eframe drives.
///
/// Chosen at runtime rather than by a cargo feature — see the note on the eframe dependency.
/// `Glow` exists for apps that need an OpenGL paint callback (backdrop blur grabs the live
/// framebuffer, which wgpu cannot do from inside eframe).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    #[default]
    Wgpu,
    Glow,
}

static GLOW_CONTEXT: std::sync::OnceLock<std::sync::Arc<glow::Context>> =
    std::sync::OnceLock::new();

/// The live `glow` context, once the app is running on [`Backend::Glow`]. `None` on wgpu.
pub fn glow_context() -> Option<std::sync::Arc<glow::Context>> {
    GLOW_CONTEXT.get().cloned()
}

pub fn run_with(
    app: AndroidApp,
    backend: Backend,
    factory: impl FnMut(&CreateContext) -> Box<dyn EguiApp> + 'static,
) {
    run_with_depth(app, backend, 0, factory);
}

/// Like [`run_with`], but asks the window for a depth attachment of `depth_bits`.
///
/// eframe defaults `NativeOptions::depth_buffer` to 0 and passes it straight to glutin's
/// `with_depth_size`, so a 3D app gets a window with **no depth attachment at all**:
/// `glEnable(GL_DEPTH_TEST)` and `glClear(GL_DEPTH_BUFFER_BIT)` then silently do nothing and the far
/// side of a model paints over the near one. Worse, `eglChooseConfig` breaks ties by *smaller* depth
/// size and eframe takes the first config, so the depth-less outcome is deterministic rather than a
/// lottery some devices lose.
///
/// Pass 24 for anything that depth-tests. 0 keeps the old behaviour.
pub fn run_with_depth(
    app: AndroidApp,
    backend: Backend,
    depth_bits: u8,
    mut factory: impl FnMut(&CreateContext) -> Box<dyn EguiApp> + 'static,
) {
    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Info),
    );
    std::panic::set_hook(Box::new(|info| {
        log::error!("egui-android panic: {info}");
    }));

    host::set_android_app(app.clone());
    ime_bridge::register_natives();

    // Under glow, egui's wgpu paint callbacks are silently skipped rather than failing — the
    // painter logs "Unsupported render callback" and draws nothing — so a plugin viewport would
    // just be blank. Say so once, loudly, instead of leaving it to be discovered.
    #[cfg(feature = "plugins")]
    if backend == Backend::Glow {
        log::error!(
            "egui-android: the `plugins` feature draws through a wgpu paint callback, which the \
             glow renderer ignores. Plugin viewports will render blank."
        );
    }

    let mut options = eframe::NativeOptions::default();
    options.android_app = Some(app);
    options.renderer = match backend {
        Backend::Wgpu => eframe::Renderer::Wgpu,
        Backend::Glow => eframe::Renderer::Glow,
    };
    options.depth_buffer = depth_bits;
    log::info!("egui-android: renderer {backend:?}, depth {depth_bits} bits");

    let result = eframe::run_native(
        "egui-android",
        options,
        Box::new(move |cc| {
            crate::ime_bridge::set_wake_context(&cc.egui_ctx);
            if let Some(gl) = cc.gl.clone() {
                let _ = GLOW_CONTEXT.set(gl);
            }
            // Install the plugin paint callback into eframe's wgpu renderer (feature `plugins`).
            #[cfg(feature = "plugins")]
            if let Some(rs) = cc.wgpu_render_state.as_ref() {
                let mut renderer = rs.renderer.write();
                egui_ios_plugin_host::install(&mut renderer, rs.target_format, 1);
            }
            let cx = CreateContext {
                width_px: 0,
                height_px: 0,
                pixels_per_point: cc.egui_ctx.pixels_per_point(),
            };
            let app = factory(&cx);
            // Before the first frame, like the iOS runtime does in `__ffi`. Without this the trait
            // method silently did nothing on Android and every app's palette fell back to egui's
            // default — comfyui and privaxy each carry a workaround calling their own `apply` from
            // `update`, which costs a frame of unstyled UI and only fixed those two.
            app.theme(&cc.egui_ctx);
            Ok(Box::new(Adapter {
                app,
                host: Host::new(),
                started: false,
                pending_events: Vec::new(),
                frame: 0,
                has_clip: false,
                next_clip_poll: 0,
                last_focus: None,
                bar_rect: None,
                bar_touch: false,
                ime_hold_frames: 0,
                ime_bridge_hot: false,
                ime_hide_arm: 0,
                ime_recover_arm: 0,
                ime_seen_open: false,
                ime_recover_cooldown: 0,
                last_ime: None,
                ime_synced_focus: None,
                ime_force_sync: false,
                ime_seed_restart: false,
                ime_password: false,
                ime_inset_pt: 0.0,
            }))
        }),
    );
    if let Err(e) = result {
        log::error!("egui-android run_native failed: {e}");
    }
}

pub mod host;
pub mod ime_bridge;
pub mod video;
pub mod vpn;
pub use host::{HostExt, ScreenOrientation, StylusProbe, device_orientation_deg};

#[cfg(feature = "plugins")]
pub mod plugins;

/// Generates `android_main` for a type implementing [`EguiApp`].
///
/// `factory` is any `Fn(&CreateContext) -> impl EguiApp`, e.g. `app!(MyApp::new)`.
#[macro_export]
macro_rules! app {
    ($factory:path) => {
        #[unsafe(no_mangle)]
        fn android_main(app: $crate::AndroidApp) {
            $crate::run(app, |cc| ::std::boxed::Box::new($factory(cc)));
        }
    };
    // Second form picks the renderer, e.g. `app!(MyApp::new, Backend::Glow)`.
    ($factory:path, $backend:expr) => {
        #[unsafe(no_mangle)]
        fn android_main(app: $crate::AndroidApp) {
            $crate::run_with(app, $backend, |cc| ::std::boxed::Box::new($factory(cc)));
        }
    };
    // Third form also asks for a depth attachment, e.g. `app!(MyApp::new, Backend::Glow, 24)`.
    // Without it eframe requests 0 bits and depth testing silently does nothing.
    ($factory:path, $backend:expr, $depth:expr) => {
        #[unsafe(no_mangle)]
        fn android_main(app: $crate::AndroidApp) {
            $crate::run_with_depth(app, $backend, $depth, |cc| ::std::boxed::Box::new($factory(cc)));
        }
    };
}

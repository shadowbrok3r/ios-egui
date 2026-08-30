//! The layer stack, on a phone.
//!
//! The phone could always *make* layers — the Alphas tab, the pen, Auto pavé,
//! Channel set — and the only stack operation it had was Clear layers, which
//! deletes all of them. A design pulled from the desktop arrived with a stack
//! that could not be listed, muted, reordered or removed, and the DFM findings
//! named a layer index with nothing to point at.
//!
//! This is the phone-shaped tenth of the desktop's 3081-line panel: the list,
//! the stack operations, and the one editor body that applies to *every* layer
//! variant — name, opacity, blend, and the window that moves a layer onto a
//! side face. The 2400 lines of per-variant editors stay on the desktop.
//!
//! Two deliberate departures from the desktop. Solo is a button, because there
//! is no Alt key. And there is **no drag-to-reorder**: a drag source inside a
//! vertically scrolling touch list steals the scroll — the last-registered drag
//! widget wins — so reordering is buttons.
//!
//! The stack operations are pure and live here rather than on the app struct,
//! so their off-by-one behaviour is host-testable.

use egui_mobile::egui;
use ringdesign_core::field::{Blend, LayerStack, SideFacePick, VGate, Window};
use ringdesign_core::field::FieldContext;

/// Move the entry at `i` by `delta`, keeping the selection on it.
pub fn move_layer(stack: &mut LayerStack, sel: &mut Option<usize>, i: usize, delta: isize) -> bool {
    let n = stack.layers.len();
    let j = i as isize + delta;
    if i >= n || j < 0 || j as usize >= n {
        return false;
    }
    stack.layers.swap(i, j as usize);
    *sel = Some(j as usize);
    true
}

/// Lift `from` out and insert it at `to` — what the Top and Bottom buttons do.
pub fn move_layer_to(stack: &mut LayerStack, sel: &mut Option<usize>, from: usize, to: usize) -> bool {
    let n = stack.layers.len();
    if from >= n || to >= n || from == to {
        return false;
    }
    let e = stack.layers.remove(from);
    stack.layers.insert(to, e);
    *sel = Some(to);
    true
}

/// Mute everything but `i` — or restore all, when `i` is already the only one
/// enabled, so the same button is its own undo.
pub fn solo_layer(stack: &mut LayerStack, sel: &mut Option<usize>, i: usize) -> bool {
    if i >= stack.layers.len() {
        return false;
    }
    let already = stack.layers.iter().enumerate().all(|(j, e)| e.enabled == (j == i));
    for (j, e) in stack.layers.iter_mut().enumerate() {
        e.enabled = already || j == i;
    }
    *sel = Some(i);
    true
}

pub fn duplicate_layer(stack: &mut LayerStack, sel: &mut Option<usize>, i: usize) -> bool {
    let Some(e) = stack.layers.get(i).cloned() else { return false };
    let mut copy = e;
    copy.name = format!("{} copy", copy.name);
    stack.layers.insert(i + 1, copy);
    *sel = Some(i + 1);
    true
}

pub fn remove_layer(stack: &mut LayerStack, sel: &mut Option<usize>, i: usize) -> bool {
    if i >= stack.layers.len() {
        return false;
    }
    stack.layers.remove(i);
    *sel = None;
    true
}

/// Append `layer` and select it.
pub fn add_layer(
    stack: &mut LayerStack,
    sel: &mut Option<usize>,
    name: impl Into<String>,
    layer: ringdesign_core::field::Layer,
) {
    stack.layers.push(ringdesign_core::field::LayerEntry::new(name, layer));
    *sel = Some(stack.layers.len() - 1);
}

/// Put a swept wire on a side face when the profile has one.
///
/// Carried verbatim from the desktop, because it is the difference between a
/// castable wire and one that undercuts: a rail across the crown leans back on
/// its crest-side flank wherever the dome's draft is shallower than the wire's
/// own slope, while the same wire on a face square to the pull measures 0.000%.
/// Without a side face the wire is still added — it just lands wherever the
/// preset put it, and the field verdict will say so.
fn place_curve(
    stack: &mut LayerStack,
    sel: &mut Option<usize>,
    ctx: &FieldContext,
    name: &str,
    mut l: ringdesign_core::curve::CurveLayer,
) -> &'static str {
    match ctx.side_faces_std().and_then(|sf| sf.wider()) {
        Some((lo, hi)) => {
            l.retarget_v(0.5 * (lo + hi), (hi - lo) * 0.3);
            add_layer(stack, sel, name, ringdesign_core::field::Layer::Curve(l));
            if let Some(e) = stack.layers.last_mut() {
                e.window.v_gate = VGate::SideFaces(SideFacePick::Wider);
            }
            "on the wider side face"
        }
        None => {
            add_layer(stack, sel, name, ringdesign_core::field::Layer::Curve(l));
            "no side face — square the sides so it pulls clean"
        }
    }
}

/// The add menu: only the variants whose defaults already cast and whose knobs
/// fit the generic editor. Flutes, Openwork and Decals each need a real editor
/// before their defaults are useful, and stay on the desktop.
///
/// Returns a status line when something was added.
pub fn add_menu(
    ui: &mut egui::Ui,
    design: &mut ringdesign_core::RingDesign,
    sel: &mut Option<usize>,
) -> Option<String> {
    use ringdesign_core::curve::CurveLayer;
    use ringdesign_core::field::{BorderLayer, Layer, MilgrainLayer, SeatPadLayer, SeatRunLayer};

    let ctx = design.field_context();
    let mut note: Option<String> = None;

    crate::theme::up_menu(ui, "Add layer", |ui| {
        if ui.button("Border").clicked() {
            add_layer(&mut design.layers, sel, "Border", Layer::Border(BorderLayer::default()));
            note = Some("border added".into());
        }
        if ui.button("Milgrain").clicked() {
            add_layer(&mut design.layers, sel, "Milgrain", Layer::Milgrain(MilgrainLayer::default()));
            note = Some("milgrain added".into());
        }
        if ui.button("Gem seat pad").clicked() {
            add_layer(&mut design.layers, sel, "Gem Seat Pad", Layer::SeatPad(SeatPadLayer::default()));
            note = Some("seat pad added".into());
        }
        if ui
            .button("Eternity row")
            .on_hover_text("A row of identical seats. Window it for a half row.")
            .clicked()
        {
            let mut run = SeatRunLayer::default();
            run.seat.v_mm = ctx.crest_v_mm;
            run.solve_spacing(&ctx);
            add_layer(&mut design.layers, sel, "Eternity row", Layer::SeatRun(run));
            note = Some("eternity row added".into());
        }
        ui.separator();
        for (label, name, make) in [
            ("S-scroll", "S-scroll", 0u8),
            ("Running vine", "Vine", 1),
            ("Wavy rail", "Wavy rail", 2),
        ] {
            if ui.button(label).clicked() {
                let l = match make {
                    0 => CurveLayer::preset_scroll(&ctx),
                    1 => CurveLayer::preset_vine(&ctx),
                    _ => CurveLayer::preset_wave_rail(&ctx),
                };
                let where_ = place_curve(&mut design.layers, sel, &ctx, name, l);
                note = Some(format!("{name}: {where_}"));
            }
        }
        ui.separator();
        if ui
            .button("Halo")
            .on_hover_text(
                "A centre stone on a domed plate with the melee as markers — a proud accent \
                 ring does not cast, so the setter beads them into the plate.",
            )
            .clicked()
        {
            let spec = ringdesign_core::pave::HaloSpec::default();
            note = Some(match ringdesign_core::pave::halo(design, &spec) {
                Some((entry, accents)) => {
                    design.layers.layers.push(entry);
                    *sel = Some(design.layers.layers.len() - 1);
                    format!("halo: centre plus {accents} markers")
                }
                // A refusal has a reason, and it is always the band: say which
                // way to change it rather than doing nothing.
                None => "no room for a halo — widen the band or shrink the centre stone".into(),
            });
        }
    });
    note
}

/// Draw the stack and its editor. Returns whether anything changed.
pub fn sheet(
    ui: &mut egui::Ui,
    stack: &mut LayerStack,
    ctx: &FieldContext,
    dfm: &[ringdesign_core::dfm::DfmFinding],
    sel: &mut Option<usize>,
) -> bool {
    let mut dirty = false;

    ui.horizontal_wrapped(|ui| {
        ui.label(
            egui::RichText::new(match stack.layers.len() {
                0 => "no layers".to_string(),
                1 => "1 layer".to_string(),
                n => format!("{n} layers"),
            })
            .small()
            .weak(),
        );
    });
    if stack.layers.is_empty() {
        ui.label(
            egui::RichText::new(
                "Put a pattern on the band from Alphas, draw one in Band, or run a stock \
                 generator in Design.",
            )
            .small()
            .weak(),
        );
        return false;
    }
    ui.separator();

    // Collected and applied after the loop: every one of these mutates the
    // stack, and the rows borrow it.
    let mut act: Option<Act> = None;

    for i in 0..stack.layers.len() {
        let flagged = dfm.iter().any(|f| f.layer == i);
        let (name, kind, enabled) = {
            let e = &stack.layers[i];
            (e.name.clone(), e.layer.kind_label(), e.enabled)
        };
        let picked = *sel == Some(i);

        ui.horizontal(|ui| {
            let mut on = enabled;
            if ui.checkbox(&mut on, "").changed() {
                act = Some(Act::SetEnabled(i, on));
            }
            let title = if flagged {
                egui::RichText::new(format!("{name}  ·  {kind}"))
                    .color(egui::Color32::from_rgb(220, 170, 90))
            } else if enabled {
                egui::RichText::new(format!("{name}  ·  {kind}"))
            } else {
                egui::RichText::new(format!("{name}  ·  {kind}")).weak()
            };
            if ui.add(crate::theme::selectable(picked, title)).clicked() {
                *sel = if picked { None } else { Some(i) };
            }
        });

        if !picked {
            continue;
        }

        // The stack reads top-down on screen and composites bottom-up, so Up
        // moves a layer earlier in the list, not later in the field.
        ui.horizontal_wrapped(|ui| {
            if ui.add_enabled(i > 0, egui::Button::new("Up").small()).clicked() {
                act = Some(Act::Move(i, -1));
            }
            if ui
                .add_enabled(i + 1 < stack.layers.len(), egui::Button::new("Down").small())
                .clicked()
            {
                act = Some(Act::Move(i, 1));
            }
            if ui.add_enabled(i > 0, egui::Button::new("Top").small()).clicked() {
                act = Some(Act::MoveTo(i, 0));
            }
            if ui
                .add_enabled(i + 1 < stack.layers.len(), egui::Button::new("Bottom").small())
                .clicked()
            {
                act = Some(Act::MoveTo(i, stack.layers.len() - 1));
            }
            if ui
                .small_button("Solo")
                .on_hover_text("Mute every other layer, or bring them all back")
                .clicked()
            {
                act = Some(Act::Solo(i));
            }
            if ui.small_button("Copy").clicked() {
                act = Some(Act::Duplicate(i));
            }
            if ui.small_button("Delete").clicked() {
                act = Some(Act::Remove(i));
            }
        });

        if let Some(f) = dfm.iter().find(|f| f.layer == i) {
            ui.label(
                egui::RichText::new(&f.message)
                    .small()
                    .color(egui::Color32::from_rgb(220, 170, 90)),
            );
        }

        let e = &mut stack.layers[i];
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new("name").small().weak());
            dirty |= ui
                .add(egui::TextEdit::singleline(&mut e.name).desired_width(140.0))
                .changed();
        });
        dirty |= ui
            .add(egui::Slider::new(&mut e.opacity, 0.0..=1.0).text("opacity"))
            .changed();
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new("blend").small().weak());
            let cur = e.blend;
            egui::ComboBox::from_id_salt(("blend", i))
                .selected_text(cur.label())
                .show_ui(ui, |ui| {
                    for &b in Blend::ALL {
                        if ui.selectable_label(cur == b, b.label()).clicked() && cur != b {
                            e.blend = b;
                            dirty = true;
                        }
                    }
                });
        });
        if e.blend.is_smooth() {
            dirty |= ui
                .add(egui::Slider::new(&mut e.soft_mm, 0.0..=2.0).text("fillet mm"))
                .changed();
        }
        dirty |= window_controls(ui, i, &mut e.window, ctx);
        ui.add_space(4.0);
        ui.separator();
    }

    if let Some(a) = act {
        dirty |= match a {
            Act::SetEnabled(i, on) => {
                stack.layers[i].enabled = on;
                true
            }
            Act::Move(i, d) => move_layer(stack, sel, i, d),
            Act::MoveTo(from, to) => move_layer_to(stack, sel, from, to),
            Act::Solo(i) => solo_layer(stack, sel, i),
            Act::Duplicate(i) => duplicate_layer(stack, sel, i),
            Act::Remove(i) => remove_layer(stack, sel, i),
        };
    }
    dirty
}

enum Act {
    SetEnabled(usize, bool),
    Move(usize, isize),
    MoveTo(usize, usize),
    Solo(usize),
    Duplicate(usize),
    Remove(usize),
}

/// Where the layer is allowed to act: an arc of the ring, and a strip across
/// the band.
///
/// This is the control worth more than any per-variant editor, because the
/// v-gate's side-face setting is what puts relief on the two faces square to
/// the mould pull — the ground measured at 0.000% undercut at every relief the
/// band can carry.
fn window_controls(ui: &mut egui::Ui, id: usize, w: &mut Window, ctx: &FieldContext) -> bool {
    let mut c = false;
    let v_max = ctx.band_v_len_mm.max(0.5);

    ui.horizontal_wrapped(|ui| {
        let mut on = !w.v_gate.is_off();
        if ui.checkbox(&mut on, "across the band").changed() {
            w.v_gate = if on {
                VGate::Band {
                    center_mm: ctx.crest_v_mm,
                    span_mm: (v_max * 0.4).max(0.5),
                    fade_mm: 0.4,
                }
            } else {
                VGate::Off
            };
            c = true;
        }
    });
    match &mut w.v_gate {
        VGate::Off => {}
        VGate::Band { center_mm, span_mm, fade_mm } => {
            c |= ui.add(egui::Slider::new(center_mm, 0.0..=v_max).text("centre v mm")).changed();
            c |= ui.add(egui::Slider::new(span_mm, 0.0..=v_max).text("span mm")).changed();
            c |= ui.add(egui::Slider::new(fade_mm, 0.0..=2.0).text("fade mm")).changed();
            if ui
                .small_button("Snap to side faces")
                .on_hover_text(
                    "Track the faces square to the pull instead of a fixed strip. Relief \
                     there pulls straight out, whatever the profile becomes.",
                )
                .clicked()
            {
                w.v_gate = VGate::SideFaces(SideFacePick::Wider);
                c = true;
            }
        }
        VGate::SideFaces(pick) => {
            let label = |p: SideFacePick| match p {
                SideFacePick::Low => "Low edge",
                SideFacePick::High => "High edge",
                SideFacePick::Wider => "Wider face",
                SideFacePick::Both => "Both faces",
            };
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("faces").small().weak());
                let cur = *pick;
                egui::ComboBox::from_id_salt(("vgate", id))
                    .selected_text(label(cur))
                    .show_ui(ui, |ui| {
                        for p in [
                            SideFacePick::Wider,
                            SideFacePick::Low,
                            SideFacePick::High,
                            SideFacePick::Both,
                        ] {
                            if ui.selectable_label(cur == p, label(p)).clicked() && cur != p {
                                *pick = p;
                                c = true;
                            }
                        }
                    });
            });
            if ctx.side_faces_std().is_none() {
                ui.label(
                    egui::RichText::new(
                        "This profile has no side faces — the layer passes nothing. \
                         Square the sides in Design.",
                    )
                    .small()
                    .color(egui::Color32::from_rgb(220, 170, 90)),
                );
            }
            if ui.small_button("Use a fixed strip").clicked() {
                w.v_gate = VGate::Band {
                    center_mm: ctx.crest_v_mm,
                    span_mm: (v_max * 0.4).max(0.5),
                    fade_mm: 0.4,
                };
                c = true;
            }
        }
    }

    ui.horizontal_wrapped(|ui| {
        c |= ui.checkbox(&mut w.enabled, "round the ring").changed();
        if w.enabled {
            c |= ui
                .checkbox(&mut w.invert, "outside")
                .on_hover_text("Keep the layer everywhere but the arc")
                .changed();
        }
    });
    if w.enabled {
        c |= ui.add(egui::Slider::new(&mut w.theta_deg, 0.0..=360.0).text("centre deg")).changed();
        c |= ui.add(egui::Slider::new(&mut w.span_deg, 0.0..=360.0).text("span deg")).changed();
        c |= ui.add(egui::Slider::new(&mut w.fade_deg, 0.0..=90.0).text("fade deg")).changed();
        if w.fade_deg < 1.0 && w.span_deg > 0.0 {
            ui.label(
                egui::RichText::new(
                    "No fade leaves a vertical wall at each end of the arc.",
                )
                .small()
                .color(egui::Color32::from_rgb(220, 170, 90)),
            );
        }
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use ringdesign_core::field::{Layer, LayerEntry, MilgrainLayer};

    fn stack(n: usize) -> LayerStack {
        let mut s = LayerStack::default();
        for i in 0..n {
            s.layers.push(LayerEntry::new(
                format!("L{i}"),
                Layer::Milgrain(MilgrainLayer::default()),
            ));
        }
        s
    }

    fn names(s: &LayerStack) -> Vec<String> {
        s.layers.iter().map(|e| e.name.clone()).collect()
    }

    #[test]
    fn moving_swaps_and_the_selection_follows() {
        let mut s = stack(3);
        let mut sel = Some(0);
        assert!(move_layer(&mut s, &mut sel, 0, 1));
        assert_eq!(names(&s), ["L1", "L0", "L2"]);
        assert_eq!(sel, Some(1), "the selection stays on the layer that moved");
    }

    #[test]
    fn moving_off_either_end_does_nothing() {
        let mut s = stack(3);
        let mut sel = Some(0);
        assert!(!move_layer(&mut s, &mut sel, 0, -1));
        assert!(!move_layer(&mut s, &mut sel, 2, 1));
        assert!(!move_layer(&mut s, &mut sel, 9, 1), "an out-of-range index is refused");
        assert_eq!(names(&s), ["L0", "L1", "L2"]);
    }

    #[test]
    fn send_to_top_and_bottom_rotate_rather_than_swap() {
        let mut s = stack(4);
        let mut sel = None;
        assert!(move_layer_to(&mut s, &mut sel, 3, 0));
        assert_eq!(names(&s), ["L3", "L0", "L1", "L2"], "not a swap with L0");
        assert_eq!(sel, Some(0));
        assert!(move_layer_to(&mut s, &mut sel, 0, 3));
        assert_eq!(names(&s), ["L0", "L1", "L2", "L3"]);
    }

    #[test]
    fn solo_mutes_the_others_and_toggles_back() {
        let mut s = stack(3);
        let mut sel = None;
        solo_layer(&mut s, &mut sel, 1);
        assert_eq!(
            s.layers.iter().map(|e| e.enabled).collect::<Vec<_>>(),
            [false, true, false]
        );
        // The same button is its own undo.
        solo_layer(&mut s, &mut sel, 1);
        assert!(s.layers.iter().all(|e| e.enabled));
    }

    /// Soloing a *different* layer from an already-soloed one moves the solo
    /// rather than restoring everything — the toggle keys on "this one only".
    #[test]
    fn soloing_a_second_layer_moves_the_solo() {
        let mut s = stack(3);
        let mut sel = None;
        solo_layer(&mut s, &mut sel, 0);
        solo_layer(&mut s, &mut sel, 2);
        assert_eq!(
            s.layers.iter().map(|e| e.enabled).collect::<Vec<_>>(),
            [false, false, true]
        );
    }

    #[test]
    fn a_copy_lands_directly_above_its_original() {
        let mut s = stack(2);
        let mut sel = None;
        assert!(duplicate_layer(&mut s, &mut sel, 0));
        assert_eq!(names(&s), ["L0", "L0 copy", "L1"]);
        assert_eq!(sel, Some(1), "and is selected, so the next edit lands on it");
    }

    #[test]
    fn removing_drops_the_selection_rather_than_leaving_it_dangling() {
        let mut s = stack(3);
        let mut sel = Some(2);
        assert!(remove_layer(&mut s, &mut sel, 2));
        assert_eq!(names(&s), ["L0", "L1"]);
        assert_eq!(sel, None, "a stale index would point at the wrong layer");
        assert!(!remove_layer(&mut s, &mut sel, 5));
    }
}

#[cfg(test)]
mod add_tests {
    use super::*;
    use ringdesign_core::{ProfileStyle, RingDesign};

    /// A wire across the crown leans back on its crest-side flank; the same wire
    /// on a face square to the pull measures 0.000%. The placement rule is the
    /// whole difference, so pin that it fires when there is a face to use.
    #[test]
    fn a_curve_lands_on_the_side_face_when_the_profile_has_one() {
        let mut d = RingDesign::default();
        d.profile.apply_style(ProfileStyle::Flat);
        d.profile.width_mm = 6.0;
        d.profile.flatten_sides();
        let ctx = d.field_context();
        assert!(ctx.side_faces_std().and_then(|s| s.wider()).is_some(), "test needs a side face");

        let mut sel = None;
        let where_ = place_curve(
            &mut d.layers,
            &mut sel,
            &ctx,
            "S-scroll",
            ringdesign_core::curve::CurveLayer::preset_scroll(&ctx),
        );
        assert_eq!(where_, "on the wider side face");
        let e = d.layers.layers.last().expect("added");
        assert!(
            matches!(e.window.v_gate, VGate::SideFaces(SideFacePick::Wider)),
            "the gate is what keeps it there as the profile changes: {:?}",
            e.window.v_gate
        );
        assert_eq!(sel, Some(0));
    }

    /// A half-round spends most of its thickness on the crown and honestly has
    /// no side face. Adding must still work and say so, not silently no-op.
    #[test]
    fn a_curve_on_a_domed_band_is_still_added_and_says_why() {
        let mut d = RingDesign::default();
        d.profile.apply_style(ProfileStyle::HalfRound);
        let ctx = d.field_context();
        let mut sel = None;
        let where_ = place_curve(
            &mut d.layers,
            &mut sel,
            &ctx,
            "Vine",
            ringdesign_core::curve::CurveLayer::preset_vine(&ctx),
        );
        assert!(where_.contains("no side face"), "{where_}");
        assert_eq!(d.layers.layers.len(), 1, "the layer is added either way");
    }

    #[test]
    fn add_layer_selects_what_it_added() {
        let mut s = LayerStack::default();
        let mut sel = None;
        for n in ["a", "b"] {
            add_layer(
                &mut s,
                &mut sel,
                n,
                ringdesign_core::field::Layer::Milgrain(Default::default()),
            );
        }
        assert_eq!(sel, Some(1), "the newest, so the editor opens on it");
        assert_eq!(s.layers.len(), 2);
    }
}

//! A native-feeling terminal rendered with ratatui inside a WASM plugin.
//!
//! Input handling is the point: egui's translated iOS keyboard text and `Key` events drive a
//! char-indexed line editor; touch drags and the scroll wheel move the scrollback; tapping
//! focuses the terminal and raises the iOS soft keyboard via `HostHandle::request_keyboard`.

mod calc;
mod editor;
mod render;
mod shell;
mod theme;

use egui_ios_plugin_sdk::{CreateConfig, HostHandle, PluginApp, egui, plugin};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use editor::LineEditor;
use render::TerminalSurface;
use shell::{Effect, OutLine};

/// Cap on retained scrollback lines.
const SCROLLBACK_CAP: usize = 2000;

struct Terminal {
    surface: TerminalSurface,
    editor: LineEditor,
    scrollback: Vec<OutLine>,
    /// Display rows scrolled up from the bottom; 0 pins to the newest output.
    scroll_offset: usize,
    /// Sub-row scroll remainder, so slow drags accumulate instead of being rounded away.
    scroll_accum: f32,
    /// Pointer position on the previous frame while a drag is in progress.
    last_pointer: Option<egui::Pos2>,
    /// The terminal has been tapped, so it wants the keyboard and shows a solid cursor.
    focused: bool,
    /// egui time of the last keystroke, for cursor-blink reset.
    last_input: f64,
    /// Bumped whenever scrollback changes; invalidates the wrapped-row cache.
    scrollback_rev: u64,
    /// Wrapped display rows cached by (scrollback_rev, width) so idle frames skip re-wrapping.
    display_cache: Vec<(String, Color)>,
    cache_key: Option<(u64, usize)>,
}

impl Terminal {
    fn new(_cfg: &CreateConfig) -> Self {
        let mut term = Terminal {
            surface: TerminalSurface::new(),
            editor: LineEditor::new(),
            scrollback: Vec::new(),
            scroll_offset: 0,
            scroll_accum: 0.0,
            last_pointer: None,
            focused: false,
            last_input: 0.0,
            scrollback_rev: 0,
            display_cache: Vec::new(),
            cache_key: None,
        };
        term.push(OutLine_new("Terminal ready — tap to type, `help` for commands.", theme::ACCENT));
        term.push(OutLine_new("swipe to scroll · ↑/↓ history · Ctrl+L clear", theme::DIM));
        term
    }

    fn push(&mut self, line: OutLine) {
        self.scrollback.push(line);
        if self.scrollback.len() > SCROLLBACK_CAP {
            let overflow = self.scrollback.len() - SCROLLBACK_CAP;
            self.scrollback.drain(0..overflow);
        }
        self.scrollback_rev = self.scrollback_rev.wrapping_add(1);
    }

    fn clear_scrollback(&mut self) {
        self.scrollback.clear();
        self.scrollback_rev = self.scrollback_rev.wrapping_add(1);
    }

    /// Wrapped display rows for `width`, rebuilt only when the scrollback or width changes.
    fn ensure_display(&mut self, width: usize) -> usize {
        if self.cache_key != Some((self.scrollback_rev, width)) {
            self.display_cache = self.display_rows(width);
            self.cache_key = Some((self.scrollback_rev, width));
        }
        self.display_cache.len()
    }

    fn submit(&mut self) {
        let line = self.editor.take();
        self.push(OutLine_new(format!("❯ {line}"), theme::DIM));
        let response = shell::run(&line, self.editor.history());
        if let Effect::Clear = response.effect {
            self.clear_scrollback();
        }
        for out in response.lines {
            self.push(out);
        }
        self.scroll_offset = 0;
    }

    /// Translate this frame's egui events into edits. Returns true if anything changed.
    fn handle_keys(&mut self, ui: &egui::Ui, host: &HostHandle) -> bool {
        let events = ui.input(|i| i.events.clone());
        let mut activity = false;
        for ev in events {
            match ev {
                egui::Event::Text(text) => {
                    for c in text.chars() {
                        if c == '\n' || c == '\r' {
                            self.submit();
                        } else if !c.is_control() {
                            self.editor.insert(c);
                        }
                        activity = true;
                    }
                }
                egui::Event::Key { key, pressed: true, modifiers, .. } => {
                    let ctrl = modifiers.ctrl || modifiers.command;
                    match key {
                        egui::Key::Enter => self.submit(),
                        egui::Key::Backspace => self.editor.backspace(),
                        egui::Key::Delete => self.editor.delete(),
                        egui::Key::ArrowLeft => self.editor.left(),
                        egui::Key::ArrowRight => self.editor.right(),
                        egui::Key::ArrowUp => self.editor.history_prev(),
                        egui::Key::ArrowDown => self.editor.history_next(),
                        egui::Key::Home => self.editor.home(),
                        egui::Key::End => self.editor.end(),
                        egui::Key::Escape => {
                            self.focused = false;
                            host.request_keyboard(false);
                        }
                        egui::Key::A if ctrl => self.editor.home(),
                        egui::Key::E if ctrl => self.editor.end(),
                        egui::Key::U if ctrl => self.editor.kill_to_start(),
                        egui::Key::L if ctrl => self.clear_scrollback(),
                        egui::Key::C if ctrl => {
                            self.push(OutLine_new(format!("❯ {}^C", self.editor.text()), theme::DIM));
                            self.editor.clear();
                        }
                        _ => {}
                    }
                    activity = true;
                }
                _ => {}
            }
        }
        if activity {
            self.scroll_offset = 0;
            self.last_input = ui.input(|i| i.time);
        }
        activity
    }

    fn handle_scroll(&mut self, ui: &egui::Ui, max_offset: usize) {
        let row_h = self.surface.cell.y.max(1.0);
        let (down, pos, wheel) = ui.input(|i| {
            (i.pointer.primary_down(), i.pointer.interact_pos(), i.smooth_scroll_delta.y)
        });
        // Grab-scroll: track the pointer's own vertical movement between frames while held.
        let mut drag = 0.0;
        match (down, pos) {
            (true, Some(p)) => {
                if let Some(prev) = self.last_pointer {
                    drag = p.y - prev.y;
                }
                self.last_pointer = Some(p);
            }
            _ => self.last_pointer = None,
        }
        // Dragging down (positive) reveals older lines above; accumulate sub-row remainders.
        self.scroll_accum += drag + wheel;
        let rows = (self.scroll_accum / row_h).trunc() as i64;
        if rows != 0 {
            self.scroll_accum -= rows as f32 * row_h;
            let next = self.scroll_offset as i64 + rows;
            self.scroll_offset = next.clamp(0, max_offset as i64) as usize;
        }
    }

    /// Build wrapped display rows from the scrollback for the given width.
    fn display_rows(&self, width: usize) -> Vec<(String, Color)> {
        let mut rows = Vec::new();
        for line in &self.scrollback {
            for piece in wrap(&line.text, width) {
                rows.push((piece, line.color));
            }
        }
        rows
    }

    fn draw(&mut self, blink_on: bool) {
        let (cols, rows) = self.surface.grid();
        let width = cols as usize;
        let body_rows = (rows as usize).saturating_sub(2);

        let total = self.ensure_display(width);
        let max_off = total.saturating_sub(body_rows);
        let scroll_offset = self.scroll_offset.min(max_off);
        self.scroll_offset = scroll_offset;
        let start = max_off - scroll_offset;
        let end = (start + body_rows).min(total);

        // Bottom-anchor: pad empty rows above short scrollback.
        let pad = body_rows.saturating_sub(end - start);
        let mut body_lines: Vec<Line> = Vec::with_capacity(body_rows);
        for _ in 0..pad {
            body_lines.push(Line::default());
        }
        for (text, color) in &self.display_cache[start..end] {
            body_lines.push(Line::from(Span::styled(text.clone(), Style::new().fg(*color))));
        }

        let header = self.header_line(width);
        let input = self.input_line(blink_on);

        self.surface
            .terminal_mut()
            .draw(|frame| {
                let areas = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)])
                    .split(frame.area());
                frame.render_widget(Paragraph::new(header), areas[0]);
                frame.render_widget(Paragraph::new(body_lines), areas[1]);
                frame.render_widget(Paragraph::new(input), areas[2]);
            })
            .expect("tui draw");
    }

    fn header_line(&self, width: usize) -> Line<'static> {
        let title = if self.focused { " terminal — tap to hide keyboard" } else { " terminal — tap to type" };
        let hint = if self.scroll_offset > 0 {
            format!("↓ {} newer ", self.scroll_offset)
        } else {
            String::new()
        };
        let pad = width.saturating_sub(title.chars().count() + hint.chars().count());
        Line::from(vec![
            Span::styled(title.to_string(), Style::new().fg(theme::ACCENT).add_modifier(Modifier::BOLD)),
            Span::raw(" ".repeat(pad)),
            Span::styled(hint, Style::new().fg(theme::WARNING)),
        ])
    }

    fn input_line(&self, blink_on: bool) -> Line<'static> {
        let mut spans = vec![Span::styled("❯ ", Style::new().fg(theme::ACCENT).add_modifier(Modifier::BOLD))];
        let chars = self.editor.chars();
        let cursor = self.editor.cursor();
        let cursor_style = Style::new().fg(theme::ACCENT).bg(theme::BG).add_modifier(Modifier::REVERSED);
        for (i, &c) in chars.iter().enumerate() {
            let style = if i == cursor && blink_on {
                cursor_style
            } else {
                Style::new().fg(theme::TEXT)
            };
            spans.push(Span::styled(c.to_string(), style));
        }
        if cursor >= chars.len() {
            let style = if blink_on { cursor_style } else { Style::new() };
            spans.push(Span::styled(" ", style));
        }
        Line::from(spans)
    }
}

// Free helper (module-name-collision-free) to build an OutLine.
#[allow(non_snake_case)]
fn OutLine_new(text: impl Into<String>, color: Color) -> OutLine {
    OutLine { text: text.into(), color }
}

impl PluginApp for Terminal {
    fn update(&mut self, ui: &mut egui::Ui, host: &HostHandle) {
        let avail = ui.available_size();
        let (rect, resp) = ui.allocate_exact_size(avail, egui::Sense::click_and_drag());

        // Tap toggles focus, so a soft-keyboard-only device can dismiss the keyboard by
        // tapping again — Escape is unreachable there.
        if resp.clicked() {
            self.focused = !self.focused;
            if self.focused {
                host.haptic(6);
            }
        }
        // Latched each frame: the host bridges this to the iOS soft keyboard.
        host.request_keyboard(self.focused);

        // Size the grid, then handle input against the fitted dimensions.
        let (_cols, rows) = self.surface.fit(ui, avail);
        self.handle_keys(ui, host);

        let width = self.surface.grid().0 as usize;
        let body_rows = (rows as usize).saturating_sub(2);
        let max_off = self.ensure_display(width).saturating_sub(body_rows);
        self.handle_scroll(ui, max_off);

        let time = ui.input(|i| i.time);
        let blink_on = if !self.focused {
            false
        } else if time - self.last_input < 0.5 {
            true
        } else {
            (time * 1.2) as i64 % 2 == 0
        };

        self.draw(blink_on);
        self.surface.paint(ui.painter(), rect);

        // Keep the cursor blinking and any drag momentum smooth.
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(120));
    }

    fn save_state(&self) -> Vec<u8> {
        // Preserve the scrollback text across hot reloads (colors reset to default).
        self.scrollback
            .iter()
            .map(|l| l.text.clone())
            .collect::<Vec<_>>()
            .join("\n")
            .into_bytes()
    }

    fn restore_state(&mut self, bytes: &[u8]) {
        if let Ok(text) = std::str::from_utf8(bytes) {
            self.scrollback = text
                .split('\n')
                .map(|t| OutLine_new(t.to_string(), theme::MUTED))
                .collect();
            self.scrollback_rev = self.scrollback_rev.wrapping_add(1);
        }
    }
}

/// Wrap `text` to `width` columns, breaking at spaces where possible without dropping content.
fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 || text.is_empty() {
        return vec![String::new()];
    }
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= width {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let mut end = (i + width).min(chars.len());
        if end < chars.len() {
            if let Some(sp) = (i + 1..end).rev().find(|&j| chars[j] == ' ') {
                end = sp;
            }
        }
        out.push(chars[i..end].iter().collect::<String>());
        i = end;
        if i < chars.len() && chars[i] == ' ' {
            i += 1; // consume the break space
        }
    }
    out
}

plugin!(Terminal::new);

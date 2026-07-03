//! A native-feeling terminal rendered with ratatui inside a WASM plugin.
//!
//! Two modes share one surface: a local pocket shell (line editor + built-in commands) and an
//! interactive SSH session (a VT emulator driven by the native `ssh.*` host ops). `ssh user@host`
//! or a hand-off from the Devices plugin opens a password prompt, then a full PTY.

mod calc;
mod editor;
mod render;
mod shell;
mod sshmode;
mod theme;
mod toolbar;
mod vt;

use egui_ios_plugin_sdk::abi::{self, net};
use egui_ios_plugin_sdk::{CreateConfig, HostHandle, PluginApp, egui, plugin};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use editor::LineEditor;
use render::TerminalSurface;
use shell::{Effect, OutLine};
use sshmode::{Phase, SshClient};
use toolbar::Act;

/// Cap on retained scrollback lines.
const SCROLLBACK_CAP: usize = 2000;

/// A password prompt while opening an SSH session.
struct AuthPrompt {
    user: String,
    host: String,
    port: u16,
    password: String,
}

enum Mode {
    Local,
    Auth(AuthPrompt),
    Ssh(SshClient),
}

struct Terminal {
    surface: TerminalSurface,
    editor: LineEditor,
    scrollback: Vec<OutLine>,
    scroll_offset: usize,
    scroll_accum: f32,
    last_pointer: Option<egui::Pos2>,
    focused: bool,
    last_input: f64,
    scrollback_rev: u64,
    display_cache: Vec<(String, Color)>,
    cache_key: Option<(u64, usize)>,
    mode: Mode,
    toolbar_hidden: bool,
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
            mode: Mode::Local,
            toolbar_hidden: false,
        };
        term.push(OutLine::new("Terminal ready -- tap to type, `help` for commands.", theme::ACCENT));
        term.push(OutLine::new("`ssh user@host` to connect - swipe to scroll - Up/Down history", theme::DIM));
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
        self.push(OutLine::new(format!("> {line}"), theme::DIM));
        if let Some((user, host, port)) = parse_ssh(&line) {
            self.mode = Mode::Auth(AuthPrompt { user, host, port, password: String::new() });
            self.focused = true;
            return;
        }
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
                            self.push(OutLine::new(format!("> {}^C", self.editor.text()), theme::DIM));
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
        self.scroll_accum += drag + wheel;
        let rows = (self.scroll_accum / row_h).trunc() as i64;
        if rows != 0 {
            self.scroll_accum -= rows as f32 * row_h;
            let next = self.scroll_offset as i64 + rows;
            self.scroll_offset = next.clamp(0, max_offset as i64) as usize;
        }
    }

    fn display_rows(&self, width: usize) -> Vec<(String, Color)> {
        let mut rows = Vec::new();
        for line in &self.scrollback {
            for piece in wrap(&line.text, width) {
                rows.push((piece, line.color));
            }
        }
        rows
    }

    fn blink(&self, time: f64) -> bool {
        if !self.focused {
            false
        } else if time - self.last_input < 0.5 {
            true
        } else {
            (time * 1.2) as i64 % 2 == 0
        }
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
        let title = if self.focused { " terminal -- tap to hide keyboard" } else { " terminal -- tap to type" };
        let hint = if self.scroll_offset > 0 {
            format!("v {} newer ", self.scroll_offset)
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

    // ── Local / auth console ─────────────────────────────────────────────────

    fn update_console(&mut self, ui: &mut egui::Ui, host: &HostHandle) {
        let avail = ui.available_size();
        let (rect, resp) = ui.allocate_exact_size(avail, egui::Sense::click_and_drag());
        if resp.clicked() {
            self.focused = !self.focused;
            if self.focused {
                host.haptic(6);
            }
        }
        host.request_keyboard(self.focused);
        let (_cols, rows) = self.surface.fit(ui, avail);

        self.handle_keys(ui, host);
        let width = self.surface.grid().0 as usize;
        let body_rows = (rows as usize).saturating_sub(2);
        let max_off = self.ensure_display(width).saturating_sub(body_rows);
        self.handle_scroll(ui, max_off);
        let time = ui.input(|i| i.time);
        let blink_on = self.blink(time);
        self.draw(blink_on);
        self.surface.paint(ui.painter(), rect);
    }

    fn update_auth(&mut self, ui: &mut egui::Ui, host: &HostHandle) {
        let full = ui.max_rect();
        let result = toolbar::render(ui, full, self.focused, self.toolbar_hidden);
        if result.toggle_hidden {
            self.toolbar_hidden = !self.toolbar_hidden;
        }
        let content = result.content;
        let resp = ui.allocate_rect(content, egui::Sense::click_and_drag());
        if resp.clicked() {
            self.focused = !self.focused;
            if self.focused {
                host.haptic(6);
            }
        }
        host.request_keyboard(self.focused);
        self.surface.fit(ui, content.size());

        let events = ui.input(|i| i.events.clone());
        let mut connect = false;
        let mut cancel = false;
        if let Mode::Auth(p) = &mut self.mode {
            for ev in events {
                match ev {
                    egui::Event::Text(t) => {
                        for c in t.chars() {
                            if !c.is_control() {
                                p.password.push(c);
                            }
                        }
                    }
                    egui::Event::Key { key: egui::Key::Enter, pressed: true, .. } => connect = true,
                    egui::Event::Key { key: egui::Key::Backspace, pressed: true, .. } => {
                        p.password.pop();
                    }
                    egui::Event::Key { key: egui::Key::Escape, pressed: true, .. } => cancel = true,
                    _ => {}
                }
            }
        }
        for &act in &result.actions {
            match act {
                Act::Submit => connect = true,
                Act::Cancel | Act::Disconnect => cancel = true,
                Act::Bytes(_) => {}
            }
        }

        if cancel {
            self.push(OutLine_new("ssh: cancelled", theme::DIM));
            self.mode = Mode::Local;
            return;
        }
        if connect {
            let (user, host_s, port, password) = if let Mode::Auth(p) = &self.mode {
                (p.user.clone(), p.host.clone(), p.port, p.password.clone())
            } else {
                return;
            };
            let (cols, rows_full) = self.surface.grid();
            let vt_rows = rows_full.saturating_sub(1).max(1);
            self.push(OutLine_new(format!("Connecting to {user}@{host_s}:{port}…"), theme::ACCENT));
            match SshClient::connect(
                host,
                &user,
                &host_s,
                port,
                net::SshAuth::Password(password),
                cols,
                vt_rows,
            ) {
                Ok(client) => self.mode = Mode::Ssh(client),
                Err(e) => {
                    self.push(OutLine_new(format!("ssh: {e}"), theme::ERROR));
                    self.mode = Mode::Local;
                }
            }
            return;
        }
        self.draw_auth();
        self.surface.paint(ui.painter(), content);
    }

    fn draw_auth(&mut self) {
        let (user, host_s, masked) = if let Mode::Auth(p) = &self.mode {
            (p.user.clone(), p.host.clone(), "•".repeat(p.password.chars().count()))
        } else {
            return;
        };
        let header = Line::from(Span::styled(
            format!(" SSH → {user}@{host_s}"),
            Style::new().fg(theme::ACCENT).add_modifier(Modifier::BOLD),
        ));
        let prompt = Line::from(vec![
            Span::styled("  password: ", Style::new().fg(theme::TEXT)),
            Span::styled(masked, Style::new().fg(theme::SUCCESS)),
            Span::styled("▏", Style::new().fg(theme::ACCENT)),
        ]);
        let hint = Line::from(Span::styled(
            "  Enter to connect · Esc to cancel",
            Style::new().fg(theme::DIM),
        ));
        self.surface
            .terminal_mut()
            .draw(|frame| {
                let areas = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(1), Constraint::Length(2), Constraint::Min(1)])
                    .split(frame.area());
                frame.render_widget(Paragraph::new(header), areas[0]);
                frame.render_widget(Paragraph::new(prompt), areas[1]);
                frame.render_widget(Paragraph::new(hint), areas[2]);
            })
            .expect("tui draw");
    }

    // ── SSH session ──────────────────────────────────────────────────────────

    fn update_ssh(&mut self, ui: &mut egui::Ui, host: &HostHandle) {
        let full = ui.max_rect();
        let result = toolbar::render(ui, full, self.focused, self.toolbar_hidden);
        if result.toggle_hidden {
            self.toolbar_hidden = !self.toolbar_hidden;
        }
        let content = result.content;
        let resp = ui.allocate_rect(content, egui::Sense::click_and_drag());
        if resp.clicked() {
            self.focused = !self.focused;
            if self.focused {
                host.haptic(6);
            }
        }
        host.request_keyboard(self.focused);
        let (cols, rows) = self.surface.fit(ui, content.size());
        let vt_rows = rows.saturating_sub(1).max(1);

        if let Mode::Ssh(c) = &mut self.mode {
            c.resize(host, cols, vt_rows);
            c.poll(host);
        }

        let ready = matches!(&self.mode, Mode::Ssh(c) if matches!(c.phase, Phase::Ready));
        let ended = matches!(&self.mode, Mode::Ssh(c) if matches!(c.phase, Phase::Ended(_)));

        // Toolbar keys → PTY bytes (or a disconnect).
        let mut bytes: Vec<u8> = Vec::new();
        let mut disconnect = false;
        for &act in &result.actions {
            match act {
                Act::Bytes(b) => bytes.extend_from_slice(b),
                Act::Submit => bytes.push(b'\r'),
                Act::Cancel => bytes.extend_from_slice(b"\x1b"),
                Act::Disconnect => disconnect = true,
            }
        }

        if disconnect {
            if let Mode::Ssh(c) = &self.mode {
                c.close(host);
            }
            self.push(OutLine_new("ssh: disconnected", theme::DIM));
            self.mode = Mode::Local;
            return;
        }

        if ended {
            // Once the session is over, a toolbar key, keypress, or tap returns to local.
            let go = !bytes.is_empty()
                || ui.input(|i| {
                    i.events.iter().any(|e| {
                        matches!(e, egui::Event::Key { pressed: true, .. })
                            || matches!(e, egui::Event::PointerButton { pressed: true, .. })
                    })
                });
            if go {
                if let Mode::Ssh(c) = &self.mode {
                    c.close(host);
                }
                self.mode = Mode::Local;
                return;
            }
        } else if ready {
            if self.focused {
                bytes.extend(sshmode::input_bytes(ui));
            }
            if let Mode::Ssh(c) = &self.mode {
                c.write(host, &bytes);
            }
        }

        self.draw_ssh();
        self.surface.paint(ui.painter(), content);
    }

    fn draw_ssh(&mut self) {
        let (header_text, body) = if let Mode::Ssh(c) = &self.mode {
            let text = match &c.phase {
                Phase::Connecting => format!(" ssh {}@{} — connecting…", c.user, c.host),
                Phase::Ready => format!(" ssh {}@{}", c.user, c.host),
                Phase::Ended(m) => format!(" ssh {}@{} — {m} · tap to return", c.user, c.host),
            };
            let show_cursor = self.focused && matches!(c.phase, Phase::Ready);
            (text, c.vt.to_lines(show_cursor))
        } else {
            return;
        };
        let header = Line::from(Span::styled(
            header_text,
            Style::new().fg(theme::ACCENT).add_modifier(Modifier::BOLD),
        ));
        self.surface
            .terminal_mut()
            .draw(|frame| {
                let areas = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(1), Constraint::Min(1)])
                    .split(frame.area());
                frame.render_widget(Paragraph::new(header), areas[0]);
                frame.render_widget(Paragraph::new(body), areas[1]);
            })
            .expect("tui draw");
    }
}

/// Free helper (module-name-collision-free) to build an OutLine.
#[allow(non_snake_case)]
fn OutLine_new(text: impl Into<String>, color: Color) -> OutLine {
    OutLine { text: text.into(), color }
}

impl PluginApp for Terminal {
    fn update(&mut self, ui: &mut egui::Ui, host: &HostHandle) {
        match self.mode {
            Mode::Ssh(_) => self.update_ssh(ui, host),
            Mode::Auth(_) => self.update_auth(ui, host),
            Mode::Local => self.update_console(ui, host),
        }
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(120));
    }

    fn on_host_event(&mut self, topic: &str, payload: &[u8], host: &HostHandle) {
        if topic == net::EVENT_SSH_OPEN {
            if let Ok(req) = abi::decode::<net::SshOpenRequest>(payload) {
                // Close a live session first so a new hand-off doesn't orphan its host thread.
                if let Mode::Ssh(c) = &self.mode {
                    c.close(host);
                }
                let port = if req.port == 0 { 22 } else { req.port };
                self.mode = Mode::Auth(AuthPrompt {
                    user: req.user,
                    host: req.host,
                    port,
                    password: String::new(),
                });
                self.focused = true;
            }
        }
    }

    fn save_state(&self) -> Vec<u8> {
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

/// Parse an `ssh [user@]host [-p port]` command line; `None` if it isn't an ssh command.
fn parse_ssh(line: &str) -> Option<(String, String, u16)> {
    let mut it = line.split_whitespace();
    if it.next()? != "ssh" {
        return None;
    }
    let toks: Vec<&str> = it.collect();
    let mut target: Option<&str> = None;
    let mut port = 22u16;
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i];
        if t == "-p" {
            if let Some(p) = toks.get(i + 1) {
                port = p.parse().unwrap_or(22);
                i += 1;
            }
        } else if let Some(rest) = t.strip_prefix("-p") {
            port = rest.parse().unwrap_or(22);
        } else if !t.starts_with('-') && target.is_none() {
            target = Some(t);
        }
        i += 1;
    }
    let target = target?;
    let (user, host) = match target.split_once('@') {
        Some((u, h)) => (u.to_string(), h.to_string()),
        None => ("root".to_string(), target.to_string()),
    };
    if host.is_empty() {
        return None;
    }
    Some((user, host, port))
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
            i += 1;
        }
    }
    out
}

plugin!(Terminal::new);

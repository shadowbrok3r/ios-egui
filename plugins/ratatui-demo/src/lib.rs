//! Ratatui running inside a WASM plugin: draws to a `TestBackend` buffer, which is painted
//! as a monospace cell grid with egui. Arrow keys / taps drive the list selection.

mod theme;
use theme::THEME;

use egui_ios_plugin_sdk::{CreateConfig, HostHandle, PluginApp, egui, plugin};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Gauge, List, ListItem, ListState, Paragraph, Sparkline};

const FONT_SIZE: f32 = 13.0;

struct TuiDemo {
    terminal: Terminal<TestBackend>,
    grid: (u16, u16),
    list_state: ListState,
    history: Vec<u64>,
}

impl TuiDemo {
    fn new(_cfg: &CreateConfig) -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        TuiDemo {
            terminal: Terminal::new(TestBackend::new(60, 20)).expect("terminal"),
            grid: (60, 20),
            list_state,
            history: Vec::new(),
        }
    }

    fn draw_tui(&mut self, time: f64, cols: u16, rows: u16) {
        if self.grid != (cols, rows) {
            self.terminal = Terminal::new(TestBackend::new(cols, rows)).expect("terminal resize");
            self.grid = (cols, rows);
        }

        let level = ((time * 1.4).sin() * 0.5 + 0.5) * 100.0;
        self.history.push(level as u64);
        if self.history.len() > 128 {
            self.history.remove(0);
        }

        let items: Vec<ListItem> = HOSTS
            .iter()
            .map(|h| ListItem::new(format!("  {h}")))
            .collect();
        let list_state = &mut self.list_state;
        let history = &self.history;

        self.terminal
            .draw(|frame| {
                let outer = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(3),
                        Constraint::Min(5),
                        Constraint::Length(4),
                    ])
                    .split(frame.area());

                frame.render_widget(
                    Paragraph::new("ratatui inside a WASM plugin -- Up/Down to select")
                        .style(THEME.title())
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_style(THEME.border(true))
                                .title(" terminal "),
                        ),
                    outer[0],
                );

                let middle = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                    .split(outer[1]);

                frame.render_stateful_widget(
                    List::new(items)
                        .style(Style::default().fg(THEME.text))
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_style(THEME.border(false))
                                .title(Line::from(" hosts ").style(THEME.title())),
                        )
                        .highlight_style(THEME.menu_highlight())
                        .highlight_symbol(">"),
                    middle[0],
                    list_state,
                );

                let right = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(3), Constraint::Min(2)])
                    .split(middle[1]);

                // Gauge color reflects load severity, exercising the semantic palette.
                let load_color = if level > 80.0 {
                    THEME.error
                } else if level > 50.0 {
                    THEME.warning
                } else {
                    THEME.success
                };
                frame.render_widget(
                    Gauge::default()
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_style(THEME.border(false))
                                .title(" load "),
                        )
                        .gauge_style(Style::default().fg(load_color))
                        .percent(level as u16),
                    right[0],
                );
                frame.render_widget(
                    Sparkline::default()
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_style(THEME.border(false))
                                .title(" history "),
                        )
                        .style(Style::default().fg(THEME.accent))
                        .data(history),
                    right[1],
                );

                frame.render_widget(
                    Paragraph::new(format!(
                        "selected: {}   cells: {cols}x{rows}",
                        HOSTS[list_state.selected().unwrap_or(0)]
                    ))
                    .style(Style::default().fg(THEME.text_muted))
                    .block(Block::default().borders(Borders::ALL).border_style(THEME.border(false))),
                    outer[2],
                );
            })
            .expect("tui draw");
    }
}

const HOSTS: &[&str] = &[
    "web-01.internal",
    "web-02.internal",
    "db-primary",
    "db-replica",
    "cache-01",
    "builds.local",
];

impl PluginApp for TuiDemo {
    fn update(&mut self, ui: &mut egui::Ui, host: &HostHandle) {
        // Keyboard drives the list like a real TUI.
        let (up, down) = ui.input(|i| {
            (
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::ArrowDown),
            )
        });
        if up || down {
            let len = HOSTS.len();
            let cur = self.list_state.selected().unwrap_or(0);
            let next = if down { (cur + 1) % len } else { (cur + len - 1) % len };
            self.list_state.select(Some(next));
            host.haptic(6);
        }

        let font = egui::FontId::monospace(FONT_SIZE);
        let (cell_w, cell_h) = ui
            .ctx()
            .fonts_mut(|f| (f.glyph_width(&font, 'M'), f.row_height(&font)));
        let avail = ui.available_size();
        let cols = ((avail.x / cell_w) as u16).clamp(20, 300);
        let rows = ((avail.y / cell_h) as u16).clamp(8, 200);

        let time = ui.input(|i| i.time);
        self.draw_tui(time, cols, rows);

        let (rect, response) = ui.allocate_exact_size(avail, egui::Sense::click());
        if response.clicked() {
            // Tap advances the selection so the demo is usable without a keyboard.
            let len = HOSTS.len();
            let cur = self.list_state.selected().unwrap_or(0);
            self.list_state.select(Some((cur + 1) % len));
            host.haptic(0);
        }

        let painter = ui.painter();
        painter.rect_filled(rect, 0.0, egui::Color32::from_rgb(10, 10, 16));

        let buffer = self.terminal.backend().buffer().clone();
        for y in 0..rows {
            // Coalesce consecutive cells with identical style into one text run.
            let mut x = 0u16;
            while x < cols {
                let cell = &buffer[(x, y)];
                let (fg, bg) = (cell.fg, cell.bg);
                let run_start = x;
                let mut text = String::new();
                while x < cols {
                    let c = &buffer[(x, y)];
                    if c.fg != fg || c.bg != bg {
                        break;
                    }
                    text.push_str(c.symbol());
                    x += 1;
                }
                let run_rect = egui::Rect::from_min_size(
                    rect.min
                        + egui::vec2(f32::from(run_start) * cell_w, f32::from(y) * cell_h),
                    egui::vec2(f32::from(x - run_start) * cell_w, cell_h),
                );
                if let Some(bg) = tui_color(bg, false) {
                    painter.rect_filled(run_rect, 0.0, bg);
                }
                if !text.trim().is_empty() {
                    painter.text(
                        run_rect.min,
                        egui::Align2::LEFT_TOP,
                        text,
                        font.clone(),
                        tui_color(fg, true).unwrap_or(egui::Color32::LIGHT_GRAY),
                    );
                }
            }
        }

        // Steady tick for the gauge/sparkline animation.
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
    }
}

/// Map a ratatui color to egui. `None` for a transparent/default background.
fn tui_color(color: Color, foreground: bool) -> Option<egui::Color32> {
    use egui::Color32 as C;
    Some(match color {
        Color::Reset => return if foreground { Some(C::LIGHT_GRAY) } else { None },
        Color::Black => C::from_rgb(20, 20, 20),
        Color::Red => C::from_rgb(204, 60, 60),
        Color::Green => C::from_rgb(70, 190, 90),
        Color::Yellow => C::from_rgb(210, 190, 60),
        Color::Blue => C::from_rgb(70, 110, 220),
        Color::Magenta => C::from_rgb(190, 80, 190),
        Color::Cyan => C::from_rgb(70, 190, 200),
        Color::Gray => C::from_rgb(170, 170, 170),
        Color::DarkGray => C::from_rgb(100, 100, 100),
        Color::LightRed => C::from_rgb(240, 110, 110),
        Color::LightGreen => C::from_rgb(120, 230, 130),
        Color::LightYellow => C::from_rgb(240, 230, 120),
        Color::LightBlue => C::from_rgb(120, 160, 250),
        Color::LightMagenta => C::from_rgb(230, 130, 230),
        Color::LightCyan => C::from_rgb(130, 230, 240),
        Color::White => C::from_rgb(235, 235, 235),
        Color::Rgb(r, g, b) => C::from_rgb(r, g, b),
        Color::Indexed(i) => indexed_color(i),
    })
}

/// Standard xterm 256-color palette approximation.
fn indexed_color(i: u8) -> egui::Color32 {
    match i {
        0..=15 => {
            let base = [
                (0, 0, 0), (205, 0, 0), (0, 205, 0), (205, 205, 0),
                (0, 0, 238), (205, 0, 205), (0, 205, 205), (229, 229, 229),
                (127, 127, 127), (255, 0, 0), (0, 255, 0), (255, 255, 0),
                (92, 92, 255), (255, 0, 255), (0, 255, 255), (255, 255, 255),
            ][i as usize];
            egui::Color32::from_rgb(base.0, base.1, base.2)
        }
        16..=231 => {
            let i = i - 16;
            let steps = [0u8, 95, 135, 175, 215, 255];
            egui::Color32::from_rgb(
                steps[(i / 36) as usize],
                steps[((i % 36) / 6) as usize],
                steps[(i % 6) as usize],
            )
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            egui::Color32::from_rgb(v, v, v)
        }
    }
}

plugin!(TuiDemo::new);

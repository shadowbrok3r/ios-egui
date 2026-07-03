//! Bridges a ratatui `TestBackend` buffer to egui: sizes the grid to the available space and
//! the monospace font, then paints each cell. This is what lets ratatui run natively inside an
//! egui plugin — the same surface receives egui's translated touch and keyboard input.

use egui_ios_plugin_sdk::egui;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Color;

pub const FONT_SIZE: f32 = 14.0;

pub struct TerminalSurface {
    terminal: Terminal<TestBackend>,
    grid: (u16, u16),
    pub cell: egui::Vec2,
}

impl TerminalSurface {
    pub fn new() -> Self {
        TerminalSurface {
            terminal: Terminal::new(TestBackend::new(80, 24)).expect("terminal"),
            grid: (80, 24),
            cell: egui::vec2(8.0, FONT_SIZE),
        }
    }

    pub fn font(&self) -> egui::FontId {
        egui::FontId::monospace(FONT_SIZE)
    }

    /// Compute the cell grid for `avail`, resizing the ratatui backend if it changed.
    pub fn fit(&mut self, ui: &egui::Ui, avail: egui::Vec2) -> (u16, u16) {
        let font = self.font();
        let (cw, ch) = ui
            .ctx()
            .fonts_mut(|f| (f.glyph_width(&font, 'M'), f.row_height(&font)));
        self.cell = egui::vec2(cw.max(1.0), ch.max(1.0));
        let cols = ((avail.x / self.cell.x) as u16).clamp(20, 400);
        let rows = ((avail.y / self.cell.y) as u16).clamp(6, 300);
        if self.grid != (cols, rows) {
            self.terminal = Terminal::new(TestBackend::new(cols, rows)).expect("resize");
            self.grid = (cols, rows);
        }
        (cols, rows)
    }

    pub fn grid(&self) -> (u16, u16) {
        self.grid
    }

    pub fn terminal_mut(&mut self) -> &mut Terminal<TestBackend> {
        &mut self.terminal
    }

    /// Paint the current buffer into `rect`, coalescing runs of same-styled cells into one text
    /// shape per run (keeps the shape count low even on a large grid).
    pub fn paint(&self, painter: &egui::Painter, rect: egui::Rect) {
        let font = self.font();
        let (cols, rows) = self.grid;
        let buffer = self.terminal.backend().buffer();
        painter.rect_filled(rect, 0.0, color_to_egui(crate::theme::BG, false).unwrap());

        for y in 0..rows {
            let mut x = 0u16;
            while x < cols {
                let first = &buffer[(x, y)];
                let (fg, bg, reversed) = (first.fg, first.bg, is_reversed(first));
                let run_start = x;
                let mut text = String::new();
                while x < cols {
                    let c = &buffer[(x, y)];
                    if c.fg != fg || c.bg != bg || is_reversed(c) != reversed {
                        break;
                    }
                    text.push_str(c.symbol());
                    x += 1;
                }
                let run = egui::Rect::from_min_size(
                    rect.min + egui::vec2(f32::from(run_start) * self.cell.x, f32::from(y) * self.cell.y),
                    egui::vec2(f32::from(x - run_start) * self.cell.x, self.cell.y),
                );
                let (mut fg_e, bg_e) = (color_to_egui(fg, true), color_to_egui(bg, false));
                let (fg_e, bg_e) = if reversed {
                    // A reversed cell (the block cursor) swaps fg/bg.
                    let bg = fg_e.unwrap_or(egui::Color32::LIGHT_GRAY);
                    (bg_e, Some(bg))
                } else {
                    (std::mem::take(&mut fg_e), bg_e)
                };
                if let Some(bg) = bg_e {
                    painter.rect_filled(run, 0.0, bg);
                }
                if !text.trim().is_empty() {
                    painter.text(
                        run.min,
                        egui::Align2::LEFT_TOP,
                        text,
                        font.clone(),
                        fg_e.unwrap_or(egui::Color32::LIGHT_GRAY),
                    );
                }
            }
        }
    }
}

fn is_reversed(cell: &ratatui::buffer::Cell) -> bool {
    cell.modifier.contains(ratatui::style::Modifier::REVERSED)
}

/// Map a ratatui color to egui. `None` = transparent/default background.
pub fn color_to_egui(color: Color, foreground: bool) -> Option<egui::Color32> {
    use egui::Color32 as C;
    Some(match color {
        Color::Reset => return if foreground { Some(color_to_egui(crate::theme::TEXT, true).unwrap()) } else { None },
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
        Color::Indexed(i) => indexed(i),
    })
}

fn indexed(i: u8) -> egui::Color32 {
    match i {
        0..=15 => {
            let p = [
                (0, 0, 0), (205, 0, 0), (0, 205, 0), (205, 205, 0),
                (0, 0, 238), (205, 0, 205), (0, 205, 205), (229, 229, 229),
                (127, 127, 127), (255, 0, 0), (0, 255, 0), (255, 255, 0),
                (92, 92, 255), (255, 0, 255), (0, 255, 255), (255, 255, 255),
            ][i as usize];
            egui::Color32::from_rgb(p.0, p.1, p.2)
        }
        16..=231 => {
            let i = i - 16;
            let s = [0u8, 95, 135, 175, 215, 255];
            egui::Color32::from_rgb(s[(i / 36) as usize], s[((i % 36) / 6) as usize], s[(i % 6) as usize])
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            egui::Color32::from_rgb(v, v, v)
        }
    }
}

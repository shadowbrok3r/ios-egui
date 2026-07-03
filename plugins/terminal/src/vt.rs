//! A pragmatic VT/xterm screen emulator: enough of the escape-sequence set to drive an
//! interactive login shell, `ls`/`git`, pagers, `vim` and `htop` over SSH. Bytes go in via
//! [`Vt::feed`]; the screen renders to ratatui `Line`s that the terminal surface paints.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use vte::{Params, Parser, Perform};

#[derive(Clone, PartialEq)]
struct Cell {
    ch: char,
    fg: Color,
    bg: Color,
    bold: bool,
    reverse: bool,
}

impl Cell {
    fn blank(bg: Color) -> Self {
        Cell { ch: ' ', fg: Color::Reset, bg, bold: false, reverse: false }
    }
}

struct Pen {
    fg: Color,
    bg: Color,
    bold: bool,
    reverse: bool,
}

impl Pen {
    fn reset(&mut self) {
        self.fg = Color::Reset;
        self.bg = Color::Reset;
        self.bold = false;
        self.reverse = false;
    }
}

struct Screen {
    cols: usize,
    rows: usize,
    cells: Vec<Cell>,
    cx: usize,
    cy: usize,
    pen: Pen,
    scroll_top: usize,
    scroll_bot: usize,
    cursor_visible: bool,
    saved: Option<(usize, usize)>,
    /// Main-screen backup while an alternate screen is active.
    alt_backup: Option<(Vec<Cell>, usize, usize)>,
}

impl Screen {
    fn new(cols: usize, rows: usize) -> Self {
        Screen {
            cols,
            rows,
            cells: vec![Cell::blank(Color::Reset); cols * rows],
            cx: 0,
            cy: 0,
            pen: Pen { fg: Color::Reset, bg: Color::Reset, bold: false, reverse: false },
            scroll_top: 0,
            scroll_bot: rows.saturating_sub(1),
            cursor_visible: true,
            saved: None,
            alt_backup: None,
        }
    }

    fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let mut next = vec![Cell::blank(Color::Reset); cols * rows];
        // Copy the overlapping top-left region so a resize doesn't wipe the screen.
        for y in 0..rows.min(self.rows) {
            for x in 0..cols.min(self.cols) {
                next[y * cols + x] = self.cells[y * self.cols + x].clone();
            }
        }
        self.cells = next;
        self.cols = cols;
        self.rows = rows;
        self.cx = self.cx.min(cols - 1);
        self.cy = self.cy.min(rows - 1);
        self.scroll_top = 0;
        self.scroll_bot = rows - 1;
    }

    fn cell_mut(&mut self, x: usize, y: usize) -> &mut Cell {
        &mut self.cells[y * self.cols + x]
    }

    fn put(&mut self, c: char) {
        if self.cx >= self.cols {
            self.cx = 0;
            self.line_feed();
        }
        let (cols, pen_fg, pen_bg, bold, reverse) =
            (self.cols, self.pen.fg, self.pen.bg, self.pen.bold, self.pen.reverse);
        let idx = self.cy * cols + self.cx;
        if let Some(cell) = self.cells.get_mut(idx) {
            *cell = Cell { ch: c, fg: pen_fg, bg: pen_bg, bold, reverse };
        }
        self.cx += 1;
    }

    fn line_feed(&mut self) {
        if self.cy == self.scroll_bot {
            self.scroll_up(1);
        } else if self.cy + 1 < self.rows {
            self.cy += 1;
        }
    }

    fn reverse_index(&mut self) {
        if self.cy == self.scroll_top {
            self.scroll_down(1);
        } else if self.cy > 0 {
            self.cy -= 1;
        }
    }

    fn scroll_up(&mut self, n: usize) {
        let bg = self.pen.bg;
        for _ in 0..n {
            for y in self.scroll_top..self.scroll_bot {
                for x in 0..self.cols {
                    self.cells[y * self.cols + x] = self.cells[(y + 1) * self.cols + x].clone();
                }
            }
            for x in 0..self.cols {
                self.cells[self.scroll_bot * self.cols + x] = Cell::blank(bg);
            }
        }
    }

    fn scroll_down(&mut self, n: usize) {
        let bg = self.pen.bg;
        for _ in 0..n {
            let mut y = self.scroll_bot;
            while y > self.scroll_top {
                for x in 0..self.cols {
                    self.cells[y * self.cols + x] = self.cells[(y - 1) * self.cols + x].clone();
                }
                y -= 1;
            }
            for x in 0..self.cols {
                self.cells[self.scroll_top * self.cols + x] = Cell::blank(bg);
            }
        }
    }

    fn erase_line(&mut self, mode: u16) {
        let bg = self.pen.bg;
        let (start, end) = match mode {
            1 => (0, self.cx + 1),
            2 => (0, self.cols),
            _ => (self.cx, self.cols),
        };
        for x in start..end.min(self.cols) {
            *self.cell_mut(x, self.cy) = Cell::blank(bg);
        }
    }

    fn erase_display(&mut self, mode: u16) {
        let bg = self.pen.bg;
        let cell = self.cy * self.cols + self.cx;
        let (start, end) = match mode {
            1 => (0, cell + 1),
            2 | 3 => (0, self.cells.len()),
            _ => (cell, self.cells.len()),
        };
        for i in start..end.min(self.cells.len()) {
            self.cells[i] = Cell::blank(bg);
        }
    }

    fn insert_lines(&mut self, n: usize) {
        if self.cy < self.scroll_top || self.cy > self.scroll_bot {
            return;
        }
        let bg = self.pen.bg;
        for _ in 0..n {
            let mut y = self.scroll_bot;
            while y > self.cy {
                for x in 0..self.cols {
                    self.cells[y * self.cols + x] = self.cells[(y - 1) * self.cols + x].clone();
                }
                y -= 1;
            }
            for x in 0..self.cols {
                self.cells[self.cy * self.cols + x] = Cell::blank(bg);
            }
        }
    }

    fn delete_lines(&mut self, n: usize) {
        if self.cy < self.scroll_top || self.cy > self.scroll_bot {
            return;
        }
        let bg = self.pen.bg;
        for _ in 0..n {
            for y in self.cy..self.scroll_bot {
                for x in 0..self.cols {
                    self.cells[y * self.cols + x] = self.cells[(y + 1) * self.cols + x].clone();
                }
            }
            for x in 0..self.cols {
                self.cells[self.scroll_bot * self.cols + x] = Cell::blank(bg);
            }
        }
    }

    fn delete_chars(&mut self, n: usize) {
        let bg = self.pen.bg;
        let row = self.cy * self.cols;
        for x in self.cx..self.cols {
            let src = x + n;
            self.cells[row + x] = if src < self.cols {
                self.cells[row + src].clone()
            } else {
                Cell::blank(bg)
            };
        }
    }

    fn insert_chars(&mut self, n: usize) {
        let bg = self.pen.bg;
        let row = self.cy * self.cols;
        let mut x = self.cols;
        while x > self.cx {
            x -= 1;
            self.cells[row + x] = if x >= self.cx + n {
                self.cells[row + x - n].clone()
            } else {
                Cell::blank(bg)
            };
        }
    }

    fn enter_alt(&mut self) {
        if self.alt_backup.is_none() {
            self.alt_backup = Some((self.cells.clone(), self.cx, self.cy));
            let bg = self.pen.bg;
            for c in &mut self.cells {
                *c = Cell::blank(bg);
            }
            self.cx = 0;
            self.cy = 0;
        }
    }

    fn leave_alt(&mut self) {
        if let Some((cells, cx, cy)) = self.alt_backup.take() {
            if cells.len() == self.cells.len() {
                self.cells = cells;
            }
            self.cx = cx.min(self.cols - 1);
            self.cy = cy.min(self.rows - 1);
        }
    }

    fn apply_sgr(&mut self, flat: &[u16]) {
        let mut i = 0;
        if flat.is_empty() {
            self.pen.reset();
            return;
        }
        while i < flat.len() {
            match flat[i] {
                0 => self.pen.reset(),
                1 => self.pen.bold = true,
                22 => self.pen.bold = false,
                7 => self.pen.reverse = true,
                27 => self.pen.reverse = false,
                30..=37 => self.pen.fg = basic_color(flat[i] - 30),
                39 => self.pen.fg = Color::Reset,
                40..=47 => self.pen.bg = basic_color(flat[i] - 40),
                49 => self.pen.bg = Color::Reset,
                90..=97 => self.pen.fg = basic_color(flat[i] - 90 + 8),
                100..=107 => self.pen.bg = basic_color(flat[i] - 100 + 8),
                38 | 48 => {
                    let is_fg = flat[i] == 38;
                    let color = match flat.get(i + 1) {
                        Some(5) => {
                            let c = flat.get(i + 2).copied().unwrap_or(0);
                            i += 2;
                            Color::Indexed(c as u8)
                        }
                        Some(2) => {
                            let r = flat.get(i + 2).copied().unwrap_or(0) as u8;
                            let g = flat.get(i + 3).copied().unwrap_or(0) as u8;
                            let b = flat.get(i + 4).copied().unwrap_or(0) as u8;
                            i += 4;
                            Color::Rgb(r, g, b)
                        }
                        _ => Color::Reset,
                    };
                    if is_fg {
                        self.pen.fg = color;
                    } else {
                        self.pen.bg = color;
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }
}

fn basic_color(n: u16) -> Color {
    match n {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        7 => Color::Gray,
        8 => Color::DarkGray,
        9 => Color::LightRed,
        10 => Color::LightGreen,
        11 => Color::LightYellow,
        12 => Color::LightBlue,
        13 => Color::LightMagenta,
        14 => Color::LightCyan,
        _ => Color::White,
    }
}

/// First subparam of the `idx`-th param, or `default` when absent or zero.
fn arg(params: &Params, idx: usize, default: u16) -> u16 {
    match params.iter().nth(idx).and_then(|p| p.first().copied()) {
        Some(0) | None => default,
        Some(v) => v,
    }
}

impl Perform for Screen {
    fn print(&mut self, c: char) {
        self.put(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x0a | 0x0b | 0x0c => self.line_feed(),
            0x0d => self.cx = 0,
            0x08 => self.cx = self.cx.saturating_sub(1),
            0x09 => {
                let next = ((self.cx / 8) + 1) * 8;
                self.cx = next.min(self.cols.saturating_sub(1));
            }
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        let private = intermediates.first() == Some(&b'?');
        match action {
            'A' => self.cy = self.cy.saturating_sub(arg(params, 0, 1) as usize),
            'B' | 'e' => self.cy = (self.cy + arg(params, 0, 1) as usize).min(self.rows - 1),
            'C' | 'a' => self.cx = (self.cx + arg(params, 0, 1) as usize).min(self.cols - 1),
            'D' => self.cx = self.cx.saturating_sub(arg(params, 0, 1) as usize),
            'E' => {
                self.cy = (self.cy + arg(params, 0, 1) as usize).min(self.rows - 1);
                self.cx = 0;
            }
            'F' => {
                self.cy = self.cy.saturating_sub(arg(params, 0, 1) as usize);
                self.cx = 0;
            }
            'G' | '`' => self.cx = (arg(params, 0, 1) as usize - 1).min(self.cols - 1),
            'd' => self.cy = (arg(params, 0, 1) as usize - 1).min(self.rows - 1),
            'H' | 'f' => {
                self.cy = (arg(params, 0, 1) as usize - 1).min(self.rows - 1);
                self.cx = (arg(params, 1, 1) as usize - 1).min(self.cols - 1);
            }
            'J' => self.erase_display(arg(params, 0, 0)),
            'K' => self.erase_line(arg(params, 0, 0)),
            'L' => self.insert_lines(arg(params, 0, 1) as usize),
            'M' => self.delete_lines(arg(params, 0, 1) as usize),
            'P' => self.delete_chars(arg(params, 0, 1) as usize),
            '@' => self.insert_chars(arg(params, 0, 1) as usize),
            'S' => self.scroll_up(arg(params, 0, 1) as usize),
            'T' => self.scroll_down(arg(params, 0, 1) as usize),
            'm' => {
                let flat: Vec<u16> = params.iter().flat_map(|p| p.iter().copied()).collect();
                self.apply_sgr(&flat);
            }
            'r' => {
                let top = arg(params, 0, 1) as usize - 1;
                let bot = arg(params, 1, self.rows as u16) as usize - 1;
                if top < bot && bot < self.rows {
                    self.scroll_top = top;
                    self.scroll_bot = bot;
                    self.cx = 0;
                    self.cy = top;
                }
            }
            'h' if private => self.set_mode(arg(params, 0, 0), true),
            'l' if private => self.set_mode(arg(params, 0, 0), false),
            's' => self.saved = Some((self.cx, self.cy)),
            'u' => {
                if let Some((x, y)) = self.saved {
                    self.cx = x.min(self.cols - 1);
                    self.cy = y.min(self.rows - 1);
                }
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
        match byte {
            b'M' => self.reverse_index(),
            b'D' => self.line_feed(),
            b'E' => {
                self.line_feed();
                self.cx = 0;
            }
            b'7' => self.saved = Some((self.cx, self.cy)),
            b'8' => {
                if let Some((x, y)) = self.saved {
                    self.cx = x.min(self.cols - 1);
                    self.cy = y.min(self.rows - 1);
                }
            }
            b'c' => {
                self.pen.reset();
                self.erase_display(2);
                self.cx = 0;
                self.cy = 0;
            }
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {}
}

impl Screen {
    fn set_mode(&mut self, mode: u16, on: bool) {
        match mode {
            25 => self.cursor_visible = on,
            47 | 1047 | 1049 => {
                if on {
                    self.enter_alt();
                } else {
                    self.leave_alt();
                }
            }
            _ => {}
        }
    }
}

/// The public emulator: a vte parser feeding a [`Screen`].
pub struct Vt {
    parser: Parser,
    screen: Screen,
}

impl Vt {
    pub fn new(cols: usize, rows: usize) -> Self {
        Vt { parser: Parser::new(), screen: Screen::new(cols.max(1), rows.max(1)) }
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        if (cols, rows) != (self.screen.cols, self.screen.rows) {
            self.screen.resize(cols, rows);
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.parser.advance(&mut self.screen, b);
        }
    }

    /// Render the screen to ratatui lines. When `show_cursor`, the cursor cell is reversed.
    pub fn to_lines(&self, show_cursor: bool) -> Vec<Line<'static>> {
        let s = &self.screen;
        let cursor_on = show_cursor && s.cursor_visible;
        let mut lines = Vec::with_capacity(s.rows);
        for y in 0..s.rows {
            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut run = String::new();
            let mut run_style = Style::default();
            let mut run_open = false;
            for x in 0..s.cols {
                let cell = &s.cells[y * s.cols + x];
                let is_cursor = cursor_on && x == s.cx && y == s.cy;
                let style = cell_style(cell, is_cursor);
                if run_open && style == run_style {
                    run.push(cell.ch);
                } else {
                    if run_open {
                        spans.push(Span::styled(std::mem::take(&mut run), run_style));
                    }
                    run.push(cell.ch);
                    run_style = style;
                    run_open = true;
                }
            }
            if run_open {
                spans.push(Span::styled(run, run_style));
            }
            lines.push(Line::from(spans));
        }
        lines
    }
}

fn cell_style(cell: &Cell, is_cursor: bool) -> Style {
    let mut style = Style::default().fg(cell.fg).bg(cell.bg);
    if cell.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if cell.reverse ^ is_cursor {
        style = style.add_modifier(Modifier::REVERSED);
    }
    style
}

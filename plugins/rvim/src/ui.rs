//! All ratatui drawing: bufferline, gutter, syntax-colored text, cursor, visual/search
//! highlights, statusline, command/message line, touchbar, finder overlay, dashboard.

use std::num::NonZeroU16;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::{Buffer as Grid, CellDiffOption};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use crate::buffer::{Position, TextBuffer};
use crate::finder::FinderState;
use crate::highlight::{Highlighter, HlSpan};
use crate::state::{Buffer as EdBuffer, EditorState, MsgKind};
use crate::theme;
use crate::vim::{Key, Mode, VimEngine};

/// Per-frame inputs the renderer needs beyond the editor state.
pub struct DrawCtx<'a> {
    pub st: &'a mut EditorState,
    pub vim: &'a VimEngine,
    pub hl: &'a mut Highlighter,
    /// Blink phase: the cursor cell renders solid when true.
    pub blink_on: bool,
    /// Sticky-Ctrl armed from the touchbar.
    pub ctrl_armed: bool,
    pub focused: bool,
}

/// Where things landed on the grid, for mapping taps back to buffer positions.
#[derive(Clone, Copy, Default)]
pub struct LayoutInfo {
    /// Columns taken by the line-number gutter.
    pub gutter_w: u16,
    /// First grid row of the text area.
    pub text_top: u16,
    pub text_rows: u16,
    /// Grid row of the touchbar.
    pub touchbar_row: u16,
    pub cols: u16,
}

/// A tap on the touchbar resolves to one of these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TouchAction {
    Key(Key),
    /// Arm Ctrl for the next key.
    StickyCtrl,
    ToggleKeyboard,
}

/// Vertical scrolloff margin in rows.
const V_MARGIN: usize = 2;
/// Horizontal scrolloff margin in columns.
const H_MARGIN: usize = 4;
/// Char budget for the matching-bracket scan.
const BRACKET_BUDGET: usize = 2000;

/// Row assignment for each chrome element; `None` = dropped for lack of space.
struct VLayout {
    bufferline: Option<u16>,
    text_top: u16,
    text_rows: u16,
    status: Option<u16>,
    cmdline: Option<u16>,
    touchbar: u16,
}

/// Split `rows` into chrome rows, dropping bufferline, then cmdline, then statusline.
fn vertical_layout(rows: u16) -> VLayout {
    let touchbar = rows.saturating_sub(1);
    match touchbar {
        0 => VLayout { bufferline: None, text_top: 0, text_rows: 0, status: None, cmdline: None, touchbar },
        1 => VLayout { bufferline: None, text_top: 0, text_rows: 1, status: None, cmdline: None, touchbar },
        2 => VLayout { bufferline: None, text_top: 0, text_rows: 1, status: Some(1), cmdline: None, touchbar },
        3 => VLayout { bufferline: None, text_top: 0, text_rows: 1, status: Some(1), cmdline: Some(2), touchbar },
        r => VLayout {
            bufferline: Some(0),
            text_top: 1,
            text_rows: r - 3,
            status: Some(r - 2),
            cmdline: Some(r - 1),
            touchbar,
        },
    }
}

/// Draw one frame; also scroll-follows the cursor (mutates the active buffer's scroll).
pub fn draw(term: &mut Terminal<TestBackend>, ctx: DrawCtx) -> LayoutInfo {
    let area = term.backend().buffer().area;
    let (cols, rows) = (area.width, area.height);
    let vl = vertical_layout(rows);

    let DrawCtx { st, vim, hl, blink_on, ctrl_armed, focused } = ctx;

    let gutter_w = match st.buf() {
        Some(b) => gutter_width(b.text.line_count(), st.options.number, st.options.relativenumber, cols),
        None => 0,
    };
    if let Some(b) = st.buf_mut() {
        follow_scroll(b, vl.text_rows, cols.saturating_sub(gutter_w));
    }

    let st: &EditorState = st;
    let spans: &[Vec<HlSpan>] = match st.buf() {
        Some(b) => hl.spans(&b.name, &b.text),
        None => &[],
    };

    let info = LayoutInfo {
        gutter_w,
        text_top: vl.text_top,
        text_rows: vl.text_rows,
        touchbar_row: vl.touchbar,
        cols,
    };

    let _ = term.draw(|frame| {
        let g = frame.buffer_mut();
        g.set_style(area, Style::new().fg(theme::TEXT).bg(theme::BG));
        if let Some(row) = vl.bufferline {
            draw_bufferline(g, row, cols, st);
        }
        if st.buf().is_some() {
            draw_text(g, &vl, gutter_w, cols, st, vim, spans, blink_on);
        } else {
            draw_dashboard(g, &vl, cols);
        }
        if let Some(row) = vl.status {
            draw_statusline(g, row, cols, st, vim, &vl);
        }
        if let Some(row) = vl.cmdline {
            draw_cmdline(g, row, cols, st, vim, blink_on, focused);
        }
        draw_touchbar(g, vl.touchbar, cols, ctrl_armed);
        if let Some(f) = &st.finder {
            draw_finder(g, &vl, cols, f);
        }
    });

    info
}

/// Resolve a tap at grid column `col` on the touchbar row.
pub fn touchbar_action_at(col: u16, cols: u16) -> Option<TouchAction> {
    touchbar_cells(cols)
        .into_iter()
        .find(|&(start, end, _, _)| col >= start && col < end)
        .map(|(_, _, _, action)| action)
}

const TOUCH_FULL: [(&str, TouchAction); 10] = [
    ("esc", TouchAction::Key(Key::Esc)),
    ("ctrl", TouchAction::StickyCtrl),
    ("tab", TouchAction::Key(Key::Tab)),
    (":", TouchAction::Key(Key::Char(':'))),
    ("/", TouchAction::Key(Key::Char('/'))),
    ("←", TouchAction::Key(Key::Left)),
    ("↓", TouchAction::Key(Key::Down)),
    ("↑", TouchAction::Key(Key::Up)),
    ("→", TouchAction::Key(Key::Right)),
    ("⌨", TouchAction::ToggleKeyboard),
];

const TOUCH_NARROW: [(&str, TouchAction); 6] = [
    ("esc", TouchAction::Key(Key::Esc)),
    ("ctrl", TouchAction::StickyCtrl),
    ("tab", TouchAction::Key(Key::Tab)),
    (":", TouchAction::Key(Key::Char(':'))),
    ("/", TouchAction::Key(Key::Char('/'))),
    ("⌨", TouchAction::ToggleKeyboard),
];

/// Evenly-spread touchbar cells `(start, end_exclusive, label, action)` partitioning `[0, cols)`.
fn touchbar_cells(cols: u16) -> Vec<(u16, u16, &'static str, TouchAction)> {
    let items: &[(&'static str, TouchAction)] = if cols < 40 { &TOUCH_NARROW } else { &TOUCH_FULL };
    let n = items.len() as u32;
    items
        .iter()
        .enumerate()
        .map(|(i, &(label, action))| {
            let start = (i as u32 * cols as u32 / n) as u16;
            let end = ((i as u32 + 1) * cols as u32 / n) as u16;
            (start, end, label, action)
        })
        .collect()
}

/// Gutter width: digits(line_count).max(3) + 1, or 0 when numbers are off or space is tight.
fn gutter_width(line_count: usize, number: bool, relativenumber: bool, cols: u16) -> u16 {
    if !number && !relativenumber {
        return 0;
    }
    let mut digits: u16 = 1;
    let mut n = line_count;
    while n >= 10 {
        digits += 1;
        n /= 10;
    }
    let w = digits.max(3) + 1;
    if w.saturating_add(4) > cols { 0 } else { w }
}

/// Clamp scroll and move it so the cursor stays in view with scrolloff margins.
fn follow_scroll(buf: &mut EdBuffer, text_rows: u16, text_cols: u16) {
    let line_count = buf.text.line_count();
    let cur = buf.text.clamp(buf.cursor, true);
    let (mut top, mut left) = buf.scroll;
    top = top.min(line_count.saturating_sub(1));

    let rows = text_rows as usize;
    if rows > 0 {
        let margin = V_MARGIN.min(rows.saturating_sub(1) / 2);
        let below = margin.min(line_count.saturating_sub(1) - cur.line);
        if cur.line < top + margin {
            top = cur.line.saturating_sub(margin);
        } else if cur.line + below >= top + rows {
            top = cur.line + below + 1 - rows;
        }
        top = top.min(line_count.saturating_sub(1));
    }

    let width = text_cols as usize;
    if width > 0 {
        let margin = H_MARGIN.min(width.saturating_sub(1) / 2);
        let right = margin.min(buf.text.line_len(cur.line).saturating_sub(cur.col));
        if cur.col < left + margin {
            left = cur.col.saturating_sub(margin);
        } else if cur.col + right >= left + width {
            left = cur.col + right + 1 - width;
        }
    }

    buf.scroll = (top, left);
}

/// Set one cell's char and style; diff width pinned to 1 so wide chars keep cell-per-char.
fn set_cell(g: &mut Grid, x: u16, y: u16, ch: char, style: Style) {
    if let Some(cell) = g.cell_mut((x, y)) {
        cell.set_char(ch);
        cell.set_style(style);
        cell.set_diff_option(CellDiffOption::ForcedWidth(NonZeroU16::MIN));
    }
}

/// Write `s` one char per cell starting at `x`, clipped at `max_x`; returns the next x.
fn put_str(g: &mut Grid, x: u16, y: u16, s: &str, style: Style, max_x: u16) -> u16 {
    let mut x = x;
    for ch in s.chars() {
        if x >= max_x {
            break;
        }
        set_cell(g, x, y, ch, style);
        x += 1;
    }
    x
}

fn draw_bufferline(g: &mut Grid, row: u16, cols: u16, st: &EditorState) {
    let base = Style::new().fg(theme::DIM).bg(theme::SURFACE_DIM);
    g.set_style(Rect::new(0, row, cols, 1), base);
    let brand = Style::new().fg(theme::ACCENT).bg(theme::SURFACE_DIM);
    if st.buffers.is_empty() {
        if cols >= 5 {
            put_str(g, cols - 5, row, "rvim ", brand, cols);
        }
        return;
    }
    let active = st.active.min(st.buffers.len() - 1);
    let labels: Vec<String> = st
        .buffers
        .iter()
        .map(|b| if b.modified() { format!(" {} ● ", b.name) } else { format!(" {} ", b.name) })
        .collect();
    // First tab shown, advanced until the active tab fits on screen.
    let mut start = 0usize;
    while start < active {
        let total: usize = labels[start..=active].iter().map(|l| l.chars().count()).sum();
        if total <= cols as usize {
            break;
        }
        start += 1;
    }
    let mut x = 0u16;
    for (i, label) in labels.iter().enumerate().skip(start) {
        if x >= cols {
            break;
        }
        let style = if i == active {
            Style::new().fg(theme::ACCENT).bg(theme::SURFACE)
        } else {
            base
        };
        x = put_str(g, x, row, label, style, cols);
    }
    if x + 6 <= cols {
        put_str(g, cols - 5, row, "rvim ", brand, cols);
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_text(
    g: &mut Grid,
    vl: &VLayout,
    gutter_w: u16,
    cols: u16,
    st: &EditorState,
    vim: &VimEngine,
    spans: &[Vec<HlSpan>],
    blink_on: bool,
) {
    let Some(b) = st.buf() else { return };
    if vl.text_rows == 0 || cols == 0 {
        return;
    }
    let (top, left) = b.scroll;
    let text_cols = cols.saturating_sub(gutter_w) as usize;
    if text_cols == 0 {
        return;
    }
    let line_count = b.text.line_count();
    let sel = vim.visual_range(b.cursor);
    let pat: Vec<char> = if !st.search.pattern.is_empty() && !st.search.suppressed {
        st.search.pattern.chars().collect()
    } else {
        Vec::new()
    };
    let bracket = bracket_pair(&b.text, b.cursor);

    for r in 0..vl.text_rows {
        let y = vl.text_top + r;
        let li = top + r as usize;
        if li >= line_count {
            put_str(g, 0, y, "~", Style::new().fg(theme::DIM), cols);
            continue;
        }
        let cursor_line = li == b.cursor.line;
        if cursor_line {
            g.set_style(Rect::new(0, y, cols, 1), Style::new().bg(theme::CURSORLINE_BG));
        }
        if gutter_w > 0 {
            let num = if cursor_line || !st.options.relativenumber {
                li + 1
            } else {
                li.abs_diff(b.cursor.line)
            };
            let fg = if cursor_line { theme::LINENR_CUR } else { theme::LINENR };
            let bg = if cursor_line { theme::CURSORLINE_BG } else { theme::BG };
            let s = format!("{:>w$} ", num, w = (gutter_w - 1) as usize);
            put_str(g, 0, y, &s, Style::new().fg(fg).bg(bg), gutter_w);
        }
        let line = b.text.line(li);
        let row_spans: &[HlSpan] = spans.get(li).map(Vec::as_slice).unwrap_or(&[]);
        let matches = if pat.is_empty() {
            Vec::new()
        } else {
            line_matches(line, &pat, left.saturating_sub(pat.len()), text_cols + 2 * pat.len())
        };
        let mut si = 0usize;
        for (ci, ch) in line.chars().enumerate().skip(left).take(text_cols) {
            while si < row_spans.len() && row_spans[si].start + row_spans[si].len <= ci {
                si += 1;
            }
            let base_fg = match row_spans.get(si) {
                Some(sp) if sp.start <= ci => sp.color,
                _ => theme::TEXT,
            };
            let search = matches
                .iter()
                .find(|&&(s, l)| ci >= s && ci < s + l)
                .map(|&(s, l)| cursor_line && b.cursor.col >= s && b.cursor.col < s + l);
            let in_bracket = bracket
                .map_or(false, |(a, p)| (a.line == li && a.col == ci) || (p.line == li && p.col == ci));
            let (fg, bg) =
                cell_colors(base_fg, cursor_line, in_bracket, search, in_visual(&sel, li, ci));
            let x = gutter_w + (ci - left) as u16;
            set_cell(g, x, y, ch, Style::new().fg(fg).bg(bg));
        }
        // Linewise selection covers the text region past the line's last char too.
        if let Some((s, e, true)) = sel {
            if li >= s.line && li <= e.line {
                let drawn = b.text.line_len(li).saturating_sub(left).min(text_cols);
                let from = gutter_w + drawn as u16;
                if from < cols {
                    g.set_style(Rect::new(from, y, cols - from, 1), Style::new().bg(theme::VISUAL_BG));
                }
            }
        }
    }

    if blink_on && vim.cmdline().is_none() && st.finder.is_none() {
        let cur = b.cursor;
        let in_view = cur.line >= top
            && cur.line < top + vl.text_rows as usize
            && cur.col >= left
            && cur.col < left + text_cols;
        if in_view {
            let x = gutter_w + (cur.col - left) as u16;
            let y = vl.text_top + (cur.line - top) as u16;
            if let Some(cell) = g.cell_mut((x, y)) {
                if vim.mode() == Mode::Insert {
                    cell.set_style(Style::new().fg(theme::BG).bg(theme::ACCENT));
                } else {
                    cell.set_style(Style::new().add_modifier(Modifier::REVERSED));
                }
            }
        }
    }
}

/// Resolve a text cell's colors; later overlays win: search < current < visual.
fn cell_colors(
    base_fg: Color,
    cursor_line: bool,
    bracket: bool,
    search: Option<bool>,
    visual: bool,
) -> (Color, Color) {
    let mut fg = base_fg;
    let mut bg = if cursor_line { theme::CURSORLINE_BG } else { theme::BG };
    if bracket {
        bg = theme::MATCH_BG;
    }
    if let Some(current) = search {
        fg = theme::SEARCH_FG;
        bg = if current { theme::SEARCH_CUR_BG } else { theme::SEARCH_BG };
    }
    if visual {
        fg = base_fg;
        bg = theme::VISUAL_BG;
    }
    (fg, bg)
}

/// Whether `(line, col)` falls inside the selection; charwise includes the end col.
fn in_visual(sel: &Option<(Position, Position, bool)>, line: usize, col: usize) -> bool {
    let Some((s, e, linewise)) = sel else { return false };
    if line < s.line || line > e.line {
        return false;
    }
    if *linewise {
        return true;
    }
    (line > s.line || col >= s.col) && (line < e.line || col <= e.col)
}

/// Non-overlapping literal matches of `pat` within `line`, scanning chars `[from, from+span)`.
fn line_matches(line: &str, pat: &[char], from: usize, span: usize) -> Vec<(usize, usize)> {
    let plen = pat.len();
    if plen == 0 {
        return Vec::new();
    }
    let window: Vec<char> = line.chars().skip(from).take(span).collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + plen <= window.len() {
        if window[i..i + plen] == *pat {
            out.push((from + i, plen));
            i += plen;
        } else {
            i += 1;
        }
    }
    out
}

/// Partner of the bracket under the cursor, scanning at most `BRACKET_BUDGET` chars.
fn bracket_pair(text: &TextBuffer, cursor: Position) -> Option<(Position, Position)> {
    let ch = text.char_at(cursor)?;
    let (open, close, forward) = match ch {
        '(' => ('(', ')', true),
        '[' => ('[', ']', true),
        '{' => ('{', '}', true),
        ')' => ('(', ')', false),
        ']' => ('[', ']', false),
        '}' => ('{', '}', false),
        _ => return None,
    };
    let mut depth: usize = 1;
    let mut budget = BRACKET_BUDGET;
    if forward {
        let mut line = cursor.line;
        let mut skip = cursor.col + 1;
        while line < text.line_count() {
            for (i, c) in text.line(line).chars().enumerate().skip(skip) {
                if budget == 0 {
                    return None;
                }
                budget -= 1;
                if c == open {
                    depth += 1;
                } else if c == close {
                    depth -= 1;
                    if depth == 0 {
                        return Some((cursor, Position::new(line, i)));
                    }
                }
            }
            line += 1;
            skip = 0;
        }
    } else {
        let mut line = cursor.line;
        loop {
            let upto = if line == cursor.line {
                cursor.col
            } else {
                text.line_len(line)
            };
            let chars: Vec<char> = text.line(line).chars().take(upto).collect();
            for i in (0..chars.len()).rev() {
                if budget == 0 {
                    return None;
                }
                budget -= 1;
                let c = chars[i];
                if c == close {
                    depth += 1;
                } else if c == open {
                    depth -= 1;
                    if depth == 0 {
                        return Some((Position::new(line, i), cursor));
                    }
                }
            }
            if line == 0 {
                break;
            }
            line -= 1;
        }
    }
    None
}

fn draw_statusline(g: &mut Grid, row: u16, cols: u16, st: &EditorState, vim: &VimEngine, vl: &VLayout) {
    let base = Style::new().fg(theme::MUTED).bg(theme::SURFACE_DIM);
    g.set_style(Rect::new(0, row, cols, 1), base);
    let label = vim.mode_label();
    let (cfg, cbg) = match label {
        "INSERT" => theme::MODE_INSERT,
        "VISUAL" | "V-LINE" => theme::MODE_VISUAL,
        "REPLACE" => theme::MODE_REPLACE,
        "COMMAND" | "SEARCH" => theme::MODE_COMMAND,
        _ => theme::MODE_NORMAL,
    };
    let x = put_str(g, 0, row, &format!(" {} ", label), Style::new().fg(cfg).bg(cbg), cols);
    if let Some(b) = st.buf() {
        let name = format!(" {}{}", b.name, if b.modified() { " [+]" } else { "" });
        put_str(g, x, row, &name, base, cols);
    }
    let mut right = String::new();
    if !vim.pending_display().is_empty() {
        right.push_str(vim.pending_display());
        right.push_str("  ");
    }
    if let Some(b) = st.buf() {
        let pct = scroll_pct(b.scroll.0, vl.text_rows as usize, b.text.line_count());
        right.push_str(&format!("{}:{}  {} ", b.cursor.line + 1, b.cursor.col + 1, pct));
    }
    let w = right.chars().count() as u16;
    if w > 0 && w <= cols {
        put_str(g, cols - w, row, &right, base, cols);
    }
}

/// Statusline scroll indicator: All / Top / Bot / NN%.
fn scroll_pct(top: usize, rows: usize, line_count: usize) -> String {
    if line_count <= rows {
        "All".into()
    } else if top == 0 {
        "Top".into()
    } else if top + rows >= line_count {
        "Bot".into()
    } else {
        format!("{}%", top * 100 / (line_count - rows))
    }
}

fn draw_cmdline(g: &mut Grid, row: u16, cols: u16, st: &EditorState, vim: &VimEngine, blink_on: bool, focused: bool) {
    if cols == 0 {
        return;
    }
    if let Some((prefix, text, cur)) = vim.cmdline() {
        let avail = cols.saturating_sub(1) as usize;
        // Window start keeping the char cursor visible.
        let start = if avail > 0 && cur >= avail { cur + 1 - avail } else { 0 };
        set_cell(g, 0, row, prefix, Style::new().fg(theme::TEXT));
        let mut x = 1u16;
        for ch in text.chars().skip(start) {
            if x >= cols {
                break;
            }
            set_cell(g, x, row, ch, Style::new().fg(theme::TEXT));
            x += 1;
        }
        if blink_on {
            let cx = 1 + (cur - start) as u16;
            if cx < cols {
                if let Some(cell) = g.cell_mut((cx, row)) {
                    cell.set_style(Style::new().add_modifier(Modifier::REVERSED));
                }
            }
        }
    } else if let Some(msg) = &st.status {
        let fg = match msg.kind {
            MsgKind::Error => theme::ERROR,
            MsgKind::Info => theme::MUTED,
        };
        put_str(g, 0, row, &msg.text, Style::new().fg(fg), cols);
    } else if !focused {
        put_str(g, 0, row, "tap to type — :help for keys", Style::new().fg(theme::DIM), cols);
    }
}

fn draw_touchbar(g: &mut Grid, row: u16, cols: u16, ctrl_armed: bool) {
    let base = Style::new().fg(theme::TEXT).bg(theme::SURFACE);
    g.set_style(Rect::new(0, row, cols, 1), base);
    for (start, end, label, action) in touchbar_cells(cols) {
        let w = end.saturating_sub(start);
        if w == 0 {
            continue;
        }
        let style = if matches!(action, TouchAction::StickyCtrl) && ctrl_armed {
            let armed = Style::new().fg(theme::BG).bg(theme::ACCENT);
            g.set_style(Rect::new(start, row, w, 1), armed);
            armed
        } else {
            base
        };
        let lw = label.chars().count() as u16;
        let x = start + w.saturating_sub(lw) / 2;
        put_str(g, x, row, label, style, end);
    }
}

fn draw_finder(g: &mut Grid, vl: &VLayout, cols: u16, f: &FinderState) {
    if vl.text_rows < 3 || cols < 10 {
        return;
    }
    let w = ((cols as u32 * 4 / 5) as u16).clamp(10, cols);
    let max_h = ((vl.text_rows as u32 * 3 / 5) as u16).clamp(3, vl.text_rows);
    let h = ((f.results.len() as u16).saturating_add(3)).clamp(3, max_h);
    let x0 = (cols - w) / 2;
    let y0 = vl.text_top + (vl.text_rows - h) / 2;
    let x1 = x0 + w - 1;
    let y1 = y0 + h - 1;

    for y in y0..=y1 {
        for x in x0..=x1 {
            set_cell(g, x, y, ' ', Style::new().fg(theme::TEXT).bg(theme::BG));
        }
    }
    let border = Style::new().fg(theme::ACCENT).bg(theme::BG);
    for x in x0 + 1..x1 {
        for y in [y0, y1] {
            set_cell(g, x, y, '─', border);
        }
    }
    for y in y0 + 1..y1 {
        for x in [x0, x1] {
            set_cell(g, x, y, '│', border);
        }
    }
    for (x, y, ch) in [(x0, y0, '┌'), (x1, y0, '┐'), (x0, y1, '└'), (x1, y1, '┘')] {
        set_cell(g, x, y, ch, border);
    }
    put_str(g, x0 + 2, y0, " find file ", border, x1);

    let prompt_y = y0 + 1;
    let px = put_str(g, x0 + 2, prompt_y, "> ", border, x1);
    let px = put_str(g, px, prompt_y, &f.query, Style::new().fg(theme::TEXT).bg(theme::BG), x1);
    put_str(g, px, prompt_y, "▉", border, x1);

    let visible = h.saturating_sub(3) as usize;
    if visible == 0 {
        return;
    }
    let off = f.selected.saturating_sub(visible - 1);
    for (i, (name, hits)) in f.results.iter().enumerate().skip(off).take(visible) {
        let y = y0 + 2 + (i - off) as u16;
        let selected = i == f.selected;
        let bg = if selected { theme::SURFACE } else { theme::BG };
        if selected {
            g.set_style(Rect::new(x0 + 1, y, w - 2, 1), Style::new().bg(theme::SURFACE));
            put_str(g, x0 + 1, y, "▸", Style::new().fg(theme::ACCENT).bg(bg), x1);
        }
        let mut x = x0 + 3;
        for (ci, ch) in name.chars().enumerate() {
            if x >= x1 {
                break;
            }
            let fg = if hits.contains(&ci) { theme::ACCENT } else { theme::TEXT };
            set_cell(g, x, y, ch, Style::new().fg(fg).bg(bg));
            x += 1;
        }
    }
}

const LOGO: [&str; 5] = [
    r"              _            ",
    r" _ __ __   __(_) _ __ ___  ",
    r"| '__|\ \ / /| || '_ ` _ \ ",
    r"| |    \ V / | || | | | | |",
    r"|_|     \_/  |_||_| |_| |_|",
];

fn draw_dashboard(g: &mut Grid, vl: &VLayout, cols: u16) {
    if vl.text_rows == 0 {
        return;
    }
    let version = concat!("rvim v", env!("CARGO_PKG_VERSION"));
    let mut lines: Vec<(&str, Color)> = Vec::with_capacity(LOGO.len() + 7);
    for l in LOGO {
        lines.push((l, theme::ACCENT));
    }
    lines.push(("", theme::MUTED));
    lines.push((version, theme::MUTED));
    lines.push(("", theme::DIM));
    lines.push((":e <file>   new/open file", theme::DIM));
    lines.push(("Space f f   find file", theme::DIM));
    lines.push((":help       cheatsheet", theme::DIM));
    lines.push(("tap ⌨ to hide keyboard", theme::DIM));
    let total = lines.len() as u16;
    let start = vl.text_top + vl.text_rows.saturating_sub(total) / 2;
    for (i, (text, color)) in lines.iter().enumerate() {
        let y = start + i as u16;
        if y >= vl.text_top + vl.text_rows {
            break;
        }
        let w = text.chars().count() as u16;
        let x = cols.saturating_sub(w) / 2;
        put_str(g, x, y, text, Style::new().fg(*color), cols);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_state(text: &str) -> EditorState {
        let mut st = EditorState::new();
        st.buffers.push(EdBuffer::new("main.rs", text));
        st
    }

    fn render(st: &mut EditorState, w: u16, h: u16, blink: bool) -> (Terminal<TestBackend>, LayoutInfo) {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        let vim = VimEngine::new();
        let mut hl = Highlighter::new();
        let info = draw(
            &mut term,
            DrawCtx { st, vim: &vim, hl: &mut hl, blink_on: blink, ctrl_armed: false, focused: true },
        );
        (term, info)
    }

    fn row_text(term: &Terminal<TestBackend>, y: u16) -> String {
        let b = term.backend().buffer();
        (0..b.area.width).map(|x| b[(x, y)].symbol().to_string()).collect()
    }

    fn find_row(term: &Terminal<TestBackend>, needle: &str) -> Option<u16> {
        let area = term.backend().buffer().area;
        (0..area.height).find(|&y| row_text(term, y).contains(needle))
    }

    #[test]
    fn touchbar_hit_test_matches_cells() {
        for cols in [1u16, 5, 20, 39, 40, 80, 200] {
            let cells = touchbar_cells(cols);
            for col in 0..cols {
                let expect = cells
                    .iter()
                    .find(|&&(s, e, _, _)| col >= s && col < e)
                    .map(|&(_, _, _, a)| a);
                assert!(expect.is_some(), "col {col} uncovered at {cols} cols");
                assert_eq!(touchbar_action_at(col, cols), expect);
            }
            assert_eq!(touchbar_action_at(cols, cols), None);
        }
    }

    #[test]
    fn touchbar_narrow_drops_arrows() {
        let narrow = touchbar_cells(20);
        assert!(!narrow.iter().any(|&(_, _, l, _)| l == "←"));
        assert!(narrow.iter().any(|&(_, _, l, _)| l == "esc"));
        assert!(narrow.iter().any(|&(_, _, l, _)| l == "ctrl"));
        assert!(narrow.iter().any(|&(_, _, l, _)| l == "⌨"));
        let full = touchbar_cells(80);
        assert_eq!(full.len(), 10);
        assert!(full.iter().any(|&(_, _, l, _)| l == "↑"));
    }

    #[test]
    fn gutter_numbers_relative_and_absolute() {
        let mut st = mk_state("alpha\nbravo\ncharlie\ndelta\necho");
        st.buffers[0].cursor = Position::new(2, 0);
        let (term, info) = render(&mut st, 40, 10, false);
        assert_eq!(info.gutter_w, 4);
        assert_eq!(info.text_top, 1);
        assert_eq!(info.text_rows, 6);
        assert_eq!(info.touchbar_row, 9);
        assert!(row_text(&term, 1).starts_with("  2 alpha"));
        assert!(row_text(&term, 2).starts_with("  1 bravo"));
        assert!(row_text(&term, 3).starts_with("  3 charlie"));
        assert!(row_text(&term, 4).starts_with("  1 delta"));
        assert!(row_text(&term, 6).starts_with("~"));
    }

    #[test]
    fn gutter_absolute_when_norelativenumber() {
        let mut st = mk_state("alpha\nbravo\ncharlie");
        st.options.relativenumber = false;
        st.buffers[0].cursor = Position::new(1, 0);
        let (term, _) = render(&mut st, 40, 10, false);
        assert!(row_text(&term, 1).starts_with("  1 alpha"));
        assert!(row_text(&term, 2).starts_with("  2 bravo"));
        assert!(row_text(&term, 3).starts_with("  3 charlie"));
    }

    #[test]
    fn gutter_hidden_when_numbers_off() {
        let mut st = mk_state("alpha");
        st.options.number = false;
        st.options.relativenumber = false;
        let (term, info) = render(&mut st, 40, 10, false);
        assert_eq!(info.gutter_w, 0);
        assert!(row_text(&term, 1).starts_with("alpha"));
    }

    #[test]
    fn statusline_mode_chip_and_position() {
        let mut st = mk_state("alpha\nbravo\ncharlie\ndelta\necho");
        st.buffers[0].cursor = Position::new(2, 0);
        let (term, _) = render(&mut st, 40, 10, false);
        let status = row_text(&term, 7);
        assert!(status.contains(" NORMAL "), "{status:?}");
        assert!(status.contains("main.rs"));
        assert!(status.contains("3:1"));
        assert!(status.contains("All"));
        let b = term.backend().buffer();
        assert_eq!(b[(1, 7)].bg, theme::MODE_NORMAL.1);
        assert_eq!(b[(1, 7)].fg, theme::MODE_NORMAL.0);
    }

    #[test]
    fn bufferline_shows_tabs_and_modified_dot() {
        let mut st = mk_state("one");
        st.buffers.push(EdBuffer::new("lib.rs", "two"));
        st.buffers[1].text.insert_char(Position::new(0, 0), 'x');
        st.active = 1;
        let (term, _) = render(&mut st, 40, 10, false);
        let top = row_text(&term, 0);
        assert!(top.contains("main.rs"));
        assert!(top.contains("lib.rs ●"));
        assert!(top.contains("rvim"));
    }

    #[test]
    fn bufferline_keeps_active_visible_when_overflowing() {
        let mut st = EditorState::new();
        for i in 0..5 {
            st.buffers.push(EdBuffer::new(&format!("verylongfilename{i}.rs"), ""));
        }
        st.active = 4;
        let (term, _) = render(&mut st, 30, 10, false);
        assert!(row_text(&term, 0).contains("verylongfilename4"), "{:?}", row_text(&term, 0));
    }

    #[test]
    fn cursor_cell_reversed_and_cursorline_bg() {
        let mut st = mk_state("alpha\nbravo\ncharlie");
        st.buffers[0].cursor = Position::new(2, 1);
        let (term, info) = render(&mut st, 40, 10, true);
        let b = term.backend().buffer();
        let x = info.gutter_w + 1;
        assert!(b[(x, 3)].modifier.contains(Modifier::REVERSED));
        assert_eq!(b[(20, 3)].bg, theme::CURSORLINE_BG);
        assert_eq!(b[(20, 1)].bg, theme::BG);
    }

    #[test]
    fn no_cursor_when_blink_off() {
        let mut st = mk_state("alpha");
        let (term, info) = render(&mut st, 40, 10, false);
        let b = term.backend().buffer();
        assert!(!b[(info.gutter_w, 1)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn search_matches_highlight_with_current() {
        let mut st = mk_state("say hello hello");
        st.search.pattern = "hello".into();
        st.buffers[0].cursor = Position::new(0, 4);
        let (term, info) = render(&mut st, 40, 10, false);
        let b = term.backend().buffer();
        let g = info.gutter_w;
        for col in 4..9 {
            assert_eq!(b[(g + col, 1)].bg, theme::SEARCH_CUR_BG, "col {col}");
            assert_eq!(b[(g + col, 1)].fg, theme::SEARCH_FG);
        }
        for col in 10..15 {
            assert_eq!(b[(g + col, 1)].bg, theme::SEARCH_BG, "col {col}");
        }
        assert_eq!(b[(g + 3, 1)].bg, theme::CURSORLINE_BG);
    }

    #[test]
    fn search_suppressed_hides_matches() {
        let mut st = mk_state("say hello");
        st.search.pattern = "hello".into();
        st.search.suppressed = true;
        st.buffers[0].cursor = Position::new(0, 0);
        let (term, info) = render(&mut st, 40, 10, false);
        let b = term.backend().buffer();
        assert_eq!(b[(info.gutter_w + 4, 1)].bg, theme::CURSORLINE_BG);
    }

    #[test]
    fn bracket_match_highlights_partner() {
        let mut st = mk_state("fn main() {}");
        st.buffers[0].cursor = Position::new(0, 7);
        let (term, info) = render(&mut st, 40, 10, false);
        let b = term.backend().buffer();
        assert_eq!(b[(info.gutter_w + 7, 1)].bg, theme::MATCH_BG);
        assert_eq!(b[(info.gutter_w + 8, 1)].bg, theme::MATCH_BG);
    }

    #[test]
    fn message_line_shows_error_in_red() {
        let mut st = mk_state("alpha");
        st.error("E492: not an editor command: nope");
        let (term, _) = render(&mut st, 40, 10, false);
        let y = find_row(&term, "E492").expect("message row");
        assert_eq!(y, 8);
        let b = term.backend().buffer();
        assert_eq!(b[(0, y)].fg, theme::ERROR);
    }

    #[test]
    fn touchbar_row_labels_drawn() {
        let mut st = mk_state("alpha");
        let (term, info) = render(&mut st, 80, 10, false);
        let bar = row_text(&term, info.touchbar_row);
        for label in ["esc", "ctrl", "tab", ":", "/", "←", "↓", "↑", "→", "⌨"] {
            assert!(bar.contains(label), "missing {label} in {bar:?}");
        }
        let b = term.backend().buffer();
        assert_eq!(b[(0, info.touchbar_row)].bg, theme::SURFACE);
    }

    #[test]
    fn scroll_follows_cursor_far_away() {
        let text: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let mut st = mk_state(text.trim_end());
        st.buffers[0].cursor = Position::new(50, 0);
        let (_, info) = render(&mut st, 40, 14, false);
        let (top, _) = st.buffers[0].scroll;
        assert_eq!(info.text_rows, 10);
        assert_eq!(top, 43);
        assert!(50 >= top + 2 && 50 < top + 10 - 2);
    }

    #[test]
    fn scroll_recovers_from_wild_drag_values() {
        let mut st = mk_state("short");
        st.buffers[0].scroll = (9999, 9999);
        let (term, _) = render(&mut st, 40, 10, false);
        assert_eq!(st.buffers[0].scroll, (0, 0));
        assert!(row_text(&term, 1).contains("short"));
    }

    #[test]
    fn follow_scroll_margins() {
        let text: String = vec!["x"; 100].join("\n");
        let mut b = EdBuffer::new("t", &text);
        b.cursor = Position::new(5, 0);
        b.scroll = (50, 0);
        follow_scroll(&mut b, 10, 20);
        assert_eq!(b.scroll.0, 3);
        b.cursor = Position::new(99, 0);
        follow_scroll(&mut b, 10, 20);
        assert_eq!(b.scroll.0, 90);
        let mut b = EdBuffer::new("t", &"y".repeat(200));
        b.cursor = Position::new(0, 100);
        follow_scroll(&mut b, 10, 20);
        assert_eq!(b.scroll.1, 85);
    }

    #[test]
    fn horizontal_scroll_shows_cursor_region() {
        let mut st = mk_state(&"abcdefghij".repeat(20));
        st.buffers[0].cursor = Position::new(0, 150);
        let (term, info) = render(&mut st, 40, 10, false);
        let (_, left) = st.buffers[0].scroll;
        assert!(left > 0);
        assert!(150 >= left && 150 < left + (info.cols - info.gutter_w) as usize);
        let b = term.backend().buffer();
        let x = info.gutter_w + (150 - left) as u16;
        let expect = "abcdefghij".chars().nth(150 % 10).unwrap().to_string();
        assert_eq!(b[(x, 1)].symbol(), expect);
    }

    #[test]
    fn finder_overlay_renders_prompt_and_results() {
        let mut st = mk_state("alpha");
        let mut f = FinderState::new(Vec::new());
        f.query = "wa".into();
        f.results = vec![("walrus.rs".into(), vec![0, 1]), ("wing.rs".into(), vec![])];
        f.selected = 0;
        st.finder = Some(f);
        let (term, _) = render(&mut st, 40, 20, false);
        assert!(find_row(&term, " find file ").is_some());
        let prompt_y = find_row(&term, "> wa").expect("prompt row");
        let result_y = find_row(&term, "walrus.rs").expect("result row");
        assert!(result_y > prompt_y);
        assert!(find_row(&term, "wing.rs") > Some(result_y));
        let b = term.backend().buffer();
        let x = row_text(&term, result_y).chars().position(|c| c == 'w').unwrap() as u16;
        assert_eq!(b[(x, result_y)].fg, theme::ACCENT);
        assert_eq!(b[(x, result_y)].bg, theme::SURFACE);
        assert_eq!(b[(x + 2, result_y)].fg, theme::TEXT);
        assert!(row_text(&term, result_y).contains("▸"));
    }

    #[test]
    fn dashboard_renders_logo_and_hints() {
        let mut st = EditorState::new();
        let (term, info) = render(&mut st, 60, 20, false);
        assert_eq!(info.gutter_w, 0);
        assert!(find_row(&term, "(_)").is_some());
        assert!(find_row(&term, concat!("rvim v", env!("CARGO_PKG_VERSION"))).is_some());
        assert!(find_row(&term, "find file").is_some());
        assert!(find_row(&term, ":help").is_some());
        assert!(find_row(&term, "hide keyboard").is_some());
        assert!(row_text(&term, 17).contains("NORMAL"));
        assert!(row_text(&term, info.touchbar_row).contains("esc"));
    }

    #[test]
    fn tiny_grid_never_panics() {
        let text: String = (0..300).map(|i| format!("{} {}\n", "x".repeat(200), i)).collect();
        let mut st = mk_state(&text);
        st.buffers[0].cursor = Position::new(299, 199);
        st.buffers[0].scroll = (57, 3);
        st.search.pattern = "xx".into();
        let (_, info) = render(&mut st, 20, 6, true);
        assert_eq!(info.cols, 20);
        assert_eq!(info.touchbar_row, 5);
        assert!(st.buffers[0].scroll.0 <= 299);

        let mut st = EditorState::new();
        render(&mut st, 20, 6, true);

        let mut st = mk_state("a");
        st.finder = Some(FinderState::new(vec!["a.rs".into()]));
        render(&mut st, 20, 6, true);

        let mut st = mk_state("a");
        render(&mut st, 20, 1, true);
    }

    #[test]
    fn line_matches_windows_and_unicode() {
        let pat: Vec<char> = "ll".chars().collect();
        assert_eq!(line_matches("hello hello", &pat, 0, 100), vec![(2, 2), (8, 2)]);
        assert_eq!(line_matches("hello hello", &pat, 5, 100), vec![(8, 2)]);
        let pat: Vec<char> = "é中".chars().collect();
        assert_eq!(line_matches("xé中y", &pat, 0, 10), vec![(1, 2)]);
        assert!(line_matches("abc", &[], 0, 10).is_empty());
    }

    #[test]
    fn bracket_pair_nested_and_backward() {
        let t = TextBuffer::from_text("a(b[c]{d})");
        assert_eq!(
            bracket_pair(&t, Position::new(0, 1)),
            Some((Position::new(0, 1), Position::new(0, 9)))
        );
        assert_eq!(
            bracket_pair(&t, Position::new(0, 3)),
            Some((Position::new(0, 3), Position::new(0, 5)))
        );
        assert_eq!(
            bracket_pair(&t, Position::new(0, 9)),
            Some((Position::new(0, 1), Position::new(0, 9)))
        );
        let t = TextBuffer::from_text("((a))");
        assert_eq!(
            bracket_pair(&t, Position::new(0, 0)),
            Some((Position::new(0, 0), Position::new(0, 4)))
        );
        let t = TextBuffer::from_text("{\n  x\n}");
        assert_eq!(
            bracket_pair(&t, Position::new(0, 0)),
            Some((Position::new(0, 0), Position::new(2, 0)))
        );
        assert_eq!(bracket_pair(&t, Position::new(1, 2)), None);
        let long = format!("({}", "x".repeat(BRACKET_BUDGET + 10));
        let t = TextBuffer::from_text(&long);
        assert_eq!(bracket_pair(&t, Position::new(0, 0)), None);
    }

    #[test]
    fn cell_color_precedence() {
        let base = theme::SYN_KEYWORD;
        assert_eq!(cell_colors(base, false, false, None, false), (base, theme::BG));
        assert_eq!(cell_colors(base, true, false, None, false), (base, theme::CURSORLINE_BG));
        assert_eq!(cell_colors(base, true, true, None, false), (base, theme::MATCH_BG));
        assert_eq!(
            cell_colors(base, true, true, Some(false), false),
            (theme::SEARCH_FG, theme::SEARCH_BG)
        );
        assert_eq!(
            cell_colors(base, true, true, Some(true), false),
            (theme::SEARCH_FG, theme::SEARCH_CUR_BG)
        );
        assert_eq!(
            cell_colors(base, true, true, Some(true), true),
            (base, theme::VISUAL_BG)
        );
    }

    #[test]
    fn in_visual_charwise_and_linewise() {
        let sel = Some((Position::new(1, 2), Position::new(3, 1), false));
        assert!(!in_visual(&sel, 0, 5));
        assert!(!in_visual(&sel, 1, 1));
        assert!(in_visual(&sel, 1, 2));
        assert!(in_visual(&sel, 2, 0));
        assert!(in_visual(&sel, 3, 1));
        assert!(!in_visual(&sel, 3, 2));
        let sel = Some((Position::new(1, 2), Position::new(2, 0), true));
        assert!(in_visual(&sel, 1, 0));
        assert!(in_visual(&sel, 2, 99));
        assert!(!in_visual(&sel, 3, 0));
        assert!(!in_visual(&None, 0, 0));
    }

    #[test]
    fn scroll_pct_labels() {
        assert_eq!(scroll_pct(0, 10, 5), "All");
        assert_eq!(scroll_pct(0, 10, 50), "Top");
        assert_eq!(scroll_pct(40, 10, 50), "Bot");
        assert_eq!(scroll_pct(20, 10, 50), "50%");
    }

    #[test]
    fn unicode_lines_render_at_char_columns() {
        let mut st = mk_state("é中x");
        let (term, info) = render(&mut st, 40, 10, false);
        let b = term.backend().buffer();
        assert_eq!(b[(info.gutter_w, 1)].symbol(), "é");
        assert_eq!(b[(info.gutter_w + 1, 1)].symbol(), "中");
        assert_eq!(b[(info.gutter_w + 2, 1)].symbol(), "x");
    }

    #[test]
    fn vertical_layout_degrades_in_order() {
        let l = vertical_layout(24);
        assert_eq!(l.bufferline, Some(0));
        assert_eq!((l.text_top, l.text_rows), (1, 20));
        assert_eq!((l.status, l.cmdline, l.touchbar), (Some(21), Some(22), 23));
        let l = vertical_layout(4);
        assert_eq!(l.bufferline, None);
        assert_eq!((l.text_top, l.text_rows), (0, 1));
        assert_eq!((l.status, l.cmdline, l.touchbar), (Some(1), Some(2), 3));
        let l = vertical_layout(3);
        assert_eq!((l.status, l.cmdline), (Some(1), None));
        let l = vertical_layout(2);
        assert_eq!((l.status, l.cmdline), (None, None));
        assert_eq!(l.text_rows, 1);
        let l = vertical_layout(1);
        assert_eq!(l.text_rows, 0);
        assert_eq!(l.touchbar, 0);
    }
}

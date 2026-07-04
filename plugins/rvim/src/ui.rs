//! All ratatui drawing: bufferline, gutter, syntax-colored text, cursor, visual/search
//! highlights, statusline, command/message line, touchbar, finder overlay, dashboard.

use std::num::NonZeroU16;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::{Buffer as Grid, CellDiffOption};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use crate::buffer::{Position, TextBuffer};
use crate::finder::{FinderState, FinderTarget};
use crate::fs::Vfs;
use crate::highlight::{Highlighter, HlSpan};
use crate::state::{Buffer as EdBuffer, EditorState, MsgKind, SplitDir};
use crate::theme;
use crate::vim::{Key, Mode, ScrollRequest, VimEngine, VisualKind};

/// Per-frame inputs the renderer needs beyond the editor state.
pub struct DrawCtx<'a> {
    pub st: &'a mut EditorState,
    pub vim: &'a VimEngine,
    pub hl: &'a mut Highlighter,
    pub vfs: &'a Vfs,
    /// Blink phase: the cursor cell renders solid when true.
    pub blink_on: bool,
    /// Sticky-Ctrl armed from the touchbar.
    pub ctrl_armed: bool,
    pub focused: bool,
    /// Typing has paused; non-menu which-key panels only show then.
    pub paused: bool,
}

/// Grid rectangle one window occupies, plus its gutter width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WinRect {
    pub win: usize,
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    pub gutter_w: u16,
}

/// Where things landed on the grid, for mapping taps back to buffer positions.
/// The flat fields describe the FOCUSED window.
#[derive(Clone, Default)]
pub struct LayoutInfo {
    /// Columns taken by the focused window's line-number gutter.
    #[allow(dead_code)]
    pub gutter_w: u16,
    /// First grid row of the focused window.
    #[allow(dead_code)]
    pub text_top: u16,
    #[allow(dead_code)]
    pub text_rows: u16,
    /// Grid row of the touchbar.
    pub touchbar_row: u16,
    pub cols: u16,
    /// Every window's rectangle within the text area.
    pub windows: Vec<WinRect>,
    /// Explorer sidebar width in columns; 0 = hidden.
    pub explorer_w: u16,
    /// First grid row of the explorer sidebar (the full text area).
    pub explorer_top: u16,
    /// Sidebar rows (the full text-area height).
    pub explorer_rows: u16,
    /// First grid row of the which-key panel.
    pub whichkey_top: u16,
    /// Panel rows including the title; 0 = hidden.
    pub whichkey_rows: u16,
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

/// Minimum grid rows for the which-key panel to dock without starving the text area.
const WHICHKEY_MIN_SPARE: u16 = 5;

/// Explorer sidebar width for `cols` grid columns; `None` = too narrow to show.
fn explorer_width(cols: u16) -> Option<u16> {
    if cols < 30 {
        None
    } else {
        Some(28.min((cols as u32 * 35 / 100) as u16))
    }
}

/// Tile `n` windows along `dir` inside the given area; `(x, y, w, h)` per window.
/// One row (or column) between neighbours is left for a separator; tiles are exact.
fn tile_windows(n: usize, dir: SplitDir, x0: u16, y0: u16, w: u16, h: u16) -> Vec<(u16, u16, u16, u16)> {
    if n == 0 {
        return Vec::new();
    }
    let seps = (n - 1) as u16;
    let mut out = Vec::with_capacity(n);
    match dir {
        SplitDir::Horizontal => {
            let avail = h.saturating_sub(seps);
            let (base, extra) = (avail / n as u16, (avail % n as u16) as usize);
            let mut y = y0;
            for i in 0..n {
                let hh = base + u16::from(i < extra);
                out.push((x0, y, w, hh));
                y = y.saturating_add(hh).saturating_add(1);
            }
        }
        SplitDir::Vertical => {
            let avail = w.saturating_sub(seps);
            let (base, extra) = (avail / n as u16, (avail % n as u16) as usize);
            let mut x = x0;
            for i in 0..n {
                let ww = base + u16::from(i < extra);
                out.push((x, y0, ww, h));
                x = x.saturating_add(ww).saturating_add(1);
            }
        }
    }
    out
}

/// Draw one frame; also scroll-follows the cursor (mutates the active buffer's scroll).
pub fn draw(term: &mut Terminal<TestBackend>, ctx: DrawCtx) -> LayoutInfo {
    let area = term.backend().buffer().area;
    let (cols, rows) = (area.width, area.height);

    let DrawCtx { st, vim, hl, vfs, blink_on, ctrl_armed, focused, paused } = ctx;

    // Which-key panel overlays the rows above the touchbar without moving the text.
    // Menus (leader, Ctrl+w) show at once; prefix cheatsheets wait for a typing pause.
    let hints = vim.key_hints().filter(|h| h.immediate || paused);
    let panel_h = match &hints {
        Some(h) if !h.entries.is_empty() => {
            let (hint_rows, _) = whichkey_grid(cols, h.entries.len());
            let ph = hint_rows + 1;
            if rows >= ph + WHICHKEY_MIN_SPARE { ph } else { 0 }
        }
        _ => 0,
    };
    let vl = vertical_layout(rows);
    let whichkey_top = vl.touchbar.saturating_sub(panel_h);

    let explorer_w = if st.explorer.is_some() { explorer_width(cols) } else { None };
    let (sidebar_w, win_x0) = match explorer_w {
        Some(w) => (w, (w + 1).min(cols)),
        None => (0, 0),
    };
    let win_w = cols - win_x0;
    st.text_dims = (win_w, vl.text_rows);

    let rects: Vec<WinRect> = if st.buffers.is_empty() {
        Vec::new()
    } else {
        tile_windows(st.windows.len(), st.split_dir, win_x0, vl.text_top, win_w, vl.text_rows)
            .into_iter()
            .enumerate()
            .map(|(i, (x, y, w, h))| {
                let lines = st
                    .buffers
                    .get(st.windows[i].buffer)
                    .map(|b| b.text.line_count())
                    .unwrap_or(1);
                let g = gutter_width(lines, st.options.number, st.options.relativenumber, w);
                WinRect { win: i, x, y, w, h, gutter_w: g }
            })
            .collect()
    };
    let focus_rect = rects.get(st.active_win.min(rects.len().saturating_sub(1))).copied();

    if let Some(fr) = focus_rect {
        if let Some(b) = st.buf_mut() {
            apply_scroll_request(b, vim.take_scroll_request(), fr.h);
            follow_scroll(b, fr.h, fr.w.saturating_sub(fr.gutter_w));
        }
    }
    if sidebar_w > 0 {
        let visible = vl.text_rows.saturating_sub(1) as usize;
        let flen = vfs.len();
        if let Some(ex) = st.explorer.as_mut() {
            ex.selected = ex.selected.min(flen.saturating_sub(1));
            if ex.selected < ex.offset {
                ex.offset = ex.selected;
            } else if visible > 0 && ex.selected >= ex.offset + visible {
                ex.offset = ex.selected + 1 - visible;
            }
        }
    }

    let st: &EditorState = st;
    let info = LayoutInfo {
        gutter_w: focus_rect.map(|r| r.gutter_w).unwrap_or(0),
        text_top: focus_rect.map(|r| r.y).unwrap_or(vl.text_top),
        text_rows: focus_rect.map(|r| r.h).unwrap_or(vl.text_rows),
        touchbar_row: vl.touchbar,
        cols,
        windows: rects.clone(),
        explorer_w: sidebar_w,
        explorer_top: vl.text_top,
        explorer_rows: vl.text_rows,
        whichkey_top,
        whichkey_rows: panel_h,
    };

    let _ = term.draw(|frame| {
        let g = frame.buffer_mut();
        g.set_style(area, Style::new().fg(theme::TEXT).bg(theme::BG));
        if let Some(row) = vl.bufferline {
            draw_bufferline(g, row, cols, st);
        }
        if st.buffers.is_empty() {
            draw_dashboard(g, &vl, win_x0, win_w);
        } else {
            for (i, r) in rects.iter().enumerate() {
                let focused_win = i == st.active_win;
                let spans: &[Vec<HlSpan>] = match st.buffers.get(st.windows[r.win].buffer) {
                    Some(b) => hl.spans(&b.name, &b.text),
                    None => &[],
                };
                draw_window(g, r, st, vim, spans, blink_on, focused_win);
                if i + 1 < rects.len() {
                    match st.split_dir {
                        SplitDir::Horizontal => draw_hsep(g, r, st, focused_win),
                        SplitDir::Vertical => draw_vdiv(g, r.x + r.w, vl.text_top, vl.text_rows),
                    }
                }
            }
        }
        if sidebar_w > 0 {
            draw_explorer(g, sidebar_w, vl.text_top, vl.text_rows, st, vfs);
            draw_vdiv(g, sidebar_w, vl.text_top, vl.text_rows);
        }
        if let Some(row) = vl.status {
            draw_statusline(g, row, cols, st, vim, &vl);
        }
        if let Some(row) = vl.cmdline {
            draw_cmdline(g, row, cols, st, vim, blink_on, focused);
        }
        if panel_h > 0 {
            if let Some(h) = &hints {
                draw_whichkey(g, whichkey_top, cols, h.title, &h.entries);
            }
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

/// Apply a pending zz/zt/zb or paging request against the real viewport height.
fn apply_scroll_request(buf: &mut EdBuffer, req: Option<ScrollRequest>, text_rows: u16) {
    let Some(req) = req else { return };
    let rows = text_rows as usize;
    if rows == 0 {
        return;
    }
    let last = buf.text.line_count().saturating_sub(1);
    let cur = buf.text.clamp(buf.cursor, true);
    match req {
        ScrollRequest::Center => buf.scroll.0 = cur.line.saturating_sub(rows / 2),
        ScrollRequest::Top => buf.scroll.0 = cur.line,
        ScrollRequest::Bottom => buf.scroll.0 = (cur.line + 1).saturating_sub(rows),
        ScrollRequest::HalfDown
        | ScrollRequest::HalfUp
        | ScrollRequest::PageDown
        | ScrollRequest::PageUp => {
            let delta = match req {
                ScrollRequest::HalfDown | ScrollRequest::HalfUp => (rows / 2).max(1),
                _ => rows.saturating_sub(2).max(1),
            };
            let down = matches!(req, ScrollRequest::HalfDown | ScrollRequest::PageDown);
            let line =
                if down { (cur.line + delta).min(last) } else { cur.line.saturating_sub(delta) };
            buf.cursor = buf.text.clamp(Position::new(line, buf.desired_col), false);
            let top =
                if down { buf.scroll.0 + delta } else { buf.scroll.0.saturating_sub(delta) };
            buf.scroll.0 = top.min(last);
        }
    }
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
    let active = st.active().min(st.buffers.len() - 1);
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

/// Draw one window's gutter and text into its rect. Cursor, cursorline, visual,
/// search, and bracket highlights paint only in the focused window.
fn draw_window(
    g: &mut Grid,
    r: &WinRect,
    st: &EditorState,
    vim: &VimEngine,
    spans: &[Vec<HlSpan>],
    blink_on: bool,
    focused: bool,
) {
    let Some(b) = st.buffers.get(st.windows.get(r.win).map(|w| w.buffer).unwrap_or(0)) else {
        return;
    };
    if r.h == 0 || r.w == 0 {
        return;
    }
    let gutter_w = r.gutter_w;
    let line_count = b.text.line_count();
    let top = b.scroll.0.min(line_count.saturating_sub(1));
    let left = b.scroll.1;
    let text_cols = r.w.saturating_sub(gutter_w) as usize;
    if text_cols == 0 {
        return;
    }
    let max_x = r.x + r.w;
    let sel = if focused { vim.visual_range(b.cursor) } else { None };
    // desired_col pinned to MAX means a $-extended block: highlight to each line's end.
    let block_eol = b.desired_col == usize::MAX;
    let pat: Vec<char> = if focused && !st.search.pattern.is_empty() && !st.search.suppressed {
        st.search.pattern.chars().collect()
    } else {
        Vec::new()
    };
    let bracket = if focused { bracket_pair(&b.text, b.cursor) } else { None };

    for row in 0..r.h {
        let y = r.y + row;
        let li = top + row as usize;
        if li >= line_count {
            put_str(g, r.x, y, "~", Style::new().fg(theme::DIM), max_x);
            continue;
        }
        let cursor_line = focused && li == b.cursor.line;
        if cursor_line {
            g.set_style(Rect::new(r.x, y, r.w, 1), Style::new().bg(theme::CURSORLINE_BG));
        }
        if gutter_w > 0 {
            let num = if li == b.cursor.line || !st.options.relativenumber {
                li + 1
            } else {
                li.abs_diff(b.cursor.line)
            };
            let fg = if cursor_line { theme::LINENR_CUR } else { theme::LINENR };
            let bg = if cursor_line { theme::CURSORLINE_BG } else { theme::BG };
            let s = format!("{:>w$} ", num, w = (gutter_w - 1) as usize);
            put_str(g, r.x, y, &s, Style::new().fg(fg).bg(bg), r.x + gutter_w);
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
            let (fg, bg) = cell_colors(
                base_fg,
                cursor_line,
                in_bracket,
                search,
                in_visual(&sel, li, ci, block_eol),
            );
            let x = r.x + gutter_w + (ci - left) as u16;
            set_cell(g, x, y, ch, Style::new().fg(fg).bg(bg));
        }
        // Linewise selection covers the text region past the line's last char too.
        if let Some((s, e, VisualKind::Line)) = sel {
            if li >= s.line && li <= e.line {
                let drawn = b.text.line_len(li).saturating_sub(left).min(text_cols);
                let from = r.x + gutter_w + drawn as u16;
                if from < max_x {
                    g.set_style(Rect::new(from, y, max_x - from, 1), Style::new().bg(theme::VISUAL_BG));
                }
            }
        }
    }

    if focused && blink_on && vim.cmdline().is_none() && st.finder.is_none() && !st.explorer_focused {
        let cur = b.cursor;
        let in_view = cur.line >= top
            && cur.line < top + r.h as usize
            && cur.col >= left
            && cur.col < left + text_cols;
        if in_view {
            let x = r.x + gutter_w + (cur.col - left) as u16;
            let y = r.y + (cur.line - top) as u16;
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

/// Separator row below a stacked window: its buffer name (+ modified marker) centered.
fn draw_hsep(g: &mut Grid, r: &WinRect, st: &EditorState, focused: bool) {
    let y = r.y.saturating_add(r.h);
    let base = Style::new().fg(theme::MUTED).bg(theme::SURFACE_DIM);
    g.set_style(Rect::new(r.x, y, r.w, 1), base);
    let Some(b) = st.buffers.get(st.windows.get(r.win).map(|w| w.buffer).unwrap_or(0)) else {
        return;
    };
    let label = format!(" {}{} ", b.name, if b.modified() { " [+]" } else { "" });
    let lw = label.chars().count() as u16;
    let x = r.x + r.w.saturating_sub(lw) / 2;
    let fg = if focused { theme::ACCENT } else { theme::MUTED };
    put_str(g, x, y, &label, Style::new().fg(fg).bg(theme::SURFACE_DIM), r.x + r.w);
}

/// Vertical divider column between side-by-side regions.
fn draw_vdiv(g: &mut Grid, x: u16, top: u16, rows: u16) {
    let style = Style::new().fg(theme::BORDER_MUTED).bg(theme::BG);
    for y in top..top.saturating_add(rows) {
        set_cell(g, x, y, '│', style);
    }
}

/// Explorer sidebar: header, one row per vfs file, optional new-file input row.
fn draw_explorer(g: &mut Grid, w: u16, top: u16, rows: u16, st: &EditorState, vfs: &Vfs) {
    let Some(ex) = &st.explorer else { return };
    if rows == 0 || w == 0 {
        return;
    }
    put_str(g, 0, top, " files", Style::new().fg(theme::ACCENT).bg(theme::BG), w);
    let visible = rows.saturating_sub(1) as usize;
    let mut shown = 0usize;
    for (i, name) in vfs.names().enumerate().skip(ex.offset).take(visible) {
        let y = top + 1 + (i - ex.offset) as u16;
        let selected = i == ex.selected;
        let bg = if selected { theme::SURFACE } else { theme::BG };
        if selected {
            g.set_style(Rect::new(0, y, w, 1), Style::new().bg(theme::SURFACE));
        }
        let fg = if selected {
            if st.explorer_focused { theme::ACCENT } else { theme::MUTED }
        } else if name.ends_with(".rs") {
            theme::TEXT
        } else {
            theme::MUTED
        };
        let x = put_str(g, 0, y, " ", Style::new().fg(fg).bg(bg), w);
        put_str(g, x, y, name, Style::new().fg(fg).bg(bg), w);
        shown += 1;
    }
    if let Some(nn) = &ex.new_name {
        let y = top + 1 + shown.min(visible.saturating_sub(1)) as u16;
        let style = Style::new().fg(theme::ACCENT).bg(theme::BG);
        let x = put_str(g, 0, y, " + ", style, w);
        let x = put_str(g, x, y, nn, Style::new().fg(theme::TEXT).bg(theme::BG), w);
        put_str(g, x, y, "▉", style, w);
    }
}

/// Hint grid shape for the which-key panel: (hint rows, columns).
fn whichkey_grid(cols: u16, n: usize) -> (u16, u16) {
    let n = n.max(1) as u16;
    let mut ncols: u16 = if cols >= 80 {
        4
    } else if cols >= 40 {
        2
    } else {
        1
    };
    if n.div_ceil(ncols) > 3 {
        ncols = n.div_ceil(3);
    }
    (n.div_ceil(ncols).min(3), ncols)
}

/// Docked which-key panel: an ACCENT title row plus hint rows in columns.
fn draw_whichkey(g: &mut Grid, top: u16, cols: u16, title: &str, hints: &[(&str, &str)]) {
    let (rows, ncols) = whichkey_grid(cols, hints.len());
    let bg = theme::SURFACE_DIM;
    g.set_style(Rect::new(0, top, cols, rows + 1), Style::new().fg(theme::TEXT).bg(bg));
    // The panel overlays other chrome; blank the band before writing hints.
    let blank = " ".repeat(cols as usize);
    for y in top..top + rows + 1 {
        put_str(g, 0, y, &blank, Style::new().bg(bg), cols);
    }
    put_str(g, 0, top, title, Style::new().fg(theme::ACCENT).bg(bg), cols);
    let col_w = (cols / ncols).max(1);
    for (i, (key, label)) in hints.iter().enumerate() {
        let row = i / ncols as usize;
        if row >= rows as usize {
            break;
        }
        let y = top + 1 + row as u16;
        let cell = (i % ncols as usize) as u16;
        let x = cell * col_w;
        let max = if cell + 1 == ncols { cols } else { x + col_w };
        let x = put_str(g, x + 1, y, key, Style::new().fg(theme::ACCENT).bg(bg), max);
        put_str(g, x + 1, y, label, Style::new().fg(theme::TEXT).bg(bg), max);
    }
}

/// Resolve a tap inside the which-key panel; `row` is relative to the panel top.
/// Only hints whose first key token is a single char are tappable — pattern rows
/// like "a-z" are documentation, not buttons.
pub fn whichkey_action_at(
    col: u16,
    row: u16,
    cols: u16,
    hints: &[(&str, &str)],
) -> Option<Key> {
    let hint_row = row.checked_sub(1)?;
    let (rows, ncols) = whichkey_grid(cols, hints.len());
    if hint_row >= rows {
        return None;
    }
    let col_w = (cols / ncols).max(1);
    let cell = (col / col_w).min(ncols - 1);
    let idx = hint_row as usize * ncols as usize + cell as usize;
    let first = hints.get(idx)?.0.split_whitespace().next()?;
    let mut chars = first.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Some(Key::Char(c)),
        _ => None,
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
fn in_visual(
    sel: &Option<(Position, Position, VisualKind)>,
    line: usize,
    col: usize,
    block_eol: bool,
) -> bool {
    let Some((s, e, kind)) = sel else { return false };
    if line < s.line || line > e.line {
        return false;
    }
    match kind {
        VisualKind::Line => true,
        VisualKind::Char => (line > s.line || col >= s.col) && (line < e.line || col <= e.col),
        VisualKind::Block => {
            let (c1, c2) = if s.col <= e.col { (s.col, e.col) } else { (e.col, s.col) };
            col >= c1 && (block_eol || col <= c2)
        }
    }
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
        "VISUAL" | "V-LINE" | "V-BLOCK" => theme::MODE_VISUAL,
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
    if let Some(reg) = vim.recording_reg() {
        right.push_str(&format!("recording @{reg}  "));
    }
    if !vim.pending_display().is_empty() {
        right.push_str(vim.pending_display());
        right.push_str("  ");
    }
    if st.windows.len() > 1 {
        right.push_str(&format!("⧉ {}/{}  ", st.active_win + 1, st.windows.len()));
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
    let title = match f.target {
        FinderTarget::Files => " find file ",
        FinderTarget::Buffers => " buffers ",
    };
    put_str(g, x0 + 2, y0, title, border, x1);

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

fn draw_dashboard(g: &mut Grid, vl: &VLayout, x0: u16, cols: u16) {
    if vl.text_rows == 0 {
        return;
    }
    let version = concat!("rvim v", env!("CARGO_PKG_VERSION"));
    let mut lines: Vec<(&str, Color)> = Vec::with_capacity(LOGO.len() + 8);
    for l in LOGO {
        lines.push((l, theme::ACCENT));
    }
    lines.push(("", theme::MUTED));
    lines.push((version, theme::MUTED));
    lines.push(("", theme::DIM));
    lines.push((":e <file>   new/open file", theme::DIM));
    lines.push(("Space f f   find file", theme::DIM));
    lines.push(("Space e     file explorer", theme::DIM));
    lines.push(("Space w     save file", theme::DIM));
    lines.push(("Space q     quit file", theme::DIM));
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
        let x = x0 + cols.saturating_sub(w) / 2;
        put_str(g, x, y, text, Style::new().fg(*color), x0 + cols);
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
        render_with(st, &VimEngine::new(), &Vfs::load(), w, h, blink)
    }

    fn render_with(
        st: &mut EditorState,
        vim: &VimEngine,
        vfs: &Vfs,
        w: u16,
        h: u16,
        blink: bool,
    ) -> (Terminal<TestBackend>, LayoutInfo) {
        render_paused(st, vim, vfs, w, h, blink, true)
    }

    fn render_paused(
        st: &mut EditorState,
        vim: &VimEngine,
        vfs: &Vfs,
        w: u16,
        h: u16,
        blink: bool,
        paused: bool,
    ) -> (Terminal<TestBackend>, LayoutInfo) {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut hl = Highlighter::new();
        let info = draw(
            &mut term,
            DrawCtx {
                st,
                vim,
                hl: &mut hl,
                vfs,
                blink_on: blink,
                ctrl_armed: false,
                focused: true,
                paused,
            },
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
        st.set_active(1);
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
        st.set_active(4);
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
        let sel = Some((Position::new(1, 2), Position::new(3, 1), VisualKind::Char));
        assert!(!in_visual(&sel, 0, 5, false));
        assert!(!in_visual(&sel, 1, 1, false));
        assert!(in_visual(&sel, 1, 2, false));
        assert!(in_visual(&sel, 2, 0, false));
        assert!(in_visual(&sel, 3, 1, false));
        assert!(!in_visual(&sel, 3, 2, false));
        let sel = Some((Position::new(1, 2), Position::new(2, 0), VisualKind::Line));
        assert!(in_visual(&sel, 1, 0, false));
        assert!(in_visual(&sel, 2, 99, false));
        assert!(!in_visual(&sel, 3, 0, false));
        assert!(!in_visual(&None, 0, 0, false));
    }

    #[test]
    fn in_visual_block_is_a_rectangle() {
        // Anchor right of the cursor end: cols still normalize to 1..=3.
        let sel = Some((Position::new(1, 3), Position::new(3, 1), VisualKind::Block));
        assert!(in_visual(&sel, 2, 1, false));
        assert!(in_visual(&sel, 2, 3, false));
        assert!(!in_visual(&sel, 2, 0, false));
        assert!(!in_visual(&sel, 2, 4, false));
        assert!(!in_visual(&sel, 0, 2, false));
        assert!(!in_visual(&sel, 4, 2, false));
        assert!(in_visual(&sel, 2, 99, true), "block $ extends to line ends");
        assert!(!in_visual(&sel, 2, 0, true));
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

    fn leader_pending_vim(st: &mut EditorState, vfs: &mut Vfs) -> VimEngine {
        let mut vim = VimEngine::new();
        vim.handle_key(st, vfs, &egui_ios_plugin_sdk::HostHandle, Key::Char(' '));
        vim
    }

    #[test]
    fn whichkey_hit_test_matches_drawn_cells() {
        let hints: &[(&str, &str)] = &[
            ("e", "explorer"),
            ("f", "find…"),
            ("o", "other window"),
            ("t", "split…"),
            ("w", "save"),
            ("c", "close buffer"),
            ("q", "close window"),
            ("h", "help"),
        ];
        for cols in [20u16, 39, 40, 79, 80, 120] {
            let (rows, ncols) = whichkey_grid(cols, hints.len());
            let col_w = (cols / ncols).max(1);
            for (i, (k, _)) in hints.iter().enumerate() {
                let row = i / ncols as usize;
                if row >= rows as usize {
                    continue;
                }
                let x = (i % ncols as usize) as u16 * col_w;
                let got = whichkey_action_at(x, 1 + row as u16, cols, hints);
                assert_eq!(got, k.chars().next().map(Key::Char), "cols {cols} hint {i}");
            }
            assert_eq!(whichkey_action_at(0, 0, cols, hints), None, "title row is inert");
            assert_eq!(whichkey_action_at(0, rows + 1, cols, hints), None);
        }
        let (rows, ncols) = whichkey_grid(60, 8);
        assert_eq!((rows, ncols), (3, 3), "overflow bumps the column count");
        assert_eq!(whichkey_grid(80, 8), (2, 4));
        assert_eq!(whichkey_grid(30, 2), (2, 1));
    }

    #[test]
    fn whichkey_pattern_rows_are_inert() {
        let hints: &[(&str, &str)] = &[
            ("a-z", "named register"),
            ("w e b", "word motions"),
            ("$ 0 ^", "line ends"),
        ];
        // Single-char first token taps; range patterns do not.
        assert_eq!(whichkey_action_at(0, 1, 30, hints), None, "a-z row is documentation");
        assert_eq!(whichkey_action_at(0, 2, 30, hints), Some(Key::Char('w')));
        assert_eq!(whichkey_action_at(0, 3, 30, hints), Some(Key::Char('$')));
    }

    #[test]
    fn whichkey_prefix_panels_wait_for_a_pause() {
        let mut st = mk_state("alpha\nbeta");
        let mut vfs = Vfs::load();
        let mut vim = VimEngine::new();
        vim.handle_key(&mut st, &mut vfs, &egui_ios_plugin_sdk::HostHandle, Key::Char('d'));

        let (_, info) = render_paused(&mut st, &vim, &vfs, 80, 20, false, false);
        assert_eq!(info.whichkey_rows, 0, "mid-typing: no operator panel");
        let (term, info) = render_paused(&mut st, &vim, &vfs, 80, 20, false, true);
        assert!(info.whichkey_rows > 0, "paused: the operator cheatsheet shows");
        assert!(row_text(&term, info.whichkey_top).contains(" d"));

        // Leader menus are immediate regardless of the pause state.
        let mut st = mk_state("alpha");
        let vim = leader_pending_vim(&mut st, &mut vfs);
        let (_, info) = render_paused(&mut st, &vim, &vfs, 80, 20, false, false);
        assert!(info.whichkey_rows > 0, "leader menu shows without waiting");
    }

    #[test]
    fn whichkey_panel_docks_above_touchbar() {
        let mut st = mk_state("alpha");
        let mut vfs = Vfs::load();
        let vim = leader_pending_vim(&mut st, &mut vfs);
        let (term, info) = render_with(&mut st, &vim, &vfs, 80, 20, false);
        assert_eq!(info.whichkey_rows, 3);
        assert_eq!(info.whichkey_top, 16);
        assert_eq!(info.touchbar_row, 19);
        assert_eq!(info.text_rows, 16, "panel overlays; the text area does not move");
        let title = row_text(&term, 16);
        assert!(title.contains(" space"), "{title:?}");
        let hints = row_text(&term, 17) + &row_text(&term, 18);
        for label in ["explorer", "find…", "save", "help"] {
            assert!(hints.contains(label), "missing {label} in {hints:?}");
        }
        let b = term.backend().buffer();
        assert_eq!(b[(0, 16)].bg, theme::SURFACE_DIM);
        let ex = row_text(&term, 17).find("e explorer").unwrap() as u16;
        assert_eq!(b[(ex, 17)].fg, theme::ACCENT);
        assert_eq!(b[(ex + 2, 17)].fg, theme::TEXT);
        // No panel without a pending leader.
        let (_, info) = render(&mut st, 80, 20, false);
        assert_eq!(info.whichkey_rows, 0);
    }

    #[test]
    fn whichkey_panel_skipped_on_tiny_grids() {
        let mut st = mk_state("alpha");
        let mut vfs = Vfs::load();
        let vim = leader_pending_vim(&mut st, &mut vfs);
        let (_, info) = render_with(&mut st, &vim, &vfs, 20, 6, false);
        assert_eq!(info.whichkey_rows, 0);
    }

    #[test]
    fn tile_windows_exactly_partitions_the_area() {
        for n in 1..=4usize {
            for h in [1u16, 3, 7, 10, 24] {
                let tiles = tile_windows(n, SplitDir::Horizontal, 2, 1, 30, h);
                assert_eq!(tiles.len(), n);
                let mut y = 1u16;
                for (i, &(x, ty, w, th)) in tiles.iter().enumerate() {
                    assert_eq!((x, w), (2, 30));
                    assert_eq!(ty, y, "window {i} starts after the previous separator");
                    y = y.saturating_add(th).saturating_add(1);
                }
                if h >= (2 * n - 1) as u16 {
                    let total: u16 = tiles.iter().map(|t| t.3).sum::<u16>() + (n as u16 - 1);
                    assert_eq!(total, h, "H tiles + separators fill exactly at h={h} n={n}");
                }
            }
            let tiles = tile_windows(n, SplitDir::Vertical, 5, 2, 41, 9);
            let total: u16 = tiles.iter().map(|t| t.2).sum::<u16>() + (n as u16 - 1);
            assert_eq!(total, 41);
            assert!(tiles.iter().all(|&(_, y, _, h)| y == 2 && h == 9));
            let mut x = 5u16;
            for &(tx, _, tw, _) in &tiles {
                assert_eq!(tx, x);
                x += tw + 1;
            }
        }
        assert!(tile_windows(0, SplitDir::Horizontal, 0, 0, 10, 10).is_empty());
    }

    #[test]
    fn horizontal_split_renders_separator_and_statusline_count() {
        let mut st = mk_state("alpha\nbravo");
        st.split(SplitDir::Horizontal);
        let (term, info) = render(&mut st, 40, 14, false);
        assert_eq!(info.windows.len(), 2);
        let sep_y = info.windows[0].y + info.windows[0].h;
        assert!(row_text(&term, sep_y).contains("main.rs"), "{:?}", row_text(&term, sep_y));
        let b = term.backend().buffer();
        assert_eq!(b[(0, sep_y)].bg, theme::SURFACE_DIM);
        let status = row_text(&term, 11);
        assert!(status.contains("⧉ 2/2"), "{status:?}");
        // Both windows show the buffer's first line with their own gutter.
        assert!(row_text(&term, info.windows[0].y).contains("1 alpha"));
        assert!(row_text(&term, info.windows[1].y).contains("1 alpha"));
        // Cursorline paints only in the focused (second) window.
        assert_eq!(b[(20, info.windows[1].y)].bg, theme::CURSORLINE_BG);
        assert_ne!(b[(20, info.windows[0].y)].bg, theme::CURSORLINE_BG);
    }

    #[test]
    fn scroll_follow_uses_focused_window_height() {
        let text: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let mut st = mk_state(text.trim_end());
        st.split(SplitDir::Horizontal);
        st.buffers[0].cursor = Position::new(50, 0);
        let (_, info) = render(&mut st, 40, 14, false);
        let fr = info.windows[1];
        assert_eq!((info.text_top, info.text_rows), (fr.y, fr.h));
        let top = st.buffers[0].scroll.0;
        assert!(fr.h < 10, "split window is shorter than the full text area");
        assert!(50 >= top && 50 < top + fr.h as usize, "cursor in view at top={top} h={}", fr.h);
    }

    #[test]
    fn vertical_split_renders_divider() {
        let mut st = mk_state("alpha");
        st.split(SplitDir::Vertical);
        let (term, info) = render(&mut st, 41, 12, false);
        assert_eq!(info.windows.len(), 2);
        let div_x = info.windows[0].x + info.windows[0].w;
        let b = term.backend().buffer();
        assert_eq!(b[(div_x, info.windows[0].y)].symbol(), "│");
        assert_eq!(b[(div_x, info.windows[0].y)].fg, theme::BORDER_MUTED);
        assert!(row_text(&term, info.windows[0].y).matches("1 alpha").count() >= 2);
    }

    #[test]
    fn explorer_sidebar_renders_files_and_divider() {
        let mut st = mk_state("alpha");
        st.explorer = Some(crate::explorer::ExplorerState::new());
        st.explorer_focused = true;
        let mut vfs = Vfs::load();
        vfs.write("a.rs", "");
        vfs.write("b.md", "");
        let (term, info) = render_with(&mut st, &VimEngine::new(), &vfs, 60, 14, false);
        assert_eq!(info.explorer_w, 21);
        assert!(row_text(&term, 1).starts_with(" files"));
        let a_row = find_row(&term, "a.rs").unwrap();
        let b_row = find_row(&term, "b.md").unwrap();
        assert_eq!((a_row, b_row), (2, 3));
        let b = term.backend().buffer();
        assert_eq!(b[(1, a_row)].fg, theme::ACCENT, "selected row, explorer focused");
        assert_eq!(b[(1, a_row)].bg, theme::SURFACE);
        assert_eq!(b[(1, b_row)].fg, theme::MUTED, "non-rs file dimmed");
        assert_eq!(b[(21, 2)].symbol(), "│");
        assert_eq!(info.windows[0].x, 22);
        // Unfocused explorer dims the selection.
        st.explorer_focused = false;
        let (term, _) = render_with(&mut st, &VimEngine::new(), &vfs, 60, 14, false);
        let b = term.backend().buffer();
        assert_eq!(b[(1, 2)].fg, theme::MUTED);
    }

    #[test]
    fn explorer_new_name_row_renders() {
        let mut st = mk_state("alpha");
        let mut ex = crate::explorer::ExplorerState::new();
        ex.new_name = Some("fresh.rs".into());
        st.explorer = Some(ex);
        let mut vfs = Vfs::load();
        vfs.write("a.rs", "");
        let (term, _) = render_with(&mut st, &VimEngine::new(), &vfs, 60, 14, false);
        assert!(find_row(&term, "+ fresh.rs").is_some());
    }

    #[test]
    fn explorer_skipped_below_30_cols() {
        let mut st = mk_state("alpha");
        st.explorer = Some(crate::explorer::ExplorerState::new());
        let mut vfs = Vfs::load();
        vfs.write("a.rs", "");
        let (term, info) = render_with(&mut st, &VimEngine::new(), &vfs, 25, 10, false);
        assert_eq!(info.explorer_w, 0);
        assert!(find_row(&term, "files").is_none());
        assert!(row_text(&term, 1).contains("alpha"));
    }

    #[test]
    fn explorer_offset_follows_selection() {
        let mut st = mk_state("alpha");
        let mut ex = crate::explorer::ExplorerState::new();
        ex.selected = 9;
        st.explorer = Some(ex);
        let mut vfs = Vfs::load();
        for i in 0..10 {
            vfs.write(&format!("f{i}.rs"), "");
        }
        let (term, _) = render_with(&mut st, &VimEngine::new(), &vfs, 60, 8, false);
        // 4 text rows; header + 3 file rows: the selected f9.rs must be visible.
        assert!(find_row(&term, "f9.rs").is_some());
        assert_eq!(st.explorer.as_ref().unwrap().offset, 7);
    }

    #[test]
    fn buffers_finder_overlay_title() {
        let mut st = mk_state("alpha");
        st.finder = Some(FinderState::buffers(vec!["main.rs".into()]));
        let (term, _) = render(&mut st, 40, 20, false);
        assert!(find_row(&term, " buffers ").is_some());
        assert!(find_row(&term, " find file ").is_none());
    }

    #[test]
    fn tiny_grid_with_explorer_and_splits_never_panics() {
        let mut st = mk_state("fn main() {}\nx");
        st.explorer = Some(crate::explorer::ExplorerState::new());
        st.explorer_focused = true;
        st.split(SplitDir::Horizontal);
        st.split(SplitDir::Horizontal);
        st.split(SplitDir::Vertical);
        let mut vfs = Vfs::load();
        vfs.write("a.rs", "x");
        let mut vim = VimEngine::new();
        vim.handle_key(&mut st, &mut vfs, &egui_ios_plugin_sdk::HostHandle, Key::Char(' '));
        for (w, h) in [(20u16, 6u16), (30, 6), (20, 3), (35, 8), (10, 2)] {
            let (_, info) = render_with(&mut st, &vim, &vfs, w, h, true);
            assert_eq!(info.cols, w);
        }
    }

    #[test]
    fn unicode_file_names_render_everywhere_without_panic() {
        let mut st = mk_state("fn main() {}\né中🎉");
        st.buffers.push(EdBuffer::new("中文ファイル.md", "本文"));
        st.buffers[1].text.insert_char(Position::default(), 'x');
        st.explorer = Some(crate::explorer::ExplorerState::new());
        st.explorer_focused = true;
        st.split(SplitDir::Horizontal);
        st.split(SplitDir::Vertical);
        st.windows[0].buffer = 1;
        let mut vfs = Vfs::load();
        for name in ["héllo wörld.rs", "中文ファイル.md", "🎉 party.rs", "русский.txt"] {
            vfs.write(name, "é中");
        }
        let mut vim = VimEngine::new();
        vim.handle_key(&mut st, &mut vfs, &egui_ios_plugin_sdk::HostHandle, Key::Char(' '));
        for (w, h) in [(0u16, 0u16), (1, 1), (20, 6), (30, 8), (45, 12), (80, 24), (200, 4)] {
            let (_, info) = render_with(&mut st, &vim, &vfs, w, h, true);
            assert_eq!(info.cols, w);
        }
        let (term, _) = render_with(&mut st, &VimEngine::new(), &vfs, 60, 16, true);
        assert!(find_row(&term, "héllo").is_some());
        assert!(find_row(&term, "中文").is_some());
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


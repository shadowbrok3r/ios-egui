//! All ratatui drawing: bufferline, gutter, syntax-colored text, cursor, visual/search
//! highlights, statusline, command/message line, touchbar, finder overlay, dashboard.

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use crate::highlight::Highlighter;
use crate::state::EditorState;
use crate::vim::{Key, VimEngine};

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

/// Draw one frame; also scroll-follows the cursor (mutates the active buffer's scroll).
pub fn draw(term: &mut Terminal<TestBackend>, ctx: DrawCtx) -> LayoutInfo {
    // STUB: implemented by the ui module owner.
    let _ = ctx;
    let size = term.backend().buffer().area;
    LayoutInfo { cols: size.width, ..Default::default() }
}

/// Resolve a tap at grid column `col` on the touchbar row.
pub fn touchbar_action_at(col: u16, cols: u16) -> Option<TouchAction> {
    // STUB: implemented by the ui module owner.
    let _ = (col, cols);
    None
}

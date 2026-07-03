//! Mastertech terminal palette (Catppuccin Mocha + deep-pink accent), extended with
//! syntax-token colors. Base colors match the `terminal` and `ratatui-demo` plugins.
#![allow(dead_code)]

use ratatui::style::Color;

const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

pub const BG: Color = Color::Rgb(6, 6, 10);
pub const SURFACE: Color = rgb(0x313244);
pub const SURFACE_DIM: Color = rgb(0x181825);
pub const BORDER_MUTED: Color = rgb(0x585b70);
pub const TEXT: Color = rgb(0xcdd6f4);
pub const MUTED: Color = rgb(0xbac2de);
pub const DIM: Color = rgb(0x7f849c);
pub const ACCENT: Color = Color::Rgb(255, 20, 147);
pub const TERTIARY: Color = rgb(0xcba6f7);
pub const SUCCESS: Color = rgb(0xa6e3a1);
pub const ERROR: Color = rgb(0xf38ba8);
pub const WARNING: Color = rgb(0xf9e2af);
pub const CYAN: Color = rgb(0x89dceb);

// Syntax tokens (Catppuccin Mocha).
pub const SYN_KEYWORD: Color = rgb(0xcba6f7);
pub const SYN_STRING: Color = rgb(0xa6e3a1);
pub const SYN_NUMBER: Color = rgb(0xfab387);
pub const SYN_COMMENT: Color = rgb(0x7f849c);
pub const SYN_FUNCTION: Color = rgb(0x89b4fa);
pub const SYN_TYPE: Color = rgb(0xf9e2af);
pub const SYN_MACRO: Color = rgb(0x94e2d5);
pub const SYN_LIFETIME: Color = rgb(0x89dceb);
pub const SYN_ATTRIBUTE: Color = rgb(0xfab387);
pub const SYN_SELF: Color = rgb(0xf38ba8);
pub const SYN_OPERATOR: Color = rgb(0x9399b2);

// Editor chrome.
pub const CURSORLINE_BG: Color = rgb(0x1e1e2e);
pub const VISUAL_BG: Color = rgb(0x45475a);
pub const MATCH_BG: Color = rgb(0x585b70);
pub const SEARCH_BG: Color = rgb(0xf9e2af);
pub const SEARCH_FG: Color = Color::Rgb(6, 6, 10);
pub const SEARCH_CUR_BG: Color = Color::Rgb(255, 20, 147);
pub const LINENR: Color = rgb(0x585b70);
pub const LINENR_CUR: Color = Color::Rgb(255, 20, 147);

// Statusline mode chips: (foreground, background).
pub const MODE_NORMAL: (Color, Color) = (Color::Rgb(6, 6, 10), ACCENT);
pub const MODE_INSERT: (Color, Color) = (Color::Rgb(6, 6, 10), SUCCESS);
pub const MODE_VISUAL: (Color, Color) = (Color::Rgb(6, 6, 10), TERTIARY);
pub const MODE_REPLACE: (Color, Color) = (Color::Rgb(6, 6, 10), ERROR);
pub const MODE_COMMAND: (Color, Color) = (Color::Rgb(6, 6, 10), WARNING);

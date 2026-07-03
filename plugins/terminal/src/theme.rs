//! Mastertech terminal palette (Catppuccin Mocha + deep-pink accent).
#![allow(dead_code)]

use ratatui::style::Color;

const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

pub const BG: Color = Color::Rgb(6, 6, 10);
pub const TEXT: Color = rgb(0xcdd6f4);
pub const MUTED: Color = rgb(0xbac2de);
pub const DIM: Color = rgb(0x7f849c);
pub const ACCENT: Color = Color::Rgb(255, 20, 147);
pub const TERTIARY: Color = rgb(0xcba6f7);
pub const SUCCESS: Color = rgb(0xa6e3a1);
pub const ERROR: Color = rgb(0xf38ba8);
pub const WARNING: Color = rgb(0xf9e2af);
pub const CYAN: Color = rgb(0x89dceb);

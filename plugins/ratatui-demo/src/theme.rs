//! Mastertech terminal palette: Catppuccin Mocha with a deep-pink accent and mauve tertiary,
//! matching `MastertechProject`'s default `AppTheme`.

use ratatui::style::{Color, Modifier, Style};

#[allow(dead_code)]
pub struct AppTheme {
    pub bg: Color,
    pub surface: Color,
    pub border_muted: Color,
    pub text: Color,
    pub text_muted: Color,
    pub accent: Color,
    pub tertiary: Color,
    pub success: Color,
    pub error: Color,
    pub warning: Color,
}

const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// The built-in default: pink accent + mauve tertiary on the dark app background.
pub const THEME: AppTheme = AppTheme {
    bg: Color::Rgb(6, 6, 10),
    surface: rgb(0x313244),   // Catppuccin surface0
    border_muted: rgb(0x585b70), // surface2
    text: rgb(0xcdd6f4),
    text_muted: rgb(0xbac2de), // subtext1
    accent: Color::Rgb(255, 20, 147), // deep pink
    tertiary: rgb(0xcba6f7),   // mauve
    success: rgb(0xa6e3a1),
    error: rgb(0xf38ba8),
    warning: rgb(0xf9e2af),
};

impl AppTheme {
    pub fn title(&self) -> Style {
        Style::new().fg(self.accent).add_modifier(Modifier::BOLD)
    }

    pub fn border(&self, focused: bool) -> Style {
        Style::new().fg(if focused { self.accent } else { self.border_muted })
    }

    pub fn menu_highlight(&self) -> Style {
        Style::new().bg(self.surface).fg(self.accent).add_modifier(Modifier::BOLD)
    }
}

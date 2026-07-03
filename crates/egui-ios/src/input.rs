//! Translation of iOS hardware-keyboard HID usage codes and modifier flags to egui.

use egui::{Key, Modifiers};

// UIKeyModifierFlags bit positions.
const MOD_SHIFT: i32 = 1 << 17;
const MOD_CONTROL: i32 = 1 << 18;
const MOD_ALTERNATE: i32 = 1 << 19;
const MOD_COMMAND: i32 = 1 << 20;

/// Map `UIKey.modifierFlags` to egui modifiers.
pub fn ios_modifiers_to_egui(flags: i32) -> Modifiers {
    let command = flags & MOD_COMMAND != 0;
    Modifiers {
        alt: flags & MOD_ALTERNATE != 0,
        ctrl: flags & MOD_CONTROL != 0,
        shift: flags & MOD_SHIFT != 0,
        mac_cmd: command,
        command,
    }
}

/// Map a USB HID keyboard usage code (HID page 0x07) to an egui [`Key`].
pub fn hid_to_egui_key(hid: i32) -> Option<Key> {
    let key = match hid {
        0x04 => Key::A,
        0x05 => Key::B,
        0x06 => Key::C,
        0x07 => Key::D,
        0x08 => Key::E,
        0x09 => Key::F,
        0x0A => Key::G,
        0x0B => Key::H,
        0x0C => Key::I,
        0x0D => Key::J,
        0x0E => Key::K,
        0x0F => Key::L,
        0x10 => Key::M,
        0x11 => Key::N,
        0x12 => Key::O,
        0x13 => Key::P,
        0x14 => Key::Q,
        0x15 => Key::R,
        0x16 => Key::S,
        0x17 => Key::T,
        0x18 => Key::U,
        0x19 => Key::V,
        0x1A => Key::W,
        0x1B => Key::X,
        0x1C => Key::Y,
        0x1D => Key::Z,
        0x1E => Key::Num1,
        0x1F => Key::Num2,
        0x20 => Key::Num3,
        0x21 => Key::Num4,
        0x22 => Key::Num5,
        0x23 => Key::Num6,
        0x24 => Key::Num7,
        0x25 => Key::Num8,
        0x26 => Key::Num9,
        0x27 => Key::Num0,
        0x28 => Key::Enter,
        0x29 => Key::Escape,
        0x2A => Key::Backspace,
        0x2B => Key::Tab,
        0x2C => Key::Space,
        0x2D => Key::Minus,
        0x2E => Key::Equals,
        0x2F => Key::OpenBracket,
        0x30 => Key::CloseBracket,
        0x31 => Key::Backslash,
        0x33 => Key::Semicolon,
        0x34 => Key::Quote,
        0x35 => Key::Backtick,
        0x36 => Key::Comma,
        0x37 => Key::Period,
        0x38 => Key::Slash,
        0x3A => Key::F1,
        0x3B => Key::F2,
        0x3C => Key::F3,
        0x3D => Key::F4,
        0x3E => Key::F5,
        0x3F => Key::F6,
        0x40 => Key::F7,
        0x41 => Key::F8,
        0x42 => Key::F9,
        0x43 => Key::F10,
        0x44 => Key::F11,
        0x45 => Key::F12,
        0x49 => Key::Insert,
        0x4A => Key::Home,
        0x4B => Key::PageUp,
        0x4C => Key::Delete,
        0x4D => Key::End,
        0x4E => Key::PageDown,
        0x4F => Key::ArrowRight,
        0x50 => Key::ArrowLeft,
        0x51 => Key::ArrowDown,
        0x52 => Key::ArrowUp,
        _ => return None,
    };
    Some(key)
}

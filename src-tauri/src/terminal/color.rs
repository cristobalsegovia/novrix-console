//! Terminal cell colors and character styling.
//!
//! These are terminal-specific value types owned by the Terminal_Emulator (the
//! `domain` layer carries no cell/color/style types). They model exactly what
//! the SGR (Select Graphic Rendition) escape sequences the emulator interprets
//! can set: a foreground/background color and the bold/underline attributes
//! named in Requirement 10.3.
//!
//! Everything here is a pure, `Copy` value type with `serde` derives so the
//! grid snapshot the emulator exposes can cross the Tauri IPC boundary to the
//! xterm.js frontend (and be inspected directly by the property tests in tasks
//! 14.2-14.5).

use serde::{Deserialize, Serialize};

/// A terminal color.
///
/// The emulator interprets at least the 16 basic ANSI colors (8 standard +
/// 8 bright) plus the 256-color indexed palette and 24-bit truecolor, so the
/// reference model and the frontend can represent any color an SGR sequence
/// selects. [`Color::Default`] means "use the terminal's default" and is what a
/// freshly reset cell carries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Color {
    /// The terminal's default foreground/background color.
    #[default]
    Default,
    /// One of the 16 basic ANSI colors.
    Ansi(AnsiColor),
    /// An index `0..=255` into the xterm 256-color palette.
    Indexed(u8),
    /// A 24-bit truecolor value.
    Rgb { r: u8, g: u8, b: u8 },
}

/// The 16 basic ANSI colors: the 8 standard colors and their 8 bright variants.
///
/// Standard colors are selected by SGR codes `30..=37` (foreground) and
/// `40..=47` (background); the bright variants by `90..=97` and `100..=107`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AnsiColor {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    BrightBlack,
    BrightRed,
    BrightGreen,
    BrightYellow,
    BrightBlue,
    BrightMagenta,
    BrightCyan,
    BrightWhite,
}

impl AnsiColor {
    /// Map a standard-color offset `0..=7` (an SGR code minus 30 or 40) to the
    /// corresponding standard [`AnsiColor`]. Returns `None` for out-of-range
    /// offsets.
    pub fn from_standard_offset(offset: u16) -> Option<Self> {
        Some(match offset {
            0 => AnsiColor::Black,
            1 => AnsiColor::Red,
            2 => AnsiColor::Green,
            3 => AnsiColor::Yellow,
            4 => AnsiColor::Blue,
            5 => AnsiColor::Magenta,
            6 => AnsiColor::Cyan,
            7 => AnsiColor::White,
            _ => return None,
        })
    }

    /// Map a bright-color offset `0..=7` (an SGR code minus 90 or 100) to the
    /// corresponding bright [`AnsiColor`]. Returns `None` for out-of-range
    /// offsets.
    pub fn from_bright_offset(offset: u16) -> Option<Self> {
        Some(match offset {
            0 => AnsiColor::BrightBlack,
            1 => AnsiColor::BrightRed,
            2 => AnsiColor::BrightGreen,
            3 => AnsiColor::BrightYellow,
            4 => AnsiColor::BrightBlue,
            5 => AnsiColor::BrightMagenta,
            6 => AnsiColor::BrightCyan,
            7 => AnsiColor::BrightWhite,
            _ => return None,
        })
    }
}

/// The styling attributes a cell can carry, as set by SGR sequences.
///
/// This is the full visual style applied to a glyph: its foreground and
/// background color plus the bold/underline attributes called out in
/// Requirement 10.3. A default `CellStyle` is the "reset" state (`SGR 0`): both
/// colors default, no bold, no underline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CellStyle {
    /// Foreground (text) color.
    pub foreground: Color,
    /// Background color.
    pub background: Color,
    /// Whether the bold attribute is set.
    pub bold: bool,
    /// Whether the underline attribute is set.
    pub underline: bool,
}

impl CellStyle {
    /// Reset the style to its default (`SGR 0`) state.
    pub fn reset(&mut self) {
        *self = CellStyle::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_color_is_default_variant() {
        assert_eq!(Color::default(), Color::Default);
    }

    #[test]
    fn default_style_is_fully_reset() {
        let s = CellStyle::default();
        assert_eq!(s.foreground, Color::Default);
        assert_eq!(s.background, Color::Default);
        assert!(!s.bold);
        assert!(!s.underline);
    }

    #[test]
    fn standard_and_bright_offsets_map_as_expected() {
        assert_eq!(AnsiColor::from_standard_offset(1), Some(AnsiColor::Red));
        assert_eq!(AnsiColor::from_standard_offset(7), Some(AnsiColor::White));
        assert_eq!(AnsiColor::from_standard_offset(8), None);
        assert_eq!(
            AnsiColor::from_bright_offset(4),
            Some(AnsiColor::BrightBlue)
        );
        assert_eq!(AnsiColor::from_bright_offset(9), None);
    }
}

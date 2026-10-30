//! Semantic Apple HIG colors for Dark and Light modes across all ColorDepth levels.

use ratatui::style::Color;

use super::env::{Appearance, ColorDepth};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub label: Color,
    pub secondary: Color,
    pub tertiary: Color,
    pub quaternary: Color,
    pub separator: Color,
    pub fill: Color,
    pub selection: Color,
    pub accent: Color,
    pub accent_text: Color,
    pub green: Color,
    pub orange: Color,
    pub red: Color,
    pub yellow: Color,
    pub teal: Color,
    pub purple: Color,
    pub gray: Color,
    pub on_accent: Color,
}

impl Palette {
    pub fn resolve(appearance: Appearance, depth: ColorDepth) -> Self {
        match depth {
            ColorDepth::NoColor => Self::no_color(),
            ColorDepth::Ansi16 => Self::ansi16(appearance),
            ColorDepth::Ansi256 => Self::ansi256(appearance),
            ColorDepth::TrueColor => Self::true_color(appearance),
        }
    }

    fn true_color(appearance: Appearance) -> Self {
        match appearance {
            Appearance::Dark => Self {
                label: Color::Rgb(0xF5, 0xF5, 0xF7),
                secondary: Color::Rgb(0xA1, 0xA1, 0xA6),
                tertiary: Color::Rgb(0x6E, 0x6E, 0x73),
                quaternary: Color::Rgb(0x48, 0x48, 0x4A),
                separator: Color::Rgb(0x38, 0x38, 0x3A),
                fill: Color::Rgb(0x2C, 0x2C, 0x2E),
                selection: Color::Rgb(0x1E, 0x3A, 0x5F),
                accent: Color::Rgb(0x0A, 0x84, 0xFF),
                accent_text: Color::Rgb(0x40, 0x9C, 0xFF),
                green: Color::Rgb(0x30, 0xD1, 0x58),
                orange: Color::Rgb(0xFF, 0x9F, 0x0A),
                red: Color::Rgb(0xFF, 0x45, 0x3A),
                yellow: Color::Rgb(0xFF, 0xD6, 0x0A),
                teal: Color::Rgb(0x64, 0xD2, 0xFF),
                purple: Color::Rgb(0xBF, 0x5A, 0xF2),
                gray: Color::Rgb(0x8E, 0x8E, 0x93),
                on_accent: Color::Rgb(0xFF, 0xFF, 0xFF),
            },
            Appearance::Light => Self {
                label: Color::Rgb(0x1D, 0x1D, 0x1F),
                secondary: Color::Rgb(0x6E, 0x6E, 0x73),
                tertiary: Color::Rgb(0xA1, 0xA1, 0xA6),
                quaternary: Color::Rgb(0xC7, 0xC7, 0xCC),
                separator: Color::Rgb(0xD2, 0xD2, 0xD7),
                fill: Color::Rgb(0xE8, 0xE8, 0xED),
                selection: Color::Rgb(0xD6, 0xE6, 0xFF),
                accent: Color::Rgb(0x00, 0x7A, 0xFF),
                accent_text: Color::Rgb(0x00, 0x60, 0xDF),
                green: Color::Rgb(0x24, 0x8A, 0x3D),
                orange: Color::Rgb(0xC9, 0x34, 0x00),
                red: Color::Rgb(0xD7, 0x00, 0x15),
                yellow: Color::Rgb(0xB2, 0x50, 0x00),
                teal: Color::Rgb(0x00, 0x71, 0xA4),
                purple: Color::Rgb(0x89, 0x44, 0xAB),
                gray: Color::Rgb(0x8E, 0x8E, 0x93),
                on_accent: Color::Rgb(0xFF, 0xFF, 0xFF),
            },
        }
    }

    fn ansi256(appearance: Appearance) -> Self {
        match appearance {
            Appearance::Dark => Self {
                label: Color::Indexed(255),
                secondary: Color::Indexed(248),
                tertiary: Color::Indexed(242),
                quaternary: Color::Indexed(239),
                separator: Color::Indexed(237),
                fill: Color::Indexed(236),
                selection: Color::Indexed(24),
                accent: Color::Indexed(33),
                accent_text: Color::Indexed(75),
                green: Color::Indexed(77),
                orange: Color::Indexed(214),
                red: Color::Indexed(203),
                yellow: Color::Indexed(220),
                teal: Color::Indexed(81),
                purple: Color::Indexed(135),
                gray: Color::Indexed(245),
                on_accent: Color::Indexed(231),
            },
            Appearance::Light => Self {
                label: Color::Indexed(234),
                secondary: Color::Indexed(243),
                tertiary: Color::Indexed(248),
                quaternary: Color::Indexed(251),
                separator: Color::Indexed(252),
                fill: Color::Indexed(254),
                selection: Color::Indexed(153),
                accent: Color::Indexed(27),
                accent_text: Color::Indexed(26),
                green: Color::Indexed(28),
                orange: Color::Indexed(166),
                red: Color::Indexed(160),
                yellow: Color::Indexed(136),
                teal: Color::Indexed(31),
                purple: Color::Indexed(97),
                gray: Color::Indexed(245),
                on_accent: Color::Indexed(231),
            },
        }
    }

    fn ansi16(_appearance: Appearance) -> Self {
        Self {
            label: Color::Reset,
            secondary: Color::DarkGray,
            tertiary: Color::DarkGray,
            quaternary: Color::DarkGray,
            separator: Color::DarkGray,
            fill: Color::DarkGray,
            selection: Color::Blue,
            accent: Color::Blue,
            accent_text: Color::Cyan,
            green: Color::Green,
            orange: Color::Yellow,
            red: Color::Red,
            yellow: Color::Yellow,
            teal: Color::Cyan,
            purple: Color::Magenta,
            gray: Color::DarkGray,
            on_accent: Color::White,
        }
    }

    fn no_color() -> Self {
        Self {
            label: Color::Reset,
            secondary: Color::Reset,
            tertiary: Color::Reset,
            quaternary: Color::Reset,
            separator: Color::Reset,
            fill: Color::Reset,
            selection: Color::Reset,
            accent: Color::Reset,
            accent_text: Color::Reset,
            green: Color::Reset,
            orange: Color::Reset,
            red: Color::Reset,
            yellow: Color::Reset,
            teal: Color::Reset,
            purple: Color::Reset,
            gray: Color::Reset,
            on_accent: Color::Reset,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_palette_resolve_all() {
        for app in [Appearance::Dark, Appearance::Light] {
            for depth in [
                ColorDepth::TrueColor,
                ColorDepth::Ansi256,
                ColorDepth::Ansi16,
                ColorDepth::NoColor,
            ] {
                let p = Palette::resolve(app, depth);
                if depth == ColorDepth::NoColor {
                    assert_eq!(p.label, Color::Reset);
                    assert_eq!(p.accent, Color::Reset);
                } else if depth == ColorDepth::TrueColor {
                    assert!(matches!(p.accent, Color::Rgb(..)));
                } else if depth == ColorDepth::Ansi256 {
                    assert!(matches!(p.accent, Color::Indexed(..)));
                }
            }
        }
    }
}

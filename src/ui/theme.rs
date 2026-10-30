//! TUI Color Theme, Design Tokens, and Styling.

use ratatui::style::{Modifier, Style};

use super::env::{Appearance, ColorDepth, UiEnv};
use super::glyphs::Glyphs;
use super::palette::Palette;
use crate::core::stores::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonKind {
    Plain,
    Primary,
    Destructive,
}

pub struct Theme {
    pub p: Palette,
    pub g: Glyphs,
    pub env: UiEnv,
}

impl Default for Theme {
    fn default() -> Self {
        Self::new()
    }
}

impl Theme {
    pub fn new() -> Self {
        Self::with_env(UiEnv::detect(&Config::default()))
    }

    pub fn with_env(env: UiEnv) -> Self {
        let p = Palette::resolve(env.appearance, env.depth);
        let g = if env.ascii_glyphs {
            Glyphs::ascii()
        } else {
            Glyphs::unicode()
        };

        Self { p, g, env }
    }

    pub fn plain() -> Self {
        Self::with_env(UiEnv {
            appearance: Appearance::Dark,
            depth: ColorDepth::NoColor,
            ascii_glyphs: true,
            reduce_motion: false,
        })
    }

    // Typography
    pub fn headline(&self) -> Style {
        Style::default()
            .fg(self.p.label)
            .add_modifier(Modifier::BOLD)
    }

    pub fn body(&self) -> Style {
        Style::default().fg(self.p.label)
    }

    pub fn callout(&self) -> Style {
        Style::default().fg(self.p.secondary)
    }

    pub fn footnote(&self) -> Style {
        Style::default().fg(self.p.tertiary)
    }

    pub fn section_header(&self) -> Style {
        Style::default()
            .fg(self.p.tertiary)
            .add_modifier(Modifier::BOLD)
    }

    // Chrome
    pub fn separator(&self) -> Style {
        Style::default().fg(self.p.separator)
    }

    pub fn keycap(&self) -> Style {
        if self.env.depth == ColorDepth::NoColor {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
                .bg(self.p.fill)
                .fg(self.p.label)
                .add_modifier(Modifier::BOLD)
        }
    }

    pub fn keycap_label(&self) -> Style {
        Style::default().fg(self.p.secondary)
    }

    // States
    pub fn row(&self, selected: bool) -> Style {
        if selected {
            if self.env.depth == ColorDepth::NoColor {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
                    .bg(self.p.selection)
                    .fg(self.p.label)
                    .add_modifier(Modifier::BOLD)
            }
        } else {
            Style::default().fg(self.p.label)
        }
    }

    pub fn accent_text(&self) -> Style {
        Style::default().fg(self.p.accent_text)
    }

    pub fn success(&self) -> Style {
        Style::default()
            .fg(self.p.green)
            .add_modifier(Modifier::BOLD)
    }

    pub fn warning(&self) -> Style {
        Style::default()
            .fg(self.p.yellow)
            .add_modifier(Modifier::BOLD)
    }

    pub fn danger(&self) -> Style {
        Style::default().fg(self.p.red).add_modifier(Modifier::BOLD)
    }

    pub fn button(&self, kind: ButtonKind, focused: bool) -> Style {
        match kind {
            ButtonKind::Plain => {
                if focused {
                    if self.env.depth == ColorDepth::NoColor {
                        Style::default().add_modifier(Modifier::REVERSED)
                    } else {
                        Style::default()
                            .bg(self.p.fill)
                            .fg(self.p.label)
                            .add_modifier(Modifier::BOLD)
                    }
                } else {
                    Style::default().fg(self.p.secondary)
                }
            }
            ButtonKind::Primary => {
                if focused {
                    if self.env.depth == ColorDepth::NoColor {
                        Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
                    } else {
                        Style::default()
                            .bg(self.p.accent)
                            .fg(self.p.on_accent)
                            .add_modifier(Modifier::BOLD)
                    }
                } else {
                    Style::default().fg(self.p.accent_text)
                }
            }
            ButtonKind::Destructive => {
                if focused {
                    if self.env.depth == ColorDepth::NoColor {
                        Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
                    } else {
                        Style::default()
                            .bg(self.p.red)
                            .fg(self.p.on_accent)
                            .add_modifier(Modifier::BOLD)
                    }
                } else {
                    Style::default().fg(self.p.red)
                }
            }
        }
    }

    pub fn size(&self, bytes: u64) -> Style {
        if bytes < 1_000_000_000 {
            Style::default().fg(self.p.secondary)
        } else if bytes < 10_000_000_000 {
            Style::default()
                .fg(self.p.label)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(self.p.orange)
                .add_modifier(Modifier::BOLD)
        }
    }
}

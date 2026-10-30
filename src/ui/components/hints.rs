//! Keycap hints and primary actions for toolbar.

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use crate::ui::env::ColorDepth;
use crate::ui::theme::{ButtonKind, Theme};

#[derive(Debug, Clone)]
pub struct Hint<'a> {
    pub key: &'a str,
    pub desc: &'a str,
}

impl<'a> Hint<'a> {
    pub fn new(key: &'a str, desc: &'a str) -> Self {
        Self { key, desc }
    }

    pub fn to_spans(&self, theme: &Theme) -> Vec<Span<'static>> {
        let keycap_style = theme.keycap();
        let desc_style = theme.callout();
        vec![
            Span::styled(format!(" {} ", self.key), keycap_style),
            Span::raw(" "),
            Span::styled(self.desc.to_string(), desc_style),
        ]
    }
}

#[derive(Debug, Clone)]
pub struct PrimaryAction<'a> {
    pub label: String,
    pub key: &'a str,
    pub enabled: bool,
    pub destructive: bool,
}

impl<'a> PrimaryAction<'a> {
    pub fn new(label: impl Into<String>, key: &'a str) -> Self {
        Self {
            label: label.into(),
            key,
            enabled: true,
            destructive: false,
        }
    }

    pub fn destructive(label: impl Into<String>, key: &'a str) -> Self {
        Self {
            label: label.into(),
            key,
            enabled: true,
            destructive: true,
        }
    }

    pub fn disabled(label: impl Into<String>, key: &'a str) -> Self {
        Self {
            label: label.into(),
            key,
            enabled: false,
            destructive: false,
        }
    }

    pub fn to_spans(&self, theme: &Theme) -> Vec<Span<'static>> {
        if !self.enabled {
            vec![Span::styled(
                format!(" {}  {} ", self.label, self.key),
                theme.footnote(),
            )]
        } else if self.destructive {
            if theme.env.depth == ColorDepth::NoColor {
                vec![Span::styled(
                    format!(" {}  {} ", self.label, self.key),
                    theme.headline().add_modifier(Modifier::REVERSED),
                )]
            } else {
                vec![Span::styled(
                    format!(" {}  {} ", self.label, self.key),
                    theme.button(ButtonKind::Destructive, true),
                )]
            }
        } else if theme.env.depth == ColorDepth::NoColor {
            vec![Span::styled(
                format!(" {}  {} ", self.label, self.key),
                theme.headline().add_modifier(Modifier::REVERSED),
            )]
        } else {
            vec![Span::styled(
                format!(" {}  {} ", self.label, self.key),
                theme.button(ButtonKind::Primary, true),
            )]
        }
    }
}

pub fn render_hints_line<'a>(hints: &[Hint<'a>], theme: &Theme) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, hint) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("   "));
        }
        spans.extend(hint.to_spans(theme));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hint_spans() {
        let theme = Theme::new();
        let hint = Hint::new("↵", "Open");
        let spans = hint.to_spans(&theme);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content, " ↵ ");
        assert_eq!(spans[2].content, "Open");
    }

    #[test]
    fn test_primary_action() {
        let theme = Theme::new();
        let act = PrimaryAction::new("Review 2 items", "↵");
        let spans = act.to_spans(&theme);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, " Review 2 items  ↵ ");
    }
}

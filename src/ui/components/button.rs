//! HIG Button and ButtonGroup components.

use ratatui::text::{Line, Span};

use crate::ui::theme::{ButtonKind, Theme};

#[derive(Debug, Clone)]
pub struct Button<'a> {
    pub label: &'a str,
    pub kind: ButtonKind,
    pub focused: bool,
}

impl<'a> Button<'a> {
    pub fn new(label: &'a str) -> Self {
        Self {
            label,
            kind: ButtonKind::Plain,
            focused: false,
        }
    }

    pub fn primary(label: &'a str) -> Self {
        Self {
            label,
            kind: ButtonKind::Primary,
            focused: false,
        }
    }

    pub fn destructive(label: &'a str) -> Self {
        Self {
            label,
            kind: ButtonKind::Destructive,
            focused: false,
        }
    }

    pub fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    pub fn to_span(&self, theme: &Theme) -> Span<'static> {
        let style = theme.button(self.kind, self.focused);
        Span::styled(format!(" {} ", self.label), style)
    }
}

pub fn render_button_group<'a>(buttons: &[Button<'a>], theme: &Theme) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, btn) in buttons.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(btn.to_span(theme));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_button_rendering() {
        let theme = Theme::new();
        let cancel = Button::new("Cancel").focused(false);
        let remove = Button::destructive("Remove 12 items").focused(true);
        let group = render_button_group(&[cancel, remove], &theme);
        assert_eq!(group.spans.len(), 3);
        assert_eq!(group.spans[0].content, " Cancel ");
        assert_eq!(group.spans[1].content, "  ");
        assert_eq!(group.spans[2].content, " Remove 12 items ");
    }
}

//! Help Sheet modal displaying full keyboard shortcuts for the current screen.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::button::Button;
use super::hints::Hint;
use super::modal::Modal;
use crate::ui::theme::Theme;

pub struct HelpSheet<'a> {
    pub shortcuts: Vec<Hint<'a>>,
}

impl<'a> HelpSheet<'a> {
    pub fn new(shortcuts: Vec<Hint<'a>>) -> Self {
        Self { shortcuts }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        // Arrange shortcuts in 2 columns
        let mut lines = Vec::new();
        let mid = self.shortcuts.len().div_ceil(2);

        let col1 = &self.shortcuts[..mid];
        let col2 = &self.shortcuts[mid..];

        for i in 0..mid {
            let mut row_spans = Vec::new();

            // Col 1
            if let Some(h) = col1.get(i) {
                row_spans.push(Span::styled(format!(" {:<6} ", h.key), theme.keycap()));
                row_spans.push(Span::raw(" "));
                row_spans.push(Span::styled(format!("{:<14}", h.desc), theme.callout()));
            }

            row_spans.push(Span::raw("    "));

            // Col 2
            if let Some(h) = col2.get(i) {
                row_spans.push(Span::styled(format!(" {:<6} ", h.key), theme.keycap()));
                row_spans.push(Span::raw(" "));
                row_spans.push(Span::styled(format!("{:<14}", h.desc), theme.callout()));
            }

            lines.push(Line::from(row_spans));
        }

        Modal::new("Keyboard Shortcuts")
            .message(lines)
            .buttons(vec![Button::primary("Done").focused(true)])
            .hints(Line::from(vec![
                Span::styled("↵/⎋/?", theme.callout()),
                Span::styled(" Close", theme.footnote()),
            ]))
            .render(frame, area, theme);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn test_help_sheet_render() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::new();

        let sheet = HelpSheet::new(vec![
            Hint::new("↑↓", "Move"),
            Hint::new("␣", "Select"),
            Hint::new("↵", "Open"),
            Hint::new("⎋", "Back"),
        ]);

        terminal
            .draw(|f| {
                sheet.render(f, f.area(), &theme);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let text = format!("{buffer:?}");
        assert!(text.contains("Keyboard Shortcuts"));
        assert!(text.contains("Move"));
        assert!(text.contains("Select"));
    }
}

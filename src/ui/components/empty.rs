//! Empty state placeholder component.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::ui::theme::Theme;

pub struct EmptyState<'a> {
    pub glyph: &'a str,
    pub glyph_color: ratatui::style::Color,
    pub title: &'a str,
    pub subtitle: &'a str,
    pub hint: Option<&'a str>,
}

impl<'a> EmptyState<'a> {
    pub fn new(glyph: &'a str, title: &'a str, subtitle: &'a str) -> Self {
        Self {
            glyph,
            glyph_color: ratatui::style::Color::Reset,
            title,
            subtitle,
            hint: None,
        }
    }

    pub fn glyph_color(mut self, color: ratatui::style::Color) -> Self {
        self.glyph_color = color;
        self
    }

    pub fn hint(mut self, hint: &'a str) -> Self {
        self.hint = Some(hint);
        self
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let total_lines = if self.hint.is_some() { 6 } else { 4 };
        if area.height < total_lines as u16 {
            return;
        }

        let top_pad = (area.height as usize).saturating_sub(total_lines) / 2;

        let mut lines = Vec::new();
        for _ in 0..top_pad {
            lines.push(Line::from(""));
        }

        // Glyph
        let glyph_style = if self.glyph_color == ratatui::style::Color::Reset {
            theme.callout()
        } else {
            ratatui::style::Style::default().fg(self.glyph_color)
        };
        lines.push(
            Line::from(vec![Span::styled(self.glyph, glyph_style)]).alignment(Alignment::Center),
        );
        lines.push(Line::from(""));

        // Title
        lines.push(
            Line::from(vec![Span::styled(self.title, theme.headline())])
                .alignment(Alignment::Center),
        );

        // Subtitle
        lines.push(
            Line::from(vec![Span::styled(self.subtitle, theme.callout())])
                .alignment(Alignment::Center),
        );

        // Optional Hint
        if let Some(h) = self.hint {
            lines.push(Line::from(""));
            lines.push(
                Line::from(vec![Span::styled(h, theme.footnote())]).alignment(Alignment::Center),
            );
        }

        frame.render_widget(Paragraph::new(lines), area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn test_empty_state_render() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::new();

        terminal
            .draw(|f| {
                EmptyState::new("✓", "Nothing to clean", "Your caches are already tidy.").render(
                    f,
                    f.area(),
                    &theme,
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let text = format!("{buffer:?}");
        assert!(text.contains("Nothing to clean"));
        assert!(text.contains("already tidy"));
    }
}

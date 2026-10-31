//! macOS-style Sheet / Alert Modal with backdrop dimming and drop shadow.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use super::button::{Button, render_button_group};
use super::frame::frame_style;
use crate::ui::env::ColorDepth;
use crate::ui::theme::Theme;

pub fn dim_backdrop(buf: &mut Buffer, area: Rect) {
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let cell = &mut buf[(x, y)];
            cell.set_style(cell.style().add_modifier(Modifier::DIM));
        }
    }
}

pub struct Modal<'a> {
    pub title: &'a str,
    pub message: Vec<Line<'a>>,
    pub buttons: Vec<Button<'a>>,
    pub hints: Option<Line<'a>>,
    pub tint: Option<Color>,
}

impl<'a> Modal<'a> {
    pub fn new(title: &'a str) -> Self {
        Self {
            title,
            message: Vec::new(),
            buttons: Vec::new(),
            hints: None,
            tint: None,
        }
    }

    /// Colors the border, for dialogs that report work in progress.
    pub fn tint(mut self, color: Color) -> Self {
        self.tint = Some(color);
        self
    }

    pub fn message(mut self, message: Vec<Line<'a>>) -> Self {
        self.message = message;
        self
    }

    pub fn buttons(mut self, buttons: Vec<Button<'a>>) -> Self {
        self.buttons = buttons;
        self
    }

    pub fn hints(mut self, hints: Line<'a>) -> Self {
        self.hints = Some(hints);
        self
    }

    fn max_width(area: Rect) -> usize {
        (area.width.saturating_sub(8) as usize).clamp(36, 58)
    }

    /// Width and height a message fills in `area` without wrapping or losing the buttons.
    pub fn message_size(area: Rect) -> (usize, usize) {
        (
            Self::max_width(area).saturating_sub(4),
            (area.height as usize).saturating_sub(10),
        )
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        if area.width < 20 || area.height < 6 {
            return;
        }

        // 1. Dim background
        dim_backdrop(frame.buffer_mut(), area);

        // 2. Measure modal size
        let max_w = Self::max_width(area);

        let mut wrapped_lines: Vec<Line> = Vec::new();
        for line in &self.message {
            let full_str: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            if full_str.width() <= max_w.saturating_sub(4) {
                wrapped_lines.push(line.clone());
                continue;
            }
            let words = full_str.split_whitespace();
            let mut cur_line = String::new();
            for word in words {
                if cur_line.is_empty() {
                    cur_line.push_str(word);
                } else if cur_line.width() + 1 + word.width() <= max_w - 4 {
                    cur_line.push(' ');
                    cur_line.push_str(word);
                } else {
                    wrapped_lines.push(Line::from(Span::styled(cur_line.clone(), theme.body())));
                    cur_line = word.to_string();
                }
            }
            if !cur_line.is_empty() {
                wrapped_lines.push(Line::from(Span::styled(cur_line, theme.body())));
            }
        }

        let content_h = 1 /* title */ + 1 /* gap */ + wrapped_lines.len().max(1) + 1 /* gap */ + 1 /* buttons */ + 1 /* hints */ + 1 /* gap */;
        let modal_h = (content_h as u16 + 2).min(area.height.saturating_sub(2));
        let modal_w = (max_w as u16 + 4).min(area.width.saturating_sub(4));

        let modal_x = area.x + (area.width.saturating_sub(modal_w)) / 2;
        let modal_y = area.y + (area.height.saturating_sub(modal_h)) / 2;

        let modal_rect = Rect {
            x: modal_x,
            y: modal_y,
            width: modal_w,
            height: modal_h,
        };

        // 3. Render Drop Shadow (1 col right, 1 row bottom)
        if theme.env.depth != ColorDepth::NoColor {
            let shadow_rect = Rect {
                x: modal_rect.x + 1,
                y: modal_rect.y + 1,
                width: modal_rect.width,
                height: modal_rect.height,
            };
            let buf = frame.buffer_mut();
            for y in shadow_rect.top()..shadow_rect.bottom().min(area.bottom()) {
                let x = shadow_rect.right().saturating_sub(1);
                if x < area.right() && y < area.bottom() {
                    buf[(x, y)].set_bg(theme.p.fill);
                }
            }
            let bottom_y = shadow_rect.bottom().saturating_sub(1);
            if bottom_y < area.bottom() {
                for x in shadow_rect.left()..shadow_rect.right().min(area.right()) {
                    buf[(x, bottom_y)].set_bg(theme.p.fill);
                }
            }
        }

        // 4. Clear modal area
        frame.render_widget(Clear, modal_rect);

        // 5. Border
        let block = Block::default()
            .borders(Borders::ALL)
            .border_set(border::ROUNDED)
            .border_style(self.tint.map_or(theme.separator(), frame_style));
        frame.render_widget(block, modal_rect);

        // 6. Content inside modal
        let inner = Rect {
            x: modal_rect.x + 2,
            y: modal_rect.y + 1,
            width: modal_rect.width.saturating_sub(4),
            height: modal_rect.height.saturating_sub(2),
        };

        let mut lines = Vec::new();

        // Title
        lines.push(Line::from(vec![Span::styled(self.title, theme.headline())]));
        lines.push(Line::from(""));

        // Message
        lines.extend(wrapped_lines);

        // Pad down to buttons
        let current_h = lines.len();
        let target_h = (inner.height as usize).saturating_sub(2);
        if target_h > current_h {
            for _ in 0..(target_h - current_h) {
                lines.push(Line::from(""));
            }
        }

        // Buttons (right-aligned)
        let btn_line = render_button_group(&self.buttons, theme);
        let btn_w: usize = btn_line.spans.iter().map(|s| s.content.width()).sum();
        let btn_pad = (inner.width as usize).saturating_sub(btn_w);
        let mut btn_spans = vec![Span::raw(" ".repeat(btn_pad))];
        btn_spans.extend(btn_line.spans);
        lines.push(Line::from(btn_spans));

        // Hints
        let default_hints = Line::from(vec![
            Span::styled("⇥", theme.callout()),
            Span::styled(" Switch   ", theme.footnote()),
            Span::styled("↵", theme.callout()),
            Span::styled(" Choose   ", theme.footnote()),
            Span::styled("⎋", theme.callout()),
            Span::styled(" Cancel", theme.footnote()),
        ]);
        let hints_to_render = self.hints.as_ref().unwrap_or(&default_hints);
        let hint_w: usize = hints_to_render
            .spans
            .iter()
            .map(|s| s.content.width())
            .sum();
        let hint_pad = (inner.width as usize).saturating_sub(hint_w);
        let mut hint_spans = vec![Span::raw(" ".repeat(hint_pad))];
        for s in &hints_to_render.spans {
            hint_spans.push(Span::styled(s.content.to_string(), s.style));
        }
        lines.push(Line::from(hint_spans));

        frame.render_widget(Paragraph::new(lines), inner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn test_modal_render() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::new();

        terminal
            .draw(|f| {
                Modal::new("Remove 12 items?")
                    .message(vec![Line::from("3.4 GB will be permanently deleted.")])
                    .buttons(vec![
                        Button::new("Cancel").focused(true),
                        Button::destructive("Remove 12 items"),
                    ])
                    .render(f, f.area(), &theme);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let text = format!("{buffer:?}");
        assert!(text.contains("Remove 12 items?"));
        assert!(text.contains("permanently deleted"));
    }
}

//! Application Window Chrome (Header, Breadcrumbs, Separators, Toolbar).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use unicode_width::UnicodeWidthStr;

use super::hints::{Hint, PrimaryAction, render_hints_line};
use super::toast::Toast;
use crate::ui::theme::Theme;

pub const PAD_LEFT: u16 = 3;
pub const PAD_RIGHT: u16 = 2;
pub const MAX_CONTENT_WIDTH: u16 = 110;

pub struct AppChrome<'a> {
    pub crumbs: Vec<&'a str>,
    pub status: Option<Line<'a>>,
    pub hints: Vec<Hint<'a>>,
    pub primary: Option<PrimaryAction<'a>>,
    pub toast: Option<&'a Toast>,
}

impl<'a> AppChrome<'a> {
    pub fn new(crumbs: Vec<&'a str>) -> Self {
        Self {
            crumbs,
            status: None,
            hints: Vec::new(),
            primary: None,
            toast: None,
        }
    }

    pub fn status(mut self, status: Line<'a>) -> Self {
        self.status = Some(status);
        self
    }

    pub fn hints(mut self, hints: Vec<Hint<'a>>) -> Self {
        self.hints = hints;
        self
    }

    pub fn primary(mut self, primary: PrimaryAction<'a>) -> Self {
        self.primary = Some(primary);
        self
    }

    pub fn toast(mut self, toast: Option<&'a Toast>) -> Self {
        self.toast = toast;
        self
    }

    pub fn render(&self, frame: &mut Frame, theme: &Theme) -> Rect {
        let area = frame.area();
        if area.height < 4 || area.width < 10 {
            return area;
        }

        // --- 1. HEADER (Row 0) ---
        let header_rect = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        };

        let mut crumb_spans = Vec::new();
        crumb_spans.push(Span::raw(" ")); // 1 space margin

        if area.width < 64 && !self.crumbs.is_empty() {
            // Compact: only last crumb
            let last = self.crumbs.last().unwrap();
            crumb_spans.push(Span::styled(*last, theme.headline()));
        } else {
            let n = self.crumbs.len();
            for (i, crumb) in self.crumbs.iter().enumerate() {
                if i > 0 {
                    crumb_spans.push(Span::styled(
                        format!(" {} ", theme.g.breadcrumb_sep),
                        theme.footnote(),
                    ));
                }
                if i == 0 || i == n - 1 {
                    crumb_spans.push(Span::styled(*crumb, theme.headline()));
                } else {
                    crumb_spans.push(Span::styled(*crumb, theme.callout()));
                }
            }
        }

        let left_w: usize = crumb_spans.iter().map(|s| s.content.width()).sum();
        let right_w = self
            .status
            .as_ref()
            .map(|st| st.spans.iter().map(|s| s.content.width()).sum())
            .unwrap_or(0);

        let pad = (area.width as usize).saturating_sub(left_w + right_w + 1);
        crumb_spans.push(Span::raw(" ".repeat(pad)));

        if let Some(status) = &self.status {
            for s in &status.spans {
                crumb_spans.push(Span::styled(s.content.to_string(), s.style));
            }
            crumb_spans.push(Span::raw(" "));
        }

        frame.render_widget(Paragraph::new(Line::from(crumb_spans)), header_rect);

        // --- 2. TOP SEPARATOR (Row 1) ---
        let top_sep_rect = Rect {
            x: area.x,
            y: area.y + 1,
            width: area.width,
            height: 1,
        };
        let sep_block = Block::default()
            .borders(Borders::BOTTOM)
            .border_style(theme.separator());
        frame.render_widget(sep_block.clone(), top_sep_rect);

        // --- 3. BOTTOM SEPARATOR (Row height - 2) ---
        let bot_sep_rect = Rect {
            x: area.x,
            y: area.y + area.height.saturating_sub(2),
            width: area.width,
            height: 1,
        };
        frame.render_widget(sep_block, bot_sep_rect);

        // --- 4. TOOLBAR (Row height - 1) ---
        let toolbar_rect = Rect {
            x: area.x,
            y: area.y + area.height.saturating_sub(1),
            width: area.width,
            height: 1,
        };

        let mut left_spans = Vec::new();
        left_spans.push(Span::raw(" "));

        if let Some(t) = self.toast
            && !t.is_expired()
        {
            left_spans.push(t.to_span(theme));
        } else if area.width < 64 {
            let help_hint = Hint::new("?", "Help");
            left_spans.extend(help_hint.to_spans(theme));
        } else {
            let max_hints = 4;
            let take_hints: Vec<Hint> = self.hints.iter().take(max_hints).cloned().collect();
            let hints_line = render_hints_line(&take_hints, theme);
            left_spans.extend(hints_line.spans);
        }

        let mut right_spans = Vec::new();
        if let Some(act) = &self.primary {
            right_spans.extend(act.to_spans(theme));
            right_spans.push(Span::raw(" "));
        }

        let left_len: usize = left_spans.iter().map(|s| s.content.width()).sum();
        let right_len: usize = right_spans.iter().map(|s| s.content.width()).sum();
        let bar_pad = (area.width as usize).saturating_sub(left_len + right_len);

        left_spans.push(Span::raw(" ".repeat(bar_pad)));
        left_spans.extend(right_spans);

        frame.render_widget(Paragraph::new(Line::from(left_spans)), toolbar_rect);

        // --- 5. CONTENT RECT ---
        let total_avail_h = area.height.saturating_sub(4);
        let content_y = area.y + 2;

        let eff_w = area.width.saturating_sub(PAD_LEFT + PAD_RIGHT);
        let content_w = eff_w.min(MAX_CONTENT_WIDTH);
        let offset_x = if eff_w > MAX_CONTENT_WIDTH {
            (eff_w - MAX_CONTENT_WIDTH) / 2
        } else {
            0
        };

        Rect {
            x: area.x + PAD_LEFT + offset_x,
            y: content_y,
            width: content_w,
            height: total_avail_h,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn test_app_chrome_render() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::new();
        let chrome = AppChrome::new(vec!["taert", "Clean caches"])
            .hints(vec![Hint::new("↵", "Open"), Hint::new("⎋", "Back")])
            .primary(PrimaryAction::new("Review", "↵"));

        terminal
            .draw(|f| {
                let rect = chrome.render(f, &theme);
                assert_eq!(rect.y, 2);
                assert_eq!(rect.height, 20);
            })
            .unwrap();
    }
}

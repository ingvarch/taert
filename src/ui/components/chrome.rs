//! Application Window Chrome (Header, Breadcrumbs, Separators, Toolbar).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use unicode_width::UnicodeWidthStr;

use super::hints::{Hint, PrimaryAction, fit_hints, render_hints_line};
use super::toast::Toast;
use crate::ui::theme::Theme;

pub const PAD_LEFT: u16 = 3;
pub const PAD_RIGHT: u16 = 2;
pub const MAX_CONTENT_WIDTH: u16 = 110;
const TOOLBAR_MARGINS: usize = 2;

pub struct AppChrome<'a> {
    pub crumbs: Vec<&'a str>,
    pub status: Option<Line<'a>>,
    pub hints: Vec<Hint<'a>>,
    pub primary: Option<PrimaryAction<'a>>,
    pub toast: Option<&'a Toast>,
    pub full_width: bool,
    pub behind_dialog: bool,
}

impl<'a> AppChrome<'a> {
    pub fn new(crumbs: Vec<&'a str>) -> Self {
        Self {
            crumbs,
            status: None,
            hints: Vec::new(),
            primary: None,
            toast: None,
            full_width: false,
            behind_dialog: false,
        }
    }

    /// For data that uses every column. Prose stays at `MAX_CONTENT_WIDTH`.
    pub fn full_width(mut self) -> Self {
        self.full_width = true;
        self
    }

    /// A dialog lists its own keys, so the toolbar stays empty behind it.
    pub fn behind_dialog(mut self, dialog_open: bool) -> Self {
        self.behind_dialog = dialog_open;
        self
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

        let mut right_spans = Vec::new();
        if let Some(act) = self.primary.as_ref().filter(|_| !self.behind_dialog) {
            right_spans.extend(act.to_spans(theme));
            right_spans.push(Span::raw(" "));
        }
        let right_len: usize = right_spans.iter().map(|s| s.content.width()).sum();

        let mut left_spans = Vec::new();
        left_spans.push(Span::raw(" "));

        if let Some(t) = self.toast
            && !t.is_expired()
        {
            left_spans.push(t.to_span(theme));
        } else if !self.behind_dialog {
            let room = (area.width as usize).saturating_sub(right_len + TOOLBAR_MARGINS);
            let hints = fit_hints(&self.hints, room);
            left_spans.extend(render_hints_line(&hints, theme).spans);
        }

        let left_len: usize = left_spans.iter().map(|s| s.content.width()).sum();
        let bar_pad = (area.width as usize).saturating_sub(left_len + right_len);

        left_spans.push(Span::raw(" ".repeat(bar_pad)));
        left_spans.extend(right_spans);

        frame.render_widget(Paragraph::new(Line::from(left_spans)), toolbar_rect);

        self.content_rect(area)
    }

    pub fn content_rect(&self, area: Rect) -> Rect {
        if area.height < 4 || area.width < 10 {
            return area;
        }

        let total_avail_h = area.height.saturating_sub(4);
        let content_y = area.y + 2;

        let eff_w = area.width.saturating_sub(PAD_LEFT + PAD_RIGHT);
        let content_w = if self.full_width {
            eff_w
        } else {
            eff_w.min(MAX_CONTENT_WIDTH)
        };

        Rect {
            x: area.x + PAD_LEFT,
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

    fn content_rect(chrome: &AppChrome, width: u16, height: u16) -> Rect {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let theme = Theme::new();
        let mut rect = Rect::default();
        terminal.draw(|f| rect = chrome.render(f, &theme)).unwrap();
        rect
    }

    fn toolbar(chrome: &AppChrome, width: u16) -> String {
        let height = 24;
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let theme = Theme::new();
        terminal
            .draw(|f| {
                chrome.render(f, &theme);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..width)
            .map(|x| buffer[(x, height - 1)].symbol())
            .collect()
    }

    fn six_hints() -> Vec<Hint<'static>> {
        vec![
            Hint::new("␣", "Select"),
            Hint::new("A", "All"),
            Hint::new("N", "None"),
            Hint::new("/", "Search"),
            Hint::new("⎋", "Back"),
            Hint::new("?", "More"),
        ]
    }

    #[test]
    fn every_hint_shows_while_the_row_has_room() {
        let row = toolbar(&AppChrome::new(vec!["taert"]).hints(six_hints()), 120);

        for label in ["Select", "All", "None", "Search", "Back", "More"] {
            assert!(row.contains(label), "{row}");
        }
    }

    #[test]
    fn a_narrow_row_drops_hints_from_the_end_and_keeps_the_last() {
        let row = toolbar(&AppChrome::new(vec!["taert"]).hints(six_hints()), 66);

        assert!(row.contains("Search"), "{row}");
        assert!(!row.contains("Back"), "{row}");
        assert!(row.contains("More"), "{row}");
    }

    #[test]
    fn the_primary_action_takes_its_room_from_the_hints() {
        let chrome = AppChrome::new(vec!["taert"])
            .hints(six_hints())
            .primary(PrimaryAction::new("Review 2 items", "↵"));

        let row = toolbar(&chrome, 80);

        assert!(row.contains("Review 2 items"), "{row}");
        assert!(!row.contains("Back"), "{row}");
        assert!(row.contains("More"), "{row}");
    }

    #[test]
    fn a_narrow_window_still_shows_the_screen_s_own_hint() {
        let chrome = AppChrome::new(vec!["taert"]).hints(vec![Hint::new("⎋", "Cancel")]);

        assert!(toolbar(&chrome, 50).contains("Cancel"));
    }

    #[test]
    fn content_keeps_a_readable_width_at_the_left_edge() {
        let rect = content_rect(&AppChrome::new(vec!["taert"]), 205, 85);

        assert_eq!((rect.x, rect.width), (PAD_LEFT, MAX_CONTENT_WIDTH));
    }

    #[test]
    fn full_width_content_fills_the_window() {
        let rect = content_rect(&AppChrome::new(vec!["taert"]).full_width(), 205, 85);

        assert_eq!((rect.x, rect.width), (PAD_LEFT, 200));
    }

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

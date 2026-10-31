//! Progress card view component.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::frame::{FRAME_ROWS, FRAME_SIDES, card};
use super::gauge::bar;
use crate::core::path_utils::middle_truncate;
use crate::ui::theme::Theme;

const CARD_WIDTH: u16 = 76;
const BODY_ROWS: usize = 6;
const PERCENT_W: usize = 6;

pub struct ProgressView<'a> {
    pub spinner: &'a str,
    pub headline: &'a str,
    pub step_info: Option<&'a str>,
    pub fraction: Option<f64>,
    pub current_path: Option<&'a str>,
    pub counter_info: Option<&'a str>,
    pub elapsed_seconds: Option<u64>,
}

impl<'a> ProgressView<'a> {
    pub fn new(spinner: &'a str, headline: &'a str) -> Self {
        Self {
            spinner,
            headline,
            step_info: None,
            fraction: None,
            current_path: None,
            counter_info: None,
            elapsed_seconds: None,
        }
    }

    pub fn step_info(mut self, step: &'a str) -> Self {
        self.step_info = Some(step);
        self
    }

    pub fn fraction(mut self, frac: Option<f64>) -> Self {
        self.fraction = frac;
        self
    }

    pub fn current_path(mut self, path: &'a str) -> Self {
        self.current_path = Some(path);
        self
    }

    pub fn counter_info(mut self, info: &'a str) -> Self {
        self.counter_info = Some(info);
        self
    }

    pub fn elapsed_seconds(mut self, secs: u64) -> Self {
        self.elapsed_seconds = Some(secs);
        self
    }

    /// Draws a card in the middle of the window, within the rows of `area`.
    pub fn render(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let height = BODY_ROWS as u16 + FRAME_ROWS as u16;
        if area.height < height {
            return;
        }

        let window = frame.area();
        let width = CARD_WIDTH.min(area.width);
        let rect = Rect {
            x: window.x + (window.width - width) / 2,
            y: area.y + (area.height - height) / 2,
            width,
            height,
        };

        let inner = (width as usize).saturating_sub(FRAME_SIDES);
        let spinner = if theme.env.reduce_motion {
            theme.g.bullet
        } else {
            self.spinner
        };
        let title = format!("{spinner}  {}", self.headline);
        let lines = card(
            &title,
            Vec::new(),
            self.body(inner, theme),
            width as usize,
            BODY_ROWS,
            theme.p.accent,
            theme,
        );

        frame.render_widget(Paragraph::new(lines), rect);
    }

    fn body(&self, inner: usize, theme: &Theme) -> Vec<Line<'static>> {
        let step = self.step_info.unwrap_or_default();
        let step = Line::from(Span::styled(middle_truncate(step, inner), theme.callout()));

        let progress = match self.fraction {
            Some(fraction) => {
                let bar_w = inner.saturating_sub(PERCENT_W);
                let mut row = bar(fraction, bar_w as u16, theme.p.accent, theme);
                row.push(Span::styled(
                    format!("{:>PERCENT_W$}", format!("{:.0}%", fraction * 100.0)),
                    theme.footnote(),
                ));
                Line::from(row)
            }
            None => Line::default(),
        };

        let path = self.current_path.unwrap_or_default();
        let path = Line::from(Span::styled(middle_truncate(path, inner), theme.callout()));

        let mut footer = Vec::new();
        if let Some(counter) = self.counter_info {
            footer.push(Span::styled(counter.to_string(), theme.body()));
        }
        if let Some(seconds) = self.elapsed_seconds {
            if !footer.is_empty() {
                footer.push(Span::styled("  ·  ", theme.footnote()));
            }
            footer.push(Span::styled(
                format!("{}:{:02}", seconds / 60, seconds % 60),
                theme.footnote(),
            ));
        }

        vec![
            step,
            Line::default(),
            progress,
            Line::default(),
            path,
            Line::from(footer),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn test_progress_view_render() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::new();

        terminal
            .draw(|f| {
                ProgressView::new("⠋", "Scanning developer caches")
                    .step_info("Step 2 of 5 · Cargo")
                    .fraction(Some(0.40))
                    .counter_info("128 items found · 4.2 GB")
                    .elapsed_seconds(12)
                    .render(f, f.area(), &theme);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let text = format!("{buffer:?}");
        assert!(text.contains("Scanning developer caches"));
        assert!(text.contains("Cargo"));
        assert!(text.contains("128 items found"));
    }
}

//! Progress card view component.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::gauge::bar;
use crate::core::path_utils::middle_truncate;
use crate::ui::theme::Theme;

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

    pub fn render(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let total_lines = 8;
        if area.height < total_lines as u16 {
            return;
        }

        let top_pad = (area.height as usize).saturating_sub(total_lines) / 2;

        let mut lines = Vec::new();
        for _ in 0..top_pad {
            lines.push(Line::from(""));
        }

        // Row 1: Spinner + headline
        let spin_char = if theme.env.reduce_motion {
            theme.g.bullet
        } else {
            self.spinner
        };
        lines.push(
            Line::from(vec![
                Span::styled(format!("{spin_char}  "), theme.accent_text()),
                Span::styled(self.headline, theme.headline()),
            ])
            .alignment(Alignment::Center),
        );

        // Row 2: Step info
        if let Some(step) = self.step_info {
            lines.push(
                Line::from(vec![Span::styled(step, theme.callout())]).alignment(Alignment::Center),
            );
        } else {
            lines.push(Line::from(""));
        }

        lines.push(Line::from(""));

        // Row 3: Bar (if fraction available)
        if let Some(frac) = self.fraction {
            let bar_w = 46u16.min(area.width.saturating_sub(16));
            let bar_spans = bar(frac, bar_w, theme.p.accent, theme);
            let percent_str = format!("  {:>3.0}%", frac * 100.0);
            let mut row = bar_spans;
            row.push(Span::styled(percent_str, theme.footnote()));
            lines.push(Line::from(row).alignment(Alignment::Center));
        } else {
            lines.push(Line::from(""));
        }

        lines.push(Line::from(""));

        // Row 4: Current path
        if let Some(p) = self.current_path {
            let max_w = (area.width as usize).saturating_sub(12);
            let trunc = middle_truncate(p, max_w);
            lines.push(
                Line::from(vec![Span::styled(trunc, theme.callout())]).alignment(Alignment::Center),
            );
        } else {
            lines.push(Line::from(""));
        }

        // Row 5: Counters & Elapsed time
        let mut foot_spans = Vec::new();
        if let Some(cnt) = self.counter_info {
            foot_spans.push(Span::styled(cnt, theme.body()));
        }
        if let Some(sec) = self.elapsed_seconds {
            if !foot_spans.is_empty() {
                foot_spans.push(Span::styled("  ·  ", theme.footnote()));
            }
            let mins = sec / 60;
            let rem_s = sec % 60;
            foot_spans.push(Span::styled(format!("{mins}:{rem_s:02}"), theme.footnote()));
        }

        if !foot_spans.is_empty() {
            lines.push(Line::from(foot_spans).alignment(Alignment::Center));
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

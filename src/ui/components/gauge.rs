//! Gauges, Progress bars, Segmented bars, and Legends.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::ui::env::ColorDepth;
use crate::ui::theme::Theme;

pub fn bar(fraction: f64, width: u16, fill: Color, theme: &Theme) -> Vec<Span<'static>> {
    let w = width as usize;
    if w == 0 {
        return Vec::new();
    }
    let frac = fraction.clamp(0.0, 1.0);

    if theme.env.depth == ColorDepth::NoColor {
        let filled = (frac * w as f64).round() as usize;
        let empty = w.saturating_sub(filled);
        return vec![Span::raw("=".repeat(filled)), Span::raw("-".repeat(empty))];
    }

    let filled = (frac * w as f64).round() as usize;
    let empty = w.saturating_sub(filled);

    vec![
        Span::styled(theme.g.bar_fill.repeat(filled), Style::default().fg(fill)),
        Span::styled(
            theme.g.bar_track.repeat(empty),
            Style::default().fg(theme.p.quaternary),
        ),
    ]
}

pub fn segmented_bar(segments: &[(f64, Color)], width: u16, theme: &Theme) -> Vec<Span<'static>> {
    let w = width as usize;
    if w == 0 {
        return Vec::new();
    }

    if theme.env.depth == ColorDepth::NoColor {
        let total_frac: f64 = segments.iter().map(|(f, _)| *f).sum();
        return bar(total_frac, width, Color::Reset, theme);
    }

    let mut spans = Vec::new();
    let mut total_filled = 0;

    for (frac, color) in segments {
        let f = frac.clamp(0.0, 1.0);
        let chars = (f * w as f64).round() as usize;
        let count = chars.min(w.saturating_sub(total_filled));
        if count > 0 {
            spans.push(Span::styled("█".repeat(count), Style::default().fg(*color)));
            total_filled += count;
        }
    }

    let empty = w.saturating_sub(total_filled);
    if empty > 0 {
        spans.push(Span::styled(
            "░".repeat(empty),
            Style::default().fg(theme.p.quaternary),
        ));
    }

    spans
}

pub fn legend(items: &[(&str, String, Color)], theme: &Theme) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, (label, val, color)) in items.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("    "));
        }
        spans.push(Span::styled(
            format!("{} ", theme.g.bullet),
            Style::default().fg(*color),
        ));
        spans.push(Span::styled(format!("{label} "), theme.callout()));
        spans.push(Span::styled(val.clone(), theme.body()));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bar_rendering() {
        let theme = Theme::new();
        let spans = bar(0.5, 10, theme.p.accent, &theme);
        assert_eq!(spans.len(), 2);
    }

    #[test]
    fn test_legend_rendering() {
        let theme = Theme::new();
        let leg = legend(
            &[
                ("Used", "370 GB".to_string(), theme.p.accent),
                ("Free", "118 GB".to_string(), theme.p.green),
            ],
            &theme,
        );
        assert!(!leg.spans.is_empty());
    }
}

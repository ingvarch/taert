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
            spans.push(Span::styled(
                theme.g.bar_fill.repeat(count),
                Style::default().fg(*color),
            ));
            total_filled += count;
        }
    }

    let empty = w.saturating_sub(total_filled);
    if empty > 0 {
        spans.push(Span::styled(
            theme.g.bar_track.repeat(empty),
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

/// One glyph per value with the newest on the right. A missing past stays blank.
pub fn sparkline(
    values: &[f64],
    width: usize,
    max: f64,
    color: Color,
    theme: &Theme,
) -> Span<'static> {
    let blocks = theme.g.spark_blocks;
    let newest = &values[values.len().saturating_sub(width)..];
    let mut text = " ".repeat(width - newest.len());

    for value in newest {
        let level = if max > 0.0 {
            (value / max).clamp(0.0, 1.0)
        } else {
            0.0
        };
        text.push_str(blocks[(level * (blocks.len() - 1) as f64).round() as usize]);
    }

    Span::styled(text, Style::default().fg(color))
}

/// Legend rows of two aligned items, or of one when two do not fit in `width`.
pub fn legend_grid(
    items: &[(&str, String, Color)],
    indent: usize,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    const BULLET_W: usize = 2;
    const VALUE_GAP: usize = 2;
    const ITEM_GAP: usize = 3;

    let label_w = items.iter().map(|(l, _, _)| l.len()).max().unwrap_or(0);
    let value_w = items.iter().map(|(_, v, _)| v.len()).max().unwrap_or(0);
    let item_w = BULLET_W + label_w + VALUE_GAP + value_w;
    let per_row = if indent + 2 * item_w + ITEM_GAP <= width {
        2
    } else {
        1
    };

    items
        .chunks(per_row)
        .map(|row| {
            let mut spans = vec![Span::raw(" ".repeat(indent))];
            for (i, (label, value, color)) in row.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::raw(" ".repeat(ITEM_GAP)));
                }
                spans.push(Span::styled(
                    format!("{} ", theme.g.bullet),
                    Style::default().fg(*color),
                ));
                spans.push(Span::styled(
                    format!("{label:<label_w$}{}", " ".repeat(VALUE_GAP)),
                    theme.callout(),
                ));
                spans.push(Span::styled(format!("{value:>value_w$}"), theme.body()));
            }
            Line::from(spans)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::env::UiEnv;

    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn sparkline_maps_values_to_block_heights() {
        let theme = Theme::with_env(UiEnv::default());

        let span = sparkline(&[0.0, 50.0, 100.0], 3, 100.0, Color::Reset, &theme);

        assert_eq!(span.content, "▁▅█");
    }

    #[test]
    fn sparkline_leaves_the_missing_past_blank() {
        let theme = Theme::with_env(UiEnv::default());

        let span = sparkline(&[100.0], 4, 100.0, Color::Reset, &theme);

        assert_eq!(span.content, "   █");
    }

    #[test]
    fn sparkline_keeps_the_newest_values() {
        let theme = Theme::with_env(UiEnv::default());

        let span = sparkline(&[100.0, 0.0, 100.0], 2, 100.0, Color::Reset, &theme);

        assert_eq!(span.content, "▁█");
    }

    #[test]
    fn sparkline_survives_a_zero_scale() {
        let theme = Theme::with_env(UiEnv::default());

        let span = sparkline(&[5.0], 1, 0.0, Color::Reset, &theme);

        assert_eq!(span.content, "▁");
    }

    #[test]
    fn sparkline_uses_ascii_when_asked() {
        let theme = Theme::with_env(UiEnv {
            ascii_glyphs: true,
            ..UiEnv::default()
        });

        let span = sparkline(&[0.0, 100.0], 2, 100.0, Color::Reset, &theme);

        assert_eq!(span.content, "_#");
    }

    #[test]
    fn legend_grid_puts_two_items_on_a_row_and_lines_them_up() {
        let theme = Theme::with_env(UiEnv::default());
        let items = [
            ("App", "18 GB".to_string(), Color::Reset),
            ("Compressed", "1 GB".to_string(), Color::Reset),
            ("Wired", "5 GB".to_string(), Color::Reset),
        ];

        let lines = legend_grid(&items, 2, 60, &theme);

        assert_eq!(lines.len(), 2);
        assert_eq!(
            text(&lines[0]),
            "  ● App         18 GB   ● Compressed   1 GB"
        );
        assert_eq!(text(&lines[1]), "  ● Wired        5 GB");
    }

    #[test]
    fn legend_grid_stacks_items_that_do_not_fit() {
        let theme = Theme::with_env(UiEnv::default());
        let items = [
            ("App", "18 GB".to_string(), Color::Reset),
            ("Compressed", "1 GB".to_string(), Color::Reset),
        ];

        assert_eq!(legend_grid(&items, 2, 30, &theme).len(), 2);
    }

    #[test]
    fn segmented_bar_draws_like_every_other_bar() {
        let theme = Theme::with_env(UiEnv::default());

        let spans = segmented_bar(&[(0.5, Color::Reset)], 4, &theme);

        assert_eq!(text(&Line::from(spans)), "━━──");
    }

    #[test]
    fn segmented_bar_uses_ascii_when_asked() {
        let theme = Theme::with_env(UiEnv {
            ascii_glyphs: true,
            ..UiEnv::default()
        });

        let spans = segmented_bar(&[(0.5, Color::Reset)], 4, &theme);

        assert_eq!(text(&Line::from(spans)), "==--");
    }

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

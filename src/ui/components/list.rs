//! Unified ListRow, SectionHeader, and path formatting components.

use std::path::Path;

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::core::path_utils::{middle_truncate, prettify_path};
use crate::ui::env::ColorDepth;
use crate::ui::theme::Theme;

#[derive(Debug, Clone)]
pub struct ListRow<'a> {
    pub leading: Option<Span<'a>>,
    pub title: Vec<Span<'a>>,
    pub meta: Option<Span<'a>>,
    pub detail: Option<Span<'a>>,
    pub bar: Option<f64>,
    pub chevron: bool,
    pub reserve_chevron: bool,
    pub selected: bool,
}

impl<'a> ListRow<'a> {
    pub fn new(title: Vec<Span<'a>>) -> Self {
        Self {
            leading: None,
            title,
            meta: None,
            detail: None,
            bar: None,
            chevron: false,
            reserve_chevron: false,
            selected: false,
        }
    }

    pub fn leading(mut self, leading: Span<'a>) -> Self {
        self.leading = Some(leading);
        self
    }

    pub fn meta(mut self, meta: Span<'a>) -> Self {
        self.meta = Some(meta);
        self
    }

    pub fn detail(mut self, detail: Span<'a>) -> Self {
        self.detail = Some(detail);
        self
    }

    pub fn bar(mut self, fraction: f64) -> Self {
        self.bar = Some(fraction);
        self
    }

    pub fn chevron(mut self, chevron: bool) -> Self {
        self.chevron = chevron;
        self
    }

    pub fn reserve_chevron(mut self, reserve: bool) -> Self {
        self.reserve_chevron = reserve;
        self
    }

    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    pub fn render_line(&self, width: usize, theme: &Theme) -> Line<'static> {
        let mut spans = Vec::new();

        // 1. Selection indicator
        let bar_style = if self.selected {
            theme.accent_text().add_modifier(Modifier::BOLD)
        } else {
            theme.callout()
        };
        let bar_char = if self.selected {
            theme.g.selection_bar
        } else {
            " "
        };
        spans.push(Span::styled(format!("{bar_char} "), bar_style));

        // 2. Leading (icon or checkbox)
        if let Some(leading) = &self.leading {
            spans.push(Span::styled(leading.content.to_string(), leading.style));
            spans.push(Span::raw(" "));
        } else {
            spans.push(Span::raw("   "));
        }

        // Calculate right side width
        let chevron_width = if self.chevron || self.reserve_chevron {
            2
        } else {
            0
        }; // " ›" or "  "
        let detail_width = self.detail.as_ref().map(|d| d.content.width()).unwrap_or(0);

        let show_bar_graphic = self.bar.is_some() && width >= 115;
        let show_percentage = self.bar.is_some() && width >= 95;
        let bar_total_width = if show_bar_graphic {
            8 + 1 + 6 + 2 // bar(8) + space(1) + pct(6) + pad(2) = 17
        } else if show_percentage {
            6 + 2 // pct(6) + pad(2) = 8
        } else {
            0
        };

        let show_meta = self.meta.is_some() && width >= 85;
        let meta_width = if show_meta {
            self.meta
                .as_ref()
                .map(|m| m.content.width() + 2)
                .unwrap_or(0)
        } else {
            0
        };

        let right_total = chevron_width + detail_width + bar_total_width + meta_width;

        let left_fixed = 2 + 3; // bar (2) + leading (3)
        let avail_for_title = width.saturating_sub(left_fixed + right_total + 1);

        // 3. Title spans with truncation if needed
        let mut current_title_w = 0;
        for s in &self.title {
            let sw = s.content.width();
            if current_title_w + sw <= avail_for_title {
                spans.push(Span::styled(s.content.to_string(), s.style));
                current_title_w += sw;
            } else {
                let remaining = avail_for_title.saturating_sub(current_title_w);
                if remaining > 0 {
                    let truncated = middle_truncate(&s.content, remaining);
                    spans.push(Span::styled(truncated, s.style));
                    current_title_w += remaining;
                }
                break;
            }
        }

        // 4. Middle spacer
        let used_left = left_fixed + current_title_w;
        let pad = width.saturating_sub(used_left + right_total);
        if pad > 0 {
            spans.push(Span::raw(" ".repeat(pad)));
        }

        // 5. Meta (Date)
        if show_meta && let Some(meta) = &self.meta {
            spans.push(Span::styled(meta.content.to_string(), meta.style));
            spans.push(Span::raw("  "));
        }

        // 6. Optional bar / percentage
        if let Some(frac) = self.bar {
            let pct = (frac * 100.0).clamp(0.0, 100.0);
            let pct_str = if pct >= 99.95 && frac >= 0.999 {
                "100.0%".to_string()
            } else if pct >= 0.05 {
                format!("{:>5.1}%", pct)
            } else if frac > 0.0 {
                "<0.1%".to_string()
            } else {
                " 0.0%".to_string()
            };

            if show_bar_graphic {
                let b = crate::ui::components::gauge::bar(frac, 8, theme.p.accent, theme);
                spans.extend(b);
                spans.push(Span::raw(" "));
                spans.push(Span::styled(pct_str, theme.footnote()));
                spans.push(Span::raw("  "));
            } else if show_percentage {
                spans.push(Span::styled(pct_str, theme.footnote()));
                spans.push(Span::raw("  "));
            }
        }

        // 7. Detail
        if let Some(detail) = &self.detail {
            spans.push(Span::styled(detail.content.to_string(), detail.style));
        }

        // 8. Chevron
        if self.chevron {
            spans.push(Span::styled(
                format!(" {}", theme.g.chevron),
                theme.footnote(),
            ));
        } else if self.reserve_chevron {
            spans.push(Span::raw("  "));
        }

        // If selected, apply selection background style to all spans
        if self.selected {
            if theme.env.depth == ColorDepth::NoColor {
                for s in &mut spans {
                    s.style = s.style.add_modifier(Modifier::REVERSED);
                }
            } else {
                for s in &mut spans {
                    s.style = s.style.bg(theme.p.selection);
                }
            }
        }

        Line::from(spans)
    }
}

pub fn path_spans(path: &Path, max_len: usize, theme: &Theme) -> Vec<Span<'static>> {
    let pretty = prettify_path(path);
    let truncated = if pretty.len() > max_len {
        middle_truncate(&pretty, max_len)
    } else {
        pretty
    };

    if let Some((parent, file)) = truncated.rsplit_once('/') {
        vec![
            Span::styled(format!("{parent}/"), theme.callout()),
            Span::styled(file.to_string(), theme.body()),
        ]
    } else {
        vec![Span::styled(truncated, theme.body())]
    }
}

pub fn render_section_header_spans(
    title: &str,
    detail_spans: Vec<Span<'static>>,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let title_upper = title.to_uppercase();
    let title_span = Span::styled(format!("  {title_upper}"), theme.section_header());
    if !detail_spans.is_empty() {
        let left_w = title_upper.width() + 2;
        let right_w: usize = detail_spans.iter().map(|s| s.content.width()).sum();
        let pad = width.saturating_sub(left_w + right_w);
        let mut spans = Vec::with_capacity(detail_spans.len() + 2);
        spans.push(title_span);
        spans.push(Span::raw(" ".repeat(pad)));
        spans.extend(detail_spans);
        Line::from(spans)
    } else {
        Line::from(vec![title_span])
    }
}

pub fn render_section_header(
    title: &str,
    detail: Option<&str>,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    if let Some(d) = detail {
        render_section_header_spans(
            title,
            vec![Span::styled(d.to_string(), theme.footnote())],
            width,
            theme,
        )
    } else {
        render_section_header_spans(title, Vec::new(), width, theme)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_list_row_rendering() {
        let theme = Theme::new();
        let row = ListRow::new(vec![Span::raw("My Item")])
            .leading(Span::raw("●"))
            .detail(Span::raw("15.0 MB"))
            .chevron(true)
            .selected(true);

        let line = row.render_line(80, &theme);
        let rendered = line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(rendered.contains("My Item"));
        assert!(rendered.contains("15.0 MB"));
        assert!(rendered.contains("›"));
    }

    #[test]
    fn test_path_spans() {
        let theme = Theme::new();
        let spans = path_spans(Path::new("/Users/test/.cargo/cache"), 80, &theme);
        assert_eq!(spans.len(), 2);
        assert!(spans[0].content.ends_with('/'));
        assert_eq!(spans[1].content, "cache");
    }

    #[test]
    fn test_section_header_spans() {
        let theme = Theme::new();
        let spans = vec![
            Span::styled("3 items · ", theme.footnote()),
            Span::styled("1.5 GB", theme.size(1_500_000_000)),
        ];
        let line = render_section_header_spans("Cargo", spans, 80, &theme);
        let rendered = line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(rendered.contains("CARGO"));
        assert!(rendered.contains("3 items · 1.5 GB"));
    }
}

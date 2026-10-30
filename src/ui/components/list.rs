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
    pub detail: Option<Span<'a>>,
    pub bar: Option<f64>,
    pub chevron: bool,
    pub selected: bool,
}

impl<'a> ListRow<'a> {
    pub fn new(title: Vec<Span<'a>>) -> Self {
        Self {
            leading: None,
            title,
            detail: None,
            bar: None,
            chevron: false,
            selected: false,
        }
    }

    pub fn leading(mut self, leading: Span<'a>) -> Self {
        self.leading = Some(leading);
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
        let chevron_width = if self.chevron { 2 } else { 0 }; // " ›"
        let detail_width = self.detail.as_ref().map(|d| d.content.width()).unwrap_or(0);
        let bar_width = if self.bar.is_some() && width >= 100 {
            14
        } else {
            0
        };
        let right_total = chevron_width + detail_width + bar_width;

        let left_fixed = 2 + 3; // bar (2) + leading (3)
        let avail_for_title = width.saturating_sub(left_fixed + right_total + 2);

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

        // 5. Optional bar (Wide)
        if let Some(frac) = self.bar
            && bar_width > 0
        {
            let b = crate::ui::components::gauge::bar(frac, 12, theme.p.accent, theme);
            spans.extend(b);
            spans.push(Span::raw("  "));
        }

        // 6. Detail
        if let Some(detail) = &self.detail {
            spans.push(Span::styled(detail.content.to_string(), detail.style));
        }

        // 7. Chevron
        if self.chevron {
            spans.push(Span::styled(
                format!(" {}", theme.g.chevron),
                theme.footnote(),
            ));
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

pub fn render_section_header(
    title: &str,
    detail: Option<&str>,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let title_upper = title.to_uppercase();
    let title_span = Span::styled(format!("  {title_upper}"), theme.section_header());
    if let Some(d) = detail {
        let left_w = title_upper.width() + 2;
        let right_w = d.width();
        let pad = width.saturating_sub(left_w + right_w);
        Line::from(vec![
            title_span,
            Span::raw(" ".repeat(pad)),
            Span::styled(d.to_string(), theme.footnote()),
        ])
    } else {
        Line::from(vec![title_span])
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
}

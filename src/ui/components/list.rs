//! Unified ListRow, SectionHeader, and path formatting components.

use std::path::Path;

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::core::path_utils::{middle_truncate, prettify_path};
use crate::ui::analyze::SortOrder;
use crate::ui::components::text::plural;
use crate::ui::env::ColorDepth;
use crate::ui::theme::Theme;

pub const SIZE_W: usize = 9;
const BAR_MIN_ROW_WIDTH: usize = 100;
/// The selection bar and its space.
const BAR_WIDTH: usize = 2;
/// The blank cell of a row with no leading cell.
const NO_LEADING_WIDTH: usize = 3;
const BAR_MIN_WIDTH: usize = 8;

// The bar takes most of the extra width and so leads the eye from the name to the size.
fn bar_width(row_width: usize) -> usize {
    let extra = row_width.saturating_sub(BAR_MIN_ROW_WIDTH);
    BAR_MIN_WIDTH + extra - extra / 4
}

/// A column between the title and the date. It hides in a row narrower than `min_row_width`.
#[derive(Debug, Clone)]
pub struct Column<'a> {
    pub text: Span<'a>,
    pub width: usize,
    pub min_row_width: usize,
}

impl<'a> Column<'a> {
    pub fn new(text: Span<'a>, width: usize, min_row_width: usize) -> Self {
        Self {
            text,
            width,
            min_row_width,
        }
    }

    fn shown_in(&self, row_width: usize) -> bool {
        row_width >= self.min_row_width
    }
}

#[derive(Debug, Clone)]
pub struct ListRow<'a> {
    pub leading: Option<Span<'a>>,
    pub title: Vec<Span<'a>>,
    pub meta: Option<Span<'a>>,
    pub columns: Vec<Column<'a>>,
    pub detail: Option<Span<'a>>,
    pub bar: Option<f64>,
    pub bar_heading: Option<&'a str>,
    pub bar_cap: Option<usize>,
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
            columns: Vec::new(),
            detail: None,
            bar: None,
            bar_heading: None,
            bar_cap: None,
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

    pub fn columns(mut self, columns: Vec<Column<'a>>) -> Self {
        self.columns = columns;
        self
    }

    /// Keeps the bar at `cap` cells or fewer, however wide the row is.
    pub fn bar_cap(mut self, cap: usize) -> Self {
        self.bar_cap = Some(cap);
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

    /// Prints `heading` in place of the bar, to title its column.
    pub fn bar_heading(mut self, heading: &'a str) -> Self {
        self.bar_heading = Some(heading);
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

    /// Columns left for the title of a row with no detail, meta, bar or columns.
    pub fn plain_title_room(width: usize) -> usize {
        width.saturating_sub(BAR_WIDTH + NO_LEADING_WIDTH + 1)
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

        let show_bar_graphic = self.bar.is_some() && width >= BAR_MIN_ROW_WIDTH;
        let show_percentage = self.bar.is_some() && width >= 95;
        let bar_w = self
            .bar_cap
            .map_or(bar_width(width), |cap| bar_width(width).min(cap));
        let bar_total_width = if show_bar_graphic {
            bar_w + 1 + 6 + 2 // bar + space(1) + pct(6) + pad(2)
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

        let columns: Vec<&Column> = self
            .columns
            .iter()
            .filter(|column| column.shown_in(width))
            .collect();
        let columns_width: usize = columns.iter().map(|column| column.width + 2).sum();

        let right_total =
            chevron_width + detail_width + bar_total_width + meta_width + columns_width;

        let leading_width = self
            .leading
            .as_ref()
            .map_or(NO_LEADING_WIDTH, |l| l.content.width() + 1);
        let left_fixed = BAR_WIDTH + leading_width;
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

        // 5. Columns, then meta (Date)
        for column in columns {
            let text = middle_truncate(&column.text.content, column.width);
            let text = format!("{text:<w$}", w = column.width);
            spans.push(Span::styled(text, column.text.style));
            spans.push(Span::raw("  "));
        }
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
                " <0.1%".to_string()
            } else {
                "  0.0%".to_string()
            };

            if show_bar_graphic {
                if let Some(heading) = self.bar_heading {
                    let column = bar_w + 1 + 6;
                    spans.push(Span::styled(
                        format!("{heading:<column$}"),
                        theme.footnote(),
                    ));
                } else {
                    let b = crate::ui::components::gauge::bar(
                        frac,
                        bar_w as u16,
                        theme.p.accent,
                        theme,
                    );
                    spans.extend(b);
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled(pct_str, theme.footnote()));
                }
                spans.push(Span::raw("  "));
            } else if show_percentage {
                let pct = self
                    .bar_heading
                    .map_or(pct_str, |heading| format!("{heading:>6}"));
                spans.push(Span::styled(pct, theme.footnote()));
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

/// The heading row of a table of list rows.
pub struct ColumnTitles<'a> {
    pub date: &'a str,
    pub date_width: usize,
    pub order: SortOrder,
    pub share: bool,
    pub chevron: bool,
    pub columns: Vec<Column<'a>>,
    pub bar_cap: Option<usize>,
}

impl ColumnTitles<'_> {
    pub fn render(&self, width: usize, theme: &Theme) -> Line<'static> {
        let style = theme.footnote();
        let title = |column: SortOrder, label: &str| {
            let arrow = match column {
                SortOrder::Name => theme.g.sort_ascending,
                SortOrder::Size | SortOrder::Date => theme.g.sort_descending,
            };
            if column == self.order {
                format!("{label} {arrow}")
            } else {
                label.to_string()
            }
        };

        let date_w = self.date_width;
        let mut row = ListRow::new(vec![Span::styled(title(SortOrder::Name, "NAME"), style)])
            .leading(Span::raw(" "))
            .meta(Span::styled(
                format!("{:<date_w$}", title(SortOrder::Date, self.date)),
                style,
            ))
            .detail(Span::styled(
                format!("{:>SIZE_W$}", title(SortOrder::Size, "SIZE")),
                style,
            ))
            .reserve_chevron(self.chevron);
        let columns = self.columns.iter().map(|column| {
            let text = Span::styled(column.text.content.clone(), style);
            Column::new(text, column.width, column.min_row_width)
        });
        row = row.columns(columns.collect());
        if self.share {
            row = row.bar(0.0).bar_heading("SHARE");
        }
        if let Some(cap) = self.bar_cap {
            row = row.bar_cap(cap);
        }
        row.render_line(width, theme)
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

/// A row's path and how many more folders it holds. A long path gives way to the count.
pub fn row_path_spans(
    path: &Path,
    more_folders: usize,
    max_len: usize,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let count = (more_folders > 0).then(|| {
        let count = plural(more_folders, "folder", "folders");
        Span::styled(format!(" +{count}"), theme.footnote())
    });
    let count_width = count.as_ref().map_or(0, |span| span.content.width());
    let mut spans = path_spans(path, max_len.saturating_sub(count_width), theme);
    spans.extend(count);
    spans
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
    use crate::ui::env::UiEnv;

    fn bar_row(title: &str, fraction: f64) -> ListRow<'_> {
        ListRow::new(vec![Span::raw(title)])
            .detail(Span::raw("  1.0 GB"))
            .chevron(true)
            .bar(fraction)
    }

    fn rendered(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn bar_cells(line: &Line) -> usize {
        rendered(line).matches(['━', '─']).count()
    }

    #[test]
    fn a_row_with_a_leading_glyph_fills_its_width_too() {
        let theme = Theme::with_env(UiEnv::default());

        for width in [60, 95, 120, 200] {
            let line = bar_row("a", 0.5)
                .leading(Span::raw("●"))
                .render_line(width, &theme);
            assert_eq!(line.width(), width);
        }
    }

    #[test]
    fn a_heading_row_lines_up_with_the_rows_below_it() {
        let theme = Theme::with_env(UiEnv::default());

        for width in [90, 97, 100, 150, 203] {
            let row = ListRow::new(vec![Span::raw("node:22")])
                .leading(Span::raw("●"))
                .meta(Span::raw("5 months ago "))
                .detail(Span::raw("  1.0 GB"))
                .bar(0.5)
                .render_line(width, &theme);
            let heading = ListRow::new(vec![Span::raw("NAME")])
                .leading(Span::raw(" "))
                .meta(Span::raw("CREATED      "))
                .detail(Span::raw("    SIZE"))
                .bar(0.0)
                .bar_heading("SHARE")
                .render_line(width, &theme);

            let (row, heading) = (rendered(&row), rendered(&heading));
            let at = |line: &str, text: &str| line[..line.find(text).unwrap()].chars().count();
            assert_eq!(at(&heading, "NAME"), at(&row, "node:22"), "{width}");
            assert_eq!(at(&heading, "CREATED"), at(&row, "5 months"), "{width}");
            assert_eq!(heading.chars().count(), width);
            assert_eq!(row.chars().count(), width);
            assert!(heading.ends_with("SIZE"), "{heading}");
            assert!(row.ends_with("1.0 GB"), "{row}");
        }
    }

    #[test]
    fn a_heading_names_the_bar_column_where_the_bar_starts() {
        let theme = Theme::with_env(UiEnv::default());

        let row = bar_row("a", 1.0).render_line(120, &theme);
        let heading = bar_row("a", 0.0)
            .bar_heading("SHARE")
            .render_line(120, &theme);

        let bar_at = rendered(&row).chars().position(|c| c == '━').unwrap();
        let (byte_at, _) = rendered(&heading).match_indices("SHARE").next().unwrap();
        assert_eq!(rendered(&heading)[..byte_at].chars().count(), bar_at);
    }

    #[test]
    fn a_row_of_100_columns_shows_the_bar() {
        let theme = Theme::with_env(UiEnv::default());

        assert_eq!(bar_cells(&bar_row("a", 0.5).render_line(100, &theme)), 8);
    }

    #[test]
    fn a_narrower_row_shows_only_the_percentage() {
        let theme = Theme::with_env(UiEnv::default());

        let line = bar_row("a", 0.5).render_line(99, &theme);

        assert_eq!(bar_cells(&line), 0);
        assert!(rendered(&line).contains("50.0%"));
    }

    #[test]
    fn a_wider_row_gives_most_of_the_extra_space_to_the_bar() {
        let theme = Theme::with_env(UiEnv::default());

        assert_eq!(bar_cells(&bar_row("a", 0.5).render_line(200, &theme)), 83);
    }

    #[test]
    fn bars_line_up_across_titles() {
        let theme = Theme::with_env(UiEnv::default());
        let start = |title| {
            let text = rendered(&bar_row(title, 0.5).render_line(160, &theme));
            text.chars().position(|c| c == '━')
        };

        assert_eq!(start("a"), start("a-much-longer-name.bin"));
    }

    #[test]
    fn a_row_fills_its_width_at_any_percentage() {
        let theme = Theme::with_env(UiEnv::default());

        for fraction in [0.0, 0.000_001, 0.5, 1.0] {
            for width in [95, 100, 150, 200] {
                let line = bar_row("a", fraction).render_line(width, &theme);
                assert_eq!(line.width(), width, "{fraction} at {width}");
            }
        }
    }

    fn columns() -> Vec<Column<'static>> {
        vec![
            Column::new(Span::raw("ORIGIN"), 10, 100),
            Column::new(Span::raw("UPDATE"), 12, 0),
        ]
    }

    fn titles(columns: Vec<Column<'static>>) -> ColumnTitles<'static> {
        ColumnTitles {
            date: "INSTALLED",
            date_width: 17,
            order: SortOrder::Size,
            share: true,
            chevron: false,
            columns,
            bar_cap: Some(12),
        }
    }

    #[test]
    fn the_titles_show_a_column_from_its_width_and_hide_it_below() {
        let theme = Theme::with_env(UiEnv::default());

        for (width, origin) in [(100, true), (140, true), (99, false), (80, false)] {
            let text = rendered(&titles(columns()).render(width, &theme));
            assert_eq!(text.contains("ORIGIN"), origin, "{width}");
            assert!(text.contains("UPDATE"), "{width}");
            assert_eq!(text.chars().count(), width, "{width}");
        }
    }

    #[test]
    fn a_column_lines_up_with_its_title() {
        let theme = Theme::with_env(UiEnv::default());
        for width in [90, 100, 120, 200] {
            let row = ListRow::new(vec![Span::raw("bun 1.4.2")])
                .leading(Span::raw("●"))
                .meta(Span::raw(format!("{:<17}", "5 months ago")))
                .detail(Span::raw("   1.0 GB"))
                .columns(vec![
                    Column::new(Span::raw("on request"), 10, 100),
                    Column::new(Span::raw("↑ 1.4.4_1"), 12, 0),
                ])
                .bar(0.5)
                .bar_cap(12)
                .render_line(width, &theme);
            let heading = titles(columns()).render(width, &theme);

            let (row, heading) = (rendered(&row), rendered(&heading));
            let at = |line: &str, text: &str| line[..line.find(text).unwrap()].chars().count();
            assert_eq!(at(&heading, "UPDATE"), at(&row, "↑ 1.4.4_1"), "{width}");
            if width >= 100 {
                assert_eq!(at(&heading, "ORIGIN"), at(&row, "on request"), "{width}");
            } else {
                assert!(!row.contains("on request"), "{width}");
            }
            assert_eq!(at(&heading, "INSTALLED"), at(&row, "5 months"), "{width}");
            assert_eq!(row.chars().count(), width);
        }
    }

    #[test]
    fn a_capped_bar_stays_narrow_in_a_wide_row() {
        let theme = Theme::with_env(UiEnv::default());

        for width in [100, 120, 200] {
            let line = bar_row("a", 0.5).bar_cap(12).render_line(width, &theme);
            assert!(bar_cells(&line) <= 12, "{width}");
            assert_eq!(line.width(), width);
        }
        assert_eq!(
            bar_cells(&bar_row("a", 0.5).bar_cap(12).render_line(200, &theme)),
            12
        );
        assert_eq!(bar_cells(&bar_row("a", 0.5).render_line(200, &theme)), 83);
    }

    #[test]
    fn the_title_of_a_capped_bar_is_as_wide_as_the_bar() {
        let theme = Theme::with_env(UiEnv::default());

        let row = bar_row("a", 1.0).bar_cap(12).render_line(200, &theme);
        let heading = bar_row("a", 0.0)
            .bar_heading("SHARE")
            .bar_cap(12)
            .render_line(200, &theme);

        let bar_at = rendered(&row).chars().position(|c| c == '━').unwrap();
        let (byte_at, _) = rendered(&heading).match_indices("SHARE").next().unwrap();
        assert_eq!(rendered(&heading)[..byte_at].chars().count(), bar_at);
    }

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

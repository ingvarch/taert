//! Help Sheet modal displaying full keyboard shortcuts for the current screen.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::button::Button;
use super::columns::side_by_side;
use super::hints::Hint;
use super::modal::Modal;
use crate::ui::theme::Theme;

const COLUMN_GAP: usize = 4;
const KEY_GAP: usize = 2;
const KEYCAP_PADDING: usize = 2;

pub struct HelpSection<'a> {
    pub title: &'a str,
    pub shortcuts: Vec<Hint<'a>>,
}

impl<'a> HelpSection<'a> {
    pub fn new(title: &'a str, shortcuts: Vec<Hint<'a>>) -> Self {
        Self { title, shortcuts }
    }
}

pub struct HelpSheet<'a> {
    pub sections: Vec<HelpSection<'a>>,
}

impl<'a> HelpSheet<'a> {
    pub fn new(sections: Vec<HelpSection<'a>>) -> Self {
        Self { sections }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let (max_w, max_h) = Modal::message_size(area);

        Modal::new("Keyboard Shortcuts")
            .message(fit_height(self.lines(max_w, theme), max_h, theme))
            .buttons(vec![Button::primary("Done").focused(true)])
            .hints(Line::from(vec![
                Span::styled("↵/⎋/?", theme.callout()),
                Span::styled(" Close", theme.footnote()),
            ]))
            .render(frame, area, theme);
    }

    /// The first section sits beside the rest when both fit, above them otherwise.
    fn lines(&self, max_w: usize, theme: &Theme) -> Vec<Line<'a>> {
        let (first, rest) = self.sections.split_at(self.sections.len().min(1));
        let first_w = column_width(first);

        if !rest.is_empty() && first_w + COLUMN_GAP + column_width(rest) <= max_w {
            side_by_side(
                column_lines(first, theme),
                first_w,
                COLUMN_GAP,
                column_lines(rest, theme),
            )
        } else {
            column_lines(&self.sections, theme)
        }
    }
}

fn key_width(sections: &[HelpSection]) -> usize {
    let shortcuts = sections.iter().flat_map(|s| &s.shortcuts);
    shortcuts.map(|h| h.key.width()).max().unwrap_or(0)
}

fn column_width(sections: &[HelpSection]) -> usize {
    let shortcuts = sections.iter().flat_map(|s| &s.shortcuts);
    let desc_w = shortcuts.map(|h| h.desc.width()).max().unwrap_or(0);
    let row_w = key_width(sections) + KEYCAP_PADDING + KEY_GAP + desc_w;
    let title_w = sections.iter().map(|s| s.title.width()).max().unwrap_or(0);

    row_w.max(title_w)
}

fn column_lines<'a>(sections: &[HelpSection<'a>], theme: &Theme) -> Vec<Line<'a>> {
    let key_w = key_width(sections);
    let mut lines = Vec::new();

    for (i, section) in sections.iter().enumerate() {
        if i > 0 {
            lines.push(Line::default());
        }
        lines.push(Line::from(Span::styled(
            section.title.to_uppercase(),
            theme.section_header(),
        )));
        for hint in &section.shortcuts {
            lines.push(Line::from(vec![
                Span::styled(format!(" {} ", hint.key), theme.keycap()),
                Span::raw(" ".repeat(key_w - hint.key.width() + KEY_GAP)),
                Span::styled(hint.desc, theme.body()),
            ]));
        }
    }

    lines
}

fn fit_height<'a>(mut lines: Vec<Line<'a>>, max_h: usize, theme: &Theme) -> Vec<Line<'a>> {
    if lines.len() > max_h {
        lines.truncate(max_h.saturating_sub(1));
        while lines.last().is_some_and(|line| line.spans.is_empty()) {
            lines.pop();
        }
        lines.push(Line::from(Span::styled(
            "Enlarge the window to see more",
            theme.callout(),
        )));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::ui::env::UiEnv;

    fn render(sheet: &HelpSheet, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let theme = Theme::with_env(UiEnv::default());
        terminal
            .draw(|f| sheet.render(f, f.area(), &theme))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect()
    }

    fn find(rows: &[String], text: &str) -> (usize, usize) {
        rows.iter()
            .enumerate()
            .find_map(|(y, row)| row.find(text).map(|i| (row[..i].chars().count(), y)))
            .unwrap_or_else(|| panic!("{text} is not on the screen:\n{}", rows.join("\n")))
    }

    fn explore_sheet() -> HelpSheet<'static> {
        HelpSheet::new(vec![
            HelpSection::new(
                "Explore",
                vec![
                    Hint::new("␣", "Toggle select"),
                    Hint::new("A / N", "Select all / none"),
                    Hint::new("↵ / →", "Enter folder"),
                    Hint::new("←", "Up one level"),
                    Hint::new("R", "Refresh folder"),
                    Hint::new("O", "Reveal in Finder"),
                    Hint::new("D / ⌫", "Delete selected"),
                ],
            ),
            HelpSection::new(
                "Sort by",
                vec![
                    Hint::new("⇧S", "Size"),
                    Hint::new("⇧N", "Name"),
                    Hint::new("⇧D", "Date created"),
                ],
            ),
            HelpSection::new(
                "General",
                vec![
                    Hint::new("↑↓ / jk", "Navigate"),
                    Hint::new("⎋ / q", "Quit explore"),
                    Hint::new("?", "Toggle help"),
                ],
            ),
        ])
    }

    #[test]
    fn descriptions_in_a_column_line_up() {
        let rows = render(&explore_sheet(), 80, 24);

        let column = find(&rows, "Toggle select").0;
        for desc in ["Select all / none", "Enter folder", "Reveal in Finder"] {
            assert_eq!(find(&rows, desc).0, column, "{desc}");
        }
        let column = find(&rows, "Navigate").0;
        for desc in ["Size", "Date created", "Toggle help"] {
            assert_eq!(find(&rows, desc).0, column, "{desc}");
        }
    }

    #[test]
    fn a_long_description_keeps_its_row_intact() {
        let sheet = HelpSheet::new(vec![
            HelpSection::new(
                "Explore",
                vec![
                    Hint::new("A / N", "Select all / none"),
                    Hint::new("S", "Cycle sort (Size/Date/Name)"),
                ],
            ),
            HelpSection::new("General", vec![Hint::new("↑↓ / jk", "Navigate")]),
        ]);
        let rows = render(&sheet, 80, 24);

        assert_eq!(find(&rows, "Select all").1 + 1, find(&rows, "Cycle sort").1);
        assert_eq!(find(&rows, "Select all").0, find(&rows, "Cycle sort").0);
    }

    #[test]
    fn the_first_section_sits_beside_the_others_when_they_fit() {
        let rows = render(&explore_sheet(), 80, 24);

        let (explore_x, explore_y) = find(&rows, "EXPLORE");
        let (sort_x, sort_y) = find(&rows, "SORT BY");
        assert_eq!(explore_y, sort_y);
        assert!(explore_x < sort_x);
        assert_eq!(find(&rows, "GENERAL").0, sort_x);
    }

    #[test]
    fn a_narrow_window_stacks_the_sections() {
        let rows = render(&explore_sheet(), 50, 32);

        let (explore_x, explore_y) = find(&rows, "EXPLORE");
        let (general_x, general_y) = find(&rows, "GENERAL");
        assert_eq!(explore_x, general_x);
        assert!(explore_y < general_y);
    }

    #[test]
    fn a_short_window_says_rows_are_hidden_and_keeps_the_button() {
        let rows = render(&explore_sheet(), 50, 16);

        find(&rows, "Enlarge the window");
        find(&rows, "Done");
    }

    #[test]
    fn a_window_that_fits_shows_no_notice() {
        let rows = render(&explore_sheet(), 80, 24);

        assert!(!rows.join("\n").contains("Enlarge the window"));
    }
}

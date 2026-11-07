//! History screen: what taert removed and changed, run by run.

use std::path::Path;

use ratatui::Frame;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use super::components::chrome::AppChrome;
use super::components::empty::EmptyState;
use super::components::frame::{FRAME_ROWS, FRAME_SIDES, card};
use super::components::hints::{Hint, PrimaryAction};
use super::components::list::{Column, ListRow, SIZE_W};
use super::components::text::{plural, wrap_line};
use super::select::render_notes;
use super::theme::Theme;
use crate::core::bytes::format_bytes;
use crate::core::history::{HistoryLog, OutcomeKind, RunItem, RunRecord};
use crate::core::path_utils::prettify_path;

const DATE_W: usize = 16;
const MODE_W: usize = 11;
const MODE_MIN_ROW_WIDTH: usize = 90;
const RESULT_MAX_W: usize = 38;
/// The least width of the command, in cells, before RESULT hides.
const COMMAND_MIN_W: usize = 14;
/// Selection bar, symbol, chevron, and the gap before the columns.
const ROW_MARGIN_W: usize = 7;
const COLUMN_GAP_W: usize = 2;
const OUTCOME_W: usize = 9;
const PAGE: usize = 10;
const PATH_INDENT: usize = 4;
const NO_HISTORY_NOTE: &str = "TAERT_NO_HISTORY is set, so new runs are not recorded.";
const NO_HISTORY: &str = "No history yet";
const READ_FAILED: &str = "Could not read the history";

pub struct HistoryState {
    /// Newest run first.
    runs: Vec<RunRecord>,
    error: Option<String>,
    disabled: bool,
    selected: usize,
    /// The highlighted item, while a run is open.
    item: Option<usize>,
}

impl HistoryState {
    pub fn load(log: &HistoryLog, disabled: bool) -> Self {
        let (mut runs, error) = match log.load_runs() {
            Ok(runs) => (runs, None),
            Err(error) => (Vec::new(), Some(error.to_string())),
        };
        runs.reverse();
        Self {
            runs,
            error,
            disabled,
            selected: 0,
            item: None,
        }
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn is_open(&self) -> bool {
        self.item.is_some()
    }

    pub fn open_run(&mut self) {
        if self.current_run().is_some() {
            self.item = Some(0);
        }
    }

    /// Returns to the list. False when the list was already showing.
    pub fn close_run(&mut self) -> bool {
        self.item.take().is_some()
    }

    fn current_run(&self) -> Option<&RunRecord> {
        self.runs.get(self.selected)
    }

    fn cursor_and_len(&self) -> (usize, usize) {
        match (self.item, self.current_run()) {
            (Some(item), Some(run)) => (item, run.items.len()),
            _ => (self.selected, self.runs.len()),
        }
    }

    fn set_cursor(&mut self, value: usize) {
        let (_, len) = self.cursor_and_len();
        let value = value.min(len.saturating_sub(1));
        if self.item.is_some() {
            self.item = Some(value);
        } else {
            self.selected = value;
        }
    }

    pub fn next(&mut self) {
        let (cursor, _) = self.cursor_and_len();
        self.set_cursor(cursor + 1);
    }

    pub fn previous(&mut self) {
        let (cursor, _) = self.cursor_and_len();
        self.set_cursor(cursor.saturating_sub(1));
    }

    pub fn page_down(&mut self) {
        let (cursor, _) = self.cursor_and_len();
        self.set_cursor(cursor + PAGE);
    }

    pub fn page_up(&mut self) {
        let (cursor, _) = self.cursor_and_len();
        self.set_cursor(cursor.saturating_sub(PAGE));
    }

    pub fn move_to_start(&mut self) {
        self.set_cursor(0);
    }

    pub fn move_to_end(&mut self) {
        let (_, len) = self.cursor_and_len();
        self.set_cursor(len.saturating_sub(1));
    }
}

fn run_label(run: &RunRecord) -> String {
    format!("{} {}", run.date_label(), run.command_label())
}

fn date_and_command(run: &RunRecord, theme: &Theme) -> Vec<Span<'static>> {
    vec![
        Span::styled(format!("{:<DATE_W$}  ", run.date_label()), theme.body()),
        Span::styled(run.command_label(), theme.body()),
    ]
}

fn result_width(runs: &[RunRecord]) -> usize {
    runs.iter()
        .map(|run| run.summary().width())
        .max()
        .unwrap_or(0)
        .clamp("RESULT".len(), RESULT_MAX_W)
}

/// RESULT hides before it would cut the date or the start of the command.
fn result_min_row_width(result_w: usize) -> usize {
    ROW_MARGIN_W + DATE_W + COLUMN_GAP_W + COMMAND_MIN_W + result_w + COLUMN_GAP_W
}

fn list_columns(
    mode: Span<'static>,
    result: Span<'static>,
    result_w: usize,
) -> Vec<Column<'static>> {
    vec![
        Column::new(mode, MODE_W, MODE_MIN_ROW_WIDTH),
        Column::new(result, result_w, result_min_row_width(result_w)),
    ]
}

/// The window start that keeps `focus` in view among `total` lines of `height`.
fn scroll_offset(focus: usize, total: usize, height: usize) -> usize {
    if height == 0 || total <= height {
        return 0;
    }
    focus.saturating_sub(height / 2).min(total - height)
}

fn list_lines(
    state: &HistoryState,
    row_width: usize,
    height: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let style = theme.footnote();
    let result_w = result_width(&state.runs);
    let titles = ListRow::new(vec![Span::styled(
        format!("{:<DATE_W$}  COMMAND", "DATE"),
        style,
    )])
    .leading(Span::raw(" "))
    .columns(list_columns(
        Span::styled("MODE", style),
        Span::styled("RESULT", style),
        result_w,
    ))
    .reserve_chevron(true);

    let mut lines = vec![titles.render_line(row_width, theme)];
    let offset = scroll_offset(state.selected, state.runs.len(), height);
    for (index, run) in state.runs.iter().enumerate().skip(offset).take(height) {
        let row = ListRow::new(date_and_command(run, theme))
            .cut_title_at_end()
            .leading(Span::styled(
                theme.g.icon_history.to_string(),
                theme.callout(),
            ))
            .columns(list_columns(
                Span::styled(run.mode.clone(), theme.callout()),
                Span::styled(run.summary(), theme.body()),
                result_w,
            ))
            .chevron(true)
            .selected(index == state.selected);
        lines.push(row.render_line(row_width, theme));
    }
    lines
}

fn item_lines(
    run: &RunRecord,
    cursor: usize,
    row_width: usize,
    height: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let style = theme.footnote();
    let titles = ListRow::new(vec![Span::styled("PATH", style)])
        .leading(Span::raw(" "))
        .columns(vec![Column::new(
            Span::styled("OUTCOME", style),
            OUTCOME_W,
            0,
        )])
        .detail(Span::styled(format!("{:>SIZE_W$}", "SIZE"), style));
    let mut lines = vec![titles.render_line(row_width, theme)];

    if run.items.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "No items recorded.",
            theme.callout(),
        )));
        return lines;
    }

    let mut body: Vec<Line<'static>> = Vec::new();
    let mut cursor_line = 0;
    for (index, item) in run.items.iter().enumerate() {
        if index == cursor {
            cursor_line = body.len();
        }
        body.extend(item_rows(item, index == cursor, row_width, theme));
    }
    let offset = scroll_offset(cursor_line, body.len(), height);
    lines.extend(body.into_iter().skip(offset).take(height));
    lines
}

fn item_rows(
    item: &RunItem,
    selected: bool,
    row_width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let failed = item.outcome == OutcomeKind::Failed;
    let (mark, outcome_style) = if failed {
        (theme.g.error, theme.danger())
    } else {
        (theme.g.success, theme.success())
    };
    let row = ListRow::new(vec![Span::styled(
        prettify_path(Path::new(&item.path)),
        theme.body(),
    )])
    .leading(Span::styled(mark.to_string(), outcome_style))
    .columns(vec![Column::new(
        Span::styled(item.outcome.to_string(), outcome_style),
        OUTCOME_W,
        0,
    )])
    .detail(Span::styled(
        format!("{:>SIZE_W$}", format_bytes(item.bytes)),
        theme.size(item.bytes),
    ))
    .selected(selected);
    let mut rows = vec![row.render_line(row_width, theme)];
    if let Some(error) = &item.error {
        let text = Line::from(Span::styled(error.clone(), theme.danger()));
        for mut line in wrap_line(text, row_width.saturating_sub(PATH_INDENT), 0) {
            line.spans.insert(0, Span::raw(" ".repeat(PATH_INDENT)));
            rows.push(line);
        }
    }
    rows
}

pub fn render_history(frame: &mut Frame, state: &HistoryState, theme: &Theme) {
    let open_run = state.item.and_then(|_| state.current_run());
    let label = open_run.map(run_label);
    let mut crumbs = vec!["taert", "History"];
    crumbs.extend(label.as_deref());

    let status = match open_run {
        Some(run) => plural(run.items.len(), "item", "items"),
        None => plural(state.runs.len(), "run", "runs"),
    };
    let mut hints = vec![Hint::new("↑↓", "Move")];
    let mut chrome = AppChrome::new(crumbs).full_width();
    if open_run.is_none() && !state.runs.is_empty() {
        chrome = chrome.primary(PrimaryAction::new("Open", theme.g.key_enter));
    }
    hints.push(Hint::new("⎋", "Back"));
    hints.push(Hint::new("?", "More"));
    let chrome = chrome
        .status(Line::from(Span::styled(status, theme.callout())))
        .hints(hints);

    let mut area = chrome.render(frame, theme);
    if state.disabled {
        area = render_notes(frame, area, &[NO_HISTORY_NOTE.to_string()], theme);
    }

    if let Some(error) = &state.error {
        EmptyState::new(theme.g.warning, READ_FAILED, error)
            .glyph_color(theme.p.yellow)
            .render(frame, area, theme);
        return;
    }
    if state.runs.is_empty() {
        EmptyState::new(
            theme.g.icon_history,
            NO_HISTORY,
            "taert records each removal and change here.",
        )
        .render(frame, area, theme);
        return;
    }

    let width = area.width as usize;
    let row_width = width.saturating_sub(FRAME_SIDES);
    let frame_rows = (area.height as usize).saturating_sub(FRAME_ROWS);
    // The column titles take the first row.
    let height = frame_rows.saturating_sub(1);
    let (title, lines) = match (state.item, open_run) {
        (Some(cursor), Some(run)) => ("Items", item_lines(run, cursor, row_width, height, theme)),
        _ => ("History", list_lines(state, row_width, height, theme)),
    };
    let lines = card(
        title,
        Vec::new(),
        lines,
        width,
        frame_rows,
        theme.p.purple,
        theme,
    );
    frame.render_widget(Paragraph::new(lines), area);
}

/// A frame with the newest runs, for the pane beside the main menu.
pub fn recent_runs_card(
    runs: &Result<Vec<RunRecord>, String>,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(FRAME_SIDES);
    let rows = match runs {
        Err(error) => vec![
            Line::from(Span::styled(READ_FAILED, theme.callout())),
            Line::from(Span::styled(error.clone(), theme.footnote())),
        ],
        Ok(runs) if runs.is_empty() => {
            vec![Line::from(Span::styled(NO_HISTORY, theme.callout()))]
        }
        Ok(runs) => {
            let result_w = result_width(runs);
            runs.iter()
                .map(|run| {
                    ListRow::new(date_and_command(run, theme))
                        .cut_title_at_end()
                        .columns(vec![Column::new(
                            Span::styled(run.summary(), theme.body()),
                            result_w,
                            result_min_row_width(result_w),
                        )])
                        .render_line(inner, theme)
                })
                .collect()
        }
    };
    card(
        "Recent runs",
        Vec::new(),
        rows,
        width,
        0,
        theme.p.purple,
        theme,
    )
}

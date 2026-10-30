//! Interactive Select Items Screen.
//! Includes modal confirmation dialog, live removal progress dialog with spinner,
//! and completion outcome dialog.

use std::path::PathBuf;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use super::dialog::{Dialog, DialogButton, DialogHint, MessageAlignment};
use super::size_level::SizeLevel;
use super::theme::Theme;
use crate::core::bytes::format_bytes;
use crate::core::cleaner::{RemovalProgress, RemovalReport};
use crate::core::path_utils::prettify_path;
use crate::core::scanner::CategoryScan;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmButton {
    Cancel,
    Remove,
}

pub enum ModalOverlay {
    None,
    Confirm(ConfirmButton),
    Removing {
        progress: RemovalProgress,
        spinner_idx: usize,
    },
    Outcome(RemovalReport),
}

pub struct SelectState {
    pub scan: CategoryScan,
    pub focused_index: usize,
    pub path_scroll: usize,
    pub search_query: String,
    pub is_searching: bool,
    pub overlay: ModalOverlay,
}

#[derive(Debug, Clone)]
pub struct FlattenedRow {
    pub group_index: usize,
    pub item_index: Option<usize>, // None if header
    pub is_header: bool,
    pub group_name: String,
    pub size_bytes: u64,
    pub display_path: String,
    pub is_selected: bool,
    pub full_path: Option<PathBuf>,
}

impl SelectState {
    pub fn new(mut scan: CategoryScan) -> Self {
        // Interactive review starts with every item unselected
        for group in &mut scan.groups {
            for item in &mut group.items {
                item.selected = false;
            }
        }
        let mut state = Self {
            scan,
            focused_index: 0,
            path_scroll: 0,
            search_query: String::new(),
            is_searching: false,
            overlay: ModalOverlay::None,
        };
        state.move_to_start();
        state
    }

    pub fn with_preserved_selection(scan: CategoryScan) -> Self {
        let mut state = Self {
            scan,
            focused_index: 0,
            path_scroll: 0,
            search_query: String::new(),
            is_searching: false,
            overlay: ModalOverlay::None,
        };
        state.move_to_start();
        state
    }

    pub fn flattened_rows(&self) -> Vec<FlattenedRow> {
        let mut rows = Vec::new();

        for (g_idx, group) in self.scan.groups.iter().enumerate() {
            let matching_items: Vec<(usize, &_)> = group
                .items
                .iter()
                .enumerate()
                .filter(|(_, item)| {
                    if self.search_query.is_empty() {
                        true
                    } else {
                        item.path
                            .to_string_lossy()
                            .to_lowercase()
                            .contains(&self.search_query.to_lowercase())
                            || group
                                .tool
                                .to_lowercase()
                                .contains(&self.search_query.to_lowercase())
                    }
                })
                .collect();

            if matching_items.is_empty() && !self.search_query.is_empty() {
                continue;
            }

            let item_count = group.items.len();
            let count_suffix = if item_count == 1 {
                "1 item".to_string()
            } else {
                format!("{item_count} items")
            };

            // Group Header
            rows.push(FlattenedRow {
                group_index: g_idx,
                item_index: None,
                is_header: true,
                group_name: group.tool.clone(),
                size_bytes: 0,
                display_path: count_suffix,
                is_selected: false,
                full_path: None,
            });

            for (i_idx, item) in matching_items {
                let pretty_path = prettify_path(&item.path);
                rows.push(FlattenedRow {
                    group_index: g_idx,
                    item_index: Some(i_idx),
                    is_header: false,
                    group_name: group.tool.clone(),
                    size_bytes: item.size_bytes,
                    display_path: pretty_path,
                    is_selected: item.selected,
                    full_path: Some(item.path.clone()),
                });
            }
        }

        rows
    }

    pub fn next_selectable(&mut self) {
        let rows = self.flattened_rows();
        if rows.is_empty() {
            return;
        }

        let mut idx = self.focused_index + 1;
        while idx < rows.len() {
            if !rows[idx].is_header {
                self.focused_index = idx;
                self.path_scroll = 0;
                return;
            }
            idx += 1;
        }
    }

    pub fn previous_selectable(&mut self) {
        let rows = self.flattened_rows();
        if rows.is_empty() || self.focused_index == 0 {
            return;
        }

        let mut idx = self.focused_index.saturating_sub(1);
        loop {
            if !rows[idx].is_header {
                self.focused_index = idx;
                self.path_scroll = 0;
                return;
            }
            if idx == 0 {
                break;
            }
            idx -= 1;
        }
    }

    pub fn move_by(&mut self, delta: isize) {
        let rows = self.flattened_rows();
        if rows.is_empty() {
            return;
        }

        if delta > 0 {
            let mut steps = delta;
            let mut idx = self.focused_index;
            while steps > 0 && idx + 1 < rows.len() {
                idx += 1;
                if !rows[idx].is_header {
                    self.focused_index = idx;
                    steps -= 1;
                }
            }
        } else if delta < 0 {
            let mut steps = delta.abs();
            let mut idx = self.focused_index;
            while steps > 0 && idx > 0 {
                idx -= 1;
                if !rows[idx].is_header {
                    self.focused_index = idx;
                    steps -= 1;
                }
            }
        }
        self.path_scroll = 0;
    }

    pub fn page_down(&mut self) {
        self.move_by(10);
    }

    pub fn page_up(&mut self) {
        self.move_by(-10);
    }

    pub fn move_to_start(&mut self) {
        let rows = self.flattened_rows();
        if let Some((idx, _)) = rows.iter().enumerate().find(|(_, r)| !r.is_header) {
            self.focused_index = idx;
            self.path_scroll = 0;
        }
    }

    pub fn move_to_end(&mut self) {
        let rows = self.flattened_rows();
        if let Some((idx, _)) = rows.iter().enumerate().rfind(|(_, r)| !r.is_header) {
            self.focused_index = idx;
            self.path_scroll = 0;
        }
    }

    pub fn toggle_current(&mut self) {
        let rows = self.flattened_rows();
        if let Some(row) = rows.get(self.focused_index)
            && let Some(i_idx) = row.item_index
        {
            let item = &mut self.scan.groups[row.group_index].items[i_idx];
            item.selected = !item.selected;
        }
    }

    pub fn select_all(&mut self) {
        for group in &mut self.scan.groups {
            for item in &mut group.items {
                item.selected = true;
            }
        }
    }

    pub fn select_none(&mut self) {
        for group in &mut self.scan.groups {
            for item in &mut group.items {
                item.selected = false;
            }
        }
    }

    pub fn scroll_path_right(&mut self) {
        self.path_scroll = self.path_scroll.saturating_add(4);
    }

    pub fn scroll_path_left(&mut self) {
        self.path_scroll = self.path_scroll.saturating_sub(4);
    }

    pub fn current_focused_path(&self) -> Option<PathBuf> {
        let rows = self.flattened_rows();
        rows.get(self.focused_index)
            .and_then(|r| r.full_path.clone())
    }

    pub fn open_confirmation(&mut self) {
        if !self.scan.selected_items().is_empty() {
            self.overlay = ModalOverlay::Confirm(ConfirmButton::Cancel);
        }
    }

    pub fn close_overlay(&mut self) {
        self.overlay = ModalOverlay::None;
    }

    pub fn toggle_confirmation_button(&mut self) {
        if let ModalOverlay::Confirm(btn) = self.overlay {
            self.overlay = ModalOverlay::Confirm(match btn {
                ConfirmButton::Cancel => ConfirmButton::Remove,
                ConfirmButton::Remove => ConfirmButton::Cancel,
            });
        }
    }
}

pub fn render_select(frame: &mut Frame, state: &SelectState, theme: &Theme) {
    let area = frame.area();

    let total_shown_bytes: u64 = state.scan.total_all_bytes();
    let total_selected_bytes: u64 = state.scan.total_selected_bytes();
    let selected_count = state.scan.selected_items().len();

    let right_status = format!(
        "{} selected · {}",
        selected_count,
        format_bytes(total_selected_bytes)
    );

    // Framed Window
    let window_title = format!(" Choose {} ", state.scan.title);
    let window_block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(theme.border)
        .title(Line::from(vec![Span::styled(window_title, theme.title)]))
        .title_alignment(Alignment::Left);

    let inner_area = window_block.inner(area);
    frame.render_widget(window_block, area);

    // Layout inside window
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // Subtitle / Status line
            Constraint::Min(6),    // List area
            Constraint::Length(1), // Divider
            Constraint::Length(2), // Footer: Path + Keybinds
        ])
        .split(inner_area);

    // Subtitle & Status
    let left_status = format!(
        "{} shown · {}",
        state
            .scan
            .groups
            .iter()
            .map(|g| g.items.len())
            .sum::<usize>(),
        format_bytes(total_shown_bytes)
    );

    let mut header_lines = Vec::new();
    if state.is_searching {
        header_lines.push(Line::from(vec![
            Span::styled("Search: ", theme.accent),
            Span::styled(&state.search_query, theme.normal),
            Span::styled("█", theme.accent),
        ]));
    } else {
        header_lines.push(Line::from(vec![
            Span::styled(left_status, theme.muted),
            Span::raw("    "),
            Span::styled(right_status, theme.info),
        ]));
    }
    header_lines.push(Line::from(Span::styled(&state.scan.title, theme.normal)));

    frame.render_widget(Paragraph::new(header_lines), chunks[0]);

    // List rendering
    let rows = state.flattened_rows();
    let list_area_height = chunks[1].height as usize;
    let list_scroll_offset = if rows.len() > list_area_height && list_area_height > 0 {
        let half = list_area_height / 2;
        let max_offset = rows.len().saturating_sub(list_area_height);
        state.focused_index.saturating_sub(half).min(max_offset)
    } else {
        0
    };

    let mut list_lines = Vec::new();
    for (idx, row) in rows
        .iter()
        .enumerate()
        .skip(list_scroll_offset)
        .take(list_area_height)
    {
        let is_focused = idx == state.focused_index;

        if row.is_header {
            list_lines.push(Line::from(vec![
                Span::raw(" "),
                Span::styled(&row.group_name, theme.muted),
                Span::raw("  "),
                Span::styled(&row.display_path, theme.muted),
            ]));
        } else {
            let cursor = if is_focused { "›" } else { " " };
            let bullet = if row.is_selected { "●" } else { "○" };

            let cursor_style = if is_focused {
                theme.cursor
            } else {
                theme.muted
            };
            let bullet_style = if row.is_selected {
                theme.selected_marker
            } else {
                theme.unselected_marker
            };

            let size_style = SizeLevel::of(row.size_bytes).style(is_focused);
            let path_style = if is_focused {
                theme.focused_row
            } else {
                theme.normal
            };

            list_lines.push(Line::from(vec![
                Span::styled(format!("{cursor} "), cursor_style),
                Span::styled(format!("{bullet} "), bullet_style),
                Span::styled(format!("{:>9}  ", format_bytes(row.size_bytes)), size_style),
                Span::styled(&row.display_path, path_style),
            ]));
        }
    }

    frame.render_widget(Paragraph::new(list_lines), chunks[1]);

    // Divider
    let divider = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(theme.border);
    frame.render_widget(divider, chunks[2]);

    // Footer: Path + Keybind hints
    let focused_path_str = state
        .current_focused_path()
        .map(|p| prettify_path(&p))
        .unwrap_or_default();

    let display_path = if state.path_scroll < focused_path_str.len() {
        &focused_path_str[state.path_scroll..]
    } else {
        ""
    };

    let footer_lines = vec![
        Line::from(vec![
            Span::styled("Path  ", theme.muted),
            Span::styled(display_path, theme.normal),
        ]),
        Line::from(vec![
            Span::styled("↑↓ ", theme.footer_key),
            Span::styled("move  ", theme.footer_desc),
            Span::styled("←→ ", theme.footer_key),
            Span::styled("path  ", theme.footer_desc),
            Span::styled("Space ", theme.footer_key),
            Span::styled("toggle  ", theme.footer_desc),
            Span::styled("A ", theme.footer_key),
            Span::styled("all  ", theme.footer_desc),
            Span::styled("N ", theme.footer_key),
            Span::styled("none  ", theme.footer_desc),
            Span::styled("/ ", theme.footer_key),
            Span::styled("search  ", theme.footer_desc),
            Span::styled("Enter ", theme.footer_key),
            Span::styled("remove  ", theme.footer_desc),
            Span::styled("Esc/q ", theme.footer_key),
            Span::styled("cancel", theme.footer_desc),
        ]),
    ];

    frame.render_widget(Paragraph::new(footer_lines), chunks[3]);

    // Modal Overlays
    match &state.overlay {
        ModalOverlay::None => {}
        ModalOverlay::Confirm(focus_btn) => {
            render_confirmation_dialog(frame, state, *focus_btn, theme);
        }
        ModalOverlay::Removing {
            progress,
            spinner_idx,
        } => {
            render_removing_dialog(frame, progress, *spinner_idx, theme);
        }
        ModalOverlay::Outcome(report) => {
            render_outcome_dialog(frame, report, theme);
        }
    }
}

fn render_confirmation_dialog(
    frame: &mut Frame,
    state: &SelectState,
    focus_btn: ConfirmButton,
    theme: &Theme,
) {
    let count = state.scan.selected_items().len();
    let noun = if count == 1 { "item" } else { "items" };
    let title = format!("Remove {count} {noun}?");

    let msg = format!(
        "{} will be deleted",
        format_bytes(state.scan.total_selected_bytes())
    );

    let focused = match focus_btn {
        ConfirmButton::Cancel => 0,
        ConfirmButton::Remove => 1,
    };

    Dialog::new(title)
        .with_title_style(theme.warning)
        .with_border_style(theme.warning)
        .with_message(vec![Line::from(msg)])
        .with_buttons(
            vec![
                DialogButton::new("Cancel"),
                DialogButton::destructive("Remove"),
            ],
            Some(focused),
        )
        .with_hints(DialogHint::choice_hints())
        .render(frame, frame.area(), theme);
}

fn render_removing_dialog(
    frame: &mut Frame,
    progress: &RemovalProgress,
    spinner_idx: usize,
    theme: &Theme,
) {
    let count = progress.total_count;
    let noun = if count == 1 { "item" } else { "items" };
    let title = format!("Removing {count} {noun}…");

    let spinner_char = match spinner_idx % 4 {
        0 => "⠋",
        1 => "⠙",
        2 => "⠹",
        _ => "⠸",
    };

    let pretty_path = prettify_path(&progress.path);
    let path_line = Line::from(vec![
        Span::styled(format!("{spinner_char} "), theme.accent),
        Span::styled(pretty_path, theme.normal),
    ]);

    let count_line = Line::from(vec![Span::styled(
        format!("{} of {}", progress.current_index, progress.total_count),
        theme.muted,
    )]);

    Dialog::new(title)
        .with_title_style(theme.warning)
        .with_border_style(theme.warning)
        .with_message(vec![path_line, count_line])
        .with_alignment(MessageAlignment::Leading)
        .render(frame, frame.area(), theme);
}

fn render_outcome_dialog(frame: &mut Frame, report: &RemovalReport, theme: &Theme) {
    let has_failures = !report.failures.is_empty();

    let mut message = vec![Line::from(vec![
        Span::styled(format_bytes(report.freed_bytes), theme.success),
        Span::styled(
            format!(" removed across {} item(s)", report.removed_count),
            theme.normal,
        ),
    ])];

    if has_failures {
        message.push(Line::from(Span::styled(
            format!("{} item(s) skipped or failed", report.failures.len()),
            theme.warning,
        )));
    }

    Dialog::new("Clean Complete")
        .with_title_style(if has_failures {
            theme.warning
        } else {
            theme.success
        })
        .with_border_style(if has_failures {
            theme.warning
        } else {
            theme.success
        })
        .with_message(message)
        .with_buttons(vec![DialogButton::new("Done")], Some(0))
        .with_hints(vec![DialogHint::new("Enter", "close")])
        .render(frame, frame.area(), theme);
}

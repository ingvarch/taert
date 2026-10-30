//! Interactive Select Items Screen.
//! Includes modal confirmation dialog, live removal progress dialog with spinner,
//! and completion outcome dialog.

use std::path::PathBuf;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::components::button::Button;
use super::components::chrome::AppChrome;
use super::components::empty::EmptyState;
use super::components::gauge::bar;
use super::components::hints::{Hint, PrimaryAction};
use super::components::list::{ListRow, path_spans, render_section_header};
use super::components::modal::Modal;
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
    pub item_index: Option<usize>,
    pub is_header: bool,
    pub group_name: String,
    pub size_bytes: u64,
    pub display_path: String,
    pub is_selected: bool,
    pub full_path: Option<PathBuf>,
}

impl SelectState {
    pub fn new(mut scan: CategoryScan) -> Self {
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
                            .display()
                            .to_string()
                            .to_lowercase()
                            .contains(&self.search_query.to_lowercase())
                    }
                })
                .collect();

            if matching_items.is_empty() && !self.search_query.is_empty() {
                continue;
            }

            let group_bytes: u64 = matching_items.iter().map(|(_, it)| it.size_bytes).sum();

            // Group header
            rows.push(FlattenedRow {
                group_index: g_idx,
                item_index: None,
                is_header: true,
                group_name: group.tool.clone(),
                size_bytes: group_bytes,
                display_path: format!(
                    "{} items · {}",
                    matching_items.len(),
                    format_bytes(group_bytes)
                ),
                is_selected: matching_items.iter().all(|(_, it)| it.selected)
                    && !matching_items.is_empty(),
                full_path: None,
            });

            // Group items
            for (i_idx, item) in matching_items {
                rows.push(FlattenedRow {
                    group_index: g_idx,
                    item_index: Some(i_idx),
                    is_header: false,
                    group_name: group.tool.clone(),
                    size_bytes: item.size_bytes,
                    display_path: prettify_path(&item.path),
                    is_selected: item.selected,
                    full_path: Some(item.path.clone()),
                });
            }
        }

        rows
    }

    pub fn toggle_current(&mut self) {
        let rows = self.flattened_rows();
        if let Some(row) = rows.get(self.focused_index) {
            if row.is_header {
                let g_idx = row.group_index;
                let all_sel = self.scan.groups[g_idx]
                    .items
                    .iter()
                    .all(|item| item.selected);
                for item in &mut self.scan.groups[g_idx].items {
                    item.selected = !all_sel;
                }
            } else if let Some(i_idx) = row.item_index {
                let g_idx = row.group_index;
                self.scan.groups[g_idx].items[i_idx].selected =
                    !self.scan.groups[g_idx].items[i_idx].selected;
            }
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

    pub fn next_selectable(&mut self) {
        let rows = self.flattened_rows();
        if rows.is_empty() {
            return;
        }

        let mut next = self.focused_index + 1;
        while next < rows.len() {
            if !rows[next].is_header {
                self.focused_index = next;
                self.path_scroll = 0;
                return;
            }
            next += 1;
        }
        self.move_to_end();
    }

    pub fn previous_selectable(&mut self) {
        let rows = self.flattened_rows();
        if rows.is_empty() {
            return;
        }

        let mut prev = self.focused_index.saturating_sub(1);
        loop {
            if !rows[prev].is_header {
                self.focused_index = prev;
                self.path_scroll = 0;
                return;
            }
            if prev == 0 {
                break;
            }
            prev -= 1;
        }
        self.move_to_start();
    }

    pub fn page_down(&mut self) {
        let rows = self.flattened_rows();
        if rows.is_empty() {
            return;
        }
        let target = (self.focused_index + 10).min(rows.len().saturating_sub(1));
        for (idx, row) in rows.iter().enumerate().skip(target) {
            if !row.is_header {
                self.focused_index = idx;
                self.path_scroll = 0;
                return;
            }
        }
        self.move_to_end();
    }

    pub fn page_up(&mut self) {
        let rows = self.flattened_rows();
        if rows.is_empty() {
            return;
        }
        let target = self.focused_index.saturating_sub(10);
        for idx in (0..=target).rev() {
            if !rows[idx].is_header {
                self.focused_index = idx;
                self.path_scroll = 0;
                return;
            }
        }
        self.move_to_start();
    }

    pub fn move_to_start(&mut self) {
        let rows = self.flattened_rows();
        for (idx, row) in rows.iter().enumerate() {
            if !row.is_header {
                self.focused_index = idx;
                self.path_scroll = 0;
                return;
            }
        }
        self.focused_index = 0;
        self.path_scroll = 0;
    }

    pub fn move_to_end(&mut self) {
        let rows = self.flattened_rows();
        for (idx, row) in rows.iter().enumerate().rev() {
            if !row.is_header {
                self.focused_index = idx;
                self.path_scroll = 0;
                return;
            }
        }
        self.focused_index = rows.len().saturating_sub(1);
        self.path_scroll = 0;
    }

    pub fn scroll_path_left(&mut self) {
        self.path_scroll = self.path_scroll.saturating_sub(4);
    }

    pub fn scroll_path_right(&mut self) {
        self.path_scroll = self.path_scroll.saturating_add(4);
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
    let total_shown_bytes: u64 = state.scan.total_all_bytes();
    let total_selected_bytes: u64 = state.scan.total_selected_bytes();
    let selected_count = state.scan.selected_items().len();
    let total_count: usize = state.scan.groups.iter().map(|g| g.items.len()).sum();

    let right_status = Line::from(vec![
        Span::styled(format!("{selected_count}"), theme.headline()),
        Span::styled(format!(" of {total_count} selected  ·  "), theme.callout()),
        Span::styled(format_bytes(total_selected_bytes), theme.headline()),
    ]);

    let primary_action = if selected_count > 0 {
        PrimaryAction::new(format!("Review {selected_count} items"), theme.g.key_enter)
    } else {
        PrimaryAction::disabled("Review items", theme.g.key_enter)
    };

    let chrome = AppChrome::new(vec!["taert", "Clean caches", &state.scan.title])
        .status(right_status)
        .hints(vec![
            Hint::new("↑↓", "Move"),
            Hint::new("␣", "Select"),
            Hint::new("/", "Search"),
            Hint::new("?", "More"),
        ])
        .primary(primary_action);

    let content_rect = chrome.render(frame, theme);

    if total_count == 0 {
        EmptyState::new("✓", "Nothing to clean", "Your caches are already tidy.")
            .glyph_color(theme.p.green)
            .render(frame, content_rect, theme);
        return;
    }

    let mut start_y = content_rect.y;
    let mut avail_h = content_rect.height;

    // Search bar if searching or query not empty
    if state.is_searching || !state.search_query.is_empty() {
        let results_count = state
            .flattened_rows()
            .iter()
            .filter(|r| !r.is_header)
            .count();
        let search_line = Line::from(vec![
            Span::styled(format!("  {}  ", theme.g.search), theme.accent_text()),
            Span::styled(&state.search_query, theme.headline()),
            Span::styled("▏", theme.accent_text().add_modifier(Modifier::BOLD)),
            Span::raw("   "),
            Span::styled(format!("{results_count} results"), theme.footnote()),
        ]);
        let search_rect = Rect {
            x: content_rect.x,
            y: start_y,
            width: content_rect.width,
            height: 1,
        };
        frame.render_widget(Paragraph::new(search_line), search_rect);
        start_y += 2; // bar + blank line
        avail_h = avail_h.saturating_sub(2);
    }

    let list_rect = Rect {
        x: content_rect.x,
        y: start_y,
        width: content_rect.width,
        height: avail_h,
    };

    let rows = state.flattened_rows();
    let list_area_height = list_rect.height as usize;
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
            list_lines.push(render_section_header(
                &row.group_name,
                Some(&row.display_path),
                list_rect.width as usize,
                theme,
            ));
        } else {
            let bullet = if row.is_selected {
                theme.g.check_on
            } else {
                theme.g.check_off
            };
            let bullet_style = if row.is_selected {
                theme.accent_text().add_modifier(Modifier::BOLD)
            } else {
                theme.callout()
            };

            let title_spans = if let Some(path) = &row.full_path {
                let mut spans =
                    path_spans(path, (list_rect.width as usize).saturating_sub(20), theme);
                if is_focused && state.path_scroll > 0 {
                    let full_str = prettify_path(path);
                    if state.path_scroll < full_str.len() {
                        spans = vec![Span::styled(
                            full_str[state.path_scroll..].to_string(),
                            theme.body(),
                        )];
                    }
                }
                spans
            } else {
                vec![Span::styled(&row.display_path, theme.body())]
            };

            let size_style = theme.size(row.size_bytes);
            let detail = Span::styled(format!("{:>9}", format_bytes(row.size_bytes)), size_style);

            let frac = if total_shown_bytes > 0 {
                Some(row.size_bytes as f64 / total_shown_bytes as f64)
            } else {
                None
            };

            let mut list_row = ListRow::new(title_spans)
                .leading(Span::styled(bullet, bullet_style))
                .detail(detail)
                .selected(is_focused);

            if let Some(f) = frac
                && list_rect.width >= 100
            {
                list_row = list_row.bar(f);
            }

            list_lines.push(list_row.render_line(list_rect.width as usize, theme));
        }
    }

    frame.render_widget(Paragraph::new(list_lines), list_rect);

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
        "{} will be permanently deleted. This cannot be undone.",
        format_bytes(state.scan.total_selected_bytes())
    );

    let cancel_focused = matches!(focus_btn, ConfirmButton::Cancel);
    let remove_focused = matches!(focus_btn, ConfirmButton::Remove);

    Modal::new(&title)
        .message(vec![Line::from(msg)])
        .buttons(vec![
            Button::new("Cancel").focused(cancel_focused),
            Button::destructive("Remove").focused(remove_focused),
        ])
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

    let spinner_char = if theme.env.reduce_motion {
        "*"
    } else {
        match spinner_idx % 4 {
            0 => "⠋",
            1 => "⠙",
            2 => "⠹",
            _ => "⠸",
        }
    };

    let pretty_path = prettify_path(&progress.path);
    let path_line = Line::from(vec![
        Span::styled(format!("{spinner_char} "), theme.accent_text()),
        Span::styled(pretty_path, theme.body()),
    ]);

    let fraction = if progress.total_count > 0 {
        progress.current_index as f64 / progress.total_count as f64
    } else {
        0.0
    };

    let bar_spans = bar(fraction, 40, theme.p.accent, theme);

    Modal::new(&title)
        .message(vec![
            path_line,
            Line::from(bar_spans),
            Line::from(vec![Span::styled(
                format!("{} of {}", progress.current_index, progress.total_count),
                theme.footnote(),
            )]),
        ])
        .buttons(vec![])
        .render(frame, frame.area(), theme);
}

fn render_outcome_dialog(frame: &mut Frame, report: &RemovalReport, theme: &Theme) {
    let title = if report.freed_bytes > 0 {
        "Clean Complete"
    } else {
        "Nothing was removed"
    };

    let mut message = vec![Line::from(vec![
        Span::styled(format_bytes(report.freed_bytes), theme.success()),
        Span::styled(
            format!(" freed across {} items", report.removed_count),
            theme.body(),
        ),
    ])];

    if !report.failures.is_empty() {
        message.push(Line::from(""));
        message.push(Line::from(Span::styled(
            format!("{} items failed to remove:", report.failures.len()),
            theme.danger(),
        )));
        for fail in report.failures.iter().take(3) {
            message.push(Line::from(Span::styled(
                format!("  ✕ {}", prettify_path(&fail.path)),
                theme.callout(),
            )));
        }
    }

    Modal::new(title)
        .message(message)
        .buttons(vec![Button::primary("Done").focused(true)])
        .render(frame, frame.area(), theme);
}

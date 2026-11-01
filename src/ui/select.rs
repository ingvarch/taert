//! Interactive Select Items Screen.
//! Includes modal confirmation dialog, live removal progress dialog with spinner,
//! and completion outcome dialog.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use super::analyze::SortOrder;
use super::components::button::Button;
use super::components::chrome::AppChrome;
use super::components::empty::EmptyState;
use super::components::frame::{FRAME_SIDES, frame_bottom, frame_top, framed};
use super::components::gauge::bar;
use super::components::hints::{Hint, PrimaryAction};
use super::components::list::{ColumnTitles, ListRow, SIZE_W, more_folders, path_spans};
use super::components::modal::Modal;
use super::components::text::plural;
use super::theme::Theme;
use crate::core::apps;
use crate::core::bytes::format_bytes;
use crate::core::cleaner::{RemovalProgress, RemovalReport};
use crate::core::date_utils::format_age;
use crate::core::docker::{DockerObject, SCAN_ID};
use crate::core::leftovers::UninstallRole;
use crate::core::path_utils::prettify_path;
use crate::core::purger;
use crate::core::scanner::{CategoryScan, ScanItem, ToolGroup};
use crate::core::uninstaller;

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
    pub problem: Option<String>,
    /// Lists that can be sorted are drawn as tables.
    pub sort_order: Option<SortOrder>,
    /// Told before the removal starts.
    pub notes: Vec<String>,
    pub asks_password: bool,
}

const BAR_MIN_WIDTH: usize = 100;
const MAX_LISTED_FAILURES: usize = 3;
const MAX_LISTED_NOTES: usize = 3;
// Fits "About an hour ago", the longest age that lasts more than a few seconds.
const AGE_W: usize = 17;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Item,
    Section,
    Columns { date: &'static str },
    SectionEnd,
    Gap,
}

#[derive(Debug, Clone)]
pub struct FlattenedRow {
    pub group_index: usize,
    pub item_index: Option<usize>,
    pub kind: RowKind,
    pub group_name: String,
    pub size_bytes: u64,
    pub display_path: String,
    pub is_selected: bool,
    pub full_path: Option<PathBuf>,
    pub item_count: usize,
    pub created: Option<SystemTime>,
    pub more_folders: usize,
}

impl FlattenedRow {
    pub fn is_item(&self) -> bool {
        self.kind == RowKind::Item
    }
}

impl SelectState {
    pub fn new(mut scan: CategoryScan) -> Self {
        for group in &mut scan.groups {
            for item in &mut group.items {
                item.selected = false;
            }
        }
        Self::with_preserved_selection(scan)
    }

    /// A scan that could not run, with the reason to show instead of a list.
    pub fn unavailable(title: &str, problem: String) -> Self {
        let mut state = Self::new(CategoryScan {
            notes: Vec::new(),
            id: SCAN_ID.to_string(),
            title: title.to_string(),
            groups: Vec::new(),
        });
        state.problem = Some(problem);
        state
    }

    pub fn with_preserved_selection(scan: CategoryScan) -> Self {
        let is_table = scan.id == SCAN_ID || scan.id == apps::LIST_ID;
        let sort_order = is_table.then(SortOrder::default);
        let notes = scan.notes.clone();
        let mut state = Self {
            scan,
            focused_index: 0,
            path_scroll: 0,
            search_query: String::new(),
            is_searching: false,
            overlay: ModalOverlay::None,
            problem: None,
            sort_order,
            notes,
            asks_password: false,
        };
        state.move_to_start();
        state
    }

    /// Sorts inside each section and keeps the cursor on its item.
    pub fn set_sort(&mut self, sort_order: SortOrder) {
        let focused = self.flattened_rows().get(self.focused_index).cloned();

        for group in &mut self.scan.groups {
            sort_order.sort_children(&mut group.items);
        }
        self.sort_order = Some(sort_order);

        let Some(focused) = focused else { return };
        let rows = self.flattened_rows();
        let same_item = |row: &FlattenedRow| {
            row.is_item()
                && row.group_index == focused.group_index
                && row.full_path == focused.full_path
        };
        if let Some(index) = rows.iter().position(same_item) {
            self.focused_index = index;
        }
    }

    pub fn flattened_rows(&self) -> Vec<FlattenedRow> {
        let mut rows = Vec::new();

        for (g_idx, group) in self.scan.groups.iter().enumerate() {
            let matching_items: Vec<(usize, &_)> = group
                .items
                .iter()
                .enumerate()
                .filter(|(_, item)| {
                    let query = self.search_query.to_lowercase();
                    item.folders().any(|folder| {
                        folder
                            .path
                            .display()
                            .to_string()
                            .to_lowercase()
                            .contains(&query)
                    })
                })
                .collect();

            if matching_items.is_empty() && !self.search_query.is_empty() {
                continue;
            }

            let group_bytes: u64 = matching_items.iter().map(|(_, it)| it.size_bytes).sum();
            let heading_row = |kind| FlattenedRow {
                group_index: g_idx,
                item_index: None,
                kind,
                group_name: group.tool.clone(),
                size_bytes: 0,
                display_path: String::new(),
                is_selected: false,
                full_path: None,
                item_count: 0,
                created: None,
                more_folders: 0,
            };

            if !rows.is_empty() {
                rows.push(heading_row(RowKind::Gap));
            }

            // Group header
            rows.push(FlattenedRow {
                group_index: g_idx,
                item_index: None,
                kind: RowKind::Section,
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
                item_count: matching_items.len(),
                created: None,
                more_folders: 0,
            });

            if self.sort_order.is_some() {
                let is_cache = |item: &ScanItem| item.docker == Some(DockerObject::BuildCache);
                let date = if self.scan.id == apps::LIST_ID || group.items.iter().any(is_cache) {
                    "LAST USED"
                } else {
                    "CREATED"
                };
                rows.push(heading_row(RowKind::Columns { date }));
            }

            // Group items
            for (i_idx, item) in matching_items {
                rows.push(FlattenedRow {
                    group_index: g_idx,
                    item_index: Some(i_idx),
                    kind: RowKind::Item,
                    group_name: group.tool.clone(),
                    size_bytes: item.size_bytes,
                    display_path: prettify_path(&item.path),
                    is_selected: item.selected,
                    full_path: Some(item.path.clone()),
                    item_count: 0,
                    created: item.modified,
                    more_folders: item.also.len(),
                });
            }

            rows.push(heading_row(RowKind::SectionEnd));
        }

        rows
    }

    pub fn toggle_current(&mut self) {
        let rows = self.flattened_rows();
        if let Some(row) = rows.get(self.focused_index) {
            if row.kind == RowKind::Section {
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

    fn set_shown_items(&mut self, selected: bool) {
        for row in self.flattened_rows() {
            if let Some(item) = row.item_index {
                self.scan.groups[row.group_index].items[item].selected = selected;
            }
        }
    }

    /// Selects what the search shows, so nothing hidden gets selected.
    pub fn select_all(&mut self) {
        self.set_shown_items(true);
    }

    pub fn select_none(&mut self) {
        self.set_shown_items(false);
    }

    pub fn next_selectable(&mut self) {
        let rows = self.flattened_rows();
        if rows.is_empty() {
            return;
        }

        let mut next = self.focused_index + 1;
        while next < rows.len() {
            if rows[next].is_item() {
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
            if rows[prev].is_item() {
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
            if row.is_item() {
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
            if rows[idx].is_item() {
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
            if row.is_item() {
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
            if row.is_item() {
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
            self.asks_password = uninstaller::needs_password(&self.scan);
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
        Span::styled(
            format_bytes(total_selected_bytes),
            theme.size(total_selected_bytes),
        ),
    ]);

    let enter = theme.g.key_enter;
    let id = state.scan.id.as_str();
    let primary_action = if state.is_searching {
        PrimaryAction::new("Apply", enter)
    } else if id == uninstaller::PLAN_ID && selected_count > 0 {
        let items = plural(selected_count, "item", "items");
        PrimaryAction::new(format!("Uninstall {items}"), enter)
    } else if id == uninstaller::PLAN_ID {
        PrimaryAction::disabled("Uninstall", enter)
    } else if id == apps::LIST_ID && selected_count > 0 {
        let chosen = plural(selected_count, "app", "apps");
        PrimaryAction::new(format!("Review {chosen}"), enter)
    } else if id == apps::LIST_ID && total_count > 0 {
        PrimaryAction::new("Review app", enter)
    } else if selected_count > 0 {
        PrimaryAction::new(format!("Review {selected_count} items"), enter)
    } else {
        PrimaryAction::disabled("Review items", enter)
    };

    let title = state.scan.title.as_str();
    let crumbs = if id == uninstaller::PLAN_ID {
        vec!["taert", "Uninstall apps", title]
    } else if id == SCAN_ID || id == purger::SCAN_ID || id == apps::LIST_ID {
        vec!["taert", title]
    } else {
        vec!["taert", "Clean caches", title]
    };

    let chrome = AppChrome::new(crumbs)
        .full_width()
        .status(right_status)
        .hints(toolbar_hints(state, total_count > 0))
        .primary(primary_action)
        .behind_dialog(!matches!(state.overlay, ModalOverlay::None));

    let content_rect = chrome.render(frame, theme);
    let content_rect = render_notes(frame, content_rect, &state.scan.notes, theme);

    if total_count == 0 {
        render_nothing_to_show(frame, content_rect, state, theme);
        render_overlay(frame, state, theme);
        return;
    }

    let mut start_y = content_rect.y;
    let mut avail_h = content_rect.height;

    // Search bar if searching or query not empty
    if state.is_searching || !state.search_query.is_empty() {
        let results_count = state
            .flattened_rows()
            .iter()
            .filter(|r| r.is_item())
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

    let width = list_rect.width as usize;
    let row_width = width.saturating_sub(FRAME_SIDES);
    let now = SystemTime::now();
    let mut list_lines = Vec::new();
    for (idx, row) in rows
        .iter()
        .enumerate()
        .skip(list_scroll_offset)
        .take(list_area_height)
    {
        let is_focused = idx == state.focused_index;
        let color = section_color(&state.scan.groups[row.group_index], theme);

        if row.kind == RowKind::Gap {
            list_lines.push(Line::default());
        } else if row.kind == RowKind::SectionEnd {
            list_lines.push(frame_bottom(width, color, theme));
        } else if let (RowKind::Columns { date }, Some(order)) = (row.kind, state.sort_order) {
            let titles = ColumnTitles {
                date,
                date_width: AGE_W,
                order,
                share: row_width >= BAR_MIN_WIDTH,
                chevron: false,
            };
            let titles = titles.render(row_width, theme);
            list_lines.push(framed(titles, row_width, color, theme));
        } else if row.kind == RowKind::Section {
            let noun = if row.item_count == 1 { "item" } else { "items" };
            let detail_spans = vec![
                Span::styled(format!("{} {noun} · ", row.item_count), theme.footnote()),
                Span::styled(format_bytes(row.size_bytes), theme.size(row.size_bytes)),
            ];
            list_lines.push(section_frame_top(
                &state.scan.groups[row.group_index],
                detail_spans,
                width,
                color,
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

            let app_name = (state.scan.id == apps::LIST_ID)
                .then(|| row.full_path.as_deref().and_then(Path::file_stem))
                .flatten();
            let title_spans = if let Some(name) = app_name {
                vec![Span::styled(
                    name.to_string_lossy().into_owned(),
                    theme.body(),
                )]
            } else if let Some(path) = &row.full_path {
                let mut spans = path_spans(path, row_width.saturating_sub(20), theme);
                if is_focused && state.path_scroll > 0 {
                    let rest: String = prettify_path(path)
                        .chars()
                        .skip(state.path_scroll)
                        .collect();
                    if !rest.is_empty() {
                        spans = vec![Span::styled(rest, theme.body())];
                    }
                }
                spans.extend(more_folders(row.more_folders, theme));
                spans
            } else {
                vec![Span::styled(&row.display_path, theme.body())]
            };

            let size_style = theme.size(row.size_bytes);
            let detail = Span::styled(
                format!("{:>SIZE_W$}", format_bytes(row.size_bytes)),
                size_style,
            );

            let frac = if total_shown_bytes > 0 {
                Some(row.size_bytes as f64 / total_shown_bytes as f64)
            } else {
                None
            };

            let mut list_row = ListRow::new(title_spans)
                .leading(Span::styled(bullet, bullet_style))
                .detail(detail)
                .selected(is_focused);

            if state.sort_order.is_some() {
                let age = row
                    .created
                    .map_or("—".to_string(), |time| format_age(time, now));
                list_row = list_row.meta(Span::styled(format!("{age:<AGE_W$}"), theme.footnote()));
            }

            if let Some(f) = frac
                && row_width >= BAR_MIN_WIDTH
            {
                list_row = list_row.bar(f);
            }

            let line = list_row.render_line(row_width, theme);
            list_lines.push(framed(line, row_width, color, theme));
        }
    }

    frame.render_widget(Paragraph::new(list_lines), list_rect);

    render_overlay(frame, state, theme);
}

/// Draws the notes at the bottom and returns the area left above them.
fn render_notes(frame: &mut Frame, area: Rect, notes: &[String], theme: &Theme) -> Rect {
    let lines: Vec<Line> = notes
        .iter()
        .map(|note| Line::from(Span::styled(note.clone(), theme.callout())))
        .collect();
    let text = Paragraph::new(lines).wrap(Wrap { trim: true });
    let height = text
        .line_count(area.width)
        .min(usize::from(area.height / 2)) as u16;
    if height == 0 {
        return area;
    }
    let notes_area = Rect {
        y: area.y + area.height - height,
        height,
        ..area
    };
    frame.render_widget(text, notes_area);
    Rect {
        height: area.height - height,
        ..area
    }
}

fn render_overlay(frame: &mut Frame, state: &SelectState, theme: &Theme) {
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
            render_outcome_dialog(frame, report, is_plan(state), theme);
        }
    }
}

fn is_plan(state: &SelectState) -> bool {
    state.scan.id == uninstaller::PLAN_ID
}

fn section_color(group: &ToolGroup, theme: &Theme) -> Color {
    match group.items.first().and_then(|item| item.docker.as_ref()) {
        Some(DockerObject::Volume { .. }) => theme.p.orange,
        Some(DockerObject::Network { .. }) => theme.p.teal,
        Some(DockerObject::BuildCache) => theme.p.purple,
        _ => theme.p.accent,
    }
}

// Docker and app sections keep a note in brackets after their name, for plain output too.
fn section_frame_top(
    group: &ToolGroup,
    detail: Vec<Span<'static>>,
    width: usize,
    color: Color,
    theme: &Theme,
) -> Line<'static> {
    let name = group.tool.as_str();
    let has_note = group
        .items
        .iter()
        .any(|item| item.docker.is_some() || item.role.is_some());
    match name.split_once(" (") {
        Some((title, note)) if has_note => frame_top(
            title,
            note.trim_end_matches(')'),
            detail,
            width,
            color,
            theme,
        ),
        _ => frame_top(name, "", detail, width, color, theme),
    }
}

fn toolbar_hints(state: &SelectState, has_items: bool) -> Vec<Hint<'static>> {
    if state.is_searching {
        return vec![Hint::new("⎋", "Cancel")];
    }

    let mut hints = Vec::new();
    if has_items {
        hints.extend([
            Hint::new("␣", "Select"),
            Hint::new("A", "All"),
            Hint::new("N", "None"),
        ]);
        if let Some(order) = state.sort_order {
            hints.push(Hint::new("⇧S/N/D", order.hint_label()));
        }
        hints.push(Hint::new("/", "Search"));
        hints.push(Hint::new("↑↓", "Move"));
    }
    hints.push(Hint::new("⎋", "Back"));
    hints.push(Hint::new("?", "More"));
    hints
}

fn render_nothing_to_show(frame: &mut Frame, area: Rect, state: &SelectState, theme: &Theme) {
    if let Some(problem) = &state.problem {
        let title = format!("{} isn't available", state.scan.title);
        EmptyState::new(theme.g.warning, &title, problem)
            .glyph_color(theme.p.yellow)
            .render(frame, area, theme);
    } else if state.scan.id == SCAN_ID {
        EmptyState::new("✓", "Docker has nothing unused", "Every object is in use.")
            .glyph_color(theme.p.green)
            .render(frame, area, theme);
    } else if state.scan.id == apps::LIST_ID {
        EmptyState::new(
            "✓",
            "No apps to uninstall",
            "Apps in your Applications folders show up here.",
        )
        .glyph_color(theme.p.green)
        .render(frame, area, theme);
    } else if is_plan(state) {
        EmptyState::new(
            "✓",
            "Nothing left to remove",
            "Everything you chose is gone.",
        )
        .glyph_color(theme.p.green)
        .render(frame, area, theme);
    } else {
        EmptyState::new("✓", "Nothing to clean", "Your caches are already tidy.")
            .glyph_color(theme.p.green)
            .render(frame, area, theme);
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
    let bundles = state
        .scan
        .selected_items()
        .iter()
        .filter(|item| item.role == Some(UninstallRole::Bundle))
        .count();
    let title = if bundles > 0 {
        format!("Uninstall {}?", plural(bundles, "app", "apps"))
    } else {
        format!("Remove {count} {noun}?")
    };

    let msg = format!(
        "{} will be permanently deleted. This cannot be undone.",
        format_bytes(state.scan.total_selected_bytes())
    );

    let mut message = vec![Line::from(msg)];
    let volumes = state
        .scan
        .selected_items()
        .iter()
        .filter(|item| matches!(item.docker, Some(DockerObject::Volume { .. })))
        .count();
    if volumes > 0 {
        message.push(Line::from(Span::styled(
            format!(
                "The data in {} is deleted for good.",
                plural(volumes, "volume", "volumes")
            ),
            theme.warning(),
        )));
    }

    if bundles > 0 {
        message.push(Line::from(Span::styled(
            "Open apps are quit first.",
            theme.callout(),
        )));
    }
    if state.asks_password {
        message.push(Line::from(Span::styled(
            "macOS will ask for your password.",
            theme.warning(),
        )));
    }
    for note in &state.notes {
        message.push(Line::from(Span::styled(note.clone(), theme.callout())));
    }

    let cancel_focused = matches!(focus_btn, ConfirmButton::Cancel);
    let remove_focused = matches!(focus_btn, ConfirmButton::Remove);

    Modal::new(&title)
        .message(message)
        .buttons(vec![
            Button::new("Cancel").focused(cancel_focused),
            Button::destructive(if bundles > 0 { "Uninstall" } else { "Remove" })
                .focused(remove_focused),
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
        .tint(theme.p.orange)
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

fn render_outcome_dialog(frame: &mut Frame, report: &RemovalReport, plan: bool, theme: &Theme) {
    let title = match (report.removed_count > 0, plan) {
        (true, true) => "Uninstall Complete",
        (true, false) => "Clean Complete",
        (false, _) => "Nothing was removed",
    };

    let mut message = vec![Line::from(vec![
        Span::styled(format_bytes(report.freed_bytes), theme.success()),
        Span::styled(
            format!(
                " freed across {}",
                plural(report.removed_count, "item", "items")
            ),
            theme.body(),
        ),
    ])];

    if !report.failures.is_empty() {
        message.push(Line::from(""));
        message.push(Line::from(Span::styled(
            format!(
                "{} failed to remove:",
                plural(report.failures.len(), "item", "items")
            ),
            theme.danger(),
        )));
        for fail in report.failures.iter().take(MAX_LISTED_FAILURES) {
            message.push(Line::from(Span::styled(
                format!("  ✕ {}", prettify_path(&fail.path)),
                theme.callout(),
            )));
        }
        if plan {
            let reason = report.failures[0].error.clone();
            message.push(Line::from(Span::styled(reason, theme.callout())));
        }
    }

    if !report.notes.is_empty() {
        message.push(Line::from(""));
    }
    for note in report.notes.iter().take(MAX_LISTED_NOTES) {
        message.push(Line::from(Span::styled(note.clone(), theme.callout())));
    }

    Modal::new(title)
        .message(message)
        .buttons(vec![Button::primary("Done").focused(true)])
        .render(frame, frame.area(), theme);
}

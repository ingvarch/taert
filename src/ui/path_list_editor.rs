//! Interactive Path List Editor.
//! Used for editing Protected Paths (Ignore list) and Project Folders inside the TUI.

use std::path::PathBuf;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use super::dialog::{Dialog, DialogButton, DialogHint, MessageAlignment};
use super::theme::Theme;
use crate::core::path_utils::{prettify_path, standardize_path};
use crate::core::projects_store::ProjectFoldersStore;
use crate::core::stores::IgnoreStore;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathListKind {
    ProtectedPaths,
    ProjectFolders,
}

pub enum PathListMode {
    Normal,
    Adding(String),
    ConfirmRemove(usize),
}

pub struct PathListEditorState {
    pub kind: PathListKind,
    pub items: Vec<String>,
    pub selected_index: usize,
    pub mode: PathListMode,
    pub error_message: Option<String>,
}

impl PathListEditorState {
    pub fn new(kind: PathListKind, ignore: &IgnoreStore) -> Self {
        let items = match kind {
            PathListKind::ProtectedPaths => ignore.roots.clone(),
            PathListKind::ProjectFolders => ProjectFoldersStore::load_or_default().roots,
        };

        Self {
            kind,
            items,
            selected_index: 0,
            mode: PathListMode::Normal,
            error_message: None,
        }
    }

    pub fn title(&self) -> &'static str {
        match self.kind {
            PathListKind::ProtectedPaths => "Protected paths",
            PathListKind::ProjectFolders => "Project folders",
        }
    }

    pub fn next(&mut self) {
        if self.selected_index + 1 < self.items.len() {
            self.selected_index += 1;
        }
    }

    pub fn previous(&mut self) {
        if self.selected_index > 0 {
            self.selected_index -= 1;
        }
    }

    pub fn page_down(&mut self) {
        let count = self.items.len();
        if count > 0 {
            self.selected_index = (self.selected_index + 10).min(count - 1);
        }
    }

    pub fn page_up(&mut self) {
        self.selected_index = self.selected_index.saturating_sub(10);
    }

    pub fn move_to_start(&mut self) {
        self.selected_index = 0;
    }

    pub fn move_to_end(&mut self) {
        let count = self.items.len();
        if count > 0 {
            self.selected_index = count - 1;
        }
    }

    pub fn start_adding(&mut self) {
        self.error_message = None;
        self.mode = PathListMode::Adding(String::new());
    }

    pub fn cancel_input(&mut self) {
        self.mode = PathListMode::Normal;
        self.error_message = None;
    }

    pub fn prompt_remove(&mut self) {
        if !self.items.is_empty() {
            self.mode = PathListMode::ConfirmRemove(self.selected_index);
        }
    }

    pub fn confirm_remove(&mut self, ignore: &mut IgnoreStore) {
        if let PathListMode::ConfirmRemove(idx) = self.mode
            && idx < self.items.len()
        {
            let removed_path = self.items.remove(idx);
            if self.kind == PathListKind::ProtectedPaths {
                ignore.remove(&removed_path);
                let _ = ignore.save_to(&IgnoreStore::default_path().unwrap());
            } else if self.kind == PathListKind::ProjectFolders {
                let mut store = ProjectFoldersStore::load_or_default();
                store.remove(&removed_path);
                let _ = store.save_to(&ProjectFoldersStore::default_path().unwrap());
            }
            if self.selected_index >= self.items.len() && self.selected_index > 0 {
                self.selected_index = self.items.len() - 1;
            }
        }
        self.mode = PathListMode::Normal;
    }

    pub fn commit_add(&mut self, ignore: &mut IgnoreStore) {
        if let PathListMode::Adding(ref input) = self.mode {
            let trimmed = input.trim();
            if trimmed.is_empty() {
                self.error_message = Some("Path cannot be empty".to_string());
                return;
            }

            let canonical = standardize_path(trimmed);
            let path_str = canonical.display().to_string();

            if self.items.contains(&path_str) {
                self.error_message = Some("Path already exists in list".to_string());
                return;
            }

            self.items.push(path_str.clone());
            if self.kind == PathListKind::ProtectedPaths {
                ignore.add(path_str);
                let _ = ignore.save_to(&IgnoreStore::default_path().unwrap());
            } else if self.kind == PathListKind::ProjectFolders {
                let mut store = ProjectFoldersStore::load_or_default();
                store.add(path_str);
                let _ = store.save_to(&ProjectFoldersStore::default_path().unwrap());
            }

            self.selected_index = self.items.len() - 1;
            self.mode = PathListMode::Normal;
            self.error_message = None;
        }
    }
}

pub fn render_path_list_editor(frame: &mut Frame, state: &PathListEditorState, theme: &Theme) {
    let area = frame.area();

    let window_block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(theme.border)
        .title(Line::from(vec![Span::styled(
            format!(" {} ", state.title()),
            theme.title,
        )]))
        .title_alignment(Alignment::Left);

    let inner_area = window_block.inner(area);
    frame.render_widget(window_block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // Subtitle
            Constraint::Min(6),    // Path list
            Constraint::Length(1), // Divider
            Constraint::Length(1), // Footer keyhints
        ])
        .split(inner_area);

    let subtitle = match state.kind {
        PathListKind::ProtectedPaths => "Paths excluded from all cleaning and purge scans",
        PathListKind::ProjectFolders => "Folders scanned by project artifact purger",
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(subtitle, theme.muted))),
        chunks[0],
    );

    // List rendering
    let list_area_height = chunks[1].height as usize;
    let scroll_offset = if state.items.len() > list_area_height && list_area_height > 0 {
        let half = list_area_height / 2;
        let max_offset = state.items.len().saturating_sub(list_area_height);
        state.selected_index.saturating_sub(half).min(max_offset)
    } else {
        0
    };

    let mut list_lines = Vec::new();
    for (idx, path_str) in state
        .items
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(list_area_height)
    {
        let is_selected = idx == state.selected_index;
        let cursor = if is_selected { "› " } else { "  " };

        let cursor_style = if is_selected {
            theme.cursor
        } else {
            theme.muted
        };
        let row_style = if is_selected {
            theme.focused_row
        } else {
            theme.normal
        };

        let pretty = prettify_path(&PathBuf::from(path_str));
        list_lines.push(Line::from(vec![
            Span::styled(cursor, cursor_style),
            Span::styled(pretty, row_style),
        ]));
    }

    if state.items.is_empty() {
        list_lines.push(Line::from(Span::styled(
            "  (no paths configured, press A to add)",
            theme.muted,
        )));
    }

    frame.render_widget(Paragraph::new(list_lines), chunks[1]);

    let divider = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(theme.border);
    frame.render_widget(divider, chunks[2]);

    let footer_text = match state.mode {
        PathListMode::Normal => Line::from(vec![
            Span::styled("↑↓ ", theme.footer_key),
            Span::styled("move  ", theme.footer_desc),
            Span::styled("A ", theme.footer_key),
            Span::styled("add  ", theme.footer_desc),
            Span::styled("D ", theme.footer_key),
            Span::styled("remove  ", theme.footer_desc),
            Span::styled("Esc/q ", theme.footer_key),
            Span::styled("back", theme.footer_desc),
        ]),
        PathListMode::Adding(_) => Line::from(vec![
            Span::styled("Enter ", theme.footer_key),
            Span::styled("confirm add  ", theme.footer_desc),
            Span::styled("Esc ", theme.footer_key),
            Span::styled("cancel", theme.footer_desc),
        ]),
        PathListMode::ConfirmRemove(_) => Line::from(vec![
            Span::styled("Enter/Y ", theme.footer_key),
            Span::styled("confirm remove  ", theme.footer_desc),
            Span::styled("Esc/N ", theme.footer_key),
            Span::styled("cancel", theme.footer_desc),
        ]),
    };
    frame.render_widget(Paragraph::new(footer_text), chunks[3]);

    // Modal input for Adding path
    if let PathListMode::Adding(ref input) = state.mode {
        render_input_dialog(
            frame,
            " Add path ",
            input,
            state.error_message.as_deref(),
            theme,
        );
    }

    // Modal confirmation for Removing path
    if let PathListMode::ConfirmRemove(idx) = state.mode
        && let Some(target) = state.items.get(idx)
    {
        render_remove_dialog(frame, target, theme);
    }
}

fn render_input_dialog(
    frame: &mut Frame,
    title: &str,
    input: &str,
    err: Option<&str>,
    theme: &Theme,
) {
    let mut message = vec![Line::from(vec![
        Span::styled("Path: ", theme.muted),
        Span::styled(input, theme.normal),
        Span::styled("█", theme.accent),
    ])];

    if let Some(e) = err {
        message.push(Line::from(Span::styled(e.to_string(), theme.error)));
    }

    Dialog::new(title)
        .with_message(message)
        .with_hints(vec![
            DialogHint::new("Enter", "confirm"),
            DialogHint::new("Esc", "cancel"),
        ])
        .with_alignment(MessageAlignment::Leading)
        .render(frame, frame.area(), theme);
}

fn render_remove_dialog(frame: &mut Frame, path_str: &str, theme: &Theme) {
    let pretty = prettify_path(&PathBuf::from(path_str));
    let title = format!("Remove {pretty}?");
    let message = format!("Stop tracking {pretty}?");

    Dialog::new(title)
        .with_title_style(theme.warning)
        .with_border_style(theme.warning)
        .with_message(vec![Line::from(message)])
        .with_buttons(
            vec![
                DialogButton::new("Cancel"),
                DialogButton::destructive("Remove"),
            ],
            Some(1),
        )
        .with_hints(DialogHint::choice_hints())
        .render(frame, frame.area(), theme);
}

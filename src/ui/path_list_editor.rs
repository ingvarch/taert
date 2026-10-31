//! Interactive Path List Editor.
//! Used for editing Protected Paths (Ignore list) and Project Folders inside the TUI.

use std::path::{Path, PathBuf};

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::components::button::Button;
use super::components::chrome::AppChrome;
use super::components::empty::EmptyState;
use super::components::frame::{FRAME_ROWS, FRAME_SIDES, card};
use super::components::hints::Hint;
use super::components::list::{ListRow, path_spans};
use super::components::modal::Modal;
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
    let count_text = format!("{} configured", state.items.len());
    let mut chrome =
        AppChrome::new(vec!["taert", "Preferences", state.title()]).status(Line::from(count_text));

    match state.mode {
        PathListMode::Normal => {
            chrome = chrome.hints(vec![
                Hint::new(theme.g.key_up_down, "Move"),
                Hint::new("A", "Add"),
                Hint::new("D", "Remove"),
                Hint::new(theme.g.key_esc, "Back"),
                Hint::new("?", "Help"),
            ]);
        }
        PathListMode::Adding(_) => {
            chrome = chrome.hints(vec![
                Hint::new(theme.g.key_enter, "Add"),
                Hint::new(theme.g.key_esc, "Cancel"),
            ]);
        }
        PathListMode::ConfirmRemove(_) => {
            chrome = chrome.hints(vec![
                Hint::new(theme.g.key_enter, "Remove"),
                Hint::new(theme.g.key_esc, "Cancel"),
            ]);
        }
    }

    let inner = chrome.render(frame, theme);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // Subtitle
            Constraint::Min(4),    // Path list or Empty state
            Constraint::Length(2), // + - Action row
        ])
        .split(inner);

    let subtitle = match state.kind {
        PathListKind::ProtectedPaths => "Paths excluded from all cleaning and purge scans",
        PathListKind::ProjectFolders => "Folders scanned by project artifact purger",
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(subtitle, theme.callout()))),
        chunks[0],
    );

    let frame_w = chunks[1].width as usize;
    let row_width = frame_w.saturating_sub(FRAME_SIDES);
    let frame_rows = (chunks[1].height as usize).saturating_sub(FRAME_ROWS);
    let color = theme.p.teal;

    let mut lines = Vec::new();
    if !state.items.is_empty() {
        let scroll_offset = if state.items.len() > frame_rows && frame_rows > 0 {
            let half = frame_rows / 2;
            let max_offset = state.items.len().saturating_sub(frame_rows);
            state.selected_index.saturating_sub(half).min(max_offset)
        } else {
            0
        };

        for (idx, path_str) in state
            .items
            .iter()
            .enumerate()
            .skip(scroll_offset)
            .take(frame_rows)
        {
            let is_selected = idx == state.selected_index;
            let max_path_len = row_width.saturating_sub(8);
            let row = ListRow::new(path_spans(Path::new(path_str), max_path_len, theme))
                .leading(Span::styled(theme.g.folder, theme.footnote()))
                .selected(is_selected);
            lines.push(row.render_line(row_width, theme));
        }
    }

    let framed_list = card(
        state.title(),
        Vec::new(),
        lines,
        frame_w,
        frame_rows,
        color,
        theme,
    );
    frame.render_widget(Paragraph::new(framed_list), chunks[1]);

    if state.items.is_empty() {
        let (empty_title, empty_msg) = match state.kind {
            PathListKind::ProtectedPaths => ("No protected paths", "Press A to add a path."),
            PathListKind::ProjectFolders => ("No project folders", "Press A to add a folder."),
        };
        let inside = Rect {
            x: chunks[1].x + FRAME_SIDES as u16 / 2,
            y: chunks[1].y + 1,
            width: row_width as u16,
            height: frame_rows as u16,
        };
        EmptyState::new(theme.g.folder, empty_title, empty_msg).render(frame, inside, theme);
    }

    // Action row
    let add_style = theme.accent_text();
    let remove_style = if !state.items.is_empty() {
        theme.accent_text()
    } else {
        theme.footnote()
    };
    let action_line = Line::from(vec![
        Span::styled("  + ", add_style),
        Span::styled("Add path (a)    ", add_style),
        Span::styled("− ", remove_style),
        Span::styled("Remove selected (d)", remove_style),
    ]);
    frame.render_widget(Paragraph::new(action_line), chunks[2]);

    // Modal input for Adding path
    if let PathListMode::Adding(ref input) = state.mode {
        render_input_dialog(
            frame,
            state.kind,
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
    kind: PathListKind,
    input: &str,
    err: Option<&str>,
    theme: &Theme,
) {
    let title = match kind {
        PathListKind::ProtectedPaths => "Add Protected Path",
        PathListKind::ProjectFolders => "Add Project Folder",
    };
    let cursor_char = "▏";
    let mut msg = vec![
        Line::from(vec![
            Span::styled("Path:  ", theme.headline()),
            Span::styled(input, theme.body()),
            Span::styled(cursor_char, theme.accent_text()),
        ]),
        Line::from(Span::styled("─".repeat(34), theme.separator())),
    ];

    if let Some(e) = err {
        msg.push(Line::from(Span::styled(e, theme.body().fg(theme.p.red))));
    }

    Modal::new(title)
        .message(msg)
        .buttons(vec![
            Button::new("Cancel (esc)"),
            Button::primary("Add (↵)"),
        ])
        .hints(Line::from(vec![
            Span::styled(theme.g.key_enter, theme.callout()),
            Span::styled(" Add   ", theme.footnote()),
            Span::styled(theme.g.key_esc, theme.callout()),
            Span::styled(" Cancel", theme.footnote()),
        ]))
        .render(frame, frame.area(), theme);
}

fn render_remove_dialog(frame: &mut Frame, path_str: &str, theme: &Theme) {
    let pretty = prettify_path(&PathBuf::from(path_str));
    let title = format!("Remove {pretty}?");
    let message = format!("Stop tracking {pretty}?");

    Modal::new(&title)
        .message(vec![Line::from(Span::styled(message, theme.body()))])
        .buttons(vec![
            Button::new("Cancel (esc)"),
            Button::destructive("Remove (↵)"),
        ])
        .hints(Line::from(vec![
            Span::styled(theme.g.key_enter, theme.callout()),
            Span::styled(" Remove   ", theme.footnote()),
            Span::styled(theme.g.key_esc, theme.callout()),
            Span::styled(" Cancel", theme.footnote()),
        ]))
        .render(frame, frame.area(), theme);
}

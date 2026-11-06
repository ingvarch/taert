//! The list behind Project folders and Protected paths: its items, adding, removing, saving,
//! and the two dialogs. Settings shows the items and passes the keys of the dialogs on.

use std::path::PathBuf;

use crossterm::event::KeyCode;
use ratatui::Frame;
use ratatui::text::{Line, Span};

use super::components::button::Button;
use super::components::modal::Modal;
use super::theme::Theme;
use crate::core::layout::Layout as MacLayout;
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

pub struct PathList {
    pub kind: PathListKind,
    pub items: Vec<String>,
    pub mode: PathListMode,
    pub error_message: Option<String>,
    ignore_path: PathBuf,
    projects_path: PathBuf,
}

impl PathList {
    pub fn new(kind: PathListKind, ignore: &IgnoreStore, layout: &MacLayout) -> Self {
        let projects_path = layout.projects_path();
        let items = match kind {
            PathListKind::ProtectedPaths => ignore.roots.clone(),
            PathListKind::ProjectFolders => {
                ProjectFoldersStore::load_or_default_at(&projects_path).roots
            }
        };

        Self {
            kind,
            items,
            mode: PathListMode::Normal,
            error_message: None,
            ignore_path: layout.ignore_path(),
            projects_path,
        }
    }

    pub fn add_label(&self) -> &'static str {
        match self.kind {
            PathListKind::ProtectedPaths => "Add path…",
            PathListKind::ProjectFolders => "Add folder…",
        }
    }

    /// The subheading above a list that shares its section with other rows.
    pub fn heading(&self) -> Option<&'static str> {
        match self.kind {
            PathListKind::ProtectedPaths => None,
            PathListKind::ProjectFolders => Some("Project folders"),
        }
    }

    pub fn footnote(&self) -> &'static str {
        match self.kind {
            PathListKind::ProtectedPaths => "Cleanup and project scans skip these paths.",
            PathListKind::ProjectFolders => "Project cleanup scans these folders.",
        }
    }

    pub fn has_dialog(&self) -> bool {
        !matches!(self.mode, PathListMode::Normal)
    }

    pub fn is_adding(&self) -> bool {
        matches!(self.mode, PathListMode::Adding(_))
    }

    pub fn start_adding(&mut self) {
        self.error_message = None;
        self.mode = PathListMode::Adding(String::new());
    }

    pub fn cancel_input(&mut self) {
        self.mode = PathListMode::Normal;
        self.error_message = None;
    }

    pub fn prompt_remove(&mut self, index: usize) {
        if index < self.items.len() {
            self.mode = PathListMode::ConfirmRemove(index);
        }
    }

    pub fn confirm_remove(&mut self, ignore: &mut IgnoreStore) {
        if let PathListMode::ConfirmRemove(idx) = self.mode
            && idx < self.items.len()
        {
            let removed_path = self.items.remove(idx);
            match self.kind {
                PathListKind::ProtectedPaths => {
                    ignore.remove(&removed_path);
                    let _ = ignore.save_to(&self.ignore_path);
                }
                PathListKind::ProjectFolders => {
                    let mut store = ProjectFoldersStore::load_or_default_at(&self.projects_path);
                    store.remove(&removed_path);
                    let _ = store.save_to(&self.projects_path);
                }
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
            match self.kind {
                PathListKind::ProtectedPaths => {
                    ignore.add(path_str);
                    let _ = ignore.save_to(&self.ignore_path);
                }
                PathListKind::ProjectFolders => {
                    let mut store = ProjectFoldersStore::load_or_default_at(&self.projects_path);
                    store.add(path_str);
                    let _ = store.save_to(&self.projects_path);
                }
            }

            self.mode = PathListMode::Normal;
            self.error_message = None;
        }
    }

    /// Handles a key while a dialog is open. Everything typed goes into the path.
    pub fn dialog_key(&mut self, code: KeyCode, ignore: &mut IgnoreStore) {
        match &mut self.mode {
            PathListMode::Adding(input) => match code {
                KeyCode::Esc => self.cancel_input(),
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(c) => input.push(c),
                KeyCode::Enter => self.commit_add(ignore),
                _ => {}
            },
            PathListMode::ConfirmRemove(_) => match code {
                KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.confirm_remove(ignore);
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => self.cancel_input(),
                _ => {}
            },
            PathListMode::Normal => {}
        }
    }

    pub fn render_dialog(&self, frame: &mut Frame, theme: &Theme) {
        match &self.mode {
            PathListMode::Adding(input) => {
                render_input_dialog(
                    frame,
                    self.kind,
                    input,
                    self.error_message.as_deref(),
                    theme,
                );
            }
            PathListMode::ConfirmRemove(idx) => {
                if let Some(target) = self.items.get(*idx) {
                    render_remove_dialog(frame, target, theme);
                }
            }
            PathListMode::Normal => {}
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::apps::testing::sandbox;

    fn typed(list: &mut PathList, path: &str) {
        list.start_adding();
        list.mode = PathListMode::Adding(path.to_string());
    }

    #[test]
    fn protected_paths_are_saved_in_the_layout() {
        let (temp, layout) = sandbox();
        let mut ignore = IgnoreStore::default();
        let mut list = PathList::new(PathListKind::ProtectedPaths, &ignore, &layout);
        let path = temp.path().canonicalize().unwrap().display().to_string();

        typed(&mut list, &path);
        list.commit_add(&mut ignore);

        let saved = IgnoreStore::load_from(&layout.ignore_path()).unwrap();
        assert_eq!(saved.roots, [path]);

        list.prompt_remove(0);
        list.confirm_remove(&mut ignore);

        assert!(
            IgnoreStore::load_from(&layout.ignore_path())
                .unwrap()
                .roots
                .is_empty()
        );
    }

    #[test]
    fn project_folders_are_read_from_and_saved_in_the_layout() {
        let (temp, layout) = sandbox();
        let first = temp.path().canonicalize().unwrap().display().to_string();
        ProjectFoldersStore::new(vec![first.clone()])
            .save_to(&layout.projects_path())
            .unwrap();
        let mut ignore = IgnoreStore::default();
        let mut list = PathList::new(PathListKind::ProjectFolders, &ignore, &layout);
        assert_eq!(list.items, std::slice::from_ref(&first));

        let second = layout.home.display().to_string();
        typed(&mut list, &second);
        list.commit_add(&mut ignore);

        let saved = ProjectFoldersStore::load_from(&layout.projects_path()).unwrap();
        assert_eq!(saved.roots, [first, second]);
        assert!(!layout.ignore_path().exists());
    }

    #[test]
    fn a_row_that_is_not_an_item_asks_nothing() {
        let (_temp, layout) = sandbox();
        let mut list = PathList::new(
            PathListKind::ProtectedPaths,
            &IgnoreStore::default(),
            &layout,
        );

        list.prompt_remove(0);

        assert!(!list.has_dialog());
    }
}

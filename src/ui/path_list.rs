//! The lists behind Project folders, Protected paths, and the custom folder names: their items,
//! adding, removing, saving, and the two dialogs. Settings shows the items and passes the keys of the dialogs on.

use std::path::PathBuf;

use crossterm::event::KeyCode;
use ratatui::Frame;
use ratatui::text::{Line, Span};

use super::components::button::Button;
use super::components::modal::Modal;
use super::theme::Theme;
use crate::core::layout::Layout as MacLayout;
use crate::core::path_utils::{complete_path, prettify_path, standardize_path};
use crate::core::projects_store::ProjectFoldersStore;
use crate::core::purger::Purger;
use crate::core::stores::{Config, IgnoreStore};

/// What the typed path is on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathStatus {
    Folder,
    File,
    Missing,
    FileForProject,
}

impl PathStatus {
    fn text(self) -> &'static str {
        match self {
            Self::Folder => "Folder found",
            Self::File => "File found",
            Self::Missing => "Nothing here yet. You can still add it.",
            Self::FileForProject => "This is a file. Project cleanup needs a folder.",
        }
    }
}
const MATCHES_SHOWN: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathListKind {
    ProtectedPaths,
    ProjectFolders,
    /// Folder names, not paths: no completion and no status line.
    ArtifactNames,
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
    matches: Vec<String>,
    /// Read when the input changes, so a slow drive never holds up drawing.
    status: Option<PathStatus>,
    home: PathBuf,
    ignore_path: PathBuf,
    projects_path: PathBuf,
    config_path: PathBuf,
}

impl PathList {
    pub fn new(
        kind: PathListKind,
        ignore: &IgnoreStore,
        config: &Config,
        layout: &MacLayout,
    ) -> Self {
        let projects_path = layout.projects_path();
        let items = match kind {
            PathListKind::ProtectedPaths => ignore.roots.clone(),
            PathListKind::ProjectFolders => {
                ProjectFoldersStore::load_or_default_at(&projects_path).roots
            }
            PathListKind::ArtifactNames => config.extra_artifacts.clone(),
        };

        Self {
            kind,
            items,
            mode: PathListMode::Normal,
            error_message: None,
            matches: Vec::new(),
            status: None,
            home: layout.home.clone(),
            ignore_path: layout.ignore_path(),
            projects_path,
            config_path: layout.config_path(),
        }
    }

    pub fn add_label(&self) -> &'static str {
        match self.kind {
            PathListKind::ProtectedPaths => "Add path…",
            PathListKind::ProjectFolders => "Add folder…",
            PathListKind::ArtifactNames => "Add name…",
        }
    }

    /// The subheading above a list that shares its section with other rows.
    pub fn heading(&self) -> Option<&'static str> {
        match self.kind {
            PathListKind::ProtectedPaths => None,
            PathListKind::ProjectFolders => Some("Project folders"),
            PathListKind::ArtifactNames => Some("Also clean folders named"),
        }
    }

    pub fn footnote(&self) -> &'static str {
        match self.kind {
            PathListKind::ProtectedPaths => "Cleanup and project scans skip these paths.",
            PathListKind::ProjectFolders => "Project cleanup scans these folders.",
            PathListKind::ArtifactNames => {
                "Project cleanup lists folders with these names. You select them yourself."
            }
        }
    }

    pub fn has_dialog(&self) -> bool {
        !matches!(self.mode, PathListMode::Normal)
    }

    pub fn is_adding(&self) -> bool {
        matches!(self.mode, PathListMode::Adding(_))
    }

    /// One line about the typed path: whether it exists, and what it is.
    pub fn status(&self) -> Option<&'static str> {
        self.status.map(PathStatus::text)
    }

    fn refresh_status(&mut self) {
        self.status = match &self.mode {
            PathListMode::Adding(input)
                if self.kind != PathListKind::ArtifactNames && !input.trim().is_empty() =>
            {
                Some(match standardize_path(input, &self.home).metadata() {
                    Err(_) => PathStatus::Missing,
                    Ok(meta) if meta.is_dir() => PathStatus::Folder,
                    Ok(_) if self.kind == PathListKind::ProjectFolders => {
                        PathStatus::FileForProject
                    }
                    Ok(_) => PathStatus::File,
                })
            }
            _ => None,
        };
    }

    pub fn start_adding(&mut self) {
        self.error_message = None;
        self.mode = PathListMode::Adding(String::new());
        self.refresh_status();
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

    pub fn confirm_remove(&mut self, ignore: &mut IgnoreStore, config: &mut Config) {
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
                PathListKind::ArtifactNames => {
                    config.extra_artifacts.retain(|name| *name != removed_path);
                    let _ = config.save_to(&self.config_path);
                }
            }
        }
        self.mode = PathListMode::Normal;
    }

    pub fn commit_add(&mut self, ignore: &mut IgnoreStore, config: &mut Config) {
        if let PathListMode::Adding(ref input) = self.mode {
            let trimmed = input.trim();
            if self.kind == PathListKind::ArtifactNames {
                let name = trimmed.to_string();
                self.commit_name(name, ignore, config);
                return;
            }
            if trimmed.is_empty() {
                self.error_message = Some("Path cannot be empty".to_string());
                return;
            }

            if self.status == Some(PathStatus::FileForProject) {
                return;
            }

            let canonical = standardize_path(trimmed, &self.home);
            let path_str = canonical.display().to_string();

            if self.items.contains(&path_str) {
                self.error_message = Some("Path already exists in list".to_string());
                return;
            }

            self.add_item(path_str, ignore, config);
        }
    }

    /// Adds a checked item, saves the list, and closes the dialog.
    fn add_item(&mut self, item: String, ignore: &mut IgnoreStore, config: &mut Config) {
        self.items.push(item.clone());
        match self.kind {
            PathListKind::ProtectedPaths => {
                ignore.add(item);
                let _ = ignore.save_to(&self.ignore_path);
            }
            PathListKind::ProjectFolders => {
                let mut store = ProjectFoldersStore::load_or_default_at(&self.projects_path);
                store.add(item);
                let _ = store.save_to(&self.projects_path);
            }
            PathListKind::ArtifactNames => {
                config.extra_artifacts.push(item);
                let _ = config.save_to(&self.config_path);
            }
        }
        self.mode = PathListMode::Normal;
        self.error_message = None;
    }

    fn commit_name(&mut self, name: String, ignore: &mut IgnoreStore, config: &mut Config) {
        let refusal = if matches!(name.as_str(), "" | "." | "..") {
            Some("Type a folder name.".to_string())
        } else if name.contains('/') {
            Some("Type a name without /.".to_string())
        } else if Purger::is_standard_name(&name) {
            Some(format!("taert already cleans {name}."))
        } else if self.items.contains(&name) {
            Some("This name is already in the list.".to_string())
        } else {
            None
        };
        if refusal.is_some() {
            self.error_message = refusal;
            return;
        }
        self.add_item(name, ignore, config);
    }

    /// Handles a key while a dialog is open. What you type goes into the input, and Tab
    /// completes a path.
    pub fn dialog_key(&mut self, code: KeyCode, ignore: &mut IgnoreStore, config: &mut Config) {
        if code != KeyCode::Tab {
            self.matches.clear();
        }
        match &mut self.mode {
            PathListMode::Adding(input) => match code {
                KeyCode::Esc => self.cancel_input(),
                KeyCode::Backspace => {
                    input.pop();
                    self.refresh_status();
                }
                KeyCode::Char(c) => {
                    input.push(c);
                    self.refresh_status();
                }
                KeyCode::Tab if self.kind == PathListKind::ArtifactNames => {}
                KeyCode::Tab => {
                    let folders_only = self.kind == PathListKind::ProjectFolders;
                    let done = complete_path(input, &self.home, folders_only);
                    *input = done.text;
                    self.matches = done.matches;
                    self.refresh_status();
                }
                KeyCode::Enter => self.commit_add(ignore, config),
                _ => {}
            },
            PathListMode::ConfirmRemove(_) => match code {
                KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.confirm_remove(ignore, config);
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
                render_input_dialog(frame, self, input, theme);
            }
            PathListMode::ConfirmRemove(idx) => {
                if let Some(target) = self.items.get(*idx) {
                    render_remove_dialog(frame, self.kind, target, theme);
                }
            }
            PathListMode::Normal => {}
        }
    }
}

fn render_input_dialog(frame: &mut Frame, list: &PathList, input: &str, theme: &Theme) {
    let title = match list.kind {
        PathListKind::ProtectedPaths => "Add Protected Path",
        PathListKind::ProjectFolders => "Add Project Folder",
        PathListKind::ArtifactNames => "Add Folder Name",
    };
    let label = match list.kind {
        PathListKind::ArtifactNames => "Name:  ",
        _ => "Path:  ",
    };
    let cursor_char = "▏";
    let mut msg = vec![
        Line::from(vec![
            Span::styled(label, theme.headline()),
            Span::styled(input, theme.body()),
            Span::styled(cursor_char, theme.accent_text()),
        ]),
        Line::from(Span::styled("─".repeat(34), theme.separator())),
    ];

    if let Some(status) = list.status {
        let style = if status == PathStatus::FileForProject {
            theme.body().fg(theme.p.red)
        } else {
            theme.footnote()
        };
        msg.push(Line::from(Span::styled(status.text(), style)));
    }
    for name in list.matches.iter().take(MATCHES_SHOWN) {
        msg.push(Line::from(Span::styled(name.as_str(), theme.body())));
    }
    if list.matches.len() > MATCHES_SHOWN {
        let more = format!("+{} more", list.matches.len() - MATCHES_SHOWN);
        msg.push(Line::from(Span::styled(more, theme.footnote())));
    }
    if let Some(e) = list.error_message.as_deref() {
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

fn render_remove_dialog(frame: &mut Frame, kind: PathListKind, item: &str, theme: &Theme) {
    let (pretty, message) = match kind {
        PathListKind::ArtifactNames => (
            item.to_string(),
            format!("Stop listing folders named {item}?"),
        ),
        _ => {
            let pretty = prettify_path(&PathBuf::from(item));
            let message = format!("Stop tracking {pretty}?");
            (pretty, message)
        }
    };
    let title = format!("Remove {pretty}?");

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
    use std::fs;

    use super::*;
    use crate::core::apps::testing::sandbox;

    fn typed(list: &mut PathList, path: &str) {
        list.start_adding();
        list.mode = PathListMode::Adding(path.to_string());
        list.refresh_status();
    }

    #[test]
    fn the_status_is_checked_when_the_input_changes_not_on_each_draw() {
        let (temp, layout) = sandbox();
        let mut ignore = IgnoreStore::default();
        let mut list = PathList::new(
            PathListKind::ProtectedPaths,
            &ignore,
            &Config::default(),
            &layout,
        );
        list.start_adding();
        for c in "~/later".chars() {
            list.dialog_key(KeyCode::Char(c), &mut ignore, &mut Config::default());
        }
        assert_eq!(
            list.status(),
            Some("Nothing here yet. You can still add it.")
        );

        std::fs::create_dir(layout.home.join("later")).unwrap();
        assert_eq!(
            list.status(),
            Some("Nothing here yet. You can still add it.")
        );

        list.dialog_key(KeyCode::Backspace, &mut ignore, &mut Config::default());
        list.dialog_key(KeyCode::Char('r'), &mut ignore, &mut Config::default());
        assert_eq!(list.status(), Some("Folder found"));
        drop(temp);
    }

    #[test]
    fn protected_paths_are_saved_in_the_layout() {
        let (temp, layout) = sandbox();
        let mut ignore = IgnoreStore::default();
        let mut list = PathList::new(
            PathListKind::ProtectedPaths,
            &ignore,
            &Config::default(),
            &layout,
        );
        let path = temp.path().canonicalize().unwrap().display().to_string();

        typed(&mut list, &path);
        list.commit_add(&mut ignore, &mut Config::default());

        let saved = IgnoreStore::load_from(&layout.ignore_path()).unwrap();
        assert_eq!(saved.roots, [path]);

        list.prompt_remove(0);
        list.confirm_remove(&mut ignore, &mut Config::default());

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
        let mut list = PathList::new(
            PathListKind::ProjectFolders,
            &ignore,
            &Config::default(),
            &layout,
        );
        assert_eq!(list.items, std::slice::from_ref(&first));

        let second = layout.home.display().to_string();
        typed(&mut list, &second);
        list.commit_add(&mut ignore, &mut Config::default());

        let saved = ProjectFoldersStore::load_from(&layout.projects_path()).unwrap();
        assert_eq!(saved.roots, [first, second]);
        assert!(!layout.ignore_path().exists());
    }

    fn input(list: &PathList) -> &str {
        match &list.mode {
            PathListMode::Adding(input) => input,
            _ => panic!("the dialog is not open"),
        }
    }

    fn list_in(layout: &MacLayout, kind: PathListKind) -> (PathList, IgnoreStore) {
        let ignore = IgnoreStore::default();
        (
            PathList::new(kind, &ignore, &Config::default(), layout),
            ignore,
        )
    }

    #[test]
    fn tab_completes_the_input_with_the_home_of_the_layout() {
        let (_temp, layout) = sandbox();
        fs::create_dir_all(layout.home.join("Code/alpha")).unwrap();
        fs::create_dir_all(layout.home.join("Code/alps")).unwrap();
        let (mut list, mut ignore) = list_in(&layout, PathListKind::ProjectFolders);

        typed(&mut list, "~/Co");
        list.dialog_key(KeyCode::Tab, &mut ignore, &mut Config::default());
        assert_eq!(input(&list), "~/Code/");
        assert!(list.matches.is_empty());

        list.dialog_key(KeyCode::Char('a'), &mut ignore, &mut Config::default());
        list.dialog_key(KeyCode::Char('l'), &mut ignore, &mut Config::default());
        list.dialog_key(KeyCode::Tab, &mut ignore, &mut Config::default());
        assert_eq!(input(&list), "~/Code/alp");
        assert_eq!(list.matches, ["alpha", "alps"]);

        list.dialog_key(KeyCode::Char('h'), &mut ignore, &mut Config::default());
        assert!(list.matches.is_empty());
    }

    #[test]
    fn protected_paths_complete_files_and_project_folders_do_not() {
        let (_temp, layout) = sandbox();
        fs::write(layout.home.join("notes.txt"), "").unwrap();

        let (mut protected, mut ignore) = list_in(&layout, PathListKind::ProtectedPaths);
        typed(&mut protected, "~/no");
        protected.dialog_key(KeyCode::Tab, &mut ignore, &mut Config::default());
        assert_eq!(input(&protected), "~/notes.txt");

        let (mut projects, mut ignore) = list_in(&layout, PathListKind::ProjectFolders);
        typed(&mut projects, "~/no");
        projects.dialog_key(KeyCode::Tab, &mut ignore, &mut Config::default());
        assert_eq!(input(&projects), "~/no");
    }

    #[test]
    fn the_status_line_follows_the_typing() {
        let (_temp, layout) = sandbox();
        fs::create_dir_all(layout.home.join("Code")).unwrap();
        fs::write(layout.home.join("notes.txt"), "").unwrap();
        let (mut protected, _) = list_in(&layout, PathListKind::ProtectedPaths);
        let (mut projects, _) = list_in(&layout, PathListKind::ProjectFolders);

        typed(&mut protected, "");
        assert_eq!(protected.status(), None);
        typed(&mut protected, "~/Code");
        assert_eq!(protected.status(), Some("Folder found"));
        typed(&mut protected, "~/notes.txt");
        assert_eq!(protected.status(), Some("File found"));
        typed(&mut protected, "~/new");
        assert_eq!(
            protected.status(),
            Some("Nothing here yet. You can still add it.")
        );
        typed(&mut projects, "~/notes.txt");
        assert_eq!(
            projects.status(),
            Some("This is a file. Project cleanup needs a folder.")
        );
    }

    #[test]
    fn a_file_is_refused_as_a_project_folder_and_added_as_a_protected_path() {
        let (_temp, layout) = sandbox();
        let file = layout.home.join("notes.txt");
        fs::write(&file, "").unwrap();
        let path = file.canonicalize().unwrap().display().to_string();

        let (mut projects, mut ignore) = list_in(&layout, PathListKind::ProjectFolders);
        let before = projects.items.clone();
        typed(&mut projects, "~/notes.txt");
        projects.dialog_key(KeyCode::Enter, &mut ignore, &mut Config::default());
        assert!(projects.is_adding());
        assert_eq!(projects.items, before);
        assert_eq!(
            projects.status(),
            Some("This is a file. Project cleanup needs a folder.")
        );

        typed(&mut projects, "~/notes.txt/");
        projects.dialog_key(KeyCode::Enter, &mut ignore, &mut Config::default());
        assert!(projects.is_adding());
        assert_eq!(projects.items, before);
        assert_eq!(
            projects.status(),
            Some("This is a file. Project cleanup needs a folder.")
        );

        let (mut protected, mut ignore) = list_in(&layout, PathListKind::ProtectedPaths);
        typed(&mut protected, "~/notes.txt");
        protected.dialog_key(KeyCode::Enter, &mut ignore, &mut Config::default());
        assert_eq!(protected.items, [path]);
    }

    #[test]
    fn a_missing_path_can_still_be_added() {
        let (_temp, layout) = sandbox();
        let (mut list, mut ignore) = list_in(&layout, PathListKind::ProjectFolders);

        typed(&mut list, "~/later");
        assert_eq!(
            list.status(),
            Some("Nothing here yet. You can still add it.")
        );
        list.dialog_key(KeyCode::Enter, &mut ignore, &mut Config::default());

        let expected = layout.home.join("later").display().to_string();
        assert_eq!(list.items.last(), Some(&expected));
        assert!(!list.is_adding());
    }

    #[test]
    fn a_row_that_is_not_an_item_asks_nothing() {
        let (_temp, layout) = sandbox();
        let mut list = PathList::new(
            PathListKind::ProtectedPaths,
            &IgnoreStore::default(),
            &Config::default(),
            &layout,
        );

        list.prompt_remove(0);

        assert!(!list.has_dialog());
    }

    fn names_in(layout: &MacLayout) -> (PathList, IgnoreStore, Config) {
        let ignore = IgnoreStore::default();
        let config = Config::default();
        let list = PathList::new(PathListKind::ArtifactNames, &ignore, &config, layout);
        (list, ignore, config)
    }

    fn add_name(list: &mut PathList, ignore: &mut IgnoreStore, config: &mut Config, name: &str) {
        list.start_adding();
        for c in name.chars() {
            list.dialog_key(KeyCode::Char(c), ignore, config);
        }
        list.dialog_key(KeyCode::Enter, ignore, config);
    }

    #[test]
    fn a_custom_name_is_saved_in_the_config_of_the_layout() {
        let (_temp, layout) = sandbox();
        let (mut list, mut ignore, mut config) = names_in(&layout);

        add_name(&mut list, &mut ignore, &mut config, "bazel-out");

        assert_eq!(list.items, ["bazel-out"]);
        assert_eq!(config.extra_artifacts, ["bazel-out"]);
        let saved = Config::load_from(&layout.config_path()).unwrap();
        assert_eq!(saved.extra_artifacts, ["bazel-out"]);
        assert!(!layout.ignore_path().exists());
        assert!(!layout.projects_path().exists());
    }

    #[test]
    fn the_names_come_from_the_config_and_removing_one_saves() {
        let (_temp, layout) = sandbox();
        let config = Config {
            extra_artifacts: vec!["out".into(), "bazel-out".into()],
            ..Config::default()
        };
        let mut ignore = IgnoreStore::default();
        let mut list = PathList::new(PathListKind::ArtifactNames, &ignore, &config, &layout);
        let mut config = config;
        assert_eq!(list.items, ["out", "bazel-out"]);

        list.prompt_remove(0);
        list.confirm_remove(&mut ignore, &mut config);

        assert_eq!(list.items, ["bazel-out"]);
        let saved = Config::load_from(&layout.config_path()).unwrap();
        assert_eq!(saved.extra_artifacts, ["bazel-out"]);
    }

    #[test]
    fn a_name_that_cannot_be_a_folder_name_is_refused_with_a_message() {
        let (_temp, layout) = sandbox();
        let refused = [
            ("", "Type a folder name."),
            ("   ", "Type a folder name."),
            (".", "Type a folder name."),
            ("..", "Type a folder name."),
            ("a/b", "Type a name without /."),
            ("node_modules", "taert already cleans node_modules."),
            (
                "cmake-build-debug",
                "taert already cleans cmake-build-debug.",
            ),
            ("out", "This name is already in the list."),
        ];
        for (name, message) in refused {
            let (mut list, mut ignore, mut config) = names_in(&layout);
            add_name(&mut list, &mut ignore, &mut config, "out");
            add_name(&mut list, &mut ignore, &mut config, name);

            assert!(list.is_adding(), "{name:?} closed the dialog");
            assert_eq!(list.error_message.as_deref(), Some(message), "{name:?}");
            assert_eq!(config.extra_artifacts, ["out"], "{name:?}");
        }
    }

    #[test]
    fn a_name_is_trimmed_and_the_error_clears_when_the_dialog_closes() {
        let (_temp, layout) = sandbox();
        let (mut list, mut ignore, mut config) = names_in(&layout);

        add_name(&mut list, &mut ignore, &mut config, " out ");
        assert_eq!(config.extra_artifacts, ["out"]);

        add_name(&mut list, &mut ignore, &mut config, "..");
        assert!(list.error_message.is_some());
        list.dialog_key(KeyCode::Esc, &mut ignore, &mut config);
        assert!(list.error_message.is_none());
    }

    #[test]
    fn a_name_has_no_path_status_and_tab_does_not_complete_it() {
        let (_temp, layout) = sandbox();
        fs::create_dir_all(layout.home.join("Code")).unwrap();
        let (mut list, mut ignore, mut config) = names_in(&layout);

        list.start_adding();
        for c in "~/Co".chars() {
            list.dialog_key(KeyCode::Char(c), &mut ignore, &mut config);
        }
        list.dialog_key(KeyCode::Tab, &mut ignore, &mut config);

        assert_eq!(input(&list), "~/Co");
        assert!(list.matches.is_empty());
        assert_eq!(list.status(), None);
    }
}

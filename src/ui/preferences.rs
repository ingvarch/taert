//! Interactive Preferences screen.
//! Configures artifact age threshold presets and shortcuts to project folders and protected paths.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use super::theme::Theme;
use crate::core::projects_store::ProjectFoldersStore;
use crate::core::stores::{Config, IgnoreStore};

pub const AGE_PRESETS: &[u64] = &[1, 3, 7, 14, 30, 60, 90, 180, 365];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreferencesFocus {
    AgeThreshold,
    ProjectFolders,
    ProtectedPaths,
}

pub struct PreferencesState {
    pub focus: PreferencesFocus,
    pub older_than_days: u64,
    pub folder_count: usize,
    pub path_count: usize,
    pub save_notice: Option<&'static str>,
}

impl PreferencesState {
    pub fn new(config: &Config, ignore: &IgnoreStore) -> Self {
        Self {
            focus: PreferencesFocus::AgeThreshold,
            older_than_days: config.older_than_days,
            folder_count: ProjectFoldersStore::load_or_default().roots.len(),
            path_count: ignore.roots.len(),
            save_notice: None,
        }
    }

    pub fn next(&mut self) {
        self.focus = match self.focus {
            PreferencesFocus::AgeThreshold => PreferencesFocus::ProjectFolders,
            PreferencesFocus::ProjectFolders => PreferencesFocus::ProtectedPaths,
            PreferencesFocus::ProtectedPaths => PreferencesFocus::AgeThreshold,
        };
    }

    pub fn previous(&mut self) {
        self.focus = match self.focus {
            PreferencesFocus::AgeThreshold => PreferencesFocus::ProtectedPaths,
            PreferencesFocus::ProjectFolders => PreferencesFocus::AgeThreshold,
            PreferencesFocus::ProtectedPaths => PreferencesFocus::ProjectFolders,
        };
    }

    pub fn step_age_left(&mut self, config: &mut Config) {
        if self.focus == PreferencesFocus::AgeThreshold
            && let Some(&prev) = AGE_PRESETS
                .iter()
                .rev()
                .find(|&&d| d < self.older_than_days)
        {
            self.older_than_days = prev;
            config.older_than_days = prev;
            let _ = config.save_to(&Config::default_path().unwrap());
            self.save_notice = Some("Saved age threshold");
        }
    }

    pub fn step_age_right(&mut self, config: &mut Config) {
        if self.focus == PreferencesFocus::AgeThreshold
            && let Some(&next) = AGE_PRESETS.iter().find(|&&d| d > self.older_than_days)
        {
            self.older_than_days = next;
            config.older_than_days = next;
            let _ = config.save_to(&Config::default_path().unwrap());
            self.save_notice = Some("Saved age threshold");
        }
    }
}

pub fn render_preferences(frame: &mut Frame, state: &PreferencesState, theme: &Theme) {
    let area = frame.area();

    let window_block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(theme.border)
        .title(Line::from(vec![Span::styled(" Preferences ", theme.title)]))
        .title_alignment(Alignment::Left);

    let inner_area = window_block.inner(area);
    frame.render_widget(window_block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // Subtitle
            Constraint::Min(6),    // List of settings
            Constraint::Length(1), // Notice or status
            Constraint::Length(1), // Divider
            Constraint::Length(1), // Footer keyhints
        ])
        .split(inner_area);

    let subtitle = Line::from(vec![Span::styled(
        "Change cleanup age, project folders, and protected paths",
        theme.muted,
    )]);
    frame.render_widget(Paragraph::new(subtitle), chunks[0]);

    // Preferences Items
    let mut rows = Vec::new();

    // 1. Preselect artifacts older than
    let age_focused = state.focus == PreferencesFocus::AgeThreshold;
    let age_cursor = if age_focused { "› " } else { "  " };
    let age_val = format!("‹ {} days ›", state.older_than_days);
    rows.push(Line::from(vec![
        Span::styled(
            age_cursor,
            if age_focused {
                theme.cursor
            } else {
                theme.muted
            },
        ),
        Span::styled(
            "Preselect artifacts older than    ",
            if age_focused {
                theme.focused_row
            } else {
                theme.normal
            },
        ),
        Span::styled(format!("{:<14}", age_val), theme.accent),
    ]));
    rows.push(Line::from(""));

    // 2. Project folders
    let folders_focused = state.focus == PreferencesFocus::ProjectFolders;
    let folders_cursor = if folders_focused { "› " } else { "  " };
    rows.push(Line::from(vec![
        Span::styled(
            folders_cursor,
            if folders_focused {
                theme.cursor
            } else {
                theme.muted
            },
        ),
        Span::styled(
            "Project folders                   ",
            if folders_focused {
                theme.focused_row
            } else {
                theme.normal
            },
        ),
        Span::styled(format!("{} folder(s) ›", state.folder_count), theme.muted),
    ]));
    rows.push(Line::from(""));

    // 3. Protected paths
    let paths_focused = state.focus == PreferencesFocus::ProtectedPaths;
    let paths_cursor = if paths_focused { "› " } else { "  " };
    rows.push(Line::from(vec![
        Span::styled(
            paths_cursor,
            if paths_focused {
                theme.cursor
            } else {
                theme.muted
            },
        ),
        Span::styled(
            "Protected paths                   ",
            if paths_focused {
                theme.focused_row
            } else {
                theme.normal
            },
        ),
        Span::styled(format!("{} path(s) ›", state.path_count), theme.muted),
    ]));

    frame.render_widget(Paragraph::new(rows), chunks[1]);

    if let Some(msg) = state.save_notice {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(format!("✓ {msg}"), theme.success))),
            chunks[2],
        );
    }

    let divider = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(theme.border);
    frame.render_widget(divider, chunks[3]);

    let footer_text = Line::from(vec![
        Span::styled("↑↓ ", theme.footer_key),
        Span::styled("move  ", theme.footer_desc),
        Span::styled("←→ ", theme.footer_key),
        Span::styled("change age  ", theme.footer_desc),
        Span::styled("Enter ", theme.footer_key),
        Span::styled("open list  ", theme.footer_desc),
        Span::styled("Esc/q ", theme.footer_key),
        Span::styled("back", theme.footer_desc),
    ]);
    frame.render_widget(Paragraph::new(footer_text), chunks[4]);
}

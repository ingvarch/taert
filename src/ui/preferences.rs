//! Interactive Preferences screen.
//! Configures artifact age threshold presets and shortcuts to project folders and protected paths.

use ratatui::Frame;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::components::chrome::AppChrome;
use super::components::frame::{FRAME_SIDES, card};
use super::components::hints::{Hint, PrimaryAction};
use super::components::list::ListRow;
use super::components::text::plural;
use super::components::toast::Toast;
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
    let is_link = matches!(
        state.focus,
        PreferencesFocus::ProjectFolders | PreferencesFocus::ProtectedPaths
    );

    let primary_action = if is_link {
        PrimaryAction::new("Open", theme.g.key_enter)
    } else {
        PrimaryAction::disabled("Open", theme.g.key_enter)
    };

    let toast = state.save_notice.map(Toast::success);

    let chrome = AppChrome::new(vec!["taert", "Preferences"])
        .hints(vec![
            Hint::new("↑↓", "Move"),
            Hint::new("←→", "Adjust"),
            Hint::new("?", "More"),
        ])
        .primary(primary_action)
        .toast(toast.as_ref());

    let content_rect = chrome.render(frame, theme);
    let frame_w = content_rect.width as usize;
    let width = frame_w.saturating_sub(FRAME_SIDES);

    let mut cleanup = Vec::new();

    let age_focused = state.focus == PreferencesFocus::AgeThreshold;
    let is_first = state.older_than_days <= *AGE_PRESETS.first().unwrap();
    let is_last = state.older_than_days >= *AGE_PRESETS.last().unwrap();

    let left_arr = if is_first { " " } else { "‹" };
    let right_arr = if is_last { " " } else { "›" };
    let age_str = format!("{left_arr}  {} days  {right_arr}", state.older_than_days);

    let age_row = ListRow::new(vec![Span::styled(
        "Preselect artifacts older than",
        theme.body(),
    )])
    .detail(Span::styled(
        age_str,
        if age_focused {
            theme.accent_text()
        } else {
            theme.callout()
        },
    ))
    .selected(age_focused);

    cleanup.push(age_row.render_line(width, theme));

    // Preset slider track if age is focused and regular/wide width
    if age_focused && width >= 60 {
        cleanup.push(render_presets_slider(state.older_than_days, width, theme));
        cleanup.push(render_presets_labels(width, theme));
    }

    let mut locations = Vec::new();

    let folders_focused = state.focus == PreferencesFocus::ProjectFolders;
    let folders_row = ListRow::new(vec![Span::styled("Project folders", theme.body())])
        .detail(Span::styled(
            plural(state.folder_count, "folder", "folders"),
            theme.callout(),
        ))
        .chevron(true)
        .selected(folders_focused);
    locations.push(folders_row.render_line(width, theme));

    let paths_focused = state.focus == PreferencesFocus::ProtectedPaths;
    let paths_row = ListRow::new(vec![Span::styled("Protected paths", theme.body())])
        .detail(Span::styled(
            plural(state.path_count, "path", "paths"),
            theme.callout(),
        ))
        .chevron(true)
        .selected(paths_focused);
    locations.push(paths_row.render_line(width, theme));

    let mut lines = card(
        "Cleanup",
        Vec::new(),
        cleanup,
        frame_w,
        0,
        theme.p.accent,
        theme,
    );
    lines.push(Line::default());
    lines.extend(card(
        "Locations",
        Vec::new(),
        locations,
        frame_w,
        0,
        theme.p.teal,
        theme,
    ));

    frame.render_widget(Paragraph::new(lines), content_rect);
}

fn render_presets_slider(current: u64, _width: usize, theme: &Theme) -> Line<'static> {
    let indent = "   ";
    let mut spans = vec![Span::raw(indent)];

    for (i, &preset) in AGE_PRESETS.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("───", theme.footnote()));
        }
        if preset == current {
            spans.push(Span::styled(theme.g.bullet, theme.accent_text()));
        } else {
            spans.push(Span::styled(theme.g.check_off, theme.footnote()));
        }
    }

    Line::from(spans)
}

fn render_presets_labels(_width: usize, theme: &Theme) -> Line<'static> {
    let indent = "   ";
    let mut spans = vec![Span::raw(indent)];

    let labels = ["1d", "3d", "7d", "14d", "30d", "60d", "90d", "180d", "1y"];
    for (i, lbl) in labels.iter().enumerate() {
        if i > 0 {
            let gap = 4usize.saturating_sub(lbl.len());
            spans.push(Span::raw(" ".repeat(gap.max(1))));
        }
        spans.push(Span::styled(*lbl, theme.footnote()));
    }

    Line::from(spans)
}

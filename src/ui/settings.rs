//! Settings screen: sections on the left, the rows of the highlighted section on the right.

use std::path::Path;

use crossterm::event::KeyCode;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::components::chrome::{AppChrome, SPLIT_MIN_WIDTH, split_panes};
use super::components::frame::{FRAME_ROWS, FRAME_SIDES, card};
use super::components::hints::{Hint, PrimaryAction};
use super::components::list::{ListRow, path_spans};
use super::components::picker::{PickerState, render_picker};
use super::components::text::{plural, wrap_line};
use super::components::toast::Toast;
use super::env::{Appearance, ColorDepth};
use super::path_list::{PathList, PathListKind};
use super::theme::Theme;
use super::workspace::CleanSubcategory;
use crate::core::bytes::format_bytes;
use crate::core::catalog::Category;
use crate::core::history::HistoryLog;
use crate::core::host::Host;
use crate::core::layout::Layout;
use crate::core::schedule::{
    LastRun, ScheduleFrequency, ScheduleStatus, format_reclaim_threshold, format_run_time,
    normalize_categories, remove_schedule, schedule_status, set_schedule,
};
use crate::core::stores::{
    Config, IgnoreStore, ScheduleConfig, default_schedule_categories, default_schedule_min_reclaim,
};

pub const AGE_PRESETS: &[u64] = &[1, 3, 7, 14, 30, 60, 90, 180, 365];

const SCHEDULE_CHOICES: [Option<ScheduleFrequency>; 4] = [
    None,
    Some(ScheduleFrequency::Day),
    Some(ScheduleFrequency::Week),
    Some(ScheduleFrequency::Month),
];

/// Presets of the minimum to free, in decimal units like `parse_size`.
const RECLAIM_PRESETS: [u64; 6] = [
    100_000_000,
    500_000_000,
    1_000_000_000,
    2_000_000_000,
    5_000_000_000,
    10_000_000_000,
];

/// The categories a schedule can clean, in the order of the rows.
const CLEAN_CATEGORIES: [(Category, CleanSubcategory); 4] = [
    (Category::Dev, CleanSubcategory::Developer),
    (Category::Junk, CleanSubcategory::UserJunk),
    (Category::Ai, CleanSubcategory::AiTools),
    (Category::Trash, CleanSubcategory::Trash),
];

/// The values `appearance` can take in the config, in the order of the picker.
const APPEARANCE_CHOICES: [&str; 3] = ["auto", "dark", "light"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsSection {
    ProjectCleanup,
    ScheduledCleanup,
    ProtectedPaths,
    Appearance,
}

impl SettingsSection {
    pub const ALL: [Self; 4] = [
        Self::ProjectCleanup,
        Self::ScheduledCleanup,
        Self::ProtectedPaths,
        Self::Appearance,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::ProjectCleanup => "Project cleanup",
            Self::ScheduledCleanup => "Scheduled cleanup",
            Self::ProtectedPaths => "Protected paths",
            Self::Appearance => "Appearance",
        }
    }

    fn list_kind(self) -> Option<PathListKind> {
        match self {
            Self::ProjectCleanup => Some(PathListKind::ProjectFolders),
            Self::ScheduledCleanup => None,
            Self::ProtectedPaths => Some(PathListKind::ProtectedPaths),
            Self::Appearance => None,
        }
    }

    /// The rows that come before the list of the section.
    fn fixed_rows(self) -> &'static [SettingsRow] {
        match self {
            Self::ProjectCleanup => &[SettingsRow::AgeThreshold],
            Self::ScheduledCleanup => &[SettingsRow::Run],
            Self::ProtectedPaths => &[],
            Self::Appearance => &[SettingsRow::Appearance],
        }
    }

    fn icon(self, theme: &Theme) -> &'static str {
        match self {
            Self::ProjectCleanup => theme.g.icon_projects,
            Self::ScheduledCleanup => theme.g.icon_schedule,
            Self::ProtectedPaths => theme.g.icon_protected,
            Self::Appearance => theme.g.icon_appearance,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsRow {
    AgeThreshold,
    Run,
    /// The category at this place in `CLEAN_CATEGORIES`.
    Category(usize),
    MinReclaim,
    /// Loads the launch agent again.
    Reload,
    Appearance,
    /// The path at this place in the list of the section.
    Item(usize),
    Add,
}

impl SettingsRow {
    fn title(self) -> &'static str {
        match self {
            Self::AgeThreshold => "Preselect artifacts older than",
            Self::Run => "Run",
            Self::MinReclaim => "Only when it frees at least",
            Self::Reload => "Load the schedule again",
            Self::Appearance => "Appearance",
            Self::Category(_) | Self::Item(_) | Self::Add => "",
        }
    }

    /// A row that opens a picker.
    pub fn is_popup(self) -> bool {
        matches!(
            self,
            Self::AgeThreshold | Self::Run | Self::MinReclaim | Self::Appearance
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsFocus {
    Sections,
    Rows,
}

pub struct SettingsState {
    pub section: SettingsSection,
    pub focus: SettingsFocus,
    pub row: usize,
    pub older_than_days: u64,
    pub schedule: Option<ScheduleFrequency>,
    /// What the schedule cleans, or what a new schedule would clean.
    pub categories: Vec<String>,
    pub min_reclaim: u64,
    /// One of `APPEARANCE_CHOICES`. A saved value outside them counts as `auto`.
    pub appearance: &'static str,
    pub folders: PathList,
    pub protected: PathList,
    /// What the schedule does now, read when Settings opens and after each change.
    pub status: Option<ScheduleStatus>,
    pub save_notice: Option<&'static str>,
    pub error_notice: Option<String>,
    picker: Option<(SettingsRow, PickerState)>,
}

/// The presets with `current` added in order, so the saved value is always a choice.
fn with_current(presets: &[u64], current: u64) -> Vec<u64> {
    let mut choices = presets.to_vec();
    if let Err(at) = choices.binary_search(&current) {
        choices.insert(at, current);
    }
    choices
}

fn age_label(days: u64) -> String {
    plural(days as usize, "day", "days")
}

fn appearance_value(saved: &str) -> &'static str {
    let saved = saved.to_lowercase();
    APPEARANCE_CHOICES
        .into_iter()
        .find(|&choice| choice == saved)
        .unwrap_or("auto")
}

fn appearance_label(value: &str) -> String {
    let mut chars = value.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

fn schedule_label(schedule: Option<ScheduleFrequency>) -> String {
    match schedule {
        Some(freq) => format!("Every {freq}"),
        None => "Off".to_string(),
    }
}

impl SettingsState {
    pub fn new(config: &Config, ignore: &IgnoreStore, layout: &Layout) -> Self {
        Self {
            section: SettingsSection::ProjectCleanup,
            focus: SettingsFocus::Sections,
            row: 0,
            older_than_days: config.older_than_days,
            schedule: config.schedule.as_ref().map(|s| s.every),
            categories: config
                .schedule
                .as_ref()
                .map_or_else(default_schedule_categories, |s| {
                    normalize_categories(&s.categories)
                }),
            min_reclaim: config
                .schedule
                .as_ref()
                .map_or_else(default_schedule_min_reclaim, |s| s.min_reclaim),
            appearance: appearance_value(&config.appearance),
            folders: PathList::new(PathListKind::ProjectFolders, ignore, layout),
            protected: PathList::new(PathListKind::ProtectedPaths, ignore, layout),
            status: None,
            save_notice: None,
            error_notice: None,
            picker: None,
        }
    }

    /// The list of the section, if it has one.
    pub fn list(&self) -> Option<&PathList> {
        match self.section.list_kind()? {
            PathListKind::ProjectFolders => Some(&self.folders),
            PathListKind::ProtectedPaths => Some(&self.protected),
        }
    }

    fn list_mut(&mut self) -> Option<&mut PathList> {
        match self.section.list_kind()? {
            PathListKind::ProjectFolders => Some(&mut self.folders),
            PathListKind::ProtectedPaths => Some(&mut self.protected),
        }
    }

    /// The rows of the section: its fixed rows, then one per path and the `Add` row.
    pub fn rows(&self) -> Vec<SettingsRow> {
        let mut rows = self.section.fixed_rows().to_vec();
        if let Some(list) = self.list() {
            rows.extend((0..list.items.len()).map(SettingsRow::Item));
            rows.push(SettingsRow::Add);
        }
        if self.section == SettingsSection::ScheduledCleanup && self.schedule.is_some() {
            rows.extend((0..CLEAN_CATEGORIES.len()).map(SettingsRow::Category));
            rows.push(SettingsRow::MinReclaim);
            if self.needs_reload() {
                rows.push(SettingsRow::Reload);
            }
        }
        rows
    }

    /// The agent file is missing or launchd has not loaded the agent.
    fn needs_reload(&self) -> bool {
        self.status
            .as_ref()
            .is_some_and(|s| s.plist_missing || s.agent_loaded == Some(false))
    }

    pub fn current_row(&self) -> SettingsRow {
        self.rows()[self.row]
    }

    pub fn next(&mut self) {
        match self.focus {
            SettingsFocus::Sections => self.move_section(1),
            SettingsFocus::Rows => self.row = (self.row + 1) % self.rows().len(),
        }
    }

    pub fn previous(&mut self) {
        match self.focus {
            SettingsFocus::Sections => self.move_section(SettingsSection::ALL.len() - 1),
            SettingsFocus::Rows => {
                let rows = self.rows().len();
                self.row = (self.row + rows - 1) % rows;
            }
        }
    }

    /// A dialog of a list is open, so keys go to it.
    pub fn has_dialog(&self) -> bool {
        self.list().is_some_and(PathList::has_dialog)
    }

    /// Text goes into the path of the add dialog.
    pub fn is_typing(&self) -> bool {
        self.list().is_some_and(PathList::is_adding)
    }

    /// Opens the add dialog when the focus is on a row of a list.
    pub fn start_adding(&mut self) {
        if matches!(self.current_row(), SettingsRow::Item(_) | SettingsRow::Add)
            && let Some(list) = self.list_mut()
        {
            list.start_adding();
        }
    }

    /// Asks to remove the highlighted path.
    pub fn prompt_remove(&mut self) {
        if let SettingsRow::Item(index) = self.current_row()
            && let Some(list) = self.list_mut()
        {
            list.prompt_remove(index);
        }
    }

    /// Passes a key to the open dialog. The highlight follows an added path and stays in the
    /// list after a removal.
    pub fn dialog_key(&mut self, code: KeyCode, ignore: &mut IgnoreStore) {
        let first = self.section.fixed_rows().len();
        let before = self.list().map_or(0, |list| list.items.len());
        let Some(list) = self.list_mut() else {
            return;
        };
        list.dialog_key(code, ignore);
        let after = list.items.len();
        if after > before {
            self.row = first + after - 1;
        } else if after < before {
            self.row = self.row.min(first + after.saturating_sub(1));
        }
    }

    fn move_section(&mut self, step: usize) {
        let at = SettingsSection::ALL
            .iter()
            .position(|&s| s == self.section)
            .unwrap_or(0);
        self.section = SettingsSection::ALL[(at + step) % SettingsSection::ALL.len()];
        self.row = 0;
    }

    pub fn enter_rows(&mut self) {
        self.focus = SettingsFocus::Rows;
        self.row = 0;
    }

    pub fn leave_rows(&mut self) {
        self.focus = SettingsFocus::Sections;
    }

    pub fn picker_state(&self) -> Option<&PickerState> {
        self.picker.as_ref().map(|(_, picker)| picker)
    }

    pub fn picker_mut(&mut self) -> Option<&mut PickerState> {
        self.picker.as_mut().map(|(_, picker)| picker)
    }

    pub fn close_picker(&mut self) {
        self.picker = None;
    }

    /// Opens the picker of the current row. A row of a list has none.
    pub fn open_picker(&mut self) {
        let row = self.current_row();
        let (choices, current) = match row {
            SettingsRow::AgeThreshold => {
                let days = with_current(AGE_PRESETS, self.older_than_days);
                let current = days.iter().position(|&d| d == self.older_than_days);
                (days.into_iter().map(age_label).collect(), current)
            }
            SettingsRow::Run => (
                SCHEDULE_CHOICES
                    .iter()
                    .copied()
                    .map(schedule_label)
                    .collect(),
                SCHEDULE_CHOICES.iter().position(|&c| c == self.schedule),
            ),
            SettingsRow::MinReclaim => {
                let bytes = with_current(&RECLAIM_PRESETS, self.min_reclaim);
                let current = bytes.iter().position(|&b| b == self.min_reclaim);
                (
                    bytes.into_iter().map(format_reclaim_threshold).collect(),
                    current,
                )
            }
            SettingsRow::Appearance => (
                APPEARANCE_CHOICES.map(appearance_label).to_vec(),
                APPEARANCE_CHOICES
                    .iter()
                    .position(|&c| c == self.appearance),
            ),
            SettingsRow::Category(_)
            | SettingsRow::Item(_)
            | SettingsRow::Add
            | SettingsRow::Reload => return,
        };
        self.picker = Some((
            row,
            PickerState::new(row.title().to_string(), choices, current),
        ));
    }

    /// Applies the highlighted choice and closes the picker.
    pub fn pick(&mut self, config: &mut Config, host: &dyn Host, layout: &Layout) {
        let Some((row, picker)) = self.picker.take() else {
            return;
        };
        match row {
            SettingsRow::AgeThreshold => {
                if let Some(&days) =
                    with_current(AGE_PRESETS, self.older_than_days).get(picker.highlighted)
                {
                    self.set_age(days, config, layout);
                }
            }
            SettingsRow::Run => {
                if let Some(&target) = SCHEDULE_CHOICES.get(picker.highlighted) {
                    self.apply_schedule(target, config, host, layout);
                }
            }
            SettingsRow::MinReclaim => {
                if let Some(&bytes) =
                    with_current(&RECLAIM_PRESETS, self.min_reclaim).get(picker.highlighted)
                {
                    self.set_min_reclaim(bytes, config, host, layout);
                }
            }
            SettingsRow::Appearance => {
                if let Some(&value) = APPEARANCE_CHOICES.get(picker.highlighted) {
                    self.set_appearance(value, config, layout);
                }
            }
            SettingsRow::Category(_)
            | SettingsRow::Item(_)
            | SettingsRow::Add
            | SettingsRow::Reload => {}
        }
    }

    /// Reads the state of the schedule again. Asks launchd once.
    pub fn refresh_status(&mut self, config: &Config, host: &dyn Host, layout: &Layout) {
        let history = HistoryLog::new(layout.history_path());
        self.status = Some(schedule_status(host, layout, config, &history));
        self.row = self.row.min(self.rows().len() - 1);
    }

    /// Writes the saved schedule to the agent file and loads it again.
    pub fn reload_schedule(&mut self, config: &mut Config, host: &dyn Host, layout: &Layout) {
        let Some(freq) = self.schedule else {
            return;
        };
        match set_schedule(
            host,
            layout,
            config,
            freq,
            self.categories.clone(),
            self.min_reclaim,
            None,
        ) {
            Ok(()) => {
                self.error_notice = None;
                self.save_notice = Some("Loaded the schedule again");
            }
            Err(message) => {
                self.save_notice = None;
                self.error_notice = Some(message);
            }
        }
        self.refresh_status(config, host, layout);
    }

    fn set_appearance(&mut self, value: &'static str, config: &mut Config, layout: &Layout) {
        if value == self.appearance {
            return;
        }
        self.appearance = value;
        config.appearance = value.to_string();
        let _ = config.save_to(&layout.config_path());
        self.error_notice = None;
        self.save_notice = Some("Saved appearance");
    }

    fn set_age(&mut self, days: u64, config: &mut Config, layout: &Layout) {
        if days == self.older_than_days {
            return;
        }
        self.older_than_days = days;
        config.older_than_days = days;
        let _ = config.save_to(&layout.config_path());
        self.error_notice = None;
        self.save_notice = Some("Saved age threshold");
    }

    /// Checks or unchecks the highlighted category. The last checked one stays checked.
    pub fn toggle_category(&mut self, config: &mut Config, host: &dyn Host, layout: &Layout) {
        let SettingsRow::Category(index) = self.current_row() else {
            return;
        };
        let id = CLEAN_CATEGORIES[index].0.id();
        let mut categories = self.categories.clone();
        if let Some(at) = categories.iter().position(|c| c == id) {
            let checked = CLEAN_CATEGORIES
                .iter()
                .filter(|(category, _)| categories.iter().any(|c| c == category.id()))
                .count();
            if checked == 1 {
                self.save_notice = None;
                self.error_notice = Some("Choose at least one category".to_string());
                return;
            }
            categories.remove(at);
        } else {
            categories.push(id.to_string());
        }
        let categories = normalize_categories(&categories);
        self.categories = categories.clone();
        self.save_schedule(config, layout, "Saved categories", |s| {
            s.categories = categories;
        });
        self.refresh_status(config, host, layout);
    }

    fn set_min_reclaim(
        &mut self,
        bytes: u64,
        config: &mut Config,
        host: &dyn Host,
        layout: &Layout,
    ) {
        if bytes == self.min_reclaim {
            return;
        }
        self.min_reclaim = bytes;
        self.save_schedule(config, layout, "Saved minimum to free", |s| {
            s.min_reclaim = bytes;
        });
        self.refresh_status(config, host, layout);
    }

    fn save_schedule(
        &mut self,
        config: &mut Config,
        layout: &Layout,
        notice: &'static str,
        edit: impl FnOnce(&mut ScheduleConfig),
    ) {
        let Some(schedule) = config.schedule.as_mut() else {
            return;
        };
        edit(schedule);
        let _ = config.save_to(&layout.config_path());
        self.error_notice = None;
        self.save_notice = Some(notice);
    }

    /// Sends `target` to launchd and the config. On failure the old value stays.
    fn apply_schedule(
        &mut self,
        target: Option<ScheduleFrequency>,
        config: &mut Config,
        host: &dyn Host,
        layout: &Layout,
    ) {
        if target == self.schedule {
            return;
        }
        let result = match target {
            Some(freq) => set_schedule(
                host,
                layout,
                config,
                freq,
                self.categories.clone(),
                self.min_reclaim,
                None,
            ),
            None => remove_schedule(host, layout, config),
        };
        match result {
            Ok(()) => {
                self.schedule = target;
                if target.is_none() {
                    self.categories = default_schedule_categories();
                    self.min_reclaim = default_schedule_min_reclaim();
                }
                self.error_notice = None;
                self.save_notice = Some(if target.is_some() {
                    "Enabled scheduled cleanup"
                } else {
                    "Disabled scheduled cleanup"
                });
            }
            Err(message) => {
                self.save_notice = None;
                self.error_notice = Some(message);
            }
        }
        self.refresh_status(config, host, layout);
    }

    fn row_value(&self, row: SettingsRow) -> String {
        match row {
            SettingsRow::AgeThreshold => age_label(self.older_than_days),
            SettingsRow::Run => schedule_label(self.schedule),
            SettingsRow::MinReclaim => format_reclaim_threshold(self.min_reclaim),
            SettingsRow::Appearance => appearance_label(self.appearance),
            SettingsRow::Category(_)
            | SettingsRow::Item(_)
            | SettingsRow::Add
            | SettingsRow::Reload => String::new(),
        }
    }
}

pub fn render_settings(frame: &mut Frame, state: &SettingsState, theme: &Theme) {
    let split = AppChrome::new(Vec::new())
        .full_width()
        .content_rect(frame.area())
        .width
        >= SPLIT_MIN_WIDTH;
    let in_rows = state.focus == SettingsFocus::Rows;

    let mut crumbs = vec!["taert", "Settings"];
    if in_rows && !split {
        crumbs.push(state.section.title());
    }

    let row = state.current_row();
    let primary = match row {
        _ if !in_rows => Some(PrimaryAction::new("Open", theme.g.key_enter)),
        _ if row.is_popup() => Some(PrimaryAction::new("Change", theme.g.key_enter)),
        SettingsRow::Add => Some(PrimaryAction::new("Add", theme.g.key_enter)),
        SettingsRow::Category(_) => Some(PrimaryAction::new("Toggle", theme.g.key_enter)),
        SettingsRow::Reload => Some(PrimaryAction::new("Load", theme.g.key_enter)),
        _ => None,
    };
    let in_list = in_rows && matches!(row, SettingsRow::Item(_) | SettingsRow::Add);
    let mut hints = Vec::new();
    if in_list {
        hints.push(Hint::new("a", "Add"));
        if matches!(row, SettingsRow::Item(_)) {
            hints.push(Hint::new("d", "Remove"));
        }
    }
    hints.push(Hint::new("↑↓", "Move"));
    hints.push(if in_rows {
        Hint::new("←", "Sections")
    } else {
        Hint::new("→", "Open")
    });
    hints.extend([Hint::new("⎋", "Back"), Hint::new("?", "More")]);

    let toast = match &state.error_notice {
        Some(message) => Some(Toast::error(message.clone())),
        None => state.save_notice.map(Toast::success),
    };

    let mut chrome = AppChrome::new(crumbs)
        .hints(hints)
        .toast(toast.as_ref())
        .behind_dialog(state.picker.is_some() || state.has_dialog())
        .full_width();
    if let Some(primary) = primary {
        chrome = chrome.primary(primary);
    }
    let content = chrome.render(frame, theme);

    if split {
        let (sidebar, pane) = split_panes(content);
        render_lines(frame, sidebar, sections_card(state, sidebar.width, theme));
        render_lines(
            frame,
            pane,
            rows_card(state, pane.width, pane.height, theme),
        );
    } else if in_rows {
        render_lines(
            frame,
            content,
            rows_card(state, content.width, content.height, theme),
        );
    } else {
        render_lines(frame, content, sections_card(state, content.width, theme));
    }

    if let Some(picker) = state.picker_state() {
        render_picker(frame, frame.area(), picker, theme);
    }
    if let Some(list) = state.list() {
        list.render_dialog(frame, theme);
    }
}

fn render_lines(frame: &mut Frame, area: Rect, lines: Vec<Line<'static>>) {
    frame.render_widget(Paragraph::new(lines), area);
}

fn sections_card(state: &SettingsState, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let width = width as usize;
    let inner = width.saturating_sub(FRAME_SIDES);
    let rows = SettingsSection::ALL
        .iter()
        .map(|&section| {
            let active = section == state.section;
            let icon_style = if active {
                theme.accent_text().add_modifier(Modifier::BOLD)
            } else {
                theme.callout()
            };
            ListRow::new(vec![Span::styled(section.title(), theme.body())])
                .leading(Span::styled(section.icon(theme), icon_style))
                .chevron(true)
                .selected(active && state.focus == SettingsFocus::Sections)
                .render_line(inner, theme)
        })
        .collect();
    card(
        "Settings",
        Vec::new(),
        rows,
        width,
        0,
        theme.p.purple,
        theme,
    )
}

fn rows_card(state: &SettingsState, width: u16, height: u16, theme: &Theme) -> Vec<Line<'static>> {
    let width = width as usize;
    let inner = width.saturating_sub(FRAME_SIDES);
    let scheduled = state.section == SettingsSection::ScheduledCleanup;
    let (lines, focus) = if scheduled {
        schedule_lines(state, inner, theme)
    } else if state.section == SettingsSection::Appearance {
        appearance_lines(state, inner, theme)
    } else {
        list_lines(state, inner, theme)
    };
    let last_run = if scheduled {
        last_run_card(state, width, theme)
    } else {
        Vec::new()
    };
    let (start, end) = visible_window(
        lines.len(),
        focus,
        (height as usize).saturating_sub(FRAME_ROWS + last_run.len()),
    );
    let heading = format!("{}  {}", state.section.icon(theme), state.section.title());
    let mut card_lines = card(
        &heading,
        Vec::new(),
        lines[start..end].to_vec(),
        width,
        0,
        theme.p.purple,
        theme,
    );
    card_lines.extend(last_run);
    card_lines
}

fn last_run_text(last_run: &LastRun) -> String {
    match last_run {
        LastRun::Never => "Has not run yet".to_string(),
        LastRun::Ran { at, freed } => {
            format!("{} · freed {}", format_run_time(at), format_bytes(*freed))
        }
        LastRun::Skipped {
            at,
            found,
            threshold,
        } => format!(
            "Skipped {}: found {}, below {}",
            format_run_time(at),
            format_bytes(*found),
            format_reclaim_threshold(*threshold)
        ),
    }
}

/// The card with the last run, while `Run` is on and the status is read.
fn last_run_card(state: &SettingsState, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let (Some(status), Some(_)) = (&state.status, state.schedule) else {
        return Vec::new();
    };
    let inner = width.saturating_sub(FRAME_SIDES);
    let text = last_run_text(&status.last_run);
    card(
        "Last run",
        Vec::new(),
        vec![text_line(&text, theme.body(), inner, theme)],
        width,
        0,
        theme.p.purple,
        theme,
    )
}

/// One sentence for each thing that stops the schedule from working.
fn problems(status: &ScheduleStatus) -> Vec<String> {
    let mut lines = Vec::new();
    if status.plist_missing {
        lines.push("The launch agent file is missing.".to_string());
    }
    if let Some(program) = &status.missing_program {
        lines.push(format!(
            "The program {} in the launch agent is missing.",
            program.display()
        ));
    }
    if status.agent_loaded == Some(false) {
        lines.push(
            "launchd has not loaded the agent. It may be switched off in System Settings › General › Login Items & Extensions."
                .to_string(),
        );
    }
    if let Some(program) = &status.needs_full_disk_access {
        lines.push(format!(
            "The last run could not remove some files. Give {} Full Disk Access in System Settings › Privacy & Security.",
            program.display()
        ));
    }
    lines
}

/// `text` broken to the width of the column of row titles.
fn wrapped_lines(text: &str, style: Style, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let room = ListRow::plain_title_room(width);
    wrap_line(Line::from(Span::styled(text.to_string(), style)), room, 0)
        .into_iter()
        .map(|part| ListRow::new(part.spans).render_line(width, theme))
        .collect()
}

/// The rows of a section other than Scheduled cleanup and Appearance, and the line of the highlighted one.
fn list_lines(state: &SettingsState, width: usize, theme: &Theme) -> (Vec<Line<'static>>, usize) {
    let first_list_row = state.section.fixed_rows().len();
    let mut lines = Vec::new();
    let mut focus = 0;
    for (i, row) in state.rows().into_iter().enumerate() {
        let selected = state.focus == SettingsFocus::Rows && i == state.row;
        if i == first_list_row
            && let Some(heading) = state.list().and_then(PathList::heading)
        {
            lines.push(Line::default());
            lines.push(text_line(heading, theme.callout(), width, theme));
        }
        if selected {
            focus = lines.len();
        }
        lines.push(row_line(state, row, selected, width, theme));
    }
    if let Some(list) = state.list() {
        lines.push(text_line(list.footnote(), theme.footnote(), width, theme));
    }
    (lines, focus)
}

/// The rows of Scheduled cleanup and the line of the highlighted one. While `Run` is off, the
/// rows below it are dimmed and out of reach.
fn schedule_lines(
    state: &SettingsState,
    width: usize,
    theme: &Theme,
) -> (Vec<Line<'static>>, usize) {
    let in_rows = state.focus == SettingsFocus::Rows;
    let mut lines = Vec::new();
    let mut focus = 0;
    let mut push = |lines: &mut Vec<Line<'static>>, at: usize, row: SettingsRow| {
        let selected = in_rows && state.row == at;
        if selected {
            focus = lines.len();
        }
        lines.push(row_line(state, row, selected, width, theme));
    };
    push(&mut lines, 0, SettingsRow::Run);
    lines.push(Line::default());
    lines.push(text_line("Clean", theme.callout(), width, theme));
    for index in 0..CLEAN_CATEGORIES.len() {
        push(&mut lines, 1 + index, SettingsRow::Category(index));
    }
    push(
        &mut lines,
        1 + CLEAN_CATEGORIES.len(),
        SettingsRow::MinReclaim,
    );
    if state.schedule.is_none() {
        lines.push(text_line(
            "Turn on Run to choose what it cleans.",
            theme.footnote(),
            width,
            theme,
        ));
    }
    if let Some(status) = state.status.as_ref().filter(|_| state.schedule.is_some()) {
        let problems = problems(status);
        if !problems.is_empty() {
            lines.push(Line::default());
        }
        for problem in problems {
            lines.extend(wrapped_lines(&problem, theme.warning(), width, theme));
        }
        if state.needs_reload() {
            push(&mut lines, 2 + CLEAN_CATEGORIES.len(), SettingsRow::Reload);
        }
    }
    (lines, focus)
}

/// The Appearance row and the notes on what decides the colors.
fn appearance_lines(
    state: &SettingsState,
    width: usize,
    theme: &Theme,
) -> (Vec<Line<'static>>, usize) {
    let selected = state.focus == SettingsFocus::Rows;
    let mut lines = vec![row_line(
        state,
        SettingsRow::Appearance,
        selected,
        width,
        theme,
    )];
    let mut notes = vec![
        "Auto uses the background color the terminal reports, and dark when it reports none."
            .to_string(),
    ];
    if theme.env.depth == ColorDepth::NoColor {
        notes.push("This terminal shows no colors (NO_COLOR or TERM=dumb).".to_string());
    } else if let Some(forced) = theme.env.forced_appearance {
        let name = match forced {
            Appearance::Dark => "dark",
            Appearance::Light => "light",
        };
        notes.push(format!(
            "TAERT_APPEARANCE is set, so this session stays {name}."
        ));
    }
    for note in notes {
        lines.extend(wrapped_lines(&note, theme.footnote(), width, theme));
    }
    (lines, 0)
}

/// A line of text that sits in the column of the row titles.
fn text_line(text: &str, style: Style, width: usize, theme: &Theme) -> Line<'static> {
    ListRow::new(vec![Span::styled(text.to_string(), style)]).render_line(width, theme)
}

fn title_style(state: &SettingsState, theme: &Theme) -> Style {
    if state.schedule.is_some() {
        theme.body()
    } else {
        theme.footnote()
    }
}

fn row_line(
    state: &SettingsState,
    row: SettingsRow,
    selected: bool,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let value_style = if selected {
        theme.accent_text()
    } else {
        theme.callout()
    };
    match row {
        SettingsRow::Item(index) => {
            let path = state.list().map_or("", |list| list.items[index].as_str());
            let max_len = width.saturating_sub(8);
            ListRow::new(path_spans(Path::new(path), max_len, theme))
                .selected(selected)
                .render_line(width, theme)
        }
        SettingsRow::Add => {
            let label = state.list().map_or("", PathList::add_label);
            ListRow::new(vec![Span::styled(label, theme.accent_text())])
                .selected(selected)
                .render_line(width, theme)
        }
        SettingsRow::Category(index) => {
            let (category, subcategory) = CLEAN_CATEGORIES[index];
            let checked = state.categories.iter().any(|c| c == category.id());
            let glyph = if checked {
                theme.g.check_on
            } else {
                theme.g.check_off
            };
            let glyph_style = if state.schedule.is_none() {
                theme.footnote()
            } else if checked {
                theme.accent_text()
            } else {
                theme.callout()
            };
            ListRow::new(vec![
                Span::styled(glyph, glyph_style),
                Span::raw(" "),
                Span::styled(subcategory.title(), title_style(state, theme)),
            ])
            .selected(selected)
            .render_line(width, theme)
        }
        SettingsRow::Reload => ListRow::new(vec![Span::styled(row.title(), theme.accent_text())])
            .selected(selected)
            .render_line(width, theme),
        SettingsRow::MinReclaim => {
            let detail = format!("{} {}", state.row_value(row), theme.g.popup);
            let value_style = if state.schedule.is_none() {
                theme.footnote()
            } else {
                value_style
            };
            ListRow::new(vec![Span::styled(row.title(), title_style(state, theme))])
                .selected(selected)
                .detail(Span::styled(detail, value_style))
                .render_line(width, theme)
        }
        SettingsRow::AgeThreshold | SettingsRow::Run | SettingsRow::Appearance => {
            let detail = format!("{} {}", state.row_value(row), theme.g.popup);
            ListRow::new(vec![Span::styled(row.title(), theme.body())])
                .selected(selected)
                .detail(Span::styled(detail, value_style))
                .render_line(width, theme)
        }
    }
}

/// The lines `start..end` of `total` that fit `room` and keep the line `focus` in view.
fn visible_window(total: usize, focus: usize, room: usize) -> (usize, usize) {
    if total <= room {
        return (0, total);
    }
    let start = focus.saturating_sub(room / 2).min(total - room);
    (start, start + room)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::apps::testing::sandbox;
    use crate::core::projects_store::ProjectFoldersStore;

    #[test]
    fn categories_typed_by_hand_show_as_the_names_scheduled_cleanup_reads() {
        let (_temp, layout) = sandbox();
        let config = Config {
            schedule: Some(crate::core::stores::ScheduleConfig {
                every: ScheduleFrequency::Week,
                categories: vec!["Dev".into(), "JUNK".into(), "dev".into()],
                min_reclaim: 1_000_000_000,
                last_skip: None,
            }),
            ..Config::default()
        };
        let state = SettingsState::new(&config, &IgnoreStore::default(), &layout);
        assert_eq!(state.categories, ["dev", "junk"]);
    }

    #[test]
    fn the_sections_come_in_order_with_their_rows() {
        let titles: Vec<_> = SettingsSection::ALL.iter().map(|s| s.title()).collect();
        assert_eq!(
            titles,
            [
                "Project cleanup",
                "Scheduled cleanup",
                "Protected paths",
                "Appearance"
            ]
        );
        let (_temp, layout) = sandbox();
        ProjectFoldersStore::new(vec!["/nowhere/code".to_string()])
            .save_to(&layout.projects_path())
            .unwrap();
        let mut state = SettingsState::new(&Config::default(), &IgnoreStore::default(), &layout);
        assert_eq!(
            state.rows(),
            [
                SettingsRow::AgeThreshold,
                SettingsRow::Item(0),
                SettingsRow::Add
            ]
        );
        state.section = SettingsSection::ScheduledCleanup;
        assert_eq!(state.rows(), [SettingsRow::Run]);
        state.section = SettingsSection::ProtectedPaths;
        assert_eq!(state.rows(), [SettingsRow::Add]);
        state.section = SettingsSection::Appearance;
        assert_eq!(state.rows(), [SettingsRow::Appearance]);
    }

    #[test]
    fn a_saved_age_outside_the_presets_is_added_in_order() {
        assert_eq!(with_current(AGE_PRESETS, 7), AGE_PRESETS);
        assert_eq!(
            with_current(AGE_PRESETS, 10),
            [1, 3, 7, 10, 14, 30, 60, 90, 180, 365]
        );
        assert_eq!(with_current(AGE_PRESETS, 400).last(), Some(&400));
    }

    #[test]
    fn the_folders_come_from_the_layout() {
        let (temp, layout) = sandbox();
        let folder = temp.path().canonicalize().unwrap().display().to_string();
        ProjectFoldersStore::new(vec![folder])
            .save_to(&layout.projects_path())
            .unwrap();

        let state = SettingsState::new(&Config::default(), &IgnoreStore::default(), &layout);

        assert_eq!(state.folders.items.len(), 1);
        assert_eq!(
            state.rows(),
            [
                SettingsRow::AgeThreshold,
                SettingsRow::Item(0),
                SettingsRow::Add
            ]
        );
    }

    #[test]
    fn a_wrapped_problem_line_keeps_every_character_of_its_path() {
        let theme = Theme::default();
        let path = "/opt/homebrew/Cellar/taert/0.10.0/bin/taert-with-a-very-long-name";
        let text = format!("Give {path} Full Disk Access.");
        for width in 20..90 {
            let lines = wrapped_lines(&text, theme.warning(), width, &theme);
            let shown: String = lines
                .iter()
                .map(|l| {
                    l.spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>()
                })
                .collect();
            assert!(!shown.contains('…'), "width {width}: {shown}");
            assert!(
                shown.replace(' ', "").contains(path),
                "width {width}: {shown}"
            );
        }
    }
}

//! Settings screen: sections on the left, the rows of the highlighted section on the right.

use std::path::{Path, PathBuf};
use std::time::Duration;

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
use crate::core::path_utils::prettify_path_in;
use crate::core::schedule::{
    LastRun, ScheduleFrequency, ScheduleStatus, agent_plist_path, format_reclaim_threshold,
    format_run_time, normalize_categories, remove_schedule, schedule_status, set_schedule,
};
use crate::core::startup::disabled_login_items_path;
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
    About,
}

impl SettingsSection {
    pub const ALL: [Self; 5] = [
        Self::ProjectCleanup,
        Self::ScheduledCleanup,
        Self::ProtectedPaths,
        Self::Appearance,
        Self::About,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::ProjectCleanup => "Project cleanup",
            Self::ScheduledCleanup => "Scheduled cleanup",
            Self::ProtectedPaths => "Protected paths",
            Self::Appearance => "Appearance",
            Self::About => "About",
        }
    }

    /// The lists of the section, in the order of their rows.
    fn list_kinds(self) -> &'static [PathListKind] {
        match self {
            Self::ProjectCleanup => &[PathListKind::ProjectFolders, PathListKind::ArtifactNames],
            Self::ScheduledCleanup => &[],
            Self::ProtectedPaths => &[PathListKind::ProtectedPaths],
            Self::Appearance | Self::About => &[],
        }
    }

    /// The rows that come before the list of the section.
    fn fixed_rows(self) -> &'static [SettingsRow] {
        match self {
            Self::ProjectCleanup => &[SettingsRow::AgeThreshold],
            Self::ScheduledCleanup => &[SettingsRow::Run],
            Self::ProtectedPaths => &[],
            Self::Appearance => &[SettingsRow::Appearance],
            Self::About => &[SettingsRow::Program],
        }
    }

    fn icon(self, theme: &Theme) -> &'static str {
        match self {
            Self::ProjectCleanup => theme.g.icon_projects,
            Self::ScheduledCleanup => theme.g.icon_schedule,
            Self::ProtectedPaths => theme.g.icon_protected,
            Self::Appearance => theme.g.icon_appearance,
            Self::About => theme.g.icon_about,
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
    /// The program taert runs from.
    Program,
    /// The file at this place in `taert_files`.
    File(usize),
    /// The path at this place in the first list of the section.
    Item(usize),
    Add,
    /// The custom folder name at this place in the names list.
    Name(usize),
    AddName,
}

impl SettingsRow {
    fn title(self) -> &'static str {
        match self {
            Self::AgeThreshold => "Preselect artifacts older than",
            Self::Run => "Run",
            Self::MinReclaim => "Only when it frees at least",
            Self::Reload => "Load the schedule again",
            Self::Appearance => "Appearance",
            Self::Program => "Program",
            Self::Category(_)
            | Self::Item(_)
            | Self::Add
            | Self::Name(_)
            | Self::AddName
            | Self::File(_) => "",
        }
    }

    /// The list a row of a list belongs to.
    fn list_kind(self, section: SettingsSection) -> Option<PathListKind> {
        match self {
            Self::Item(_) | Self::Add => section.list_kinds().first().copied(),
            Self::Name(_) | Self::AddName => Some(PathListKind::ArtifactNames),
            _ => None,
        }
    }

    fn is_list_row(self) -> bool {
        matches!(
            self,
            Self::Item(_) | Self::Add | Self::Name(_) | Self::AddName
        )
    }

    fn is_item(self) -> bool {
        matches!(self, Self::Item(_) | Self::Name(_))
    }

    fn is_add(self) -> bool {
        matches!(self, Self::Add | Self::AddName)
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
    pub names: PathList,
    pub protected: PathList,
    pub version: String,
    /// The program being run, if the system says where it is.
    pub program: Option<PathBuf>,
    home: PathBuf,
    files: Vec<AboutFile>,
    /// What the schedule does now, read when Settings opens and after each change.
    pub status: Option<ScheduleStatus>,
    pub save_notice: Option<&'static str>,
    pub error_notice: Option<String>,
    picker: Option<(SettingsRow, PickerState)>,
}

/// A file taert keeps, shown in About.
struct AboutFile {
    label: &'static str,
    path: PathBuf,
}

impl AboutFile {
    fn exists(&self) -> bool {
        self.path.exists()
    }
}

/// The files taert keeps, by name.
pub fn taert_files(layout: &Layout) -> Vec<(&'static str, PathBuf)> {
    vec![
        ("Settings", layout.config_path()),
        ("Protected paths", layout.ignore_path()),
        ("Project folders", layout.projects_path()),
        ("History", layout.history_path()),
        (
            "Switched-off login items",
            disabled_login_items_path(layout),
        ),
        ("Schedule agent", agent_plist_path(layout)),
    ]
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
            folders: PathList::new(PathListKind::ProjectFolders, ignore, config, layout),
            names: PathList::new(PathListKind::ArtifactNames, ignore, config, layout),
            protected: PathList::new(PathListKind::ProtectedPaths, ignore, config, layout),
            version: env!("CARGO_PKG_VERSION").to_string(),
            program: std::env::current_exe().ok(),
            home: layout.home.clone(),
            files: taert_files(layout)
                .into_iter()
                .map(|(label, path)| AboutFile { label, path })
                .collect(),
            status: None,
            save_notice: None,
            error_notice: None,
            picker: None,
        }
    }

    fn list_of(&self, kind: PathListKind) -> &PathList {
        match kind {
            PathListKind::ProjectFolders => &self.folders,
            PathListKind::ArtifactNames => &self.names,
            PathListKind::ProtectedPaths => &self.protected,
        }
    }

    fn list_of_mut(&mut self, kind: PathListKind) -> &mut PathList {
        match kind {
            PathListKind::ProjectFolders => &mut self.folders,
            PathListKind::ArtifactNames => &mut self.names,
            PathListKind::ProtectedPaths => &mut self.protected,
        }
    }

    /// The list of the section that has a dialog open.
    pub fn dialog_list(&self) -> Option<&PathList> {
        self.section
            .list_kinds()
            .iter()
            .map(|&kind| self.list_of(kind))
            .find(|list| list.has_dialog())
    }

    /// The index of the first row of a list of the section.
    fn list_start(&self, kind: PathListKind) -> usize {
        let before: usize = self
            .section
            .list_kinds()
            .iter()
            .take_while(|&&k| k != kind)
            .map(|&k| self.list_of(k).items.len() + 1)
            .sum();
        self.section.fixed_rows().len() + before
    }

    /// The rows of the section: its fixed rows, then for each list one row per item and the
    /// add row.
    pub fn rows(&self) -> Vec<SettingsRow> {
        let mut rows = self.section.fixed_rows().to_vec();
        for &kind in self.section.list_kinds() {
            let count = self.list_of(kind).items.len();
            match kind {
                PathListKind::ArtifactNames => {
                    rows.extend((0..count).map(SettingsRow::Name));
                    rows.push(SettingsRow::AddName);
                }
                _ => {
                    rows.extend((0..count).map(SettingsRow::Item));
                    rows.push(SettingsRow::Add);
                }
            }
        }
        if self.section == SettingsSection::About {
            rows.extend((0..self.files.len()).map(SettingsRow::File));
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
        self.dialog_list().is_some()
    }

    /// Text goes into the path of the add dialog.
    pub fn is_typing(&self) -> bool {
        self.dialog_list().is_some_and(PathList::is_adding)
    }

    /// Opens the add dialog when the focus is on a row of a list.
    pub fn start_adding(&mut self) {
        let row = self.current_row();
        if row.is_list_row()
            && let Some(kind) = row.list_kind(self.section)
        {
            self.list_of_mut(kind).start_adding();
        }
    }

    /// Asks to remove the highlighted path or name.
    pub fn prompt_remove(&mut self) {
        let row = self.current_row();
        if let (SettingsRow::Item(index) | SettingsRow::Name(index), Some(kind)) =
            (row, row.list_kind(self.section))
        {
            self.list_of_mut(kind).prompt_remove(index);
        }
    }

    /// Passes a key to the open dialog. The highlight follows an added item and stays in the
    /// list after a removal.
    pub fn dialog_key(&mut self, code: KeyCode, ignore: &mut IgnoreStore, config: &mut Config) {
        let Some(kind) = self.dialog_list().map(|list| list.kind) else {
            return;
        };
        let first = self.list_start(kind);
        let before = self.list_of(kind).items.len();
        let list = self.list_of_mut(kind);
        list.dialog_key(code, ignore, config);
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
            | SettingsRow::Reload
            | SettingsRow::Name(_)
            | SettingsRow::AddName
            | SettingsRow::Program
            | SettingsRow::File(_) => return,
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
            | SettingsRow::Reload
            | SettingsRow::Name(_)
            | SettingsRow::AddName
            | SettingsRow::Program
            | SettingsRow::File(_) => {}
        }
    }

    /// What Finder can show for the highlighted row.
    fn reveal_path(&self) -> Option<&Path> {
        match self.current_row() {
            SettingsRow::Program => self.program.as_deref(),
            SettingsRow::File(index) => {
                Some(self.files[index].path.as_path()).filter(|p| p.exists())
            }
            _ => None,
        }
    }

    /// Whether the highlighted row names something that Finder can show.
    pub fn can_reveal(&self) -> bool {
        self.reveal_path().is_some()
    }

    /// Shows the program or the file of the highlighted row in Finder.
    pub fn reveal(&self, host: &dyn Host) {
        if let Some(path) = self.reveal_path() {
            host.run(
                "/usr/bin/open",
                &["-R", &path.display().to_string()],
                Duration::from_secs(5),
            );
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
            | SettingsRow::Reload
            | SettingsRow::Name(_)
            | SettingsRow::AddName
            | SettingsRow::Program
            | SettingsRow::File(_) => String::new(),
        }
    }
}

/// The primary action of the highlighted row.
pub fn settings_primary<'a>(state: &SettingsState, enter: &'a str) -> Option<PrimaryAction<'a>> {
    let row = state.current_row();
    match row {
        _ if state.focus != SettingsFocus::Rows => Some(PrimaryAction::new("Open", enter)),
        _ if row.is_popup() => Some(PrimaryAction::new("Change", enter)),
        _ if row.is_add() => Some(PrimaryAction::new("Add", enter)),
        SettingsRow::Category(_) => Some(PrimaryAction::new("Toggle", enter)),
        SettingsRow::Reload => Some(PrimaryAction::new("Load", enter)),
        SettingsRow::Program | SettingsRow::File(_) if state.can_reveal() => {
            Some(PrimaryAction::new("Show in Finder", enter))
        }
        SettingsRow::Program | SettingsRow::File(_) => {
            Some(PrimaryAction::disabled("Show in Finder", enter))
        }
        _ => None,
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
    let primary = settings_primary(state, theme.g.key_enter);
    let in_list = in_rows && row.is_list_row();
    let mut hints = Vec::new();
    if in_list {
        hints.push(Hint::new("a", "Add"));
        if row.is_item() {
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
    if let Some(list) = state.dialog_list() {
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
    } else if state.section == SettingsSection::About {
        about_lines(state, inner, theme)
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

/// The rows of a section other than Scheduled cleanup and Appearance, and the line of the
/// highlighted one. Each list has its heading above it and its footnote below it.
fn list_lines(state: &SettingsState, width: usize, theme: &Theme) -> (Vec<Line<'static>>, usize) {
    let rows = state.rows();
    let mut lines = Vec::new();
    let mut focus = 0;
    let mut kind = None;
    for (i, &row) in rows.iter().enumerate() {
        let row_kind = row.list_kind(state.section);
        if row_kind != kind {
            if let Some(done) = kind {
                lines.extend(footnote_lines(state.list_of(done), width, theme));
            }
            kind = row_kind;
            if let Some(heading) = kind.and_then(|k| state.list_of(k).heading()) {
                lines.push(Line::default());
                lines.push(text_line(heading, theme.callout(), width, theme));
            }
        }
        let selected = state.focus == SettingsFocus::Rows && i == state.row;
        if selected {
            focus = lines.len();
        }
        lines.push(row_line(state, row, selected, width, theme));
    }
    if let Some(done) = kind {
        lines.extend(footnote_lines(state.list_of(done), width, theme));
    }
    (lines, focus)
}

/// The footnote of a list, broken into lines that fit the column of the row titles.
fn footnote_lines(list: &PathList, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    wrapped_lines(list.footnote(), theme.footnote(), width, theme)
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

/// The version, the program and the files taert keeps, and the line of the highlighted row.
fn about_lines(state: &SettingsState, width: usize, theme: &Theme) -> (Vec<Line<'static>>, usize) {
    let in_rows = state.focus == SettingsFocus::Rows;
    let mut lines = vec![
        ListRow::new(vec![Span::styled("Version", theme.body())])
            .detail(Span::styled(state.version.clone(), theme.callout()))
            .render_line(width, theme),
    ];
    let mut focus = 0;
    let mut entry = |lines: &mut Vec<Line<'static>>,
                     at: usize,
                     title: &str,
                     path: Option<String>,
                     present: bool| {
        if in_rows && state.row == at {
            focus = lines.len();
        }
        let mut title = ListRow::new(vec![Span::styled(title.to_string(), theme.body())])
            .selected(in_rows && state.row == at);
        if !present {
            title = title.detail(Span::styled("Not created yet", theme.footnote()));
        }
        lines.push(title.render_line(width, theme));
        if let Some(path) = path {
            lines.extend(wrapped_lines(&path, theme.callout(), width, theme));
        }
    };
    let program = state
        .program
        .as_ref()
        .map(|path| prettify_path_in(path, &state.home));
    entry(&mut lines, 0, "Program", program, true);
    lines.push(Line::default());
    lines.push(text_line("Files", theme.callout(), width, theme));
    for (index, file) in state.files.iter().enumerate() {
        let path = prettify_path_in(&file.path, &state.home);
        entry(&mut lines, 1 + index, file.label, Some(path), file.exists());
    }
    (lines, focus)
}

/// The title line of an About row. `about_lines` adds its path under it.
fn about_title(title: &str, selected: bool, width: usize, theme: &Theme) -> Line<'static> {
    ListRow::new(vec![Span::styled(title.to_string(), theme.body())])
        .selected(selected)
        .render_line(width, theme)
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
            let path = row
                .list_kind(state.section)
                .map_or("", |kind| state.list_of(kind).items[index].as_str());
            let max_len = width.saturating_sub(8);
            ListRow::new(path_spans(Path::new(path), max_len, theme))
                .selected(selected)
                .render_line(width, theme)
        }
        SettingsRow::Name(index) => ListRow::new(vec![Span::styled(
            state.names.items[index].clone(),
            theme.body(),
        )])
        .selected(selected)
        .render_line(width, theme),
        SettingsRow::Add | SettingsRow::AddName => {
            let label = row
                .list_kind(state.section)
                .map_or("", |kind| state.list_of(kind).add_label());
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
        SettingsRow::Program => about_title("Program", selected, width, theme),
        SettingsRow::File(index) => {
            let label = state.files.get(index).map_or("", |file| file.label);
            about_title(label, selected, width, theme)
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
                "Appearance",
                "About"
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
                SettingsRow::Add,
                SettingsRow::AddName
            ]
        );
        state.section = SettingsSection::ScheduledCleanup;
        assert_eq!(state.rows(), [SettingsRow::Run]);
        state.section = SettingsSection::ProtectedPaths;
        assert_eq!(state.rows(), [SettingsRow::Add]);
        state.section = SettingsSection::Appearance;
        assert_eq!(state.rows(), [SettingsRow::Appearance]);
        state.section = SettingsSection::About;
        let mut expected = vec![SettingsRow::Program];
        expected.extend((0..6).map(SettingsRow::File));
        assert_eq!(state.rows(), expected);
    }

    #[test]
    fn a_program_path_taert_cannot_read_is_not_called_missing() {
        let (_temp, layout) = sandbox();
        let mut state = SettingsState::new(&Config::default(), &IgnoreStore::default(), &layout);
        state.section = SettingsSection::About;
        state.focus = SettingsFocus::Rows;
        state.program = None;
        let theme = Theme::default();

        let (lines, _) = about_lines(&state, 80, &theme);
        let program_note: String = lines[1..3]
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
            .collect();

        assert!(!program_note.contains("Not created yet"), "{program_note}");
        assert!(!settings_primary(&state, "↵").unwrap().enabled);
    }

    #[test]
    fn taert_files_are_six_paths_under_the_home_folder() {
        let (_temp, layout) = sandbox();
        let files = taert_files(&layout);
        let labels: Vec<_> = files.iter().map(|(label, _)| *label).collect();
        assert_eq!(
            labels,
            [
                "Settings",
                "Protected paths",
                "Project folders",
                "History",
                "Switched-off login items",
                "Schedule agent"
            ]
        );
        assert_eq!(files.len(), 6);
        for (label, path) in &files {
            assert!(
                path.starts_with(&layout.home),
                "{label}: {}",
                path.display()
            );
        }
        assert_eq!(files[0].1, layout.config_path());
        assert_eq!(files[4].1, disabled_login_items_path(&layout));
        assert_eq!(files[5].1, agent_plist_path(&layout));
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
                SettingsRow::Add,
                SettingsRow::AddName
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

    #[test]
    fn a_wrapped_footnote_is_never_cut() {
        let (_temp, _layout, mut state, _config) = name_state(&["out"]);
        state.focus = SettingsFocus::Rows;
        let theme = Theme::default();
        for width in [50u16, 77, 87, 113, 123] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 40)).unwrap();
            terminal
                .draw(|f| render_settings(f, &state, &theme))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let text: String = (0..40u16)
                .map(|y| {
                    (0..width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            let joined: String = text
                .replace(['│', '▌'], " ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            for note in [
                "Project cleanup scans these folders.",
                "Project cleanup lists folders with these names. You select them yourself.",
            ] {
                assert!(joined.contains(note), "width {width}: {text}");
            }
        }
    }

    fn name_state(names: &[&str]) -> (tempfile::TempDir, Layout, SettingsState, Config) {
        let (temp, layout) = sandbox();
        ProjectFoldersStore::new(vec!["/nowhere/code".to_string()])
            .save_to(&layout.projects_path())
            .unwrap();
        let config = Config {
            extra_artifacts: names.iter().map(|n| n.to_string()).collect(),
            ..Config::default()
        };
        let state = SettingsState::new(&config, &IgnoreStore::default(), &layout);
        (temp, layout, state, config)
    }

    #[test]
    fn project_cleanup_lists_the_folders_and_then_the_custom_names() {
        let (_temp, _layout, state, _config) = name_state(&["out", "bazel-out"]);
        assert_eq!(
            state.rows(),
            [
                SettingsRow::AgeThreshold,
                SettingsRow::Item(0),
                SettingsRow::Add,
                SettingsRow::Name(0),
                SettingsRow::Name(1),
                SettingsRow::AddName,
            ]
        );
    }

    #[test]
    fn adding_a_name_moves_the_highlight_to_it_and_saves_it() {
        let (_temp, layout, mut state, mut config) = name_state(&["out"]);
        let mut ignore = IgnoreStore::default();
        state.focus = SettingsFocus::Rows;
        state.row = 4;
        assert_eq!(state.current_row(), SettingsRow::AddName);

        state.start_adding();
        assert!(state.is_typing());
        for c in "bazel-out".chars() {
            state.dialog_key(KeyCode::Char(c), &mut ignore, &mut config);
        }
        state.dialog_key(KeyCode::Enter, &mut ignore, &mut config);

        assert_eq!(state.current_row(), SettingsRow::Name(1));
        assert_eq!(config.extra_artifacts, ["out", "bazel-out"]);
        let saved = Config::load_from(&layout.config_path()).unwrap();
        assert_eq!(saved.extra_artifacts, ["out", "bazel-out"]);
        assert_eq!(state.folders.items, ["/nowhere/code"]);
    }

    #[test]
    fn removing_a_name_keeps_the_highlight_in_the_names() {
        let (_temp, _layout, mut state, mut config) = name_state(&["out", "bazel-out"]);
        let mut ignore = IgnoreStore::default();
        state.focus = SettingsFocus::Rows;
        state.row = 4;
        assert_eq!(state.current_row(), SettingsRow::Name(1));

        state.prompt_remove();
        state.dialog_key(KeyCode::Enter, &mut ignore, &mut config);

        assert_eq!(config.extra_artifacts, ["out"]);
        assert_eq!(state.current_row(), SettingsRow::Name(0));
    }
}

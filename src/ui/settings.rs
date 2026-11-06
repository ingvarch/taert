//! Settings screen: sections on the left, the rows of the highlighted section on the right.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::components::chrome::{AppChrome, SPLIT_MIN_WIDTH, split_panes};
use super::components::frame::{FRAME_SIDES, card};
use super::components::hints::{Hint, PrimaryAction};
use super::components::list::ListRow;
use super::components::picker::{PickerState, render_picker};
use super::components::text::plural;
use super::components::toast::Toast;
use super::theme::Theme;
use crate::core::host::Host;
use crate::core::layout::Layout;
use crate::core::projects_store::ProjectFoldersStore;
use crate::core::schedule::{ScheduleFrequency, remove_schedule, set_schedule};
use crate::core::stores::{Config, IgnoreStore};

pub const AGE_PRESETS: &[u64] = &[1, 3, 7, 14, 30, 60, 90, 180, 365];

const SCHEDULE_CHOICES: [Option<ScheduleFrequency>; 4] = [
    None,
    Some(ScheduleFrequency::Day),
    Some(ScheduleFrequency::Week),
    Some(ScheduleFrequency::Month),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsSection {
    ProjectCleanup,
    ScheduledCleanup,
    ProtectedPaths,
}

impl SettingsSection {
    pub const ALL: [Self; 3] = [
        Self::ProjectCleanup,
        Self::ScheduledCleanup,
        Self::ProtectedPaths,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::ProjectCleanup => "Project cleanup",
            Self::ScheduledCleanup => "Scheduled cleanup",
            Self::ProtectedPaths => "Protected paths",
        }
    }

    pub fn rows(self) -> &'static [SettingsRow] {
        match self {
            Self::ProjectCleanup => &[SettingsRow::ProjectFolders, SettingsRow::AgeThreshold],
            Self::ScheduledCleanup => &[SettingsRow::Run],
            Self::ProtectedPaths => &[SettingsRow::ProtectedPaths],
        }
    }

    fn icon(self, theme: &Theme) -> &'static str {
        match self {
            Self::ProjectCleanup => theme.g.icon_projects,
            Self::ScheduledCleanup => theme.g.icon_schedule,
            Self::ProtectedPaths => theme.g.icon_protected,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsRow {
    ProjectFolders,
    AgeThreshold,
    Run,
    ProtectedPaths,
}

impl SettingsRow {
    fn title(self) -> &'static str {
        match self {
            Self::ProjectFolders => "Project folders",
            Self::AgeThreshold => "Preselect artifacts older than",
            Self::Run => "Run",
            Self::ProtectedPaths => "Protected paths",
        }
    }

    /// A row that opens a picker. The others open a list.
    pub fn is_popup(self) -> bool {
        matches!(self, Self::AgeThreshold | Self::Run)
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
    pub folder_count: usize,
    pub path_count: usize,
    pub save_notice: Option<&'static str>,
    pub error_notice: Option<String>,
    picker: Option<(SettingsRow, PickerState)>,
}

fn age_choices(current: u64) -> Vec<u64> {
    let mut days = AGE_PRESETS.to_vec();
    if let Err(at) = days.binary_search(&current) {
        days.insert(at, current);
    }
    days
}

fn age_label(days: u64) -> String {
    plural(days as usize, "day", "days")
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
            folder_count: ProjectFoldersStore::load_or_default_at(&layout.projects_path())
                .roots
                .len(),
            path_count: ignore.roots.len(),
            save_notice: None,
            error_notice: None,
            picker: None,
        }
    }

    /// The state with the focus on `row` of `section`.
    pub fn at(mut self, section: SettingsSection, row: usize) -> Self {
        self.section = section;
        self.row = row;
        self.focus = SettingsFocus::Rows;
        self
    }

    pub fn current_row(&self) -> SettingsRow {
        self.section.rows()[self.row]
    }

    pub fn next(&mut self) {
        match self.focus {
            SettingsFocus::Sections => self.move_section(1),
            SettingsFocus::Rows => self.row = (self.row + 1) % self.section.rows().len(),
        }
    }

    pub fn previous(&mut self) {
        match self.focus {
            SettingsFocus::Sections => self.move_section(SettingsSection::ALL.len() - 1),
            SettingsFocus::Rows => {
                let rows = self.section.rows().len();
                self.row = (self.row + rows - 1) % rows;
            }
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

    /// Opens the picker of the current row. A row that opens a list has none.
    pub fn open_picker(&mut self) {
        let row = self.current_row();
        let (choices, current) = match row {
            SettingsRow::AgeThreshold => {
                let days = age_choices(self.older_than_days);
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
            SettingsRow::ProjectFolders | SettingsRow::ProtectedPaths => return,
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
                if let Some(&days) = age_choices(self.older_than_days).get(picker.highlighted) {
                    self.set_age(days, config, layout);
                }
            }
            SettingsRow::Run => {
                if let Some(&target) = SCHEDULE_CHOICES.get(picker.highlighted) {
                    self.apply_schedule(target, config, host, layout);
                }
            }
            SettingsRow::ProjectFolders | SettingsRow::ProtectedPaths => {}
        }
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
            Some(freq) => {
                let categories = config
                    .schedule
                    .as_ref()
                    .map(|s| s.categories.clone())
                    .unwrap_or_else(|| vec!["dev".into(), "junk".into()]);
                let min_reclaim = config
                    .schedule
                    .as_ref()
                    .map(|s| s.min_reclaim)
                    .unwrap_or(1_000_000_000);
                set_schedule(host, layout, config, freq, categories, min_reclaim, None)
            }
            None => remove_schedule(host, layout, config),
        };
        match result {
            Ok(()) => {
                self.schedule = target;
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
    }

    fn row_value(&self, row: SettingsRow) -> String {
        match row {
            SettingsRow::ProjectFolders => plural(self.folder_count, "folder", "folders"),
            SettingsRow::AgeThreshold => age_label(self.older_than_days),
            SettingsRow::Run => schedule_label(self.schedule),
            SettingsRow::ProtectedPaths => plural(self.path_count, "path", "paths"),
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

    let primary = if in_rows && state.current_row().is_popup() {
        PrimaryAction::new("Change", theme.g.key_enter)
    } else {
        PrimaryAction::new("Open", theme.g.key_enter)
    };
    let mut hints = vec![Hint::new("↑↓", "Move")];
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

    let chrome = AppChrome::new(crumbs)
        .hints(hints)
        .primary(primary)
        .toast(toast.as_ref())
        .behind_dialog(state.picker.is_some())
        .full_width();
    let content = chrome.render(frame, theme);

    if split {
        let (sidebar, pane) = split_panes(content);
        render_lines(frame, sidebar, sections_card(state, sidebar.width, theme));
        render_lines(frame, pane, rows_card(state, pane.width, theme));
    } else if in_rows {
        render_lines(frame, content, rows_card(state, content.width, theme));
    } else {
        render_lines(frame, content, sections_card(state, content.width, theme));
    }

    if let Some(picker) = state.picker_state() {
        render_picker(frame, frame.area(), picker, theme);
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

fn rows_card(state: &SettingsState, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let width = width as usize;
    let inner = width.saturating_sub(FRAME_SIDES);
    let rows = state
        .section
        .rows()
        .iter()
        .enumerate()
        .map(|(i, &row)| {
            let value = state.row_value(row);
            let selected = state.focus == SettingsFocus::Rows && i == state.row;
            let value_style = if selected {
                theme.accent_text()
            } else {
                theme.callout()
            };
            let list_row =
                ListRow::new(vec![Span::styled(row.title(), theme.body())]).selected(selected);
            if row.is_popup() {
                let detail = format!("{value} {}", theme.g.popup);
                list_row.detail(Span::styled(detail, value_style))
            } else {
                list_row
                    .detail(Span::styled(value, value_style))
                    .chevron(true)
            }
            .render_line(inner, theme)
        })
        .collect();
    let heading = format!("{}  {}", state.section.icon(theme), state.section.title());
    card(&heading, Vec::new(), rows, width, 0, theme.p.purple, theme)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::apps::testing::sandbox;

    #[test]
    fn the_sections_come_in_order_with_their_rows() {
        let titles: Vec<_> = SettingsSection::ALL.iter().map(|s| s.title()).collect();
        assert_eq!(
            titles,
            ["Project cleanup", "Scheduled cleanup", "Protected paths"]
        );
        assert_eq!(
            SettingsSection::ProjectCleanup.rows(),
            [SettingsRow::ProjectFolders, SettingsRow::AgeThreshold]
        );
    }

    #[test]
    fn a_saved_age_outside_the_presets_is_added_in_order() {
        assert_eq!(age_choices(7), AGE_PRESETS);
        assert_eq!(age_choices(10), [1, 3, 7, 10, 14, 30, 60, 90, 180, 365]);
        assert_eq!(age_choices(400).last(), Some(&400));
    }

    #[test]
    fn the_folder_count_comes_from_the_layout() {
        let (temp, layout) = sandbox();
        let folder = temp.path().canonicalize().unwrap().display().to_string();
        ProjectFoldersStore::new(vec![folder])
            .save_to(&layout.projects_path())
            .unwrap();

        let state = SettingsState::new(&Config::default(), &IgnoreStore::default(), &layout);

        assert_eq!(state.folder_count, 1);
    }
}

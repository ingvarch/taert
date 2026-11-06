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
use super::components::text::plural;
use super::components::toast::Toast;
use super::path_list::{PathList, PathListKind};
use super::theme::Theme;
use crate::core::host::Host;
use crate::core::layout::Layout;
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

    fn list_kind(self) -> Option<PathListKind> {
        match self {
            Self::ProjectCleanup => Some(PathListKind::ProjectFolders),
            Self::ScheduledCleanup => None,
            Self::ProtectedPaths => Some(PathListKind::ProtectedPaths),
        }
    }

    /// The rows that come before the list of the section.
    fn fixed_rows(self) -> &'static [SettingsRow] {
        match self {
            Self::ProjectCleanup => &[SettingsRow::AgeThreshold],
            Self::ScheduledCleanup => &[SettingsRow::Run],
            Self::ProtectedPaths => &[],
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
    AgeThreshold,
    Run,
    /// The path at this place in the list of the section.
    Item(usize),
    Add,
}

impl SettingsRow {
    fn title(self) -> &'static str {
        match self {
            Self::AgeThreshold => "Preselect artifacts older than",
            Self::Run => "Run",
            Self::Item(_) | Self::Add => "",
        }
    }

    /// A row that opens a picker.
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
    pub folders: PathList,
    pub protected: PathList,
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
            folders: PathList::new(PathListKind::ProjectFolders, ignore, layout),
            protected: PathList::new(PathListKind::ProtectedPaths, ignore, layout),
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
        rows
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
            SettingsRow::Item(_) | SettingsRow::Add => return,
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
            SettingsRow::Item(_) | SettingsRow::Add => {}
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
            SettingsRow::AgeThreshold => age_label(self.older_than_days),
            SettingsRow::Run => schedule_label(self.schedule),
            SettingsRow::Item(_) | SettingsRow::Add => String::new(),
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
        _ => None,
    };
    let in_list = in_rows && !row.is_popup();
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
    let first_list_row = state.section.fixed_rows().len();
    let mut lines = Vec::new();
    let mut focus = 0;
    for (i, row) in state.rows().into_iter().enumerate() {
        let selected = state.focus == SettingsFocus::Rows && i == state.row;
        if i == first_list_row
            && let Some(heading) = state.list().and_then(PathList::heading)
        {
            lines.push(Line::default());
            lines.push(text_line(heading, theme.callout(), inner, theme));
        }
        if selected {
            focus = lines.len();
        }
        lines.push(row_line(state, row, selected, inner, theme));
    }
    if let Some(list) = state.list() {
        lines.push(text_line(list.footnote(), theme.footnote(), inner, theme));
    }
    let (start, end) = visible_window(
        lines.len(),
        focus,
        (height as usize).saturating_sub(FRAME_ROWS),
    );
    let heading = format!("{}  {}", state.section.icon(theme), state.section.title());
    card(
        &heading,
        Vec::new(),
        lines[start..end].to_vec(),
        width,
        0,
        theme.p.purple,
        theme,
    )
}

/// A line of text that sits in the column of the row titles.
fn text_line(text: &str, style: Style, width: usize, theme: &Theme) -> Line<'static> {
    ListRow::new(vec![Span::styled(text.to_string(), style)]).render_line(width, theme)
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
        SettingsRow::AgeThreshold | SettingsRow::Run => {
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
    fn the_sections_come_in_order_with_their_rows() {
        let titles: Vec<_> = SettingsSection::ALL.iter().map(|s| s.title()).collect();
        assert_eq!(
            titles,
            ["Project cleanup", "Scheduled cleanup", "Protected paths"]
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
    }

    #[test]
    fn a_saved_age_outside_the_presets_is_added_in_order() {
        assert_eq!(age_choices(7), AGE_PRESETS);
        assert_eq!(age_choices(10), [1, 3, 7, 10, 14, 30, 60, 90, 180, 365]);
        assert_eq!(age_choices(400).last(), Some(&400));
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
}

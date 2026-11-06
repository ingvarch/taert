//! Workspace Screen (Main Menu).
//! Main menu + Clean Caches Submenu + Start Confirmations.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use super::analyze::SortOrder;
use super::components::button::Button;
use super::components::chrome::{AppChrome, SPLIT_MIN_WIDTH, split_panes};
use super::components::frame::{FRAME_ROWS, FRAME_SIDES, card};
use super::components::hints::{Hint, PrimaryAction};
use super::components::list::ListRow;
use super::components::modal::Modal;
use super::components::text::fit_width;
use super::env::ColorDepth;
use super::folder_preview::folder_rows;
use super::history::recent_runs_card;
use super::settings::{SettingsState, appearance_label};
use super::system_status::{SystemStatusState, load_summary, storage_summary};
use super::theme::Theme;
use crate::core::bytes::format_bytes;
use crate::core::cache::{FolderSnapshot, StorageCache};
use crate::core::history::{HistoryLog, RunRecord};
use crate::core::layout::Layout;
use crate::core::profiler::SystemProfiler;
use crate::core::safety::Safety;
use crate::core::schedule::ScheduleRule;
use crate::core::schedule::format_weekday_and_time;
use crate::core::stores::{Config, IgnoreStore};
use crate::core::sysinfo::SysInfo;

/// The values the Settings row of the menu shows.
pub struct SettingsSummary {
    schedule: String,
    folders: usize,
    protected: usize,
    appearance: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MainMenuChoice {
    CleanCaches,    // 1
    ProjectCleanup, // 2
    DockerCleanup,  // 3
    UninstallApps,  // 4
    Homebrew,       // 5
    Doctor,         // 6
    StartupItems,   // 7
    SlimApps,       // 8
    Compress,       // 9
    ExploreStorage, // 0
    LargeFiles,     // no number key
    SystemStatus,
    History,
    Settings,
}

const RECENT_RUNS: usize = 3;

const MENU_SECTIONS: [(&str, &[usize]); 4] = [
    ("Cleanup", &[0, 1, 2, 3, 4]),
    ("Optimize", &[5, 6, 7, 8]),
    ("Inspect", &[9, 10, 11]),
    ("taert", &[12, 13]),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanSubcategory {
    All,
    Developer,
    UserJunk,
    AiTools,
    Orphans,
    Trash,
}

impl CleanSubcategory {
    pub fn title(self) -> &'static str {
        match self {
            Self::All => "All categories",
            Self::Developer => "Developer caches",
            Self::UserJunk => "User junk",
            Self::AiTools => "AI tools",
            Self::Orphans => "Orphaned files",
            Self::Trash => "Trash",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartConfirmKind {
    ExploreHome,
    ProjectCleanup(usize),
}

pub enum WorkspaceView {
    MainMenu,
    CleanSubmenu { selected: usize },
    Confirm(StartConfirmKind, bool),
}

pub struct WorkspaceState {
    pub selected_index: usize,
    pub view: WorkspaceView,
    pub items: Vec<(MainMenuChoice, &'static str, &'static str, &'static str)>,
    pub clean_options: Vec<(CleanSubcategory, &'static str, &'static str)>,
    pub disk_status: Option<String>,
    pub system: Option<Box<SystemStatusState>>,
    pub largest: Option<FolderSnapshot>,
    /// The folder the Explore Storage row reports on.
    pub home: PathBuf,
    /// The newest runs, newest first, for the History row's pane.
    /// The newest runs for the History pane, or why they could not be read. `None` until read.
    pub recent_runs: Option<Result<Vec<RunRecord>, String>>,
    /// What the Settings pane shows. `None` until read.
    pub settings: Option<SettingsSummary>,
    pane_visible: Cell<bool>,
}

impl Default for WorkspaceState {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkspaceState {
    pub fn new() -> Self {
        let disk_status = SysInfo::disk_usage(Path::new("/")).map(|d| {
            let free_fraction = if d.total_bytes > 0 {
                d.available_bytes as f64 / d.total_bytes as f64
            } else {
                0.0
            };
            let free_pct = (free_fraction * 100.0).round();
            format!(
                "{} available · {:.0}% free",
                format_bytes(d.available_bytes),
                free_pct
            )
        });

        Self {
            selected_index: 0,
            view: WorkspaceView::MainMenu,
            disk_status,
            system: Some(Box::new(SystemStatusState::with_status(
                SystemProfiler::current_full_status(),
            ))),
            largest: None,
            home: Safety::home_dir(),
            recent_runs: None,
            settings: None,
            pane_visible: Cell::new(false),
            items: vec![
                (
                    MainMenuChoice::CleanCaches,
                    "Clean caches",
                    "Choose tools and individual paths to remove",
                    "Choose tools and individual paths to remove.\n\nIncludes developer caches, user junk, AI tool caches\nand the Trash. You'll review every item before\nanything is removed.",
                ),
                (
                    MainMenuChoice::ProjectCleanup,
                    "Project cleanup",
                    "Review dependencies and generated build artifacts",
                    "Scan project directories for build targets, dependencies,\nand intermediate artifacts. Reclaim gigabytes of space\nacross multiple repositories at once.",
                ),
                (
                    MainMenuChoice::DockerCleanup,
                    "Docker cleanup",
                    "Remove what no container uses",
                    "Find images, volumes and networks that no container uses,\nplus the build cache. Volumes can hold real data, so they\nare never selected for you.",
                ),
                (
                    MainMenuChoice::UninstallApps,
                    "Uninstall apps",
                    "Remove apps with everything they left behind",
                    "Remove an app together with its settings, caches, login\nitems and background helpers. You'll review every file\nbefore anything is removed.",
                ),
                (
                    MainMenuChoice::Homebrew,
                    "Homebrew",
                    "Review installed formulae and casks",
                    "List Homebrew formulae and casks by the space they take.\nSee which ones you installed, which are only dependencies,\nand what uses them. Remove a formula that nothing uses.",
                ),
                (
                    MainMenuChoice::Doctor,
                    "Doctor",
                    "Explain what is slowing the Mac down",
                    "Watch the Mac for a few seconds to find what is slow,\nhot, or draining battery. Lists named causes and evidence,\nand suggests what to do. Changes nothing on its own.",
                ),
                (
                    MainMenuChoice::StartupItems,
                    "Startup items",
                    "Review and switch off login and background items",
                    "Inspect launchd agents, daemons, and classic login items.\nDisable unneeded background services to save battery and\nfree CPU cycles. You'll review every change before\nanything is switched off.",
                ),
                (
                    MainMenuChoice::SlimApps,
                    "Slim apps",
                    "Remove Intel code from universal apps",
                    "Strip Intel (x86_64) code from universal apps that you only\nrun on Apple Silicon. Slices of apps you keep stay untouched,\nand the original app goes to the Trash.",
                ),
                (
                    MainMenuChoice::Compress,
                    "Compress",
                    "Free space with transparent APFS compression",
                    "Store files compressed on APFS and unpack on read.\nApps and tools keep working with the same bytes.\nReview candidates and estimated savings before\ncompressing.",
                ),
                (
                    MainMenuChoice::ExploreStorage,
                    "Explore storage",
                    "Browse folders and find what takes up space",
                    "Interactive directory size analyzer with breadcrumb\nnavigation. Inspect deep subfolders and reveal large\nfiles directly in Finder.",
                ),
                (
                    MainMenuChoice::LargeFiles,
                    "Large files",
                    "Find the largest and oldest files",
                    "Find the largest and oldest files in your home folder.\nReview deep files before removing them to the Trash,\nor reveal them in Finder.",
                ),
                (
                    MainMenuChoice::SystemStatus,
                    "System status",
                    "Inspect memory and available disk space",
                    "Live macOS hardware, memory pressure, and APFS\nstorage telemetry with visual breakdown.",
                ),
                (
                    MainMenuChoice::History,
                    "History",
                    "See what taert removed and changed",
                    "Every removal and change taert made, run by run.\nOpen a run to see each path, what happened to it,\nand how much space it took.",
                ),
                (
                    MainMenuChoice::Settings,
                    "Settings",
                    "Project folders, the schedule, protection, and more",
                    "Choose the folders Project cleanup scans, when scheduled\ncleanup runs, which paths taert never touches, and how\nit looks. Check permissions and find taert's files.",
                ),
            ],
            clean_options: vec![
                (
                    CleanSubcategory::All,
                    CleanSubcategory::All.title(),
                    "Developer caches, user junk, AI tools, orphaned files, and Trash",
                ),
                (
                    CleanSubcategory::Developer,
                    CleanSubcategory::Developer.title(),
                    "Package managers and build tools",
                ),
                (
                    CleanSubcategory::UserJunk,
                    CleanSubcategory::UserJunk.title(),
                    "Logs, Xcode files, app and browser caches",
                ),
                (
                    CleanSubcategory::AiTools,
                    CleanSubcategory::AiTools.title(),
                    "Caches and logs from local AI tools",
                ),
                (
                    CleanSubcategory::Orphans,
                    CleanSubcategory::Orphans.title(),
                    "Leftovers of apps that are gone",
                ),
                (
                    CleanSubcategory::Trash,
                    CleanSubcategory::Trash.title(),
                    "Review items already in the Trash",
                ),
            ],
        }
    }

    pub fn at(row: usize, submenu_row: Option<usize>) -> Self {
        let mut state = Self::new();
        state.selected_index = row;
        if let Some(selected) = submenu_row {
            state.view = WorkspaceView::CleanSubmenu { selected };
        }
        state
    }

    /// True until `load_history` has run for this menu.
    pub fn history_pending(&self) -> bool {
        self.recent_runs.is_none()
    }

    /// Reads the newest runs for the pane.
    pub fn load_history(&mut self, log: &HistoryLog) {
        let runs = log.load_runs().map_err(|error| error.to_string());
        self.recent_runs = Some(runs.map(|mut runs| {
            runs.reverse();
            runs.truncate(RECENT_RUNS);
            runs
        }));
    }

    /// True until `load_settings` has run for this menu.
    pub fn settings_pending(&self) -> bool {
        self.settings.is_none()
    }

    /// Reads the settings for the pane.
    pub fn load_settings(&mut self, config: &Config, ignore: &IgnoreStore, layout: &Layout) {
        self.load_settings_at(config, ignore, layout, SystemTime::now());
    }

    /// Like `load_settings`, with the next run counted from `now`.
    pub fn load_settings_at(
        &mut self,
        config: &Config,
        ignore: &IgnoreStore,
        layout: &Layout,
        now: SystemTime,
    ) {
        let saved = SettingsState::new(config, ignore, layout);
        let schedule =
            config
                .schedule
                .as_ref()
                .map(|sched| match ScheduleRule::from_config(sched) {
                    Ok(rule) => format!(
                        "{} · next run {}",
                        rule.describe(),
                        format_weekday_and_time(rule.next_run(now))
                    ),
                    Err(message) => message,
                });
        let appearance = saved.appearance;
        self.settings = Some(SettingsSummary {
            schedule: schedule.unwrap_or_else(|| "Off".to_string()),
            folders: saved.folders.items.len(),
            protected: saved.protected.items.len(),
            appearance: appearance_label(appearance),
        });
    }

    pub fn submenu_row(&self) -> Option<usize> {
        match self.view {
            WorkspaceView::CleanSubmenu { selected } => Some(selected),
            _ => None,
        }
    }

    pub fn tick(&mut self, cache: &Arc<RwLock<StorageCache>>) {
        let choice = self.current_main_choice();
        let live = self.pane_visible.get() && choice == MainMenuChoice::SystemStatus;
        if let Some(system) = &mut self.system {
            system.set_live(live);
            system.tick();
        }

        if choice == MainMenuChoice::ExploreStorage && self.largest.is_none() {
            self.largest = cache.read().unwrap().get_snapshot(&self.home);
            if let Some(snapshot) = &mut self.largest {
                SortOrder::Size.sort_children(&mut snapshot.children);
            }
        }
    }

    pub fn next(&mut self) {
        match &mut self.view {
            WorkspaceView::MainMenu => {
                if self.selected_index + 1 < self.items.len() {
                    self.selected_index += 1;
                } else {
                    self.selected_index = 0;
                }
            }
            WorkspaceView::CleanSubmenu { selected } => {
                if *selected + 1 < self.clean_options.len() {
                    *selected += 1;
                } else {
                    *selected = 0;
                }
            }
            WorkspaceView::Confirm(_, start_focused) => {
                *start_focused = !*start_focused;
            }
        }
    }

    pub fn previous(&mut self) {
        match &mut self.view {
            WorkspaceView::MainMenu => {
                if self.selected_index > 0 {
                    self.selected_index -= 1;
                } else {
                    self.selected_index = self.items.len().saturating_sub(1);
                }
            }
            WorkspaceView::CleanSubmenu { selected } => {
                if *selected > 0 {
                    *selected -= 1;
                } else {
                    *selected = self.clean_options.len().saturating_sub(1);
                }
            }
            WorkspaceView::Confirm(_, start_focused) => {
                *start_focused = !*start_focused;
            }
        }
    }

    pub fn move_to_start(&mut self) {
        match &mut self.view {
            WorkspaceView::MainMenu => self.selected_index = 0,
            WorkspaceView::CleanSubmenu { selected } => *selected = 0,
            WorkspaceView::Confirm(_, start_focused) => *start_focused = false,
        }
    }

    pub fn move_to_end(&mut self) {
        match &mut self.view {
            WorkspaceView::MainMenu => self.selected_index = self.items.len().saturating_sub(1),
            WorkspaceView::CleanSubmenu { selected } => {
                *selected = self.clean_options.len().saturating_sub(1);
            }
            WorkspaceView::Confirm(_, start_focused) => *start_focused = true,
        }
    }

    pub fn select_by_number(&mut self, num: usize) -> Option<MainMenuChoice> {
        if matches!(self.view, WorkspaceView::MainMenu) && num >= 1 && num <= self.items.len() {
            self.selected_index = num - 1;
            Some(self.items[self.selected_index].0)
        } else {
            None
        }
    }

    pub fn current_main_choice(&self) -> MainMenuChoice {
        self.items[self.selected_index].0
    }

    pub fn open_clean_submenu(&mut self) {
        self.view = WorkspaceView::CleanSubmenu { selected: 0 };
    }

    pub fn open_confirmation(&mut self, kind: StartConfirmKind) {
        self.view = WorkspaceView::Confirm(kind, true);
    }
}

pub fn render_workspace(frame: &mut Frame, state: &WorkspaceState, theme: &Theme) {
    let is_submenu = state.submenu_row().is_some();
    let crumbs = if is_submenu {
        vec!["taert", "Clean caches"]
    } else {
        vec!["taert"]
    };

    let status_line = state
        .disk_status
        .as_ref()
        .map(|s| Line::from(vec![Span::styled(s.clone(), theme.callout())]));

    let hints = vec![
        Hint::new("↑↓", "Move"),
        Hint::new("⎋", if is_submenu { "Back" } else { "Quit" }),
        Hint::new("?", "More"),
    ];

    let mut chrome = AppChrome::new(crumbs)
        .hints(hints)
        .primary(PrimaryAction::new("Open", theme.g.key_enter))
        .behind_dialog(matches!(state.view, WorkspaceView::Confirm(..)));

    if let Some(st) = status_line {
        chrome = chrome.status(st);
    }

    if !is_submenu {
        chrome = chrome.full_width();
    }

    let content_rect = chrome.render(frame, theme);
    let split = !is_submenu && content_rect.width >= SPLIT_MIN_WIDTH;
    state.pane_visible.set(split);

    if let Some(selected) = state.submenu_row() {
        render_submenu_content(frame, content_rect, state, selected, theme);
    } else if split {
        render_split_view(frame, content_rect, state, theme);
    } else {
        render_regular_menu(frame, content_rect, state, theme);
    }

    // Modal Confirmation Dialog (if open)
    if let WorkspaceView::Confirm(kind, start_focused) = &state.view {
        render_start_confirmation(frame, *kind, *start_focused, theme);
    }
}

fn menu_icon(choice: MainMenuChoice, theme: &Theme) -> &str {
    match choice {
        MainMenuChoice::CleanCaches => theme.g.icon_clean,
        MainMenuChoice::ProjectCleanup => theme.g.icon_projects,
        MainMenuChoice::DockerCleanup => theme.g.icon_docker,
        MainMenuChoice::UninstallApps => theme.g.icon_uninstall,
        MainMenuChoice::Homebrew => theme.g.package,
        MainMenuChoice::Doctor => theme.g.icon_doctor,
        MainMenuChoice::StartupItems => theme.g.icon_startup,
        MainMenuChoice::SlimApps => theme.g.icon_uninstall,
        MainMenuChoice::Compress => theme.g.icon_clean,
        MainMenuChoice::ExploreStorage => theme.g.icon_storage,
        MainMenuChoice::LargeFiles => theme.g.icon_storage,
        MainMenuChoice::SystemStatus => theme.g.icon_status,
        MainMenuChoice::History => theme.g.icon_history,
        MainMenuChoice::Settings => theme.g.icon_prefs,
    }
}

fn section_color(title: &str, theme: &Theme) -> Color {
    match title {
        "Cleanup" => theme.p.accent,
        "Optimize" => theme.p.orange,
        "Inspect" => theme.p.teal,
        _ => theme.p.purple,
    }
}

fn menu_row(state: &WorkspaceState, index: usize, width: usize, theme: &Theme) -> Line<'static> {
    let (choice, title, _, _) = &state.items[index];
    let is_selected = index == state.selected_index;

    let icon_style = if is_selected {
        theme.accent_text().add_modifier(Modifier::BOLD)
    } else {
        theme.callout()
    };

    ListRow::new(vec![Span::styled(title.to_string(), theme.body())])
        .leading(Span::styled(
            menu_icon(*choice, theme).to_string(),
            icon_style,
        ))
        .chevron(true)
        .selected(is_selected)
        .render_line(width, theme)
}

/// Menu lines with a mark on each line that holds a menu row.
struct MenuLines {
    lines: Vec<Line<'static>>,
    is_row: Vec<bool>,
    selected: usize,
}

impl MenuLines {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            is_row: Vec::new(),
            selected: 0,
        }
    }

    fn text(&mut self, line: Line<'static>) {
        self.lines.push(line);
        self.is_row.push(false);
    }

    /// Adds one frame. `body` holds `(line, row index)` pairs, with `None` for a description line.
    fn frame(
        &mut self,
        title: &str,
        body: Vec<(Line<'static>, Option<usize>)>,
        width: usize,
        theme: &Theme,
        selected_index: usize,
    ) {
        let color = section_color(title, theme);
        let first = self.lines.len() + 1;
        for (k, (_, row)) in body.iter().enumerate() {
            if *row == Some(selected_index) {
                self.selected = first + k;
            }
        }
        let marks: Vec<bool> = body.iter().map(|(_, row)| row.is_some()).collect();
        let rows = body.into_iter().map(|(line, _)| line).collect();
        self.lines
            .extend(card(title, Vec::new(), rows, width, 0, color, theme));
        self.is_row.push(false);
        self.is_row.extend(marks);
        self.is_row.push(false);
    }

    /// Cuts the lines to `height` around the selected row and says how many rows are out of view.
    fn window(self, height: usize, theme: &Theme) -> Vec<Line<'static>> {
        let total = self.lines.len();
        if total <= height || height == 0 {
            return self.lines;
        }
        let offset = self.selected.saturating_sub(height / 2).min(total - height);
        let last = offset + height - 1;
        let above = self.is_row[..=offset].iter().filter(|&&r| r).count();
        let below = self.is_row[last..].iter().filter(|&&r| r).count();
        let hidden_above = offset > 0 && above > 0;
        let hidden_below = last < total - 1 && below > 0;

        let mut lines: Vec<Line<'static>> =
            self.lines.into_iter().skip(offset).take(height).collect();
        if hidden_above {
            lines[0] = hint_line(format!("{above} more above"), theme);
        }
        if hidden_below {
            lines[height - 1] = hint_line(format!("{below} more below"), theme);
        }
        lines
    }
}

fn hint_line(text: String, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(text, theme.footnote()))
}

fn render_regular_menu(frame: &mut Frame, area: Rect, state: &WorkspaceState, theme: &Theme) {
    let width = area.width as usize;
    let inner = width.saturating_sub(FRAME_SIDES);

    let mut menu = MenuLines::new();
    menu.text(Line::from(Span::styled(
        "Your Mac, crystal clean.",
        theme.callout(),
    )));
    menu.text(Line::from(""));

    for (title, indices) in MENU_SECTIONS {
        let mut rows = Vec::new();
        for &i in indices {
            rows.push((menu_row(state, i, inner, theme), Some(i)));
            if i == state.selected_index {
                rows.push((render_desc_line(state.items[i].2, inner, theme), None));
            }
        }
        menu.frame(title, rows, width, theme, state.selected_index);
    }

    let lines = menu.window(area.height as usize, theme);
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_desc_line(desc: &str, width: usize, theme: &Theme) -> Line<'static> {
    let indent = "     "; // bar (2) + leading (3)
    let desc_style = theme.callout();
    let desc_w = desc.width();
    let pad = width.saturating_sub(indent.len() + desc_w);

    let mut spans = vec![
        Span::raw(indent),
        Span::styled(desc.to_string(), desc_style),
        Span::raw(" ".repeat(pad)),
    ];

    if theme.env.depth == ColorDepth::NoColor {
        for s in &mut spans {
            s.style = s.style.add_modifier(Modifier::REVERSED);
        }
    } else {
        for s in &mut spans {
            s.style = s.style.bg(theme.p.selection);
        }
    }

    Line::from(spans)
}

fn render_split_view(frame: &mut Frame, area: Rect, state: &WorkspaceState, theme: &Theme) {
    let (sidebar_area, pane) = split_panes(area);

    let width = sidebar_area.width as usize;
    let inner = width.saturating_sub(FRAME_SIDES);
    let mut sidebar = MenuLines::new();
    let mut detail_color = theme.p.accent;
    for (title, indices) in MENU_SECTIONS {
        if indices.contains(&state.selected_index) {
            detail_color = section_color(title, theme);
        }
        let rows = indices
            .iter()
            .map(|&i| (menu_row(state, i, inner, theme), Some(i)))
            .collect();
        sidebar.frame(title, rows, width, theme, state.selected_index);
    }
    let sidebar = sidebar.window(sidebar_area.height as usize, theme);
    frame.render_widget(Paragraph::new(sidebar), sidebar_area);

    let (choice, title, _, long_desc) = &state.items[state.selected_index];

    let mut about: Vec<Line<'static>> = long_desc
        .lines()
        .map(|line| Line::from(Span::styled(line.to_string(), theme.callout())))
        .collect();
    about.push(Line::default());
    about.push(Line::from(Span::styled(
        format!("{} Open", theme.g.key_enter),
        theme.headline(),
    )));

    let pane_w = pane.width as usize;
    let heading = format!("{}  {title}", menu_icon(*choice, theme));
    let mut lines = card(&heading, Vec::new(), about, pane_w, 0, detail_color, theme);

    let used = lines.len() + 1;
    let data = detail_data(
        state,
        *choice,
        pane_w,
        (pane.height as usize).saturating_sub(used),
        theme,
    );
    if !data.is_empty() {
        lines.push(Line::default());
        lines.extend(data);
    }
    frame.render_widget(Paragraph::new(lines), pane);
}

fn detail_data(
    state: &WorkspaceState,
    choice: MainMenuChoice,
    width: usize,
    height: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let Some(system) = &state.system else {
        return Vec::new();
    };

    match choice {
        MainMenuChoice::CleanCaches
        | MainMenuChoice::ProjectCleanup
        | MainMenuChoice::Compress
        | MainMenuChoice::SlimApps
        | MainMenuChoice::LargeFiles
        | MainMenuChoice::UninstallApps
        | MainMenuChoice::Homebrew => storage_summary(system, width, theme),
        MainMenuChoice::ExploreStorage => {
            let mut lines = storage_summary(system, width, theme);
            lines.push(Line::default());
            match &state.largest {
                Some(snapshot) => {
                    let rows = height.saturating_sub(lines.len() + FRAME_ROWS);
                    let inner = width.saturating_sub(FRAME_SIDES);
                    let items = folder_rows(snapshot, inner, rows, theme);
                    let color = theme.p.teal;
                    let title = "Largest in home folder";
                    lines.extend(card(title, Vec::new(), items, width, 0, color, theme));
                }
                None => lines.push(Line::from(Span::styled(
                    "Open Explore storage to see the largest items in your home folder.",
                    theme.callout(),
                ))),
            }
            lines
        }
        MainMenuChoice::SystemStatus => load_summary(system, width, height, theme),
        MainMenuChoice::History => state
            .recent_runs
            .as_ref()
            .map_or_else(Vec::new, |runs| recent_runs_card(runs, width, theme)),
        MainMenuChoice::Settings => state
            .settings
            .as_ref()
            .map_or_else(Vec::new, |settings| settings_card(settings, width, theme)),
        MainMenuChoice::DockerCleanup | MainMenuChoice::Doctor | MainMenuChoice::StartupItems => {
            Vec::new()
        }
    }
}

/// A frame with the settings, for the pane beside the main menu.
fn settings_card(settings: &SettingsSummary, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(FRAME_SIDES);
    let rows = [
        ("Scheduled cleanup", settings.schedule.clone()),
        ("Project folders", settings.folders.to_string()),
        ("Protected paths", settings.protected.to_string()),
        ("Appearance", settings.appearance.clone()),
    ]
    .into_iter()
    .map(|(name, value)| {
        let value = fit_width(&value, inner.saturating_sub(name.width() + 2));
        let value = value.trim_end().to_string();
        let gap = inner.saturating_sub(name.width() + value.width());
        Line::from(vec![
            Span::styled(name, theme.callout()),
            Span::raw(" ".repeat(gap)),
            Span::styled(value, theme.body()),
        ])
    })
    .collect::<Vec<_>>();
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

fn render_submenu_content(
    frame: &mut Frame,
    area: Rect,
    state: &WorkspaceState,
    selected: usize,
    theme: &Theme,
) {
    let width = area.width as usize;
    let inner = width.saturating_sub(FRAME_SIDES);

    let mut rows = Vec::new();
    for (i, (subcat, title, desc)) in state.clean_options.iter().enumerate() {
        let is_selected = i == selected;
        let icon = match subcat {
            CleanSubcategory::All => theme.g.icon_clean,
            CleanSubcategory::Developer => theme.g.icon_projects,
            CleanSubcategory::UserJunk => theme.g.check_off,
            CleanSubcategory::AiTools => "◎",
            CleanSubcategory::Orphans => theme.g.icon_uninstall,
            CleanSubcategory::Trash => theme.g.key_backspace,
        };

        let icon_style = if is_selected {
            theme.accent_text().add_modifier(Modifier::BOLD)
        } else {
            theme.callout()
        };

        let mut row = ListRow::new(vec![Span::styled(*title, theme.body())])
            .leading(Span::styled(icon, icon_style))
            .selected(is_selected);

        if *subcat == CleanSubcategory::All {
            row = row.detail(Span::styled("Recommended", theme.footnote()));
        }

        rows.push(row.render_line(inner, theme));

        if is_selected {
            rows.push(render_desc_line(desc, inner, theme));
        }
    }

    let mut lines = vec![
        Line::from(Span::styled(
            "Scan first. Choose what to remove afterward.",
            theme.callout(),
        )),
        Line::from(""),
    ];
    lines.extend(card(
        "Clean caches",
        Vec::new(),
        rows,
        width,
        0,
        theme.p.accent,
        theme,
    ));

    frame.render_widget(Paragraph::new(lines), area);
}

fn render_start_confirmation(
    frame: &mut Frame,
    kind: StartConfirmKind,
    start_focused: bool,
    theme: &Theme,
) {
    let (title, message) = match kind {
        StartConfirmKind::ExploreHome => (
            "Explore your home folder?",
            "Measuring every folder can take a few minutes.".to_string(),
        ),
        StartConfirmKind::ProjectCleanup(count) => (
            "Scan your project folders?",
            if count == 1 {
                "Searching 1 folder for build artifacts can take a few minutes.".to_string()
            } else {
                format!("Searching {count} folders for build artifacts can take a few minutes.")
            },
        ),
    };

    Modal::new(title)
        .message(vec![Line::from(message)])
        .buttons(vec![
            Button::new("Cancel").focused(!start_focused),
            Button::primary("Start").focused(start_focused),
        ])
        .hints(Line::from(vec![
            Span::styled("Enter", theme.callout()),
            Span::styled(" select   ", theme.footnote()),
            Span::styled("Esc", theme.callout()),
            Span::styled(" back", theme.footnote()),
        ]))
        .render(frame, frame.area(), theme);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::apps::testing::sandbox;
    use crate::core::schedule::ScheduleFrequency;
    use crate::core::stores::ScheduleConfig;

    fn local(year: i32, month: i32, day: i32, hour: i32, minute: i32) -> SystemTime {
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        tm.tm_year = year - 1900;
        tm.tm_mon = month - 1;
        tm.tm_mday = day;
        tm.tm_hour = hour;
        tm.tm_min = minute;
        tm.tm_isdst = -1;
        let secs = unsafe { libc::mktime(&mut tm) };
        SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64)
    }

    fn weekly() -> ScheduleConfig {
        ScheduleConfig {
            every: ScheduleFrequency::Week,
            categories: vec!["dev".to_string()],
            min_reclaim: 1_000_000_000,
            last_skip: None,
            ..Default::default()
        }
    }

    /// The schedule line of the Settings pane, with no run in the history.
    fn schedule_summary(config: &Config) -> String {
        let (_temp, layout) = sandbox();
        let mut state = WorkspaceState::new();
        state.load_history(&HistoryLog::new(layout.history_path()));
        state.load_settings(config, &IgnoreStore::default(), &layout);
        state.settings.expect("the settings").schedule
    }

    #[test]
    fn the_settings_pane_says_off_without_a_schedule() {
        assert_eq!(schedule_summary(&Config::default()), "Off");
    }

    #[test]
    fn the_settings_pane_describes_the_rule_and_the_next_run() {
        let config = Config {
            schedule: Some(ScheduleConfig {
                at: "21:30".into(),
                days: vec!["mon".into(), "thu".into()],
                ..weekly()
            }),
            ..Config::default()
        };

        let (_temp, layout) = sandbox();
        let mut state = WorkspaceState::new();

        state.load_settings_at(
            &config,
            &IgnoreStore::default(),
            &layout,
            local(2026, 11, 10, 10, 0),
        );

        assert_eq!(
            state.settings.expect("the settings").schedule,
            "Every week on Mon, Thu at 21:30 · next run Thu 2026-11-12 21:30"
        );
    }

    #[test]
    fn the_settings_pane_gives_the_reason_when_the_saved_rule_is_refused() {
        let config = Config {
            schedule: Some(ScheduleConfig {
                at: "25:99".into(),
                ..weekly()
            }),
            ..Config::default()
        };

        assert_eq!(
            schedule_summary(&config),
            "Time 25:99 is not HH:MM between 00:00 and 23:59."
        );
    }
}

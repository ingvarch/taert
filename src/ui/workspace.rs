//! Workspace Screen (Main Menu).
//! Main menu + Clean Caches Submenu + Start Confirmations.

use std::cell::Cell;
use std::sync::{Arc, RwLock};

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use super::analyze::SortOrder;
use super::components::button::Button;
use super::components::chrome::AppChrome;
use super::components::frame::{FRAME_ROWS, FRAME_SIDES, card};
use super::components::hints::{Hint, PrimaryAction};
use super::components::list::ListRow;
use super::components::modal::Modal;
use super::env::ColorDepth;
use super::folder_preview::folder_rows;
use super::system_status::{SystemStatusState, load_summary, storage_summary};
use super::theme::Theme;
use crate::core::bytes::format_bytes;
use crate::core::cache::{FolderSnapshot, StorageCache};
use crate::core::profiler::SystemProfiler;
use crate::core::safety::Safety;
use crate::core::sysinfo::SysInfo;

const SPLIT_MIN_WIDTH: u16 = 100;
const SIDEBAR_WIDTH: u16 = 34;
const PANE_GAP: u16 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MainMenuChoice {
    CleanCaches,    // 1
    ProjectCleanup, // 2
    DockerCleanup,  // 3
    UninstallApps,  // 4
    ExploreStorage, // 5
    SystemStatus,   // 6
    Preferences,    // 7
    ProtectedPaths, // 8
}

const MENU_SECTIONS: [(&str, &[usize]); 3] = [
    ("Cleanup", &[0, 1, 2, 3]),
    ("Inspect", &[4, 5]),
    ("Settings", &[6, 7]),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanSubcategory {
    All,
    Developer,
    UserJunk,
    AiTools,
    Trash,
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
    pane_visible: Cell<bool>,
}

impl Default for WorkspaceState {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkspaceState {
    pub fn new() -> Self {
        let disk_status = SysInfo::current_status().main_disk.map(|d| {
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
                    MainMenuChoice::ExploreStorage,
                    "Explore storage",
                    "Browse folders and find what takes up space",
                    "Interactive directory size analyzer with breadcrumb\nnavigation. Inspect deep subfolders and reveal large\nfiles directly in Finder.",
                ),
                (
                    MainMenuChoice::SystemStatus,
                    "System status",
                    "Inspect memory and available disk space",
                    "Live macOS hardware, memory pressure, and APFS\nstorage telemetry with visual breakdown.",
                ),
                (
                    MainMenuChoice::Preferences,
                    "Preferences",
                    "Change cleanup age, project folders, and protection",
                    "Configure artifact age threshold presets and shortcuts\nto project folders and protected paths.",
                ),
                (
                    MainMenuChoice::ProtectedPaths,
                    "Protected paths",
                    "Choose paths excluded from cleanup",
                    "Manage directories and files permanently protected from\nall automated cleanup and purge operations.",
                ),
            ],
            clean_options: vec![
                (
                    CleanSubcategory::All,
                    "All categories",
                    "Developer caches, user junk, AI tools, and Trash",
                ),
                (
                    CleanSubcategory::Developer,
                    "Developer caches",
                    "Package managers and build tools",
                ),
                (
                    CleanSubcategory::UserJunk,
                    "User junk",
                    "Logs, Xcode files, app and browser caches",
                ),
                (
                    CleanSubcategory::AiTools,
                    "AI tools",
                    "Caches and logs from local AI tools",
                ),
                (
                    CleanSubcategory::Trash,
                    "Trash",
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
            let home = Safety::home_dir();
            self.largest = cache.read().unwrap().get_snapshot(&home);
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
        MainMenuChoice::ExploreStorage => theme.g.icon_storage,
        MainMenuChoice::SystemStatus => theme.g.icon_status,
        MainMenuChoice::Preferences => theme.g.icon_prefs,
        MainMenuChoice::ProtectedPaths => theme.g.icon_protected,
    }
}

fn section_color(title: &str, theme: &Theme) -> Color {
    match title {
        "Cleanup" => theme.p.accent,
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

fn render_regular_menu(frame: &mut Frame, area: Rect, state: &WorkspaceState, theme: &Theme) {
    let width = area.width as usize;
    let inner = width.saturating_sub(FRAME_SIDES);

    let mut lines = vec![
        Line::from(Span::styled("Your Mac, crystal clean.", theme.callout())),
        Line::from(""),
    ];

    for (title, indices) in MENU_SECTIONS {
        let mut rows = Vec::new();
        for &i in indices {
            rows.push(menu_row(state, i, inner, theme));
            if i == state.selected_index {
                rows.push(render_desc_line(state.items[i].2, inner, theme));
            }
        }
        let color = section_color(title, theme);
        lines.extend(card(title, Vec::new(), rows, width, 0, color, theme));
    }

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
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(SIDEBAR_WIDTH),
            Constraint::Length(PANE_GAP),
            Constraint::Min(40),
        ])
        .split(area);

    let width = chunks[0].width as usize;
    let inner = width.saturating_sub(FRAME_SIDES);
    let mut sidebar = Vec::new();
    let mut detail_color = theme.p.accent;
    for (title, indices) in MENU_SECTIONS {
        let color = section_color(title, theme);
        if indices.contains(&state.selected_index) {
            detail_color = color;
        }
        let rows = indices
            .iter()
            .map(|&i| menu_row(state, i, inner, theme))
            .collect();
        sidebar.extend(card(title, Vec::new(), rows, width, 0, color, theme));
    }
    frame.render_widget(Paragraph::new(sidebar), chunks[0]);

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

    let pane = chunks[2];
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
        | MainMenuChoice::UninstallApps => storage_summary(system, width, theme),
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
        MainMenuChoice::DockerCleanup
        | MainMenuChoice::Preferences
        | MainMenuChoice::ProtectedPaths => Vec::new(),
    }
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

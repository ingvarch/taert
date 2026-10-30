//! Workspace Screen (Main Menu).
//! 6 Main Options + Clean Caches Submenu + Start Confirmations.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use super::dialog::{Dialog, DialogButton, DialogHint};
use super::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MainMenuChoice {
    CleanCaches,    // 1
    ProjectCleanup, // 2
    ExploreStorage, // 3
    SystemStatus,   // 4
    Preferences,    // 5
    ProtectedPaths, // 6
}

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
    ProjectCleanup(usize), // folder count
}

pub enum WorkspaceView {
    MainMenu,
    CleanSubmenu { selected: usize },
    Confirm(StartConfirmKind, bool), // (kind, start_is_focused)
}

pub struct WorkspaceState {
    pub selected_index: usize,
    pub view: WorkspaceView,
    pub items: Vec<(MainMenuChoice, &'static str, &'static str)>,
    pub clean_options: Vec<(CleanSubcategory, &'static str, &'static str)>,
}

impl Default for WorkspaceState {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkspaceState {
    pub fn new() -> Self {
        Self {
            selected_index: 0,
            view: WorkspaceView::MainMenu,
            items: vec![
                (
                    MainMenuChoice::CleanCaches,
                    "Clean caches",
                    "Choose tools and individual paths to remove",
                ),
                (
                    MainMenuChoice::ProjectCleanup,
                    "Project cleanup",
                    "Review dependencies and generated build artifacts",
                ),
                (
                    MainMenuChoice::ExploreStorage,
                    "Explore storage",
                    "Browse folders and find what takes up space",
                ),
                (
                    MainMenuChoice::SystemStatus,
                    "System status",
                    "Inspect memory and available disk space",
                ),
                (
                    MainMenuChoice::Preferences,
                    "Preferences",
                    "Change cleanup age, project folders, and protection",
                ),
                (
                    MainMenuChoice::ProtectedPaths,
                    "Protected paths",
                    "Choose paths excluded from cleanup",
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
                    "Logs and generated Xcode files",
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
    let area = frame.area();

    // Framed window with rounded corners
    let window_title = match &state.view {
        WorkspaceView::CleanSubmenu { .. } => " Clean caches ",
        _ => " taert ",
    };

    let window_block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(theme.border)
        .title(Line::from(vec![Span::styled(window_title, theme.title)]))
        .title_alignment(Alignment::Left);

    let inner_area = window_block.inner(area);
    frame.render_widget(window_block, area);

    // Layout inside window
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // Subtitle line
            Constraint::Min(8),    // Menu list
            Constraint::Length(1), // Divider
            Constraint::Length(1), // Footer key hints
        ])
        .split(inner_area);

    let subtitle = match &state.view {
        WorkspaceView::CleanSubmenu { .. } => "Scan first. Choose what to remove afterward.",
        _ => "Your Mac, crystal clean.  /  0.1.0",
    };

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(subtitle, theme.muted))),
        chunks[0],
    );

    // Menu list items
    let mut menu_lines = Vec::new();

    match &state.view {
        WorkspaceView::CleanSubmenu { selected } => {
            for (i, (_subcat, title, desc)) in state.clean_options.iter().enumerate() {
                let is_selected = i == *selected;
                let num_str = format!("{}. ", i + 1);

                if is_selected {
                    menu_lines.push(Line::from(vec![
                        Span::styled("› ", theme.cursor),
                        Span::styled(num_str, theme.title),
                        Span::styled(format!("{:<28}", title), theme.focused_row),
                    ]));
                } else {
                    menu_lines.push(Line::from(vec![
                        Span::styled("  ", theme.muted),
                        Span::styled(num_str, theme.title),
                        Span::styled(format!("{:<28}", title), theme.normal),
                    ]));
                }
                menu_lines.push(Line::from(vec![
                    Span::raw("     "),
                    Span::styled(*desc, theme.muted),
                ]));
            }
        }
        _ => {
            for (i, (_choice, title, desc)) in state.items.iter().enumerate() {
                let is_selected = i == state.selected_index;
                let num_str = format!("{}. ", i + 1);

                if is_selected {
                    menu_lines.push(Line::from(vec![
                        Span::styled("› ", theme.cursor),
                        Span::styled(num_str, theme.title),
                        Span::styled(format!("{:<28}", title), theme.focused_row),
                    ]));
                } else {
                    menu_lines.push(Line::from(vec![
                        Span::styled("  ", theme.muted),
                        Span::styled(num_str, theme.title),
                        Span::styled(format!("{:<28}", title), theme.normal),
                    ]));
                }
                menu_lines.push(Line::from(vec![
                    Span::raw("     "),
                    Span::styled(*desc, theme.muted),
                ]));
            }
        }
    }

    frame.render_widget(Paragraph::new(menu_lines), chunks[1]);

    // Divider line
    let divider = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(theme.border);
    frame.render_widget(divider, chunks[2]);

    // Footer with keyhints
    let footer_text = match state.view {
        WorkspaceView::MainMenu | WorkspaceView::CleanSubmenu { .. } => Line::from(vec![
            Span::styled("↑↓ ", theme.footer_key),
            Span::styled("move  ", theme.footer_desc),
            Span::styled("PgUp/PgDn ", theme.footer_key),
            Span::styled("page  ", theme.footer_desc),
            Span::styled("1–9 ", theme.footer_key),
            Span::styled("open  ", theme.footer_desc),
            Span::styled("Enter ", theme.footer_key),
            Span::styled("open  ", theme.footer_desc),
            Span::styled("Esc/q ", theme.footer_key),
            Span::styled("cancel", theme.footer_desc),
        ]),
        WorkspaceView::Confirm(_, _) => Line::from(vec![
            Span::styled("Enter ", theme.footer_key),
            Span::styled("select  ", theme.footer_desc),
            Span::styled("Esc ", theme.footer_key),
            Span::styled("back", theme.footer_desc),
        ]),
    };
    frame.render_widget(Paragraph::new(footer_text), chunks[3]);

    // Modal Confirmation Dialog (StartConfirmation)
    if let WorkspaceView::Confirm(kind, start_focused) = &state.view {
        render_start_confirmation(frame, *kind, *start_focused, theme);
    }
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

    let focused_idx = if start_focused { 1 } else { 0 };

    Dialog::new(title)
        .with_message(vec![Line::from(message)])
        .with_buttons(
            vec![DialogButton::new("Cancel"), DialogButton::new("Start")],
            Some(focused_idx),
        )
        .with_hints(DialogHint::choice_hints())
        .render(frame, frame.area(), theme);
}

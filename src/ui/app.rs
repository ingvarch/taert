//! Main TUI Application Controller.
//! Handles crossterm terminal initialization, event loop, and screen switching.

use std::io::{self, stdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::{Frame, Terminal};

use super::analyze::{
    AnalyzeConfirmButton, AnalyzeOverlay, AnalyzeState, SortOrder, render_analyze,
};
use super::path_list_editor::{
    PathListEditorState, PathListKind, PathListMode, render_path_list_editor,
};
use super::preferences::{PreferencesFocus, PreferencesState, render_preferences};
use super::progress::{ProgressState, render_progress};
use super::review::{ReviewState, render_review};
use super::select::{ConfirmButton, ModalOverlay, SelectState, render_select};
use super::system_status::{SystemStatusState, render_system_status};
use super::theme::Theme;
use super::workspace::{
    CleanSubcategory, MainMenuChoice, StartConfirmKind, WorkspaceState, WorkspaceView,
    render_workspace,
};
use crate::core::cache::StorageCache;
use crate::core::cleaner::{Cleaner, RemovalProgress, RemovalReport};
use crate::core::docker;
use crate::core::path_utils::prettify_path;
use crate::core::projects_store::ProjectFoldersStore;
use crate::core::purger::{self, PurgeScanEvent, Purger};
use crate::core::safety::Safety;
use crate::core::scanner::{CategoryScan, CategoryScanner, ScanItem, ScanProgressEvent};
use crate::core::stores::{Config, IgnoreStore};

pub enum Screen {
    Workspace(WorkspaceState),
    Scanning(ProgressState, Receiver<ScanMessage>, Arc<AtomicBool>),
    Select(SelectState),
    Review(ReviewState),
    Analyze(Box<AnalyzeState>),
    SystemStatus(Box<SystemStatusState>),
    Preferences(PreferencesState),
    PathListEditor(PathListEditorState),
}

pub enum ScanMessage {
    Progress(ScanProgressEvent),
    PurgeProgress(PurgeScanEvent),
    Finished(CategoryScan),
    Failed(String),
}

pub enum RemovalMessage {
    Progress(RemovalProgress),
    Finished(RemovalReport),
}

/// A screen Esc can return to. It is rebuilt on return, so disk space and counts stay fresh.
#[derive(Debug, Clone, Copy)]
enum ReturnPoint {
    Workspace {
        row: usize,
        submenu_row: Option<usize>,
    },
    Preferences(PreferencesFocus),
}

pub struct App {
    pub theme: Theme,
    pub screen: Screen,
    back_stack: Vec<ReturnPoint>,
    pub ignore: IgnoreStore,
    pub config: Config,
    pub should_quit: bool,
    pub removal_rx: Option<Receiver<RemovalMessage>>,
    pub removal_spinner: usize,
    pub help_open: bool,
    pub storage_cache: Arc<RwLock<StorageCache>>,
}

impl App {
    pub fn new(ignore: IgnoreStore, config: Config) -> Self {
        Self {
            theme: Theme::new(),
            screen: Screen::Workspace(WorkspaceState::new()),
            back_stack: Vec::new(),
            ignore,
            config,
            should_quit: false,
            removal_rx: None,
            removal_spinner: 0,
            help_open: false,
            storage_cache: Arc::new(RwLock::new(StorageCache::new())),
        }
    }

    pub fn with_select(scan: CategoryScan, ignore: IgnoreStore, config: Config) -> Self {
        Self {
            theme: Theme::new(),
            screen: Screen::Select(SelectState::new(scan)),
            back_stack: Vec::new(),
            ignore,
            config,
            should_quit: false,
            removal_rx: None,
            removal_spinner: 0,
            help_open: false,
            storage_cache: Arc::new(RwLock::new(StorageCache::new())),
        }
    }

    pub fn with_analyze(path: std::path::PathBuf, ignore: IgnoreStore, config: Config) -> Self {
        let storage_cache = Arc::new(RwLock::new(StorageCache::new()));
        Self {
            theme: Theme::new(),
            screen: Screen::Analyze(Box::new(AnalyzeState::with_cache(
                path,
                ignore.clone(),
                storage_cache.clone(),
            ))),
            back_stack: Vec::new(),
            ignore,
            config,
            should_quit: false,
            removal_rx: None,
            removal_spinner: 0,
            help_open: false,
            storage_cache,
        }
    }

    pub fn run(&mut self) -> io::Result<()> {
        enable_raw_mode()?;
        let mut stdout = stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let res = self.main_loop(&mut terminal);

        // Restore terminal state
        let _ = disable_raw_mode();
        let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
        let _ = terminal.show_cursor();

        res
    }

    fn main_loop(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> io::Result<()> {
        while !self.should_quit {
            terminal.draw(|frame| {
                let area = frame.area();
                if area.width < 40 || area.height < 12 {
                    let msg = "Terminal too small (need >= 40x12)";
                    let p = ratatui::widgets::Paragraph::new(ratatui::text::Line::from(
                        ratatui::text::Span::styled(msg, self.theme.callout()),
                    ))
                    .alignment(ratatui::layout::Alignment::Center);
                    let y = area.y + area.height / 2;
                    let rect = ratatui::layout::Rect {
                        x: area.x,
                        y,
                        width: area.width,
                        height: 1,
                    };
                    frame.render_widget(p, rect);
                    return;
                }

                self.render_screen(frame);

                if self.help_open {
                    let sections = self.help_sections();
                    super::components::help::HelpSheet::new(sections).render(
                        frame,
                        area,
                        &self.theme,
                    );
                }
            })?;

            self.poll_scan();

            // Tick analyze measuring if active
            if let Screen::Analyze(ref mut analyze) = self.screen {
                analyze.tick();
            }

            // Take the latest system measurements if active
            if let Screen::SystemStatus(ref mut status) = self.screen {
                status.tick();
            }

            if let Screen::Workspace(ref mut workspace) = self.screen {
                workspace.tick(&self.storage_cache);
            }

            // Poll background removal messages
            if let Some(ref rx) = self.removal_rx {
                self.removal_spinner = (self.removal_spinner + 1) % 4;
                while let Ok(msg) = rx.try_recv() {
                    match msg {
                        RemovalMessage::Progress(prog) => {
                            if let Screen::Select(ref mut select) = self.screen {
                                select.overlay = ModalOverlay::Removing {
                                    progress: prog,
                                    spinner_idx: self.removal_spinner,
                                };
                            } else if let Screen::Analyze(ref mut analyze) = self.screen {
                                analyze.overlay = AnalyzeOverlay::Deleting {
                                    progress: prog,
                                    spinner_idx: self.removal_spinner,
                                };
                            }
                        }
                        RemovalMessage::Finished(report) => {
                            if let Screen::Select(ref mut select) = self.screen {
                                // Deselect successfully cleaned items
                                for group in &mut select.scan.groups {
                                    group.items.retain(|item| !item.selected);
                                }
                                self.storage_cache.write().unwrap().clear();
                                select.overlay = ModalOverlay::Outcome(report);
                            } else if let Screen::Analyze(ref mut analyze) = self.screen {
                                analyze.handle_removal_finished(&report);
                                analyze.overlay = AnalyzeOverlay::Outcome(report);
                            }
                            self.removal_rx = None;
                            break;
                        }
                    }
                }
            }

            // Poll user keyboard events
            if event::poll(Duration::from_millis(50))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                self.handle_key(key.code);
            }
        }

        Ok(())
    }

    fn render_screen(&self, frame: &mut Frame) {
        match &self.screen {
            Screen::Workspace(state) => render_workspace(frame, state, &self.theme),
            Screen::Scanning(state, _, _) => render_progress(frame, state, &self.theme),
            Screen::Select(state) => render_select(frame, state, &self.theme),
            Screen::Review(state) => render_review(frame, state, &self.theme),
            Screen::Analyze(state) => render_analyze(frame, state, &self.theme),
            Screen::SystemStatus(state) => render_system_status(frame, state, &self.theme),
            Screen::Preferences(state) => render_preferences(frame, state, &self.theme),
            Screen::PathListEditor(state) => render_path_list_editor(frame, state, &self.theme),
        }
    }

    fn open(&mut self, screen: Screen) {
        match &self.screen {
            Screen::Workspace(state) => self.back_stack.push(ReturnPoint::Workspace {
                row: state.selected_index,
                submenu_row: state.submenu_row(),
            }),
            Screen::Preferences(state) => {
                self.back_stack.push(ReturnPoint::Preferences(state.focus));
            }
            _ => {}
        }
        self.screen = screen;
    }

    fn go_back(&mut self) {
        self.screen = match self.back_stack.pop() {
            Some(ReturnPoint::Workspace { row, submenu_row }) => {
                Screen::Workspace(WorkspaceState::at(row, submenu_row))
            }
            Some(ReturnPoint::Preferences(focus)) => {
                let mut state = PreferencesState::new(&self.config, &self.ignore);
                state.focus = focus;
                Screen::Preferences(state)
            }
            None => Screen::Workspace(WorkspaceState::new()),
        };
    }

    fn poll_scan(&mut self) {
        if let Screen::Scanning(ref mut progress, ref rx, _) = self.screen {
            progress.tick();
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    ScanMessage::PurgeProgress(evt) => {
                        progress.apply_purge(evt);
                    }
                    ScanMessage::Progress(evt) => {
                        progress.apply_category(evt);
                    }
                    ScanMessage::Failed(problem) => {
                        self.screen = Screen::Select(SelectState::unavailable("Docker", problem));
                        break;
                    }
                    ScanMessage::Finished(scan) => {
                        let state = if scan.id == purger::SCAN_ID || scan.id == docker::SCAN_ID {
                            SelectState::with_preserved_selection(scan)
                        } else {
                            SelectState::new(scan)
                        };
                        self.screen = Screen::Select(state);
                        break;
                    }
                }
            }
        }
    }

    fn handle_key(&mut self, code: KeyCode) {
        if self.help_open {
            if matches!(
                code,
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter | KeyCode::Char('?')
            ) {
                self.help_open = false;
            }
            return;
        }

        let is_typing = match &self.screen {
            Screen::Select(select) => select.is_searching,
            Screen::PathListEditor(editor) => matches!(editor.mode, PathListMode::Adding(_)),
            _ => false,
        };
        if !is_typing && code == KeyCode::Char('?') {
            self.help_open = true;
            return;
        }

        match &mut self.screen {
            Screen::Workspace(state) => match &mut state.view {
                WorkspaceView::Confirm(kind, start_focused) => match code {
                    KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                        *start_focused = !*start_focused;
                    }
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('n') | KeyCode::Char('N') => {
                        state.view = WorkspaceView::MainMenu;
                    }
                    KeyCode::Char('s') | KeyCode::Char('S') => {
                        let kind_val = *kind;
                        self.execute_start_confirmed(kind_val);
                    }
                    KeyCode::Enter => {
                        if *start_focused {
                            let kind_val = *kind;
                            self.execute_start_confirmed(kind_val);
                        } else {
                            state.view = WorkspaceView::MainMenu;
                        }
                    }
                    _ => {}
                },

                WorkspaceView::CleanSubmenu { selected } => match code {
                    KeyCode::Up | KeyCode::Char('k') => state.previous(),
                    KeyCode::Down | KeyCode::Char('j') => state.next(),
                    KeyCode::PageUp | KeyCode::Home => state.move_to_start(),
                    KeyCode::PageDown | KeyCode::End => state.move_to_end(),
                    KeyCode::Esc | KeyCode::Char('q') => {
                        state.view = WorkspaceView::MainMenu;
                    }
                    KeyCode::Char(c) if c.is_ascii_digit() => {
                        if let Some(digit) = c.to_digit(10)
                            && (1..=state.clean_options.len()).contains(&(digit as usize))
                        {
                            let subcat = state.clean_options[digit as usize - 1].0;
                            self.start_clean_subcategory(subcat);
                        }
                    }
                    KeyCode::Enter => {
                        let subcat = state.clean_options[*selected].0;
                        self.start_clean_subcategory(subcat);
                    }
                    _ => {}
                },

                WorkspaceView::MainMenu => match code {
                    KeyCode::Up | KeyCode::Char('k') => state.previous(),
                    KeyCode::Down | KeyCode::Char('j') => state.next(),
                    KeyCode::PageUp | KeyCode::Home => state.move_to_start(),
                    KeyCode::PageDown | KeyCode::End => state.move_to_end(),
                    KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
                    KeyCode::Char(c) if c.is_ascii_digit() => {
                        if let Some(digit) = c.to_digit(10)
                            && let Some(choice) = state.select_by_number(digit as usize)
                        {
                            self.handle_main_menu_choice(choice);
                        }
                    }
                    KeyCode::Enter => {
                        let choice = state.current_main_choice();
                        self.handle_main_menu_choice(choice);
                    }
                    _ => {}
                },
            },

            Screen::Scanning(progress, _, cancel_flag) => {
                if matches!(code, KeyCode::Esc | KeyCode::Char('q')) {
                    progress.is_cancelling = true;
                    cancel_flag.store(true, Ordering::Relaxed);
                    self.go_back();
                }
            }

            Screen::Select(state) => {
                match state.overlay {
                    ModalOverlay::Confirm(confirm_btn) => {
                        match code {
                            KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                                state.toggle_confirmation_button();
                            }
                            KeyCode::Esc | KeyCode::Char('q') => {
                                state.close_overlay();
                            }
                            KeyCode::Char('y') | KeyCode::Char('Y') => {
                                self.start_removal();
                            }
                            KeyCode::Enter => {
                                if confirm_btn == ConfirmButton::Remove {
                                    self.start_removal();
                                } else {
                                    state.close_overlay();
                                }
                            }
                            _ => {}
                        }
                        return;
                    }
                    ModalOverlay::Removing { .. } => {
                        // User cannot interrupt removal process
                        return;
                    }
                    ModalOverlay::Outcome(_) => {
                        if matches!(code, KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q')) {
                            state.close_overlay();
                        }
                        return;
                    }
                    ModalOverlay::None => {}
                }

                if state.is_searching {
                    match code {
                        KeyCode::Esc => {
                            state.is_searching = false;
                            state.search_query.clear();
                        }
                        KeyCode::Enter => {
                            state.is_searching = false;
                        }
                        KeyCode::Backspace => {
                            state.search_query.pop();
                        }
                        KeyCode::Char(c) => {
                            state.search_query.push(c);
                        }
                        _ => {}
                    }
                    return;
                }

                match code {
                    KeyCode::Up | KeyCode::Char('k') => state.previous_selectable(),
                    KeyCode::Down | KeyCode::Char('j') => state.next_selectable(),
                    KeyCode::PageUp => state.page_up(),
                    KeyCode::PageDown => state.page_down(),
                    KeyCode::Home => state.move_to_start(),
                    KeyCode::End => state.move_to_end(),
                    KeyCode::Left => state.scroll_path_left(),
                    KeyCode::Right => state.scroll_path_right(),
                    KeyCode::Char(' ') => state.toggle_current(),
                    KeyCode::Char('a') | KeyCode::Char('A') => state.select_all(),
                    KeyCode::Char('S') if state.sort_order.is_some() => {
                        state.set_sort(SortOrder::Size);
                    }
                    KeyCode::Char('N') if state.sort_order.is_some() => {
                        state.set_sort(SortOrder::Name);
                    }
                    KeyCode::Char('D') if state.sort_order.is_some() => {
                        state.set_sort(SortOrder::Date);
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') => state.select_none(),
                    KeyCode::Char('/') => {
                        state.is_searching = true;
                        state.search_query.clear();
                    }
                    KeyCode::Enter => {
                        state.open_confirmation();
                    }
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.go_back();
                    }
                    _ => {}
                }
            }

            Screen::Review(state) => {
                if state.report.is_some() {
                    // Result view
                    if matches!(code, KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q')) {
                        self.go_back();
                        return;
                    }
                    match code {
                        KeyCode::Up | KeyCode::Char('k') => state.scroll_up(),
                        KeyCode::Down | KeyCode::Char('j') => state.scroll_down(),
                        KeyCode::PageUp => state.page_up(),
                        KeyCode::PageDown => state.page_down(),
                        KeyCode::Home => state.move_to_start(),
                        KeyCode::End => state.move_to_end(),
                        _ => {}
                    }
                    return;
                }

                match code {
                    KeyCode::Up | KeyCode::Char('k') => state.scroll_up(),
                    KeyCode::Down | KeyCode::Char('j') => state.scroll_down(),
                    KeyCode::PageUp => state.page_up(),
                    KeyCode::PageDown => state.page_down(),
                    KeyCode::Home => state.move_to_start(),
                    KeyCode::End => state.move_to_end(),
                    KeyCode::Enter => {
                        // Confirm deletion!
                        let items: Vec<_> =
                            state.scan.selected_items().into_iter().cloned().collect();
                        let report = Cleaner::remove_items(&items, &self.ignore);
                        state.report = Some(report);
                    }
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.screen = Screen::Select(SelectState::with_preserved_selection(
                            state.scan.clone(),
                        ));
                    }
                    _ => {}
                }
            }

            Screen::Analyze(state) => {
                if state.measuring_rx.is_some() {
                    match code {
                        KeyCode::Esc | KeyCode::Left | KeyCode::Backspace => {
                            state.cancel_measuring();
                            if state.levels.is_empty() {
                                self.go_back();
                            }
                        }
                        KeyCode::Char('q') => {
                            state.cancel_measuring();
                            self.go_back();
                        }
                        _ => {}
                    }
                    return;
                }

                // Modal Overlays
                match state.overlay {
                    AnalyzeOverlay::ConfirmDelete { ref mut button, .. } => {
                        match code {
                            KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                                *button = match button {
                                    AnalyzeConfirmButton::Cancel => AnalyzeConfirmButton::Delete,
                                    AnalyzeConfirmButton::Delete => AnalyzeConfirmButton::Cancel,
                                };
                            }
                            KeyCode::Esc | KeyCode::Char('q') => {
                                state.overlay = AnalyzeOverlay::None;
                            }
                            KeyCode::Char('y') | KeyCode::Char('Y') => {
                                self.start_analyze_removal();
                            }
                            KeyCode::Enter => {
                                if *button == AnalyzeConfirmButton::Delete {
                                    self.start_analyze_removal();
                                } else {
                                    state.overlay = AnalyzeOverlay::None;
                                }
                            }
                            _ => {}
                        }
                        return;
                    }
                    AnalyzeOverlay::Deleting { .. } => {
                        return;
                    }
                    AnalyzeOverlay::Outcome(_) => {
                        if matches!(code, KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q')) {
                            state.overlay = AnalyzeOverlay::None;
                        }
                        return;
                    }
                    AnalyzeOverlay::None => {}
                }

                match code {
                    KeyCode::Up | KeyCode::Char('k') => state.previous(),
                    KeyCode::Down | KeyCode::Char('j') => state.next(),
                    KeyCode::PageUp => state.page_up(),
                    KeyCode::PageDown => state.page_down(),
                    KeyCode::Home => state.move_to_start(),
                    KeyCode::End => state.move_to_end(),
                    KeyCode::Char(' ') => state.toggle_current(),
                    KeyCode::Char('a') | KeyCode::Char('A') => state.select_all(),
                    KeyCode::Char('n') => state.select_none(),
                    KeyCode::Char('S') => state.set_sort(SortOrder::Size),
                    KeyCode::Char('N') => state.set_sort(SortOrder::Name),
                    KeyCode::Char('D') => state.set_sort(SortOrder::Date),
                    KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace => {
                        state.open_delete_confirmation();
                    }
                    KeyCode::Enter | KeyCode::Right => {
                        state.enter_selected();
                    }
                    KeyCode::Char('r') | KeyCode::Char('R') => {
                        state.refresh_current();
                    }
                    KeyCode::Char('o') | KeyCode::Char('O') => {
                        state.reveal_in_finder();
                    }
                    KeyCode::Esc | KeyCode::Left => {
                        if !state.go_up() {
                            self.go_back();
                        }
                    }
                    KeyCode::Char('q') => self.go_back(),
                    _ => {}
                }
            }

            Screen::SystemStatus(state) => match code {
                KeyCode::Up | KeyCode::Char('k') => state.scroll_up(),
                KeyCode::Down | KeyCode::Char('j') => state.scroll_down(),
                KeyCode::PageUp => state.page_up(),
                KeyCode::PageDown => state.page_down(),
                KeyCode::Home => state.scroll_to_top(),
                KeyCode::End => state.scroll_to_bottom(),
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
                    self.go_back();
                }
                _ => {}
            },

            Screen::Preferences(state) => match code {
                KeyCode::Up | KeyCode::Char('k') => state.previous(),
                KeyCode::Down | KeyCode::Char('j') => state.next(),
                KeyCode::Left => state.step_age_left(&mut self.config),
                KeyCode::Right => state.step_age_right(&mut self.config),
                KeyCode::Enter => match state.focus {
                    PreferencesFocus::AgeThreshold => {}
                    PreferencesFocus::ProjectFolders => {
                        self.open(Screen::PathListEditor(PathListEditorState::new(
                            PathListKind::ProjectFolders,
                            &self.ignore,
                        )));
                    }
                    PreferencesFocus::ProtectedPaths => {
                        self.open(Screen::PathListEditor(PathListEditorState::new(
                            PathListKind::ProtectedPaths,
                            &self.ignore,
                        )));
                    }
                },
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.go_back();
                }
                _ => {}
            },

            Screen::PathListEditor(state) => match &mut state.mode {
                PathListMode::Adding(input) => match code {
                    KeyCode::Esc => state.cancel_input(),
                    KeyCode::Backspace => {
                        input.pop();
                    }
                    KeyCode::Char(c) => {
                        input.push(c);
                    }
                    KeyCode::Enter => {
                        state.commit_add(&mut self.ignore);
                    }
                    _ => {}
                },
                PathListMode::ConfirmRemove(_) => match code {
                    KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                        state.confirm_remove(&mut self.ignore);
                    }
                    KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                        state.cancel_input();
                    }
                    _ => {}
                },
                PathListMode::Normal => match code {
                    KeyCode::Up | KeyCode::Char('k') => state.previous(),
                    KeyCode::Down | KeyCode::Char('j') => state.next(),
                    KeyCode::PageUp => state.page_up(),
                    KeyCode::PageDown => state.page_down(),
                    KeyCode::Home => state.move_to_start(),
                    KeyCode::End => state.move_to_end(),
                    KeyCode::Char('a') | KeyCode::Char('A') => state.start_adding(),
                    KeyCode::Char('d') | KeyCode::Char('D') | KeyCode::Backspace => {
                        state.prompt_remove()
                    }
                    KeyCode::Esc | KeyCode::Char('q') => self.go_back(),
                    _ => {}
                },
            },
        }
    }

    fn help_sections(&self) -> Vec<super::components::help::HelpSection<'static>> {
        use super::components::help::HelpSection;
        use super::components::hints::Hint;

        let navigate = Hint::new("↑↓ / jk", "Navigate");
        let scroll = Hint::new("↑↓ / jk", "Scroll");
        let back = Hint::new("⎋ / q", "Back");
        let general = |mut hints: Vec<Hint<'static>>| {
            hints.push(Hint::new("?", "Toggle help"));
            HelpSection::new("General", hints)
        };
        let sort_section = || {
            HelpSection::new(
                "Sort by",
                vec![
                    Hint::new("⇧S", "Size"),
                    Hint::new("⇧N", "Name"),
                    Hint::new("⇧D", "Date created"),
                ],
            )
        };

        match &self.screen {
            Screen::Workspace(state) => vec![
                HelpSection::new(
                    "Menu",
                    vec![Hint::new("↵", "Select"), Hint::new("1-7", "Quick select")],
                ),
                general(vec![
                    navigate,
                    if state.submenu_row().is_some() {
                        back
                    } else {
                        Hint::new("⎋ / q", "Quit")
                    },
                ]),
            ],
            Screen::Scanning(_, _, _) => vec![
                HelpSection::new("Scanning", vec![Hint::new("⎋ / q", "Cancel scan")]),
                general(Vec::new()),
            ],
            Screen::Select(state) => {
                let mut sections = vec![HelpSection::new(
                    "Select items",
                    vec![
                        Hint::new("␣", "Toggle item"),
                        Hint::new("A", "Select all"),
                        Hint::new("N", "Select none"),
                        Hint::new("/", "Search"),
                        Hint::new("↵", "Review & clean"),
                    ],
                )];
                if state.sort_order.is_some() {
                    sections.push(sort_section());
                }
                sections.push(general(vec![navigate, back]));
                sections
            }
            Screen::Review(_) => vec![
                HelpSection::new("Review", vec![Hint::new("↵", "Confirm clean")]),
                general(vec![scroll, back]),
            ],
            Screen::Analyze(_) => vec![
                HelpSection::new(
                    "Explore",
                    vec![
                        Hint::new("␣", "Toggle select"),
                        Hint::new("A / N", "Select all / none"),
                        Hint::new("↵ / →", "Enter folder"),
                        Hint::new("⎋ / ←", "Up one level"),
                        Hint::new("R", "Refresh folder"),
                        Hint::new("O", "Reveal in Finder"),
                        Hint::new("D / ⌫", "Delete selected"),
                    ],
                ),
                sort_section(),
                general(vec![navigate, Hint::new("q", "Quit explore")]),
            ],
            Screen::SystemStatus(state) => {
                let mut hints = Vec::new();
                if state.can_scroll() {
                    hints.push(scroll);
                }
                hints.push(Hint::new("⎋ / q / ↵", "Back"));
                vec![general(hints)]
            }
            Screen::Preferences(_) => vec![
                HelpSection::new(
                    "Preferences",
                    vec![
                        Hint::new("←→", "Change threshold"),
                        Hint::new("↵", "Edit list"),
                    ],
                ),
                general(vec![navigate, back]),
            ],
            Screen::PathListEditor(_) => vec![
                HelpSection::new(
                    "Path list",
                    vec![
                        Hint::new("A", "Add path"),
                        Hint::new("D / ⌫", "Remove path"),
                    ],
                ),
                general(vec![navigate, back]),
            ],
        }
    }

    fn start_removal(&mut self) {
        if let Screen::Select(ref mut select) = self.screen {
            let selected_items: Vec<_> =
                select.scan.selected_items().into_iter().cloned().collect();
            if selected_items.is_empty() {
                select.close_overlay();
                return;
            }

            let initial_prog = RemovalProgress {
                current_index: 1,
                total_count: selected_items.len(),
                path: selected_items[0].path.clone(),
            };
            select.overlay = ModalOverlay::Removing {
                progress: initial_prog,
                spinner_idx: 0,
            };

            let (tx, rx) = mpsc::channel();
            let ignore = self.ignore.clone();

            thread::spawn(move || {
                let tx_prog = tx.clone();
                let report = Cleaner::remove_items_with_progress(
                    &selected_items,
                    &ignore,
                    Some(move |prog| {
                        let _ = tx_prog.send(RemovalMessage::Progress(prog));
                    }),
                );
                let _ = tx.send(RemovalMessage::Finished(report));
            });

            self.removal_rx = Some(rx);
        }
    }

    fn start_analyze_removal(&mut self) {
        if let Screen::Analyze(ref mut analyze) = self.screen {
            let items_to_remove = analyze.items_to_delete();
            if items_to_remove.is_empty() {
                analyze.overlay = AnalyzeOverlay::None;
                return;
            }

            let scan_items: Vec<ScanItem> = items_to_remove
                .into_iter()
                .map(|child| ScanItem::new(child.path, child.size_bytes, true, false))
                .collect();

            let initial_prog = RemovalProgress {
                current_index: 1,
                total_count: scan_items.len(),
                path: scan_items[0].path.clone(),
            };
            analyze.overlay = AnalyzeOverlay::Deleting {
                progress: initial_prog,
                spinner_idx: 0,
            };

            let (tx, rx) = mpsc::channel();
            let ignore = self.ignore.clone();

            thread::spawn(move || {
                let tx_prog = tx.clone();
                let report = Cleaner::remove_items_with_progress(
                    &scan_items,
                    &ignore,
                    Some(move |prog| {
                        let _ = tx_prog.send(RemovalMessage::Progress(prog));
                    }),
                );
                let _ = tx.send(RemovalMessage::Finished(report));
            });

            self.removal_rx = Some(rx);
        }
    }

    fn handle_main_menu_choice(&mut self, choice: MainMenuChoice) {
        match choice {
            MainMenuChoice::CleanCaches => {
                if let Screen::Workspace(ref mut w) = self.screen {
                    w.open_clean_submenu();
                }
            }
            MainMenuChoice::ProjectCleanup => {
                let folder_count = ProjectFoldersStore::load_or_default().roots.len();
                if let Screen::Workspace(ref mut w) = self.screen {
                    w.open_confirmation(StartConfirmKind::ProjectCleanup(folder_count));
                }
            }
            MainMenuChoice::DockerCleanup => self.launch_docker_scan(),
            MainMenuChoice::ExploreStorage => {
                if let Screen::Workspace(ref mut w) = self.screen {
                    w.open_confirmation(StartConfirmKind::ExploreHome);
                }
            }
            MainMenuChoice::SystemStatus => {
                self.open(Screen::SystemStatus(Box::default()));
            }
            MainMenuChoice::Preferences => {
                self.open(Screen::Preferences(PreferencesState::new(
                    &self.config,
                    &self.ignore,
                )));
            }
            MainMenuChoice::ProtectedPaths => {
                self.open(Screen::PathListEditor(
                    PathListEditorState::new(PathListKind::ProtectedPaths, &self.ignore)
                        .top_level(),
                ));
            }
        }
    }

    fn start_clean_subcategory(&mut self, subcat: CleanSubcategory) {
        match subcat {
            CleanSubcategory::All => self.launch_all_categories_scan(),
            CleanSubcategory::Developer => self.launch_category_scan("dev", "Developer Caches"),
            CleanSubcategory::UserJunk => self.launch_category_scan("junk", "System Junk"),
            CleanSubcategory::AiTools => self.launch_category_scan("ai", "AI Tool Caches"),
            CleanSubcategory::Trash => self.launch_category_scan("trash", "Trash"),
        }
    }

    fn execute_start_confirmed(&mut self, kind: StartConfirmKind) {
        match kind {
            StartConfirmKind::ExploreHome => {
                let home = Safety::home_dir();
                self.open(Screen::Analyze(Box::new(AnalyzeState::with_cache(
                    home,
                    self.ignore.clone(),
                    self.storage_cache.clone(),
                ))));
            }
            StartConfirmKind::ProjectCleanup(_) => {
                let roots = ProjectFoldersStore::load_or_default().existing_roots();
                self.launch_purge_scan(roots);
            }
        }
    }

    fn launch_docker_scan(&mut self) {
        let (tx, rx) = mpsc::channel();

        thread::spawn(move || {
            let message = match docker::DockerClient::connect().and_then(|client| client.scan()) {
                Ok(scan) => ScanMessage::Finished(scan),
                Err(error) => ScanMessage::Failed(error.to_string()),
            };
            let _ = tx.send(message);
        });

        self.open(Screen::Scanning(
            ProgressState::new("Reading Docker").top_level(),
            rx,
            Arc::new(AtomicBool::new(false)),
        ));
    }

    fn launch_category_scan(&mut self, cat_id: &'static str, cat_title: &'static str) {
        let (fwd_tx, fwd_rx) = mpsc::channel();
        let ignore = self.ignore.clone();
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let cancel_clone = Arc::clone(&cancel_flag);

        thread::spawn(move || {
            let (prog_tx, prog_rx) = mpsc::channel();
            let fwd_bridge = fwd_tx.clone();
            let bridge_handle = thread::spawn(move || {
                while let Ok(evt) = prog_rx.recv() {
                    let _ = fwd_bridge.send(ScanMessage::Progress(evt));
                }
            });

            let _ = fwd_tx.send(ScanMessage::Progress(ScanProgressEvent::CategoryStarted {
                title: cat_title.to_string(),
                index: 1,
                total: 1,
            }));

            let scan = CategoryScanner::scan(
                cat_id,
                cat_title,
                &ignore,
                Some(prog_tx),
                Some(cancel_clone),
            );
            let _ = bridge_handle.join();
            let _ = fwd_tx.send(ScanMessage::Finished(scan));
        });

        self.open(Screen::Scanning(
            ProgressState::new(format!("Scanning {cat_title}")),
            fwd_rx,
            cancel_flag,
        ));
    }

    fn launch_all_categories_scan(&mut self) {
        let (fwd_tx, fwd_rx) = mpsc::channel();
        let ignore = self.ignore.clone();
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let cancel_clone = Arc::clone(&cancel_flag);

        thread::spawn(move || {
            let categories = [
                ("dev", "Developer Caches"),
                ("junk", "System Junk"),
                ("ai", "AI Tool Caches"),
                ("trash", "Trash"),
            ];
            let total = categories.len();
            let mut groups = Vec::new();

            for (idx, (id, title)) in categories.iter().enumerate() {
                if cancel_clone.load(Ordering::Relaxed) {
                    break;
                }

                let _ = fwd_tx.send(ScanMessage::Progress(ScanProgressEvent::CategoryStarted {
                    title: title.to_string(),
                    index: idx + 1,
                    total,
                }));

                let (prog_tx, prog_rx) = mpsc::channel();
                let fwd_bridge = fwd_tx.clone();
                let bridge_handle = thread::spawn(move || {
                    while let Ok(evt) = prog_rx.recv() {
                        let _ = fwd_bridge.send(ScanMessage::Progress(evt));
                    }
                });

                let scan = CategoryScanner::scan(
                    id,
                    title,
                    &ignore,
                    Some(prog_tx),
                    Some(Arc::clone(&cancel_clone)),
                );
                let _ = bridge_handle.join();
                groups.extend(scan.groups);
            }

            let scan = CategoryScan {
                id: "clean".to_string(),
                title: "All Caches".to_string(),
                groups,
            };
            let _ = fwd_tx.send(ScanMessage::Finished(scan));
        });

        self.open(Screen::Scanning(
            ProgressState::new("Scanning Developer Caches, 1 of 4"),
            fwd_rx,
            cancel_flag,
        ));
    }

    fn launch_purge_scan(&mut self, roots: Vec<std::path::PathBuf>) {
        let (fwd_tx, fwd_rx) = mpsc::channel();
        let ignore = self.ignore.clone();
        let config = self.config.clone();
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let cancel_clone = Arc::clone(&cancel_flag);

        let initial_headline = if roots.len() == 1 {
            format!("Scanning {}", prettify_path(&roots[0]))
        } else {
            "Scanning project artifacts".to_string()
        };

        thread::spawn(move || {
            let (prog_tx, prog_rx) = mpsc::channel();
            let fwd_bridge = fwd_tx.clone();
            let bridge_handle = thread::spawn(move || {
                while let Ok(evt) = prog_rx.recv() {
                    let _ = fwd_bridge.send(ScanMessage::PurgeProgress(evt));
                }
            });

            let scan = Purger::scan_multiple_roots(
                &roots,
                config.older_than_days,
                &config.extra_artifacts,
                &ignore,
                Some(prog_tx),
                Some(cancel_clone),
            );
            let _ = bridge_handle.join();
            let _ = fwd_tx.send(ScanMessage::Finished(scan));
        });

        self.open(Screen::Scanning(
            ProgressState::new(initial_headline).top_level(),
            fwd_rx,
            cancel_flag,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::scanner::{ScanItem, ToolGroup};
    use crate::ui::analyze::SortOrder;
    use ratatui::backend::TestBackend;
    use std::path::Path;
    use tempfile::tempdir;

    fn analyze_app(dir: &Path) -> App {
        let mut app = App::new(IgnoreStore::default(), Config::default());
        app.screen = Screen::Analyze(Box::new(AnalyzeState::new_sync(
            dir.to_path_buf(),
            IgnoreStore::default(),
        )));
        app
    }

    fn analyze(app: &App) -> &AnalyzeState {
        match &app.screen {
            Screen::Analyze(state) => state,
            _ => panic!("expected the analyze screen"),
        }
    }

    #[test]
    fn help_counts_every_row_of_the_main_menu() {
        let app = App::new(IgnoreStore::default(), Config::default());
        let rows = WorkspaceState::new().items.len();

        let sections = app.help_sections();
        let quick = sections[0]
            .shortcuts
            .iter()
            .find(|h| h.desc == "Quick select");

        assert_eq!(quick.unwrap().key, format!("1-{rows}"));
    }

    #[test]
    fn the_third_quick_key_opens_docker_cleanup() {
        let mut state = WorkspaceState::new();

        assert_eq!(
            state.select_by_number(3),
            Some(MainMenuChoice::DockerCleanup)
        );
    }

    fn select_app(scan: CategoryScan) -> App {
        let mut app = App::new(IgnoreStore::default(), Config::default());
        app.screen = Screen::Select(SelectState::with_preserved_selection(scan));
        app
    }

    fn scan_with_id(id: &str) -> CategoryScan {
        let mut item = ScanItem::new("/nowhere/a".into(), 1, true, false);
        item.modified = None;
        CategoryScan {
            id: id.to_string(),
            title: "List".to_string(),
            groups: vec![ToolGroup {
                tool: "Group".to_string(),
                items: vec![item],
            }],
        }
    }

    fn select(app: &App) -> &SelectState {
        match &app.screen {
            Screen::Select(state) => state,
            _ => panic!("expected the select screen"),
        }
    }

    #[test]
    fn shift_letters_sort_a_docker_table() {
        let mut app = select_app(scan_with_id(docker::SCAN_ID));

        app.handle_key(KeyCode::Char('D'));
        assert_eq!(select(&app).sort_order, Some(SortOrder::Date));
        app.handle_key(KeyCode::Char('N'));
        assert_eq!(select(&app).sort_order, Some(SortOrder::Name));
        app.handle_key(KeyCode::Char('S'));
        assert_eq!(select(&app).sort_order, Some(SortOrder::Size));
    }

    #[test]
    fn sorting_leaves_the_selection_alone_and_plain_n_clears_it() {
        let mut app = select_app(scan_with_id(docker::SCAN_ID));

        app.handle_key(KeyCode::Char('N'));
        assert_eq!(select(&app).scan.selected_items().len(), 1);

        app.handle_key(KeyCode::Char('n'));
        assert!(select(&app).scan.selected_items().is_empty());
    }

    #[test]
    fn other_lists_keep_their_keys() {
        let mut app = select_app(scan_with_id("dev"));

        app.handle_key(KeyCode::Char('D'));
        assert_eq!(select(&app).sort_order, None);

        app.handle_key(KeyCode::Char('N'));
        assert!(select(&app).scan.selected_items().is_empty());
    }

    #[test]
    fn help_shows_the_sort_keys_only_for_a_table() {
        let titles =
            |app: &App| -> Vec<&str> { app.help_sections().iter().map(|s| s.title).collect() };

        let table = select_app(scan_with_id(docker::SCAN_ID));
        assert_eq!(titles(&table), ["Select items", "Sort by", "General"]);

        let list = select_app(scan_with_id("dev"));
        assert_eq!(titles(&list), ["Select items", "General"]);
    }

    #[test]
    fn help_for_explore_groups_the_sort_keys() {
        let dir = tempdir().unwrap();
        let app = analyze_app(dir.path());

        let sections = app.help_sections();
        let titles: Vec<_> = sections.iter().map(|s| s.title).collect();
        assert_eq!(titles, ["Explore", "Sort by", "General"]);

        let keys: Vec<_> = sections[1].shortcuts.iter().map(|h| h.key).collect();
        assert_eq!(keys, ["⇧S", "⇧N", "⇧D"]);
    }

    #[test]
    fn shift_letters_pick_the_sort_order() {
        let dir = tempdir().unwrap();
        let mut app = analyze_app(dir.path());

        app.handle_key(KeyCode::Char('N'));
        assert_eq!(analyze(&app).sort_order, SortOrder::Name);
        app.handle_key(KeyCode::Char('D'));
        assert_eq!(analyze(&app).sort_order, SortOrder::Date);
        app.handle_key(KeyCode::Char('S'));
        assert_eq!(analyze(&app).sort_order, SortOrder::Size);
    }

    #[test]
    fn plain_s_leaves_the_sort_order_alone() {
        let dir = tempdir().unwrap();
        let mut app = analyze_app(dir.path());

        app.handle_key(KeyCode::Char('s'));
        assert_eq!(analyze(&app).sort_order, SortOrder::Size);
    }

    #[test]
    fn shift_d_never_opens_the_delete_confirmation() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("file.txt"), "x").unwrap();
        let mut app = analyze_app(dir.path());

        app.handle_key(KeyCode::Char(' '));
        app.handle_key(KeyCode::Char('D'));
        assert!(matches!(analyze(&app).overlay, AnalyzeOverlay::None));

        app.handle_key(KeyCode::Char('d'));
        assert!(matches!(
            analyze(&app).overlay,
            AnalyzeOverlay::ConfirmDelete { .. }
        ));
    }

    #[test]
    fn shift_n_keeps_the_selection_and_plain_n_clears_it() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("file.txt"), "x").unwrap();
        let mut app = analyze_app(dir.path());
        let selected =
            |app: &App| analyze(app).current_level().unwrap().snapshot.children[0].selected;

        app.handle_key(KeyCode::Char(' '));
        app.handle_key(KeyCode::Char('N'));
        assert!(selected(&app));

        app.handle_key(KeyCode::Char('n'));
        assert!(!selected(&app));
    }

    fn press(app: &mut App, keys: &[KeyCode]) {
        for key in keys {
            app.handle_key(*key);
        }
    }

    fn finish_scan(app: &mut App, scan: CategoryScan) {
        let (tx, rx) = mpsc::channel();
        tx.send(ScanMessage::Finished(scan)).unwrap();
        let Screen::Scanning(_, scan_rx, cancel) = &mut app.screen else {
            panic!("expected the scanning screen");
        };
        cancel.store(true, Ordering::Relaxed);
        *scan_rx = rx;
        app.poll_scan();
    }

    fn empty_scan() -> CategoryScan {
        CategoryScan {
            id: "ai".to_string(),
            title: "AI Tool Caches".to_string(),
            groups: Vec::new(),
        }
    }

    #[track_caller]
    fn assert_main_menu(app: &App, row: usize) {
        let Screen::Workspace(state) = &app.screen else {
            panic!("expected the workspace screen");
        };
        assert!(matches!(state.view, WorkspaceView::MainMenu));
        assert_eq!(state.selected_index, row);
    }

    #[track_caller]
    fn assert_clean_submenu(app: &App, row: usize) {
        let Screen::Workspace(state) = &app.screen else {
            panic!("expected the workspace screen");
        };
        assert!(matches!(
            state.view,
            WorkspaceView::CleanSubmenu { selected } if selected == row
        ));
    }

    fn enter_subfolder(app: &mut App) {
        app.handle_key(KeyCode::Enter);
        let Screen::Analyze(state) = &mut app.screen else {
            panic!("expected the analyze screen");
        };
        while state.measuring_rx.is_some() {
            state.tick();
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(state.levels.len(), 2);
    }

    #[test]
    fn esc_from_an_empty_scan_walks_back_through_the_submenu_and_the_menu() {
        let mut app = App::new(IgnoreStore::default(), Config::default());
        let ai_tools = 3;

        press(
            &mut app,
            &[
                KeyCode::Enter,
                KeyCode::Down,
                KeyCode::Down,
                KeyCode::Down,
                KeyCode::Enter,
            ],
        );
        finish_scan(&mut app, empty_scan());
        assert!(matches!(app.screen, Screen::Select(_)));

        app.handle_key(KeyCode::Esc);
        assert_clean_submenu(&app, ai_tools);

        app.handle_key(KeyCode::Esc);
        assert_main_menu(&app, 0);
        assert!(!app.should_quit);

        app.handle_key(KeyCode::Esc);
        assert!(app.should_quit);
    }

    #[test]
    fn esc_during_a_scan_returns_to_the_row_of_the_submenu() {
        let mut app = App::new(IgnoreStore::default(), Config::default());

        press(
            &mut app,
            &[KeyCode::Enter, KeyCode::Down, KeyCode::Enter, KeyCode::Esc],
        );

        assert_clean_submenu(&app, 1);
    }

    #[test]
    fn esc_returns_to_the_menu_row_a_screen_was_opened_from() {
        let mut app = App::new(IgnoreStore::default(), Config::default());

        press(&mut app, &[KeyCode::Char('5'), KeyCode::Esc]);

        assert_main_menu(&app, 4);
    }

    #[test]
    fn esc_from_explore_does_not_reopen_its_start_dialog() {
        let mut app = App::new(IgnoreStore::default(), Config::default());

        press(
            &mut app,
            &[KeyCode::Char('4'), KeyCode::Enter, KeyCode::Esc],
        );

        assert_main_menu(&app, 3);
    }

    #[test]
    fn esc_from_protected_paths_opened_in_the_menu_returns_to_the_menu() {
        let mut app = App::new(IgnoreStore::default(), Config::default());

        press(&mut app, &[KeyCode::Char('7'), KeyCode::Esc]);

        assert_main_menu(&app, 6);
    }

    #[test]
    fn esc_from_a_list_opened_in_preferences_keeps_the_row_and_refreshes_counts() {
        let mut app = App::new(IgnoreStore::default(), Config::default());

        press(
            &mut app,
            &[
                KeyCode::Char('6'),
                KeyCode::Down,
                KeyCode::Down,
                KeyCode::Enter,
            ],
        );
        app.ignore.roots.push("/nowhere/protected".to_string());
        app.handle_key(KeyCode::Esc);

        let Screen::Preferences(state) = &app.screen else {
            panic!("expected the preferences screen");
        };
        assert_eq!(state.focus, PreferencesFocus::ProtectedPaths);
        assert_eq!(state.path_count, app.ignore.roots.len());

        app.handle_key(KeyCode::Esc);
        assert_main_menu(&app, 5);
    }

    #[test]
    fn esc_on_a_list_opened_from_the_command_line_opens_the_main_menu() {
        let mut app = App::with_select(empty_scan(), IgnoreStore::default(), Config::default());

        app.handle_key(KeyCode::Esc);

        assert_main_menu(&app, 0);
    }

    #[test]
    fn esc_in_explore_goes_up_one_folder_before_leaving() {
        let dir = tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let mut app = analyze_app(dir.path());
        enter_subfolder(&mut app);

        app.handle_key(KeyCode::Esc);
        assert_eq!(analyze(&app).levels.len(), 1);

        app.handle_key(KeyCode::Esc);
        assert_main_menu(&app, 0);
    }

    #[test]
    fn q_leaves_explore_from_any_folder() {
        let dir = tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let mut app = analyze_app(dir.path());
        enter_subfolder(&mut app);

        app.handle_key(KeyCode::Char('q'));

        assert_main_menu(&app, 0);
    }

    fn screen_text(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| app.render_screen(frame)).unwrap();
        format!("{:?}", terminal.backend().buffer())
    }

    #[test]
    fn protected_paths_name_preferences_only_when_opened_there() {
        let mut app = App::new(IgnoreStore::default(), Config::default());

        app.handle_key(KeyCode::Char('7'));
        assert!(!screen_text(&app).contains("Preferences"));

        press(
            &mut app,
            &[
                KeyCode::Esc,
                KeyCode::Char('6'),
                KeyCode::Down,
                KeyCode::Down,
                KeyCode::Enter,
            ],
        );
        assert!(screen_text(&app).contains("Preferences"));
    }

    #[test]
    fn scans_started_in_the_menu_are_not_shown_under_clean_caches() {
        let mut app = App::new(IgnoreStore::default(), Config::default());

        press(&mut app, &[KeyCode::Char('2'), KeyCode::Enter]);
        assert!(!screen_text(&app).contains("Clean caches"));

        press(&mut app, &[KeyCode::Esc, KeyCode::Char('3')]);
        assert!(!screen_text(&app).contains("Clean caches"));
        app.handle_key(KeyCode::Esc);
    }

    #[test]
    fn scans_started_in_clean_caches_are_shown_under_it() {
        let mut app = App::new(IgnoreStore::default(), Config::default());

        press(&mut app, &[KeyCode::Enter, KeyCode::Enter]);
        assert!(screen_text(&app).contains("Clean caches"));
        app.handle_key(KeyCode::Esc);
    }

    #[test]
    fn project_results_are_not_shown_under_clean_caches() {
        assert!(!screen_text(&select_app(scan_with_id("purge"))).contains("Clean caches"));
        assert!(screen_text(&select_app(scan_with_id("dev"))).contains("Clean caches"));
    }

    fn footer(app: &App) -> String {
        let (width, height) = (160, 30);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.render_screen(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..width)
            .map(|x| buffer[(x, height - 1)].symbol())
            .collect()
    }

    #[track_caller]
    fn assert_footer(app: &App, shown: &[&str], hidden: &[&str]) {
        let row = footer(app);
        for hint in shown {
            assert!(row.contains(hint), "missing {hint:?} in {row:?}");
        }
        for hint in hidden {
            assert!(!row.contains(hint), "unexpected {hint:?} in {row:?}");
        }
    }

    const LIST_HINTS: [&str; 5] = ["↑↓  Move", "␣  Select", "A  All", "N  None", "/  Search"];

    #[test]
    fn a_list_shows_bulk_selection_and_the_way_back() {
        let app = select_app(scan_with_id("dev"));

        assert_footer(&app, &LIST_HINTS, &[]);
        assert_footer(&app, &["⎋  Back", "?  More"], &["Sort:"]);
    }

    #[test]
    fn a_docker_table_shows_sorting_next_to_search() {
        let app = select_app(scan_with_id(docker::SCAN_ID));

        assert_footer(&app, &LIST_HINTS, &[]);
        assert_footer(&app, &["⇧S/N/D  Sort: Size", "⎋  Back", "?  More"], &[]);
    }

    #[test]
    fn an_empty_list_offers_only_back_and_help() {
        let app = App::with_select(empty_scan(), IgnoreStore::default(), Config::default());

        assert_footer(&app, &["⎋  Back", "?  More"], &LIST_HINTS);
    }

    #[test]
    fn typing_a_search_shows_only_the_keys_that_work_while_typing() {
        let mut app = select_app(scan_with_id("dev"));

        app.handle_key(KeyCode::Char('/'));

        assert_footer(&app, &["Apply", "⎋  Cancel"], &LIST_HINTS);
        assert_footer(&app, &[], &["Review"]);
    }

    #[test]
    fn explore_shows_bulk_selection_and_does_not_repeat_the_main_button() {
        let dir = tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let mut app = analyze_app(dir.path());

        assert_footer(
            &app,
            &[
                "␣  Select",
                "A  All",
                "N  None",
                "⇧S/N/D  Sort: Size",
                "D  Delete",
                "↑↓  Move",
                "⎋  Back",
                "?  More",
            ],
            &["↵  Open"],
        );

        app.handle_key(KeyCode::Char(' '));

        assert_footer(&app, &["↵  Open"], &["D  Delete"]);
    }

    #[test]
    fn an_empty_folder_offers_only_back_and_help() {
        let dir = tempdir().unwrap();
        let app = analyze_app(dir.path());

        assert_footer(
            &app,
            &["⎋  Back", "?  More"],
            &[
                "↑↓  Move",
                "␣  Select",
                "A  All",
                "N  None",
                "Sort:",
                "Delete",
            ],
        );
    }

    #[test]
    fn preferences_show_the_arrows_only_on_the_threshold_row() {
        let mut app = App::new(IgnoreStore::default(), Config::default());

        app.handle_key(KeyCode::Char('6'));
        assert_footer(&app, &["←→  Adjust", "⎋  Back"], &[]);

        app.handle_key(KeyCode::Down);
        assert_footer(&app, &["⎋  Back"], &["Adjust"]);
    }

    #[test]
    fn a_path_list_offers_removal_only_when_it_has_paths() {
        let mut app = App::new(IgnoreStore::default(), Config::default());

        press(&mut app, &[KeyCode::Char('7')]);
        assert_footer(&app, &["Add", "Back", "More"], &["Remove", "Move"]);

        app.handle_key(KeyCode::Esc);
        app.ignore.roots.push("/nowhere/protected".to_string());

        app.handle_key(KeyCode::Char('7'));
        assert_footer(&app, &["Add", "Remove", "Move", "Back", "More"], &[]);
    }

    #[test]
    fn the_main_menu_says_esc_quits_and_the_submenu_says_it_goes_back() {
        let mut app = App::new(IgnoreStore::default(), Config::default());

        assert_footer(&app, &["⎋  Quit"], &["Back"]);

        app.handle_key(KeyCode::Enter);

        assert_footer(&app, &["⎋  Back"], &["Quit"]);
    }

    #[test]
    fn a_dialog_empties_the_footer_because_it_lists_its_own_keys() {
        let toolbar = [
            "Quit", "Move", "More", "Open", "Select", "All", "Review", "Delete",
        ];

        let mut app = App::new(IgnoreStore::default(), Config::default());
        app.handle_key(KeyCode::Char('2'));
        assert_footer(&app, &[], &toolbar);

        let mut app = select_app(scan_with_id("dev"));
        app.handle_key(KeyCode::Enter);
        assert!(matches!(select(&app).overlay, ModalOverlay::Confirm(_)));
        assert_footer(&app, &[], &toolbar);

        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("file.txt"), "x").unwrap();
        let mut app = analyze_app(dir.path());
        press(&mut app, &[KeyCode::Char(' '), KeyCode::Char('d')]);
        assert_footer(&app, &[], &toolbar);
    }

    #[test]
    fn help_mentions_scrolling_only_when_status_can_scroll() {
        let mut app = App::new(IgnoreStore::default(), Config::default());
        app.handle_key(KeyCode::Char('5'));
        let draw = |app: &App, width, height| {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| app.render_screen(frame)).unwrap();
        };

        draw(&app, 205, 85);
        assert_eq!(help_row(&app, "Scroll"), None);

        draw(&app, 50, 16);
        assert!(help_row(&app, "Scroll").is_some());
    }

    fn help_row(app: &App, desc: &str) -> Option<&'static str> {
        app.help_sections()
            .iter()
            .flat_map(|section| &section.shortcuts)
            .find(|hint| hint.desc == desc)
            .map(|hint| hint.key)
    }

    #[test]
    fn help_in_the_clean_submenu_says_esc_goes_back() {
        let mut app = App::new(IgnoreStore::default(), Config::default());

        assert_eq!(help_row(&app, "Quit"), Some("⎋ / q"));

        app.handle_key(KeyCode::Enter);

        assert_eq!(help_row(&app, "Back"), Some("⎋ / q"));
        assert_eq!(help_row(&app, "Quit"), None);
    }

    #[test]
    fn help_for_explore_separates_going_up_from_leaving() {
        let dir = tempdir().unwrap();
        let app = analyze_app(dir.path());

        assert_eq!(help_row(&app, "Up one level"), Some("⎋ / ←"));
        assert_eq!(help_row(&app, "Quit explore"), Some("q"));
    }
}

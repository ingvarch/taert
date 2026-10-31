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
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

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
use crate::core::path_utils::prettify_path;
use crate::core::projects_store::ProjectFoldersStore;
use crate::core::purger::{PurgeScanEvent, Purger};
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
}

pub enum RemovalMessage {
    Progress(RemovalProgress),
    Finished(RemovalReport),
}

pub struct App {
    pub theme: Theme,
    pub screen: Screen,
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

                match &self.screen {
                    Screen::Workspace(state) => render_workspace(frame, state, &self.theme),
                    Screen::Scanning(state, _, _) => render_progress(frame, state, &self.theme),
                    Screen::Select(state) => render_select(frame, state, &self.theme),
                    Screen::Review(state) => render_review(frame, state, &self.theme),
                    Screen::Analyze(state) => render_analyze(frame, state, &self.theme),
                    Screen::SystemStatus(state) => render_system_status(frame, state, &self.theme),
                    Screen::Preferences(state) => render_preferences(frame, state, &self.theme),
                    Screen::PathListEditor(state) => {
                        render_path_list_editor(frame, state, &self.theme)
                    }
                }

                if self.help_open {
                    let sections = self.help_sections();
                    super::components::help::HelpSheet::new(sections).render(
                        frame,
                        area,
                        &self.theme,
                    );
                }
            })?;

            // Poll background scan messages if currently scanning
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
                        ScanMessage::Finished(scan) => {
                            let state = if scan.id == "purge" {
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
                    self.screen = Screen::Workspace(WorkspaceState::new());
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
                    KeyCode::Char('n') | KeyCode::Char('N') => state.select_none(),
                    KeyCode::Char('/') => {
                        state.is_searching = true;
                        state.search_query.clear();
                    }
                    KeyCode::Enter => {
                        state.open_confirmation();
                    }
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.screen = Screen::Workspace(WorkspaceState::new());
                    }
                    _ => {}
                }
            }

            Screen::Review(state) => {
                if state.report.is_some() {
                    // Result view
                    if matches!(code, KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q')) {
                        self.screen = Screen::Workspace(WorkspaceState::new());
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
                                self.screen = Screen::Workspace(WorkspaceState::new());
                            }
                        }
                        KeyCode::Char('q') => {
                            state.cancel_measuring();
                            self.screen = Screen::Workspace(WorkspaceState::new());
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
                    KeyCode::Left => {
                        if !state.go_up() {
                            self.screen = Screen::Workspace(WorkspaceState::new());
                        }
                    }
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.screen = Screen::Workspace(WorkspaceState::new());
                    }
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
                    self.screen = Screen::Workspace(WorkspaceState::new());
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
                        self.screen = Screen::PathListEditor(PathListEditorState::new(
                            PathListKind::ProjectFolders,
                            &self.ignore,
                        ));
                    }
                    PreferencesFocus::ProtectedPaths => {
                        self.screen = Screen::PathListEditor(PathListEditorState::new(
                            PathListKind::ProtectedPaths,
                            &self.ignore,
                        ));
                    }
                },
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.screen = Screen::Workspace(WorkspaceState::new());
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
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.screen =
                            Screen::Preferences(PreferencesState::new(&self.config, &self.ignore));
                    }
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

        match &self.screen {
            Screen::Workspace(_) => vec![
                HelpSection::new(
                    "Menu",
                    vec![Hint::new("↵", "Select"), Hint::new("1-5", "Quick select")],
                ),
                general(vec![navigate, Hint::new("⎋ / q", "Quit")]),
            ],
            Screen::Scanning(_, _, _) => vec![
                HelpSection::new("Scanning", vec![Hint::new("⎋ / q", "Cancel scan")]),
                general(Vec::new()),
            ],
            Screen::Select(_) => vec![
                HelpSection::new(
                    "Select items",
                    vec![
                        Hint::new("␣", "Toggle item"),
                        Hint::new("A", "Select all"),
                        Hint::new("N", "Select none"),
                        Hint::new("/", "Search"),
                        Hint::new("↵", "Review & clean"),
                    ],
                ),
                general(vec![navigate, back]),
            ],
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
                        Hint::new("←", "Up one level"),
                        Hint::new("R", "Refresh folder"),
                        Hint::new("O", "Reveal in Finder"),
                        Hint::new("D / ⌫", "Delete selected"),
                    ],
                ),
                HelpSection::new(
                    "Sort by",
                    vec![
                        Hint::new("⇧S", "Size"),
                        Hint::new("⇧N", "Name"),
                        Hint::new("⇧D", "Date created"),
                    ],
                ),
                general(vec![navigate, Hint::new("⎋ / q", "Quit explore")]),
            ],
            Screen::SystemStatus(_) => vec![general(vec![scroll, Hint::new("⎋ / q / ↵", "Back")])],
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
            MainMenuChoice::ExploreStorage => {
                if let Screen::Workspace(ref mut w) = self.screen {
                    w.open_confirmation(StartConfirmKind::ExploreHome);
                }
            }
            MainMenuChoice::SystemStatus => {
                self.screen = Screen::SystemStatus(Box::default());
            }
            MainMenuChoice::Preferences => {
                self.screen =
                    Screen::Preferences(PreferencesState::new(&self.config, &self.ignore));
            }
            MainMenuChoice::ProtectedPaths => {
                self.screen = Screen::PathListEditor(PathListEditorState::new(
                    PathListKind::ProtectedPaths,
                    &self.ignore,
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
                self.screen = Screen::Analyze(Box::new(AnalyzeState::with_cache(
                    home,
                    self.ignore.clone(),
                    self.storage_cache.clone(),
                )));
            }
            StartConfirmKind::ProjectCleanup(_) => {
                let roots = ProjectFoldersStore::load_or_default().existing_roots();
                self.launch_purge_scan(roots);
            }
        }
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

        self.screen = Screen::Scanning(
            ProgressState::new(format!("Scanning {cat_title}")),
            fwd_rx,
            cancel_flag,
        );
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

        self.screen = Screen::Scanning(
            ProgressState::new("Scanning Developer Caches, 1 of 4"),
            fwd_rx,
            cancel_flag,
        );
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

        self.screen = Screen::Scanning(ProgressState::new(initial_headline), fwd_rx, cancel_flag);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::analyze::SortOrder;
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
}

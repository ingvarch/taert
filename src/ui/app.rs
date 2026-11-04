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
use super::doctor::{DoctorActionConfirm, DoctorScreenState, render_doctor};
use super::keys::is_removal_key;
use super::path_list_editor::{
    PathListEditorState, PathListKind, PathListMode, render_path_list_editor,
};
use super::preferences::{PreferencesFocus, PreferencesState, render_preferences};
use super::progress::{ProgressState, render_progress};
use super::select::{ConfirmButton, ModalOverlay, SelectState, render_select};
use super::startup::{StartupConfirm, StartupScreenState, render_startup};
use super::system_status::{SystemStatusState, render_system_status};
use super::theme::Theme;
use super::workspace::{
    CleanSubcategory, MainMenuChoice, StartConfirmKind, WorkspaceState, WorkspaceView,
    render_workspace,
};
use crate::core::apps::{self, InstalledApp};
use crate::core::cache::StorageCache;
use crate::core::catalog::Category;
use crate::core::cleaner::{
    Cleaner, RemovalFailure, RemovalMode, RemovalProgress, RemovalReport, remove_selected,
};
use crate::core::compress::{self, Compressor};
use crate::core::docker;
use crate::core::doctor;
use crate::core::history::{HistoryLog, ItemOutcome, command_for_scan};
use crate::core::homebrew;
use crate::core::host::{Host, MacHost};
use crate::core::large_files;
use crate::core::layout::Layout;
use crate::core::path_utils::prettify_path;
use crate::core::projects_store::ProjectFoldersStore;
use crate::core::purger::{self, PurgeScanEvent, Purger};
use crate::core::scanner::{
    CategoryScan, CategoryScanner, ScanContext, ScanItem, ScanProgressEvent, scan_all_categories,
};
use crate::core::slim;
use crate::core::startup;
use crate::core::stores::{Config, IgnoreStore};
use crate::core::uninstaller::{self, PlanRequest, UninstallPlan, Uninstaller};

pub enum Screen {
    Workspace(WorkspaceState),
    Scanning(ProgressState, Receiver<ScanMessage>, Arc<AtomicBool>),
    Select(Box<SelectState>),
    Analyze(Box<AnalyzeState>),
    SystemStatus(Box<SystemStatusState>),
    Startup(StartupScreenState),
    Doctor(DoctorScreenState),
    Preferences(PreferencesState),
    PathListEditor(PathListEditorState),
}

pub enum ScanMessage {
    Progress(ScanProgressEvent),
    PurgeProgress(PurgeScanEvent),
    Finished(CategoryScan),
    Homebrew(homebrew::Inventory),
    Failed {
        id: String,
        title: String,
        problem: String,
    },
    AppsFound(Vec<InstalledApp>),
    Planned(UninstallPlan),
}

pub enum RemovalMessage {
    Progress(RemovalProgress),
    Finished(RemovalReport),
}

/// A screen Esc can return to. It is rebuilt on return, so disk space and counts stay
/// fresh. The app list comes back as it was, because a search takes a while.
enum ReturnPoint {
    Workspace {
        row: usize,
        submenu_row: Option<usize>,
    },
    Preferences(PreferencesFocus),
    AppList(Box<SelectState>),
}

/// Passes scan progress on to the screen. Join the handle after dropping the sender.
fn forward_progress(
    screen: mpsc::Sender<ScanMessage>,
) -> (mpsc::Sender<ScanProgressEvent>, thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let bridge = thread::spawn(move || {
        while let Ok(event) = rx.recv() {
            let _ = screen.send(ScanMessage::Progress(event));
        }
    });
    (tx, bridge)
}

fn review_state(plan: &UninstallPlan) -> SelectState {
    if plan.apps.is_empty() {
        return SelectState::unavailable(&plan.scan.id, &plan.scan.title, plan.notes.join(" "));
    }
    let mut state = SelectState::with_preserved_selection(plan.scan.clone())
        .with_routes(uninstaller::routes(plan));
    state.notes = plan.notes.clone();
    state
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
    pub layout: Layout,
    pub host: Arc<dyn Host>,
    /// Apps of the last search, which decide what a selection in the list means.
    pub installed: Vec<InstalledApp>,
    /// The plan shown for review. Its selection lives in the select screen.
    pub plan: Option<UninstallPlan>,
    /// Delete permanently instead of moving to the Trash
    pub permanent: bool,
    /// The background check for Homebrew updates, while it runs, also off the list.
    brew_check: Option<Receiver<BrewCheck>>,
    /// The list being read again after an upgrade, while it runs.
    brew_refresh: Option<Receiver<Result<homebrew::Inventory, String>>>,
    /// Set when the check starts, so the list checks once per run.
    brew_checked: bool,
    /// The brew the list opened by `taert brew` checks once the run starts.
    brew_pending: Option<std::path::PathBuf>,
}

/// What the background check for Homebrew updates found.
struct BrewCheck {
    update_error: Option<String>,
    latest: Result<homebrew::Latest, String>,
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
            layout: Layout::current(),
            host: Arc::new(MacHost),
            installed: Vec::new(),
            plan: None,
            permanent: false,
            brew_check: None,
            brew_refresh: None,
            brew_checked: false,
            brew_pending: None,
        }
    }

    pub fn with_permanent(mut self, permanent: bool) -> Self {
        self.permanent = permanent;
        if let Screen::Select(select) = self.screen {
            self.screen = Screen::Select(Box::new(select.with_permanent(permanent)));
        }
        self
    }

    pub fn new_startup(ignore: IgnoreStore, config: Config, include_all: bool) -> Self {
        let mut app = Self::new(ignore, config);
        app.open_startup(include_all);
        app
    }

    pub fn open_startup(&mut self, include_all: bool) {
        let btm = if include_all {
            startup::read_btm(&*self.host)
        } else {
            startup::Btm::Skipped
        };
        let scan =
            startup::scan_startup_with(&*self.host, &self.layout, &self.ignore, include_all, &btm);
        let mut state = StartupScreenState::new(scan.items, include_all);
        state.notes = scan.notes;
        state.btm = btm;
        self.open(Screen::Startup(state));
    }

    pub fn new_doctor(ignore: IgnoreStore, config: Config, seconds: u64) -> Self {
        let mut app = Self::new(ignore, config);
        app.open_doctor(seconds);
        app
    }

    pub fn new_status(ignore: IgnoreStore, config: Config) -> Self {
        let mut app = Self::new(ignore, config);
        app.open(Screen::SystemStatus(Box::default()));
        app
    }

    pub fn open_doctor(&mut self, seconds: u64) {
        let (tx, rx) = mpsc::channel();
        let host = self.host.clone();
        let layout = self.layout.clone();
        let ignore = self.ignore.clone();

        thread::spawn(move || {
            let snapshot = doctor::collect(&*host, seconds, &layout, &ignore);
            let report = doctor::diagnose(&snapshot);
            let _ = tx.send(report);
        });

        self.open(Screen::Doctor(DoctorScreenState::new_checking(seconds, rx)));
    }

    pub fn with_select(scan: CategoryScan, ignore: IgnoreStore, config: Config) -> Self {
        Self::with_select_state(SelectState::new(scan), ignore, config)
    }

    /// Opens on the Homebrew list.
    pub fn with_brew_list(
        inventory: &homebrew::Inventory,
        ignore: IgnoreStore,
        config: Config,
    ) -> Self {
        let mut app =
            Self::with_select_state(SelectState::homebrew_list(inventory), ignore, config);
        app.brew_pending = inventory.brew.clone();
        app
    }

    fn with_select_state(state: SelectState, ignore: IgnoreStore, config: Config) -> Self {
        Self {
            theme: Theme::new(),
            screen: Screen::Select(Box::new(state)),
            back_stack: Vec::new(),
            ignore,
            config,
            should_quit: false,
            removal_rx: None,
            removal_spinner: 0,
            help_open: false,
            storage_cache: Arc::new(RwLock::new(StorageCache::new())),
            layout: Layout::current(),
            host: Arc::new(MacHost),
            installed: Vec::new(),
            plan: None,
            permanent: false,
            brew_check: None,
            brew_refresh: None,
            brew_checked: false,
            brew_pending: None,
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
            layout: Layout::current(),
            host: Arc::new(MacHost),
            installed: Vec::new(),
            plan: None,
            permanent: false,
            brew_check: None,
            brew_refresh: None,
            brew_checked: false,
            brew_pending: None,
        }
    }

    /// Opens on the review of a plan made before the TUI started.
    pub fn with_plan(plan: UninstallPlan, ignore: IgnoreStore, config: Config) -> Self {
        let mut app = Self::new(ignore, config);
        app.screen = Screen::Select(Box::new(review_state(&plan)));
        app.plan = Some(plan);
        app
    }

    /// Searches for apps. Esc from the list returns to the screen this started on.
    pub fn launch_app_scan(&mut self) {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let (layout, host) = (self.layout.clone(), self.host.clone());
        let cancel_scan = cancel.clone();

        thread::spawn(move || {
            let (progress, bridge) = forward_progress(tx.clone());
            let found = apps::installed_apps(&layout, &*host, Some(progress), Some(cancel_scan));
            let _ = bridge.join();
            let _ = tx.send(ScanMessage::AppsFound(found));
        });

        self.open(Screen::Scanning(
            ProgressState::new("Finding apps").top_level(),
            rx,
            cancel,
        ));
    }

    /// Reviews the selected apps, or the focused one when none is selected.
    fn start_review(&mut self) {
        let Screen::Select(list) = &self.screen else {
            return;
        };
        let mut chosen: Vec<_> = list
            .scan
            .selected_items()
            .iter()
            .map(|item| item.path.clone())
            .collect();
        if chosen.is_empty() {
            chosen.extend(list.current_focused_path());
        }
        let selected: Vec<InstalledApp> = self
            .installed
            .iter()
            .filter(|app| chosen.contains(&app.path))
            .cloned()
            .collect();
        let Some(first) = selected.first() else {
            return;
        };
        let headline = match selected.len() {
            1 => format!("Finding what {} left behind", first.name),
            count => format!("Finding what {count} apps left behind"),
        };

        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_plan = cancel.clone();
        let installed = self.installed.clone();
        let ignore = self.ignore.clone();
        let (layout, host) = (self.layout.clone(), self.host.clone());

        thread::spawn(move || {
            let (progress, bridge) = forward_progress(tx.clone());
            let request = PlanRequest {
                selected: &selected,
                installed: &installed,
                ignore: &ignore,
                host: &*host,
                layout: &layout,
            };
            let plan = uninstaller::build_plan(&request, Some(progress), Some(cancel_plan));
            let _ = bridge.join();
            let _ = tx.send(ScanMessage::Planned(plan));
        });

        self.open(Screen::Scanning(
            ProgressState::new(headline).top_level(),
            rx,
            cancel,
        ));
    }

    /// The list on the way back is out of date once an app is gone.
    fn show_fresh_app_list(&mut self) {
        if matches!(self.back_stack.last(), Some(ReturnPoint::AppList(_))) {
            self.back_stack.pop();
        }
        self.launch_app_scan();
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
        self.start_pending_brew_check();
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
            self.poll_brew_check();
            self.poll_brew_refresh();

            // Tick analyze measuring if active
            if let Screen::Analyze(ref mut analyze) = self.screen {
                analyze.tick();
            }

            // Take the latest system measurements if active
            if let Screen::SystemStatus(ref mut status) = self.screen {
                status.tick();
            }

            if let Screen::Doctor(ref mut doctor) = self.screen {
                doctor.check_receiver();
            }

            if let Screen::Workspace(ref mut workspace) = self.screen {
                workspace.tick(&self.storage_cache);
            }

            self.poll_removal();

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
            Screen::Analyze(state) => render_analyze(frame, state, &self.theme),
            Screen::SystemStatus(state) => render_system_status(frame, state, &self.theme),
            Screen::Startup(state) => render_startup(frame, state, &self.theme),
            Screen::Doctor(state) => render_doctor(frame, state, &self.theme),
            Screen::Preferences(state) => render_preferences(frame, state, &self.theme),
            Screen::PathListEditor(state) => render_path_list_editor(frame, state, &self.theme),
        }
    }

    fn open(&mut self, screen: Screen) {
        match std::mem::replace(&mut self.screen, screen) {
            Screen::Workspace(state) => self.back_stack.push(ReturnPoint::Workspace {
                row: state.selected_index,
                submenu_row: state.submenu_row(),
            }),
            Screen::Preferences(state) => {
                self.back_stack.push(ReturnPoint::Preferences(state.focus));
            }
            Screen::Select(state) if state.scan.id == apps::LIST_ID => {
                self.back_stack.push(ReturnPoint::AppList(state));
            }
            _ => {}
        }
    }

    fn go_back(&mut self) {
        self.brew_refresh = None;
        self.screen = match self.back_stack.pop() {
            Some(ReturnPoint::Workspace { row, submenu_row }) => {
                Screen::Workspace(WorkspaceState::at(row, submenu_row))
            }
            Some(ReturnPoint::Preferences(focus)) => {
                let mut state = PreferencesState::new(&self.config, &self.ignore, &self.layout);
                state.focus = focus;
                Screen::Preferences(state)
            }
            Some(ReturnPoint::AppList(state)) => Screen::Select(state),
            None => Screen::Workspace(WorkspaceState::new()),
        };
    }

    fn poll_removal(&mut self) {
        let Some(rx) = &self.removal_rx else {
            return;
        };
        self.removal_spinner = (self.removal_spinner + 1) % 4;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                RemovalMessage::Progress(prog) => {
                    if let Screen::Select(ref mut select) = self.screen {
                        let cancel = match &select.overlay {
                            ModalOverlay::Removing { cancel, .. } => cancel.clone(),
                            _ => None,
                        };
                        select.overlay = ModalOverlay::Removing {
                            progress: prog,
                            spinner_idx: self.removal_spinner,
                            cancel,
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
                        // The Homebrew list drops its rows when the outcome closes.
                        if select.scan.id != homebrew::SCAN_ID {
                            for group in &mut select.scan.groups {
                                group.items.retain(|item| !item.selected);
                            }
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

    fn start_pending_brew_check(&mut self) {
        if let Some(brew) = self.brew_pending.take() {
            self.start_brew_check(brew);
        }
    }

    /// Starts the check for Homebrew updates on a thread, once per run.
    fn start_brew_check(&mut self, brew: std::path::PathBuf) {
        if self.brew_checked {
            return;
        }
        self.brew_checked = true;
        let (tx, rx) = mpsc::channel();
        let host = self.host.clone();
        thread::spawn(move || {
            let update_error = homebrew::update(&*host, &brew).err();
            let latest = homebrew::outdated(&*host, &brew);
            let _ = tx.send(BrewCheck {
                update_error,
                latest,
            });
        });
        self.brew_check = Some(rx);
        if let Screen::Select(select) = &mut self.screen {
            select.checking_updates = true;
        }
    }

    fn poll_brew_check(&mut self) {
        let Some(rx) = &self.brew_check else {
            return;
        };
        let check = match rx.try_recv() {
            Ok(check) => Some(check),
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => None,
        };
        self.brew_check = None;
        if let Screen::Select(select) = &mut self.screen
            && select.scan.id == homebrew::SCAN_ID
        {
            match check {
                Some(check) => select.finish_update_check(check.update_error, check.latest),
                None => select.checking_updates = false,
            }
        }
    }

    /// Reads the list again on a thread after an upgrade.
    fn start_brew_refresh(&mut self, inventory: homebrew::Inventory) {
        let (tx, rx) = mpsc::channel();
        let host = self.host.clone();
        thread::spawn(move || {
            let _ = tx.send(homebrew::refresh(&*host, &inventory));
        });
        self.brew_refresh = Some(rx);
    }

    fn poll_brew_refresh(&mut self) {
        let Some(rx) = &self.brew_refresh else {
            return;
        };
        if let Screen::Select(select) = &self.screen
            && !matches!(select.overlay, ModalOverlay::None)
        {
            return;
        }
        let read = match rx.try_recv() {
            Ok(read) => read,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err("the read stopped".to_string()),
        };
        self.brew_refresh = None;
        if let Screen::Select(select) = &mut self.screen
            && select.scan.id == homebrew::SCAN_ID
        {
            match read {
                Ok(inventory) => select.finish_refresh(&inventory),
                Err(error) => select.fail_refresh(&error),
            }
        }
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
                    ScanMessage::Failed { id, title, problem } => {
                        self.screen = Screen::Select(Box::new(SelectState::unavailable(
                            &id, &title, problem,
                        )));
                        break;
                    }
                    ScanMessage::AppsFound(found) => {
                        let list = apps::app_scan(&found, &self.layout);
                        self.installed = found;
                        self.screen = Screen::Select(Box::new(
                            SelectState::with_preserved_selection(list)
                                .with_permanent(self.permanent),
                        ));
                        break;
                    }
                    ScanMessage::Planned(plan) => {
                        self.screen = Screen::Select(Box::new(
                            review_state(&plan).with_permanent(self.permanent),
                        ));
                        self.plan = Some(plan);
                        break;
                    }
                    ScanMessage::Homebrew(inventory) => {
                        let mut list =
                            SelectState::homebrew_list(&inventory).with_permanent(self.permanent);
                        list.checking_updates = self.brew_check.is_some();
                        self.screen = Screen::Select(Box::new(list));
                        if let Some(brew) = inventory.brew {
                            self.start_brew_check(brew);
                        }
                        break;
                    }
                    ScanMessage::Finished(scan) => {
                        let state = if scan.id == purger::SCAN_ID || scan.id == docker::SCAN_ID {
                            SelectState::with_preserved_selection(scan)
                        } else {
                            SelectState::new(scan)
                        };
                        self.screen =
                            Screen::Select(Box::new(state.with_permanent(self.permanent)));
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
                        if let Some(digit) = c.to_digit(10) {
                            let num = if digit == 0 { 10 } else { digit as usize };
                            if let Some(choice) = state.select_by_number(num) {
                                self.handle_main_menu_choice(choice);
                            }
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
                    ModalOverlay::Removing { ref cancel, .. } => {
                        if let Some(cancel) = cancel
                            && code == KeyCode::Esc
                        {
                            cancel.store(true, Ordering::Relaxed);
                        }
                        return;
                    }
                    ModalOverlay::Outcome(_) => {
                        if matches!(code, KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q')) {
                            let mut refresh = None;
                            if state.upgrade.is_some() {
                                refresh = state.close_upgrade_outcome();
                            } else if state.scan.id == homebrew::SCAN_ID {
                                state.close_homebrew_outcome();
                                if state.refreshing {
                                    refresh = state.refresh_request();
                                }
                            } else {
                                state.close_overlay();
                            }
                            if state.scan.id == uninstaller::PLAN_ID {
                                self.show_fresh_app_list();
                            }
                            if let Some(inventory) = refresh {
                                self.start_brew_refresh(inventory);
                            }
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
                    // The rows just changed, so the cursor could sit on a section.
                    state.move_to_start();
                    return;
                }

                match code {
                    KeyCode::Enter if state.scan.id == homebrew::SCAN_ID => {
                        state.toggle_focused();
                    }
                    KeyCode::Right if state.scan.id == homebrew::SCAN_ID => {
                        state.expand_focused();
                    }
                    KeyCode::Left if state.scan.id == homebrew::SCAN_ID => {
                        state.collapse_focused();
                    }
                    code if is_removal_key(code) && state.scan.id == homebrew::SCAN_ID => {
                        state.uninstall_focused();
                    }
                    KeyCode::Char('u') if state.scan.id == homebrew::SCAN_ID => {
                        state.upgrade_focused();
                    }
                    KeyCode::Char('U') if state.scan.id == homebrew::SCAN_ID => {
                        state.upgrade_all();
                    }
                    code if is_removal_key(code) && state.scan.id == apps::LIST_ID => {
                        self.start_review();
                    }
                    code if is_removal_key(code) && !state.starts_with_enter() => {
                        state.open_confirmation();
                    }
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
                    KeyCode::Char('T') if state.sort_order.is_some() => {
                        state.set_sort(SortOrder::Date);
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') => state.select_none(),
                    KeyCode::Char('o') | KeyCode::Char('O') => {
                        if let Some(path) = state.current_focused_path() {
                            let _ = std::process::Command::new("open")
                                .arg("-R")
                                .arg(&path)
                                .spawn();
                        }
                    }
                    KeyCode::Char('/') => {
                        state.is_searching = true;
                        state.search_query.clear();
                    }
                    KeyCode::Enter if state.starts_with_enter() => {
                        if state.scan.id == slim::SCAN_ID && state.scan.selected_items().is_empty()
                        {
                            state.toggle_current();
                        }
                        state.open_confirmation();
                    }
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.go_back();
                    }
                    _ => {}
                }
            }

            Screen::Analyze(state) => {
                if state.measuring_rx.is_some() {
                    match code {
                        KeyCode::Esc | KeyCode::Left => {
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
                            KeyCode::Left | KeyCode::BackTab => {
                                *button = match button {
                                    AnalyzeConfirmButton::Cancel => {
                                        AnalyzeConfirmButton::MoveToTrash
                                    }
                                    AnalyzeConfirmButton::DeletePermanently => {
                                        AnalyzeConfirmButton::Cancel
                                    }
                                    AnalyzeConfirmButton::MoveToTrash => {
                                        AnalyzeConfirmButton::DeletePermanently
                                    }
                                };
                            }
                            KeyCode::Right | KeyCode::Tab => {
                                *button = match button {
                                    AnalyzeConfirmButton::Cancel => {
                                        AnalyzeConfirmButton::DeletePermanently
                                    }
                                    AnalyzeConfirmButton::DeletePermanently => {
                                        AnalyzeConfirmButton::MoveToTrash
                                    }
                                    AnalyzeConfirmButton::MoveToTrash => {
                                        AnalyzeConfirmButton::Cancel
                                    }
                                };
                            }
                            KeyCode::Esc | KeyCode::Char('q') => {
                                state.overlay = AnalyzeOverlay::None;
                            }
                            KeyCode::Char('y') | KeyCode::Char('Y') => {
                                self.start_analyze_removal(RemovalMode::Trash);
                            }
                            KeyCode::Enter => match *button {
                                AnalyzeConfirmButton::MoveToTrash => {
                                    self.start_analyze_removal(RemovalMode::Trash);
                                }
                                AnalyzeConfirmButton::DeletePermanently => {
                                    self.start_analyze_removal(RemovalMode::Permanent);
                                }
                                AnalyzeConfirmButton::Cancel => {
                                    state.overlay = AnalyzeOverlay::None;
                                }
                            },
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
                    KeyCode::Char('T') => state.set_sort(SortOrder::Date),
                    code if is_removal_key(code) => {
                        state.open_delete_confirmation(self.permanent);
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

            Screen::Startup(state) => {
                if state.confirm.is_some() {
                    match code {
                        KeyCode::Enter => {
                            if state.confirm_button == 1 {
                                state.cancel_confirm();
                            } else {
                                let action = state.confirm.take().unwrap();
                                match action {
                                    StartupConfirm::Toggle { index, enable } => {
                                        if let Some(item) = state.items.get(index) {
                                            let res = if enable {
                                                startup::enable_item(
                                                    &*self.host,
                                                    &self.layout,
                                                    &self.ignore,
                                                    item,
                                                    false,
                                                )
                                            } else {
                                                startup::disable_item(
                                                    &*self.host,
                                                    &self.layout,
                                                    &self.ignore,
                                                    item,
                                                    false,
                                                )
                                            };
                                            state.finish_action(res);
                                            state.apply_scan(startup::scan_startup_with(
                                                &*self.host,
                                                &self.layout,
                                                &self.ignore,
                                                state.include_all,
                                                &state.btm,
                                            ));
                                        }
                                    }
                                    StartupConfirm::Stop { index } => {
                                        if let Some(item) = state.items.get(index) {
                                            let res = startup::stop_item(&*self.host, item);
                                            state.finish_action(res);
                                            state.apply_scan(startup::scan_startup_with(
                                                &*self.host,
                                                &self.layout,
                                                &self.ignore,
                                                state.include_all,
                                                &state.btm,
                                            ));
                                        }
                                    }
                                    StartupConfirm::RemoveBroken { index } => {
                                        if let Some(item) = state.items.get(index) {
                                            let res =
                                                startup::remove_broken_plist(item, &self.ignore)
                                                    .map(|()| {
                                                        format!(
                                                            "Removed broken plist for {}",
                                                            item.name
                                                        )
                                                    });
                                            let removed = res.is_ok();
                                            state.finish_action(res);
                                            if removed {
                                                state.apply_scan(startup::scan_startup_with(
                                                    &*self.host,
                                                    &self.layout,
                                                    &self.ignore,
                                                    state.include_all,
                                                    &state.btm,
                                                ));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        KeyCode::Tab | KeyCode::Left | KeyCode::Right => {
                            state.toggle_confirm_button();
                        }
                        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('n') => {
                            state.cancel_confirm();
                        }
                        _ => {}
                    }
                } else {
                    match code {
                        KeyCode::Up | KeyCode::Char('k') => state.previous(),
                        KeyCode::Down | KeyCode::Char('j') => state.next(),
                        KeyCode::PageUp => state.page_up(),
                        KeyCode::PageDown => state.page_down(),
                        KeyCode::Home | KeyCode::Char('g') => state.move_to_start(),
                        KeyCode::End | KeyCode::Char('G') => state.move_to_end(),
                        KeyCode::Char(' ') | KeyCode::Enter => state.request_toggle(),
                        code if is_removal_key(code) => state.request_remove(),
                        KeyCode::Esc | KeyCode::Char('q') => self.go_back(),
                        _ => {}
                    }
                }
            }

            Screen::Doctor(state) => {
                if state.confirm.is_some() {
                    match code {
                        KeyCode::Enter => {
                            if state.confirm_button == 1 {
                                state.cancel_confirm();
                            } else if let Some(action) = state.confirm.take() {
                                state.message = None;
                                match action {
                                    DoctorActionConfirm::KillProcess { pid, name } => {
                                        state.message =
                                            Some(match self.host.signal(pid, libc::SIGTERM) {
                                                Ok(()) => {
                                                    state.remove_selected_finding();
                                                    (
                                                        format!(
                                                            "Sent SIGTERM to {name} (PID {pid})"
                                                        ),
                                                        false,
                                                    )
                                                }
                                                Err(error) => (
                                                    format!("Could not stop {name}: {error}"),
                                                    true,
                                                ),
                                            });
                                    }
                                    DoctorActionConfirm::DisableStartupItem { item } => {
                                        let res = startup::disable_item(
                                            &*self.host,
                                            &self.layout,
                                            &self.ignore,
                                            &item,
                                            false,
                                        );
                                        if res.is_ok() {
                                            state.remove_selected_finding();
                                        }
                                        state.message = Some(match res {
                                            Ok(msg) => (msg, false),
                                            Err(err) => (err, true),
                                        });
                                    }
                                    DoctorActionConfirm::ExploreStorage => {
                                        let home = self.layout.home.clone();
                                        self.open(Screen::Analyze(Box::new(
                                            AnalyzeState::with_cache(
                                                home,
                                                self.ignore.clone(),
                                                self.storage_cache.clone(),
                                            ),
                                        )));
                                    }
                                }
                            }
                        }
                        KeyCode::Tab | KeyCode::Left | KeyCode::Right => {
                            state.toggle_confirm_button();
                        }
                        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('n') => {
                            state.cancel_confirm();
                        }
                        _ => {}
                    }
                } else {
                    match code {
                        KeyCode::Up | KeyCode::Char('k') => state.previous(),
                        KeyCode::Down | KeyCode::Char('j') => state.next(),
                        KeyCode::PageUp => state.page_up(),
                        KeyCode::PageDown => state.page_down(),
                        KeyCode::Home | KeyCode::Char('g') => state.move_to_start(),
                        KeyCode::End | KeyCode::Char('G') => state.move_to_end(),
                        KeyCode::Enter => state.request_action(),
                        KeyCode::Char('r') => {
                            let sec = state.seconds;
                            self.open_doctor(sec);
                        }
                        KeyCode::Esc | KeyCode::Char('q') => self.go_back(),
                        _ => {}
                    }
                }
            }

            Screen::Preferences(state) => match code {
                KeyCode::Up | KeyCode::Char('k') => state.previous(),
                KeyCode::Down | KeyCode::Char('j') => state.next(),
                KeyCode::Left => {
                    state.step_age_left(&mut self.config, &self.layout);
                    state.step_schedule_left();
                }
                KeyCode::Right => {
                    state.step_age_right(&mut self.config, &self.layout);
                    state.step_schedule_right();
                }
                KeyCode::Enter => match state.focus {
                    PreferencesFocus::AgeThreshold => {}
                    PreferencesFocus::ScheduledCleanup => {
                        state.apply_schedule(&mut self.config, &*self.host, &self.layout);
                    }
                    PreferencesFocus::ProjectFolders => {
                        self.open(Screen::PathListEditor(PathListEditorState::new(
                            PathListKind::ProjectFolders,
                            &self.ignore,
                            &self.layout,
                        )));
                    }
                    PreferencesFocus::ProtectedPaths => {
                        self.open(Screen::PathListEditor(PathListEditorState::new(
                            PathListKind::ProtectedPaths,
                            &self.ignore,
                            &self.layout,
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
                    code if is_removal_key(code) => state.prompt_remove(),
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
        let sort_section = |date: &'static str| {
            HelpSection::new(
                "Sort by",
                vec![
                    Hint::new("⇧S", "Size"),
                    Hint::new("⇧N", "Name"),
                    Hint::new("⇧T", date),
                ],
            )
        };

        match &self.screen {
            Screen::Workspace(state) => {
                vec![
                    HelpSection::new(
                        "Menu",
                        vec![
                            Hint::new("↵", "Select"),
                            Hint::new("1-9, 0", "Quick select"),
                        ],
                    ),
                    general(vec![
                        navigate,
                        if state.submenu_row().is_some() {
                            back
                        } else {
                            Hint::new("⎋ / q", "Quit")
                        },
                    ]),
                ]
            }
            Screen::Scanning(_, _, _) => vec![
                HelpSection::new("Scanning", vec![Hint::new("⎋ / q", "Cancel scan")]),
                general(Vec::new()),
            ],
            Screen::Select(state) if state.is_single_row_list() => {
                let mut hints = vec![Hint::new("/", "Search")];
                let has_rows = state
                    .scan
                    .groups
                    .iter()
                    .any(|group| !group.items.is_empty());
                if has_rows {
                    hints.extend([
                        Hint::new("↵ / →", "Expand"),
                        Hint::new("↵ / ←", "Collapse"),
                        Hint::new("d", "Uninstall"),
                        Hint::new("u", "Upgrade"),
                        Hint::new("⇧U", "Upgrade all"),
                    ]);
                }
                let mut sections = vec![HelpSection::new("Homebrew", hints)];
                if has_rows {
                    sections.push(sort_section("Date installed"));
                }
                sections.push(general(vec![navigate, back]));
                sections
            }
            Screen::Select(state) => {
                let enter = match state.scan.id.as_str() {
                    apps::LIST_ID => "Review",
                    uninstaller::PLAN_ID => "Uninstall",
                    _ => "Review & clean",
                };
                let mut hints = vec![
                    Hint::new("␣", "Toggle item"),
                    Hint::new("A", "Select all"),
                    Hint::new("N", "Select none"),
                    Hint::new("/", "Search"),
                ];
                if state.scan.id == large_files::SCAN_ID {
                    hints.push(Hint::new("O", "Reveal in Finder"));
                }
                hints.push(Hint::new(state.action_key("↵"), enter));
                let mut sections = vec![HelpSection::new("Select items", hints)];
                if state.sort_order.is_some() {
                    sections.push(sort_section("Date created"));
                }
                sections.push(general(vec![navigate, back]));
                sections
            }
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
                        Hint::new("d", "Delete selected"),
                    ],
                ),
                sort_section("Date created"),
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
            Screen::Startup(_) => vec![HelpSection::new(
                "Startup items",
                vec![
                    Hint::new("␣", "Toggle service"),
                    Hint::new("d", "Remove broken item"),
                    navigate,
                    back,
                ],
            )],
            Screen::Doctor(_) => vec![HelpSection::new(
                "Doctor",
                vec![
                    Hint::new("↵", "Action"),
                    Hint::new("r", "Recheck"),
                    navigate,
                    back,
                ],
            )],
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
                    vec![Hint::new("A", "Add path"), Hint::new("d", "Remove path")],
                ),
                general(vec![navigate, back]),
            ],
        }
    }

    fn start_removal(&mut self) {
        if matches!(&self.screen, Screen::Select(select) if select.upgrade.is_some()) {
            return self.start_upgrade();
        }
        if let Screen::Select(ref mut select) = self.screen {
            let mut selected_items: Vec<_> =
                select.scan.selected_items().into_iter().cloned().collect();
            if select.scan.id == homebrew::SCAN_ID {
                // The history names the formula as `taert brew uninstall` does, without details.
                for item in &mut selected_items {
                    let name = homebrew::label_name(&item.path.to_string_lossy()).to_string();
                    item.path = name.into();
                }
            }
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
                cancel: None,
            };

            let (tx, rx) = mpsc::channel();
            let ignore = self.ignore.clone();
            let scan_id = select.scan.id.clone();
            let (layout, host, permanent) =
                (self.layout.clone(), self.host.clone(), self.permanent);

            if scan_id == compress::SCAN_ID {
                let cancelled = Arc::new(AtomicBool::new(false));
                if let ModalOverlay::Removing { cancel, .. } = &mut select.overlay {
                    *cancel = Some(cancelled.clone());
                }
                thread::spawn(move || {
                    let compressor = Compressor {
                        host: &*host,
                        layout: &layout,
                        ignore: &ignore,
                    };
                    let paths: Vec<_> = selected_items.iter().map(|i| i.path.clone()).collect();
                    let total_count = paths.len();
                    let history = HistoryLog::new(layout.history_path());
                    let session = history.start_run(
                        &command_for_scan(compress::SCAN_ID),
                        "interactive",
                        Some(total_count),
                    );
                    let report = compressor.compress_selected(
                        &paths,
                        &cancelled,
                        |index, path| {
                            let _ = tx.send(RemovalMessage::Progress(RemovalProgress {
                                current_index: index + 1,
                                total_count,
                                path: path.to_path_buf(),
                            }));
                        },
                        |path, saved| {
                            session.record_item(&ItemOutcome::changed(path.to_path_buf(), saved));
                        },
                    );
                    session.finish();
                    let _ = tx.send(RemovalMessage::Finished(report));
                });

                self.removal_rx = Some(rx);
                return;
            }

            if scan_id == slim::SCAN_ID {
                thread::spawn(move || {
                    let mut total_saved = 0u64;
                    let mut apps_slimmed = 0usize;
                    let total_candidates = selected_items.len();
                    let mut failures = Vec::new();
                    let mut notes = Vec::new();
                    let mut any_trashed = false;
                    let history = HistoryLog::new(layout.history_path());
                    let session = history.start_run("slim", "interactive", Some(total_candidates));

                    for (idx, item) in selected_items.iter().enumerate() {
                        let _ = tx.send(RemovalMessage::Progress(RemovalProgress {
                            current_index: idx + 1,
                            total_count: total_candidates,
                            path: item.path.clone(),
                        }));
                        let app_name = item
                            .path
                            .file_stem()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_else(|| "App".to_string());
                        let installed_app = InstalledApp {
                            name: app_name,
                            path: item.path.clone(),
                            bundle_id: None,
                            executable: None,
                            size_bytes: item.size_bytes,
                            last_used: item.modified,
                        };
                        let (binaries, _) = slim::find_universal_binaries(&item.path);
                        match slim::slim_app_logged(
                            &session,
                            &installed_app,
                            &binaries,
                            &*host,
                            false,
                        ) {
                            Ok(done) => {
                                total_saved += done.saved;
                                apps_slimmed += 1;
                                any_trashed |= done.old_app.is_none();
                                if done.old_app.is_some() || done.holding_left.is_some() {
                                    notes.push(done.summary(&installed_app.name));
                                }
                            }
                            Err((step, err)) => {
                                failures.push(crate::core::cleaner::RemovalFailure {
                                    path: item.path.clone(),
                                    error: format!("Step {step} failed: {err}"),
                                });
                            }
                        }
                    }
                    session.finish();
                    if any_trashed {
                        notes.push(slim::TRASH_NOTE.to_string());
                    }

                    let report = RemovalReport {
                        freed_bytes: total_saved,
                        trashed_bytes: 0,
                        removed_count: apps_slimmed,
                        failures,
                        notes,
                    };
                    let _ = tx.send(RemovalMessage::Finished(report));
                });

                self.removal_rx = Some(rx);
                return;
            }

            let plan = self
                .plan
                .clone()
                .filter(|_| select.scan.id == uninstaller::PLAN_ID)
                .map(|mut plan| {
                    plan.scan = select.scan.clone();
                    plan
                });

            thread::spawn(move || {
                let tx_prog = tx.clone();
                let on_progress = move |prog| {
                    let _ = tx_prog.send(RemovalMessage::Progress(prog));
                };
                let report = match plan {
                    Some(plan) => {
                        let host = &*host;
                        let layout = &layout;
                        let ignore = &ignore;
                        let mode = if permanent {
                            RemovalMode::Permanent
                        } else {
                            RemovalMode::Trash
                        };
                        let app_names: Vec<_> =
                            plan.apps.iter().map(|a| a.app.name.as_str()).collect();
                        let command = format!("uninstall {}", app_names.join(", "));
                        let total = plan.scan.selected_items().len();
                        let session = HistoryLog::new(layout.history_path()).start_run(
                            &command,
                            "interactive",
                            Some(total),
                        );
                        let rep = Uninstaller {
                            host,
                            layout,
                            ignore,
                            mode,
                        }
                        .run_with_outcome(&plan, on_progress, |outcome| {
                            session.record_item(outcome)
                        });
                        session.finish();
                        rep
                    }
                    None => {
                        let command = command_for_scan(&scan_id);
                        let total = selected_items.len();
                        let session = HistoryLog::new(layout.history_path()).start_run(
                            &command,
                            "interactive",
                            Some(total),
                        );
                        let rep = remove_selected(
                            &*host,
                            &scan_id,
                            &selected_items,
                            permanent,
                            &ignore,
                            Some(on_progress),
                            Some(|outcome: &ItemOutcome| session.record_item(outcome)),
                        );
                        session.finish();
                        rep
                    }
                };
                let _ = tx.send(RemovalMessage::Finished(report));
            });

            self.removal_rx = Some(rx);
        }
    }

    /// Runs the upgrade the question asked about, one `brew upgrade` per package. Upgrades are
    /// not written to the history.
    fn start_upgrade(&mut self) {
        let Screen::Select(select) = &mut self.screen else {
            return;
        };
        let Some(plan) = select.upgrade.clone() else {
            return;
        };
        let total_count = plan.packages.len();
        let first = plan.packages.first().map(|package| package.name.clone());
        select.overlay = ModalOverlay::Removing {
            progress: RemovalProgress {
                current_index: 1,
                total_count,
                path: first.unwrap_or_default().into(),
            },
            spinner_idx: 0,
            cancel: None,
        };
        let (tx, rx) = mpsc::channel();
        let host = self.host.clone();
        thread::spawn(move || {
            let mut report = RemovalReport::default();
            for (index, package) in plan.packages.iter().enumerate() {
                let _ = tx.send(RemovalMessage::Progress(RemovalProgress {
                    current_index: index + 1,
                    total_count,
                    path: package.name.clone().into(),
                }));
                match homebrew::upgrade(&*host, &plan.brew, package.kind, &package.name) {
                    Ok(()) => report.removed_count += 1,
                    Err(error) => report.failures.push(RemovalFailure {
                        path: package.name.clone().into(),
                        error,
                    }),
                }
            }
            let _ = tx.send(RemovalMessage::Finished(report));
        });
        self.removal_rx = Some(rx);
    }

    fn start_analyze_removal(&mut self, mut mode: RemovalMode) {
        if self.permanent {
            mode = RemovalMode::Permanent;
        }
        if let Screen::Analyze(ref mut analyze) = self.screen {
            let items_to_remove = analyze.items_to_delete();
            if items_to_remove.is_empty() {
                analyze.overlay = AnalyzeOverlay::None;
                return;
            }
            analyze.cancel_update();

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
            let host = self.host.clone();
            let layout = self.layout.clone();

            thread::spawn(move || {
                let tx_prog = tx.clone();
                let command = "analyze".to_string();
                let total = scan_items.len();
                let session = HistoryLog::new(layout.history_path()).start_run(
                    &command,
                    "interactive",
                    Some(total),
                );
                let report = Cleaner::new(&*host, mode).remove_items_with_callbacks(
                    &scan_items,
                    &ignore,
                    Some(move |prog| {
                        let _ = tx_prog.send(RemovalMessage::Progress(prog));
                    }),
                    Some(|outcome: &ItemOutcome| session.record_item(outcome)),
                );
                session.finish();
                let _ = tx.send(RemovalMessage::Finished(report));
            });

            self.removal_rx = Some(rx);
        }
    }

    fn project_folders(&self) -> ProjectFoldersStore {
        ProjectFoldersStore::load_or_default_at(&self.layout.projects_path())
    }

    fn handle_main_menu_choice(&mut self, choice: MainMenuChoice) {
        match choice {
            MainMenuChoice::CleanCaches => {
                if let Screen::Workspace(ref mut w) = self.screen {
                    w.open_clean_submenu();
                }
            }
            MainMenuChoice::ProjectCleanup => {
                let folder_count = self.project_folders().roots.len();
                if let Screen::Workspace(ref mut w) = self.screen {
                    w.open_confirmation(StartConfirmKind::ProjectCleanup(folder_count));
                }
            }
            MainMenuChoice::DockerCleanup => self.launch_docker_scan(),
            MainMenuChoice::UninstallApps => self.launch_app_scan(),
            MainMenuChoice::Homebrew => self.launch_brew_scan(),
            MainMenuChoice::Doctor => self.open_doctor(10),
            MainMenuChoice::StartupItems => self.open_startup(false),
            MainMenuChoice::SlimApps => self.launch_slim_scan(),
            MainMenuChoice::Compress => self.launch_compress_scan(),
            MainMenuChoice::ExploreStorage => {
                if let Screen::Workspace(ref mut w) = self.screen {
                    w.open_confirmation(StartConfirmKind::ExploreHome);
                }
            }
            MainMenuChoice::LargeFiles => self.launch_large_scan(),
            MainMenuChoice::SystemStatus => {
                self.open(Screen::SystemStatus(Box::default()));
            }
            MainMenuChoice::Preferences => {
                self.open(Screen::Preferences(PreferencesState::new(
                    &self.config,
                    &self.ignore,
                    &self.layout,
                )));
            }
            MainMenuChoice::ProtectedPaths => {
                self.open(Screen::PathListEditor(
                    PathListEditorState::new(
                        PathListKind::ProtectedPaths,
                        &self.ignore,
                        &self.layout,
                    )
                    .top_level(),
                ));
            }
        }
    }

    fn start_clean_subcategory(&mut self, subcat: CleanSubcategory) {
        let category = match subcat {
            CleanSubcategory::All => return self.launch_all_categories_scan(),
            CleanSubcategory::Developer => Category::Dev,
            CleanSubcategory::UserJunk => Category::Junk,
            CleanSubcategory::AiTools => Category::Ai,
            CleanSubcategory::Orphans => Category::Orphans,
            CleanSubcategory::Trash => Category::Trash,
        };
        self.launch_category_scan(category);
    }

    fn execute_start_confirmed(&mut self, kind: StartConfirmKind) {
        match kind {
            StartConfirmKind::ExploreHome => {
                let home = self.layout.home.clone();
                self.open(Screen::Analyze(Box::new(AnalyzeState::with_cache(
                    home,
                    self.ignore.clone(),
                    self.storage_cache.clone(),
                ))));
            }
            StartConfirmKind::ProjectCleanup(_) => {
                let roots = self.project_folders().existing_roots();
                self.launch_purge_scan(roots);
            }
        }
    }

    fn launch_docker_scan(&mut self) {
        let (tx, rx) = mpsc::channel();

        thread::spawn(move || {
            let message = match docker::DockerClient::connect().and_then(|client| client.scan()) {
                Ok(scan) => ScanMessage::Finished(scan),
                Err(error) => ScanMessage::Failed {
                    id: docker::SCAN_ID.to_string(),
                    title: docker::TITLE.to_string(),
                    problem: error.to_string(),
                },
            };
            let _ = tx.send(message);
        });

        self.open(Screen::Scanning(
            ProgressState::new("Reading Docker").top_level(),
            rx,
            Arc::new(AtomicBool::new(false)),
        ));
    }

    fn launch_brew_scan(&mut self) {
        let (tx, rx) = mpsc::channel();
        let layout = self.layout.clone();
        let host = self.host.clone();

        thread::spawn(move || {
            let inventory =
                homebrew::scan(&layout, &*host).unwrap_or_else(|message| homebrew::Inventory {
                    notes: vec![message],
                    ..homebrew::Inventory::default()
                });
            let _ = tx.send(ScanMessage::Homebrew(inventory));
        });

        self.open(Screen::Scanning(
            ProgressState::new("Reading Homebrew").top_level(),
            rx,
            Arc::new(AtomicBool::new(false)),
        ));
    }

    fn launch_compress_scan(&mut self) {
        let (tx, rx) = mpsc::channel();
        let layout = self.layout.clone();

        thread::spawn(move || {
            let scan = crate::core::compress::compress_candidates_scan(&layout);
            let _ = tx.send(ScanMessage::Finished(scan));
        });

        self.open(Screen::Scanning(
            ProgressState::new("Reading Candidates").top_level(),
            rx,
            Arc::new(AtomicBool::new(false)),
        ));
    }

    pub fn launch_slim_scan(&mut self) {
        let (tx, rx) = mpsc::channel();
        let layout = self.layout.clone();
        let host = self.host.clone();
        let ignore = self.ignore.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_scan = cancel.clone();

        thread::spawn(move || {
            if !slim::is_apple_silicon(&*host) {
                let _ = tx.send(ScanMessage::Failed {
                    id: slim::SCAN_ID.to_string(),
                    title: slim::TITLE.to_string(),
                    problem: "Slimming universal apps requires an Apple Silicon Mac.".to_string(),
                });
                return;
            }
            let (progress, bridge) = forward_progress(tx.clone());
            let scan = slim::slim_scan(&layout, &*host, &ignore, Some(progress), Some(cancel_scan));
            let _ = bridge.join();
            let _ = tx.send(ScanMessage::Finished(scan));
        });

        self.open(Screen::Scanning(
            ProgressState::new("Scanning Universal Apps").top_level(),
            rx,
            cancel,
        ));
    }

    pub fn launch_large_scan(&mut self) {
        let (tx, rx) = mpsc::channel();
        let host = self.host.clone();
        let ignore = self.ignore.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_scan = cancel.clone();
        let layout = self.layout.clone();

        thread::spawn(move || {
            let (progress, bridge) = forward_progress(tx.clone());
            let options = large_files::LargeScanOptions::default();
            let scan = large_files::scan(
                &layout.home,
                &options,
                &layout,
                &*host,
                &ignore,
                Some(progress),
                Some(cancel_scan),
            );
            let _ = bridge.join();
            let _ = tx.send(ScanMessage::Finished(scan));
        });

        self.open(Screen::Scanning(
            ProgressState::new("Scanning Large Files").top_level(),
            rx,
            cancel,
        ));
    }

    fn launch_category_scan(&mut self, category: Category) {
        let (fwd_tx, fwd_rx) = mpsc::channel();
        let ignore = self.ignore.clone();
        let (layout, host) = (self.layout.clone(), self.host.clone());
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let cancel_clone = Arc::clone(&cancel_flag);

        thread::spawn(move || {
            let (progress, bridge) = forward_progress(fwd_tx.clone());
            let _ = progress.send(ScanProgressEvent::CategoryStarted {
                title: category.title().to_string(),
                index: 1,
                total: 1,
            });

            let scan = CategoryScanner::scan(
                &ScanContext {
                    layout: &layout,
                    host: &*host,
                },
                category,
                &ignore,
                Some(progress),
                Some(cancel_clone),
            );
            let _ = bridge.join();
            let _ = fwd_tx.send(ScanMessage::Finished(scan));
        });

        self.open(Screen::Scanning(
            ProgressState::new(format!("Scanning {}", category.title())),
            fwd_rx,
            cancel_flag,
        ));
    }

    fn launch_all_categories_scan(&mut self) {
        let (fwd_tx, fwd_rx) = mpsc::channel();
        let ignore = self.ignore.clone();
        let (layout, host) = (self.layout.clone(), self.host.clone());
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let cancel_clone = Arc::clone(&cancel_flag);

        thread::spawn(move || {
            let (progress, bridge) = forward_progress(fwd_tx.clone());
            let scans = scan_all_categories(
                &ScanContext {
                    layout: &layout,
                    host: &*host,
                },
                &ignore,
                Some(progress),
                Some(cancel_clone),
            );
            let _ = bridge.join();
            let scan = CategoryScan::combined("All Caches", scans);
            let _ = fwd_tx.send(ScanMessage::Finished(scan));
        });

        self.open(Screen::Scanning(
            ProgressState::new(format!(
                "Scanning {}, 1 of {}",
                Category::ALL[0].title(),
                Category::ALL.len()
            )),
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
    use std::time::Instant;
    use tempfile::tempdir;

    /// An app that sees an empty Mac: no files at its home and a host that runs nothing.
    fn offline(mut app: App) -> App {
        app.layout = Layout {
            home: "/nonexistent/home".into(),
            root: "/nonexistent/root".into(),
            user_cache: None,
        };
        app.host = Arc::new(crate::core::host::fake::FakeHost::idle());
        app
    }

    fn new_app() -> App {
        offline(App::new(IgnoreStore::default(), Config::default()))
    }

    #[test]
    fn a_failed_slim_scan_shows_its_own_name() {
        let mut app = new_app();

        app.handle_key(KeyCode::Char('8'));
        let started = Instant::now();
        while matches!(app.screen, Screen::Scanning(..)) {
            assert!(started.elapsed() < Duration::from_secs(2), "still scanning");
            thread::sleep(Duration::from_millis(10));
            app.poll_scan();
        }

        assert_eq!(select(&app).scan.id, slim::SCAN_ID);
        assert_eq!(select(&app).scan.title, "Slim Universal Apps");
        let text = screen_text(&app);
        assert!(
            text.contains("Slim Universal Apps isn't available"),
            "{text}"
        );
        assert!(text.contains("requires an Apple Silicon Mac"), "{text}");
        assert!(text.contains("Slim apps"), "{text}");
        assert!(!text.contains("Docker"), "{text}");
    }

    #[test]
    fn a_failed_docker_read_still_says_docker() {
        let mut app = new_app();
        let (tx, rx) = mpsc::channel();
        tx.send(ScanMessage::Failed {
            id: docker::SCAN_ID.to_string(),
            title: docker::TITLE.to_string(),
            problem: "Can't reach Docker.".to_string(),
        })
        .unwrap();
        app.screen = Screen::Scanning(
            ProgressState::new("Reading Docker"),
            rx,
            Arc::new(AtomicBool::new(false)),
        );

        app.poll_scan();

        assert_eq!(select(&app).scan.id, docker::SCAN_ID);
        assert_eq!(select(&app).scan.title, "Docker");
        let text = screen_text(&app);
        assert!(text.contains("Docker isn't available"), "{text}");
        assert!(text.contains("Can't reach Docker."), "{text}");
    }

    fn analyze_app(dir: &Path) -> App {
        let mut app = new_app();
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
    fn help_names_the_quick_keys_of_the_main_menu() {
        let app = new_app();

        let sections = app.help_sections();
        let quick = sections[0]
            .shortcuts
            .iter()
            .find(|h| h.desc == "Quick select");

        assert_eq!(quick.unwrap().key, "1-9, 0");
    }

    #[test]
    fn the_third_quick_key_opens_docker_cleanup() {
        let mut state = WorkspaceState::new();

        assert_eq!(
            state.select_by_number(3),
            Some(MainMenuChoice::DockerCleanup)
        );
    }

    #[test]
    fn the_fifth_quick_key_opens_homebrew() {
        let mut state = WorkspaceState::new();

        assert_eq!(state.select_by_number(5), Some(MainMenuChoice::Homebrew));
    }

    #[test]
    fn the_sixth_quick_key_opens_doctor() {
        let mut state = WorkspaceState::new();

        assert_eq!(state.select_by_number(6), Some(MainMenuChoice::Doctor));
    }

    #[test]
    fn the_seventh_quick_key_opens_startup_items() {
        let mut state = WorkspaceState::new();

        assert_eq!(
            state.select_by_number(7),
            Some(MainMenuChoice::StartupItems)
        );
    }

    #[test]
    fn the_eighth_quick_key_opens_slim_apps() {
        let mut state = WorkspaceState::new();

        assert_eq!(state.select_by_number(8), Some(MainMenuChoice::SlimApps));
    }

    #[test]
    fn the_ninth_quick_key_opens_compress() {
        let mut state = WorkspaceState::new();

        assert_eq!(state.select_by_number(9), Some(MainMenuChoice::Compress));
    }

    #[test]
    fn the_zero_key_opens_explore_storage_and_large_files_has_no_number_key() {
        let mut app = new_app();

        app.handle_key(KeyCode::Char('0'));

        assert!(matches!(app.screen, Screen::Workspace(_)));
        let Screen::Workspace(state) = &app.screen else {
            unreachable!()
        };
        assert_eq!(state.current_main_choice(), MainMenuChoice::ExploreStorage);
        assert_eq!(state.items[10].0, MainMenuChoice::LargeFiles);
    }

    fn select_app(scan: CategoryScan) -> App {
        let mut app = new_app();
        app.screen = Screen::Select(Box::new(SelectState::with_preserved_selection(scan)));
        app
    }

    fn scan_with_id(id: &str) -> CategoryScan {
        let mut item = ScanItem::new("/nowhere/a".into(), 1, true, false);
        item.modified = None;
        CategoryScan {
            notes: Vec::new(),
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

        app.handle_key(KeyCode::Char('T'));
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

        app.handle_key(KeyCode::Char('T'));
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
        assert_eq!(help_row(&table, "Date created"), Some("⇧T"));

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
        assert_eq!(keys, ["⇧S", "⇧N", "⇧T"]);
        assert_eq!(help_row(&app, "Date created"), Some("⇧T"));
    }

    #[test]
    fn shift_letters_pick_the_sort_order() {
        let dir = tempdir().unwrap();
        let mut app = analyze_app(dir.path());

        app.handle_key(KeyCode::Char('N'));
        assert_eq!(analyze(&app).sort_order, SortOrder::Name);
        app.handle_key(KeyCode::Char('T'));
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
    fn the_delete_dialog_of_a_permanent_run_focuses_delete_permanently() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("file.txt"), "x").unwrap();
        let mut app = analyze_app(dir.path());
        app.permanent = true;

        app.handle_key(KeyCode::Char(' '));
        app.handle_key(KeyCode::Char('d'));

        assert!(matches!(
            analyze(&app).overlay,
            AnalyzeOverlay::ConfirmDelete {
                button: AnalyzeConfirmButton::DeletePermanently,
                ..
            }
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

    fn wait_for_removal(app: &mut App) {
        while app.removal_rx.is_some() {
            app.poll_removal();
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Makes `command` on `host` wait until the returned sender is dropped.
    fn hold(host: &crate::core::host::fake::FakeHost, command: &str) -> mpsc::Sender<()> {
        let (release, held) = mpsc::channel::<()>();
        let held = std::sync::Mutex::new(held);
        host.when(command, move || {
            let _ = held.lock().unwrap().recv();
        });
        release
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
            notes: Vec::new(),
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
        let mut app = new_app();
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
        let mut app = new_app();

        press(
            &mut app,
            &[KeyCode::Enter, KeyCode::Down, KeyCode::Enter, KeyCode::Esc],
        );

        assert_clean_submenu(&app, 1);
    }

    #[test]
    fn esc_returns_to_the_menu_row_a_screen_was_opened_from() {
        let mut app = new_app();

        press(&mut app, &[KeyCode::Char('7'), KeyCode::Esc]);

        assert_main_menu(&app, 6);
    }

    #[test]
    fn esc_from_explore_does_not_reopen_its_start_dialog() {
        let mut app = new_app();

        press(
            &mut app,
            &[KeyCode::Char('0'), KeyCode::Enter, KeyCode::Esc],
        );

        assert_main_menu(&app, 9);
    }

    #[test]
    fn esc_from_protected_paths_opened_in_the_menu_returns_to_the_menu() {
        let mut app = new_app();

        press(&mut app, &[KeyCode::End, KeyCode::Enter, KeyCode::Esc]);

        assert_main_menu(&app, 13);
    }

    #[test]
    fn esc_from_a_list_opened_in_preferences_keeps_the_row_and_refreshes_counts() {
        let mut app = new_app();

        press(
            &mut app,
            &[
                KeyCode::End,
                KeyCode::Up,
                KeyCode::Enter,
                KeyCode::Down,
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
        assert_main_menu(&app, 12);
    }

    #[test]
    fn esc_on_a_list_opened_from_the_command_line_opens_the_main_menu() {
        let mut app = offline(App::with_select(
            empty_scan(),
            IgnoreStore::default(),
            Config::default(),
        ));

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
    fn esc_cancels_the_measuring_while_the_dialog_is_hidden() {
        let dir = tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let mut app = analyze_app(dir.path());
        analyze(&app).cache.write().unwrap().clear();

        app.handle_key(KeyCode::Enter);
        assert!(analyze(&app).measuring_rx.is_some());
        assert!(!screen_text(&app).contains("Measuring"));

        app.handle_key(KeyCode::Esc);

        let state = analyze(&app);
        assert!(state.measuring_rx.is_none());
        assert!(state.cancel_flag.load(Ordering::Relaxed));
        assert_eq!(state.levels.len(), 1);
    }

    #[test]
    fn backspace_does_not_cancel_the_measuring() {
        let dir = tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let mut app = analyze_app(dir.path());
        analyze(&app).cache.write().unwrap().clear();

        app.handle_key(KeyCode::Enter);
        app.handle_key(KeyCode::Backspace);

        let state = analyze(&app);
        assert!(state.measuring_rx.is_some());
        assert!(!state.cancel_flag.load(Ordering::Relaxed));
        assert_eq!(state.levels.len(), 1);
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

    // `dir` holds `sub`; the cached view of `sub` is old and lists `rows`.
    fn make_sub_stale(app: &App, dir: &Path, rows: &[&str]) {
        use crate::core::cache::{ChildItem, FolderSnapshot};
        let sub = dir.join("sub");
        let children = rows
            .iter()
            .map(|name| ChildItem {
                name: (*name).to_string(),
                path: sub.join(name),
                size_bytes: 10,
                is_dir: false,
                is_package: false,
                selected: false,
                created: None,
            })
            .collect();
        let snapshot = FolderSnapshot {
            path: sub.clone(),
            children,
            total_bytes: 10 * rows.len() as u64,
            inaccessible_count: 0,
            skipped_count: 0,
        };
        analyze(app)
            .cache
            .write()
            .unwrap()
            .put_snapshot(sub.clone(), snapshot);
        let old = std::time::SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::open(&sub)
            .unwrap()
            .set_modified(old)
            .unwrap();
    }

    fn updating_app(dir: &Path) -> App {
        std::fs::create_dir(dir.join("sub")).unwrap();
        let mut app = analyze_app(dir);
        make_sub_stale(&app, dir, &["one.bin", "two.bin"]);
        app.handle_key(KeyCode::Enter);
        app
    }

    fn update_flag(app: &App) -> Arc<AtomicBool> {
        analyze(app).update.as_ref().unwrap().cancel.clone()
    }

    #[test]
    fn keys_work_while_a_folder_updates() {
        let dir = tempdir().unwrap();
        let mut app = updating_app(dir.path());

        assert_eq!(analyze(&app).levels.len(), 2);
        app.handle_key(KeyCode::Down);

        assert_eq!(analyze(&app).current_level().unwrap().selected, 1);
        let text = screen_text(&app);
        assert!(text.contains("Updating…"), "{text}");
        assert!(!text.contains("Measuring"), "{text}");
    }

    #[test]
    fn leaving_explore_stops_the_update() {
        let dir = tempdir().unwrap();
        let mut app = updating_app(dir.path());
        let flag = update_flag(&app);

        app.handle_key(KeyCode::Char('q'));

        assert!(flag.load(Ordering::Relaxed));
    }

    #[test]
    fn deleting_in_an_updating_folder_stops_the_update() {
        let dir = tempdir().unwrap();
        let mut app = updating_app(dir.path());
        let (_sandbox, layout) = crate::core::apps::testing::sandbox();
        app.layout = layout;
        let flag = update_flag(&app);

        press(&mut app, &[KeyCode::Char('d'), KeyCode::Enter]);

        assert!(flag.load(Ordering::Relaxed));
        let started = std::time::Instant::now();
        while app.removal_rx.is_some() {
            assert!(started.elapsed() < Duration::from_secs(5), "removal hangs");
            app.poll_removal();
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn screen_text(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| app.render_screen(frame)).unwrap();
        format!("{:?}", terminal.backend().buffer())
    }

    /// The lines of a `width` by `height` screen.
    fn screen_rows(app: &App, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.render_screen(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect()
    }

    #[test]
    fn protected_paths_name_preferences_only_when_opened_there() {
        let mut app = new_app();

        press(&mut app, &[KeyCode::End, KeyCode::Enter]);
        assert!(!screen_text(&app).contains("Preferences"));

        press(
            &mut app,
            &[
                KeyCode::Esc,
                KeyCode::End,
                KeyCode::Up,
                KeyCode::Enter,
                KeyCode::Down,
                KeyCode::Down,
                KeyCode::Down,
                KeyCode::Enter,
            ],
        );
        assert!(screen_text(&app).contains("Preferences"));
    }

    #[test]
    fn the_age_threshold_set_in_preferences_is_saved_in_the_layout() {
        let (_temp, layout) = crate::core::apps::testing::sandbox();
        let mut app = new_app();
        app.layout = layout.clone();

        press(
            &mut app,
            &[KeyCode::End, KeyCode::Up, KeyCode::Enter, KeyCode::Right],
        );

        let saved = Config::load_from(&layout.config_path()).unwrap();
        assert_eq!(saved.older_than_days, app.config.older_than_days);
        assert_ne!(saved.older_than_days, Config::default().older_than_days);
    }

    fn schedule_app(
        host: &Arc<crate::core::host::fake::FakeHost>,
    ) -> (tempfile::TempDir, Layout, App) {
        let (temp, layout) = crate::core::apps::testing::sandbox();
        let mut app = new_app();
        app.layout = layout.clone();
        app.host = host.clone();
        press(
            &mut app,
            &[KeyCode::End, KeyCode::Up, KeyCode::Enter, KeyCode::Down],
        );
        (temp, layout, app)
    }

    fn bootstraps(host: &crate::core::host::fake::FakeHost) -> usize {
        host.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.starts_with("/bin/launchctl bootstrap"))
            .count()
    }

    #[test]
    fn arrows_on_the_schedule_row_do_not_run_launchctl() {
        let host = Arc::new(crate::core::host::fake::FakeHost::idle());
        let (_temp, layout, mut app) = schedule_app(&host);

        press(&mut app, &[KeyCode::Right]);

        let text = screen_text(&app);
        assert!(text.contains("Every day"));
        assert!(text.contains("Apply"));
        assert!(!host.ran("/bin/launchctl"));
        assert!(app.config.schedule.is_none());
        assert!(!crate::core::schedule::agent_plist_path(&layout).exists());
    }

    #[test]
    fn enter_applies_the_schedule_once() {
        let host = Arc::new(crate::core::host::fake::FakeHost::idle());
        host.reply("/bin/launchctl", "");
        let (_temp, _layout, mut app) = schedule_app(&host);

        press(&mut app, &[KeyCode::Right]);

        assert_eq!(bootstraps(&host), 0);

        press(&mut app, &[KeyCode::Enter]);

        assert_eq!(bootstraps(&host), 1);
        let schedule = app.config.schedule.clone().unwrap();
        assert_eq!(
            schedule.every,
            crate::core::schedule::ScheduleFrequency::Day
        );
        assert!(screen_text(&app).contains("Enabled scheduled cleanup"));
        let calls = host.calls.lock().unwrap().len();

        press(&mut app, &[KeyCode::Enter]);

        assert_eq!(host.calls.lock().unwrap().len(), calls);
    }

    #[test]
    fn a_failed_load_shows_the_error_and_keeps_the_old_value() {
        let host = Arc::new(crate::core::host::fake::FakeHost::idle());
        host.fail("/bin/launchctl bootstrap", "Bootstrap failed: 5");
        let (_temp, layout, mut app) = schedule_app(&host);

        press(&mut app, &[KeyCode::Right, KeyCode::Enter]);

        let text = screen_text(&app);
        assert!(text.contains("Bootstrap failed: 5"), "{text}");
        assert!(!text.contains("Enabled"));
        let Screen::Preferences(prefs) = &app.screen else {
            panic!("expected the preferences screen");
        };
        assert_eq!(prefs.schedule, None);
        assert_eq!(prefs.applied_schedule, None);
        assert!(app.config.schedule.is_none());
        assert!(!layout.config_path().exists());
        let calls = host.calls.lock().unwrap().len();

        press(&mut app, &[KeyCode::Right]);

        assert_eq!(host.calls.lock().unwrap().len(), calls);
    }

    #[test]
    fn stepping_the_age_after_a_failed_apply_shows_the_new_save() {
        let host = Arc::new(crate::core::host::fake::FakeHost::idle());
        host.fail("/bin/launchctl bootstrap", "Bootstrap failed: 5");
        let (_temp, _layout, mut app) = schedule_app(&host);

        press(&mut app, &[KeyCode::Right, KeyCode::Enter]);
        assert!(screen_text(&app).contains("Bootstrap failed: 5"));

        press(&mut app, &[KeyCode::Up, KeyCode::Right]);

        let text = screen_text(&app);
        assert!(text.contains("Saved age threshold"), "{text}");
        assert!(!text.contains("Bootstrap failed: 5"), "{text}");
    }

    #[test]
    fn enter_on_off_removes_the_schedule() {
        let host = Arc::new(crate::core::host::fake::FakeHost::idle());
        host.reply("/bin/launchctl", "");
        let (_temp, layout, mut app) = schedule_app(&host);
        press(&mut app, &[KeyCode::Right, KeyCode::Enter]);
        assert!(app.config.schedule.is_some());
        let plist = crate::core::schedule::agent_plist_path(&layout);
        assert!(plist.exists());
        let bootouts = |h: &crate::core::host::fake::FakeHost| {
            h.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.starts_with("/bin/launchctl bootout"))
                .count()
        };
        let before = bootouts(&host);

        press(&mut app, &[KeyCode::Left]);

        assert!(screen_text(&app).contains("Off"));
        assert_eq!(bootouts(&host), before);

        press(&mut app, &[KeyCode::Enter]);

        assert_eq!(bootouts(&host), before + 1);
        assert!(screen_text(&app).contains("Disabled scheduled cleanup"));
        assert!(app.config.schedule.is_none());
        assert!(!plist.exists());
    }

    #[test]
    fn scans_started_in_the_menu_are_not_shown_under_clean_caches() {
        let mut app = new_app();

        press(&mut app, &[KeyCode::Char('2'), KeyCode::Enter]);
        assert!(!screen_text(&app).contains("Clean caches"));

        press(&mut app, &[KeyCode::Esc, KeyCode::Char('4')]);
        assert!(!screen_text(&app).contains("Clean caches"));
        app.handle_key(KeyCode::Esc);
    }

    #[test]
    fn scans_started_in_clean_caches_are_shown_under_it() {
        let mut app = new_app();

        press(&mut app, &[KeyCode::Enter, KeyCode::Enter]);
        assert!(screen_text(&app).contains("Clean caches"));
        app.handle_key(KeyCode::Esc);
    }

    #[test]
    fn project_results_are_not_shown_under_clean_caches() {
        assert!(!screen_text(&select_app(scan_with_id("purge"))).contains("Clean caches"));
        assert!(screen_text(&select_app(scan_with_id("dev"))).contains("Clean caches"));
    }

    #[test]
    fn removing_something_that_frees_nothing_still_counts() {
        let mut app = select_app(scan_with_id("dev"));
        let Screen::Select(state) = &mut app.screen else {
            panic!("expected the select screen");
        };
        state.overlay = ModalOverlay::Outcome(RemovalReport {
            removed_count: 1,
            ..RemovalReport::default()
        });

        assert!(!screen_text(&app).contains("Nothing was removed"));
    }

    #[test]
    fn the_outcome_shows_even_when_the_list_ended_up_empty() {
        let mut app = offline(App::with_select(
            empty_scan(),
            IgnoreStore::default(),
            Config::default(),
        ));
        let Screen::Select(state) = &mut app.screen else {
            panic!("expected the select screen");
        };
        state.overlay = ModalOverlay::Outcome(RemovalReport {
            removed_count: 2,
            freed_bytes: 2_000_000,
            ..RemovalReport::default()
        });

        assert!(screen_text(&app).contains("Clean Complete"));
    }

    fn footer(app: &App) -> String {
        screen_rows(app, 160, 30).pop().unwrap_or_default()
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
        assert_footer(&app, &["⇧S/N/T  Sort: Size", "⎋  Back", "?  More"], &[]);
    }

    #[test]
    fn an_empty_list_offers_only_back_and_help() {
        let app = offline(App::with_select(
            empty_scan(),
            IgnoreStore::default(),
            Config::default(),
        ));

        assert_footer(&app, &["⎋  Back", "?  More"], &LIST_HINTS);
    }

    fn two_item_scan() -> CategoryScan {
        let item = |path: &str| {
            let mut item = ScanItem::new(path.into(), 1, false, false);
            item.modified = None;
            item
        };
        CategoryScan {
            notes: Vec::new(),
            id: "dev".to_string(),
            title: "List".to_string(),
            groups: vec![
                ToolGroup {
                    tool: "One".to_string(),
                    items: vec![item("/nowhere/slack")],
                },
                ToolGroup {
                    tool: "Two".to_string(),
                    items: vec![item("/nowhere/zoom"), item("/nowhere/zed")],
                },
            ],
        }
    }

    fn selected_paths(app: &App) -> Vec<String> {
        let scan = &select(app).scan;
        let items = scan.selected_items();
        items
            .iter()
            .map(|item| item.path.display().to_string())
            .collect()
    }

    fn search(app: &mut App, text: &str) {
        app.handle_key(KeyCode::Char('/'));
        for letter in text.chars() {
            app.handle_key(KeyCode::Char(letter));
        }
        app.handle_key(KeyCode::Enter);
    }

    #[test]
    fn select_all_takes_only_what_the_search_shows() {
        let mut app = select_app(two_item_scan());

        search(&mut app, "slack");
        app.handle_key(KeyCode::Char('a'));

        assert_eq!(selected_paths(&app), ["/nowhere/slack"]);
    }

    #[test]
    fn select_none_leaves_what_the_search_hides() {
        let mut app = select_app(two_item_scan());
        app.handle_key(KeyCode::Char('a'));

        search(&mut app, "slack");
        app.handle_key(KeyCode::Char('n'));

        assert_eq!(selected_paths(&app), ["/nowhere/zoom", "/nowhere/zed"]);
    }

    #[test]
    fn a_search_leaves_the_cursor_on_an_item_and_never_on_a_section() {
        let mut app = select_app(two_item_scan());

        press(&mut app, &[KeyCode::Down, KeyCode::Down]);
        search(&mut app, "zed");
        app.handle_key(KeyCode::Char(' '));

        assert_eq!(selected_paths(&app), ["/nowhere/zed"]);
    }

    #[test]
    fn scrolling_a_path_never_splits_a_letter() {
        let path = "/nowhere/Яндекс/файл";
        let mut scan = scan_with_id("dev");
        scan.groups[0].items[0].path = path.into();
        let mut app = select_app(scan);

        for _ in 0..5 {
            app.handle_key(KeyCode::Right);
            screen_text(&app);
        }
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
                "⇧S/N/T  Sort: Size",
                "d  Delete",
                "↑↓  Move",
                "⎋  Back",
                "?  More",
            ],
            &["↵  Open"],
        );

        app.handle_key(KeyCode::Char(' '));

        assert_footer(&app, &["↵  Open"], &["d  Delete"]);
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
        let mut app = new_app();

        press(&mut app, &[KeyCode::End, KeyCode::Up, KeyCode::Enter]);
        assert_footer(&app, &["←→  Adjust", "⎋  Back"], &[]);

        // Moving to project folders (a list link) removes the adjust arrows
        app.handle_key(KeyCode::Down);
        app.handle_key(KeyCode::Down);
        assert_footer(&app, &["⎋  Back"], &["Adjust"]);
    }

    #[test]
    fn a_path_list_offers_removal_only_when_it_has_paths() {
        let mut app = new_app();

        press(&mut app, &[KeyCode::End, KeyCode::Enter]);
        assert_footer(&app, &["Add", "Back", "More"], &["Remove", "Move"]);

        app.handle_key(KeyCode::Esc);
        app.ignore.roots.push("/nowhere/protected".to_string());

        press(&mut app, &[KeyCode::End, KeyCode::Enter]);
        assert_footer(&app, &["Add", "Remove", "Move", "Back", "More"], &[]);
    }

    #[test]
    fn the_main_menu_says_esc_quits_and_the_submenu_says_it_goes_back() {
        let mut app = new_app();

        assert_footer(&app, &["⎋  Quit"], &["Back"]);

        app.handle_key(KeyCode::Enter);

        assert_footer(&app, &["⎋  Back"], &["Quit"]);
    }

    #[test]
    fn a_dialog_empties_the_footer_because_it_lists_its_own_keys() {
        let toolbar = [
            "Quit", "Move", "More", "Open", "Select", "All", "Review", "Delete",
        ];

        let mut app = new_app();
        app.handle_key(KeyCode::Char('2'));
        assert_footer(&app, &[], &toolbar);

        let mut app = select_app(scan_with_id("dev"));
        app.handle_key(KeyCode::Char('d'));
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
        let mut app = new_app();
        app.open(Screen::SystemStatus(Box::default()));
        let draw = |app: &App, width, height| {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| app.render_screen(frame)).unwrap();
        };

        draw(&app, 205, 85);
        assert_eq!(help_row(&app, "Scroll"), None);

        draw(&app, 50, 16);
        assert!(help_row(&app, "Scroll").is_some());
    }

    mod uninstall {
        use super::*;
        use crate::core::apps::testing::{app as sample_app, fake_app, sandbox};
        use crate::core::host::fake::FakeHost;
        use crate::core::leftovers::{Evidence, UninstallRole};
        use crate::core::uninstaller::{AppPlan, PLAN_ID};
        use tempfile::TempDir;

        fn app_in_sandbox() -> (TempDir, App) {
            let (temp, layout) = sandbox();
            let mut app = new_app();
            app.layout = layout;
            let host = FakeHost::default();
            host.simulate_trash();
            app.host = Arc::new(host);
            (temp, app)
        }

        fn deliver(app: &mut App, message: ScanMessage) {
            let (tx, rx) = mpsc::channel();
            tx.send(message).unwrap();
            let Screen::Scanning(_, scan_rx, _) = &mut app.screen else {
                panic!("expected the scanning screen");
            };
            *scan_rx = rx;
            app.poll_scan();
        }

        fn installed(app: &App, name: &str, id: &str) -> InstalledApp {
            let path = app
                .layout
                .system("Applications")
                .join(format!("{name}.app"));
            fake_app(&path, id, name);
            InstalledApp {
                path,
                ..sample_app(name, id)
            }
        }

        fn app_list(app: &mut App, apps: Vec<InstalledApp>) {
            app.handle_key(KeyCode::Char('4'));
            deliver(app, ScanMessage::AppsFound(apps));
        }

        fn plan_for(target: &InstalledApp, app: &App) -> UninstallPlan {
            let mut bundle = ScanItem::new(target.path.clone(), 100, true, false);
            bundle.role = Some(UninstallRole::Bundle);
            let cache = app.layout.user("Library/Caches").join("com.acme.foo");
            std::fs::create_dir_all(&cache).unwrap();
            let mut leftover = ScanItem::new(cache, 50, true, false);
            leftover.role = Some(UninstallRole::Leftover {
                evidence: Evidence::BundleId,
            });
            UninstallPlan {
                scan: CategoryScan {
                    notes: Vec::new(),
                    id: PLAN_ID.to_string(),
                    title: "Review".to_string(),
                    groups: vec![ToolGroup {
                        tool: target.name.clone(),
                        items: vec![bundle, leftover],
                    }],
                },
                apps: vec![AppPlan {
                    app: target.clone(),
                    cask: None,
                    login_helpers: Vec::new(),
                    shared: false,
                }],
                notes: Vec::new(),
            }
        }

        #[test]
        fn the_fourth_quick_key_opens_uninstall_apps() {
            let mut state = WorkspaceState::new();

            assert_eq!(
                state.select_by_number(4),
                Some(MainMenuChoice::UninstallApps)
            );
        }

        #[test]
        fn opening_uninstall_apps_searches_and_esc_returns_to_its_row() {
            let (_temp, mut app) = app_in_sandbox();

            app.handle_key(KeyCode::Char('4'));
            assert!(matches!(app.screen, Screen::Scanning(..)));
            assert!(screen_text(&app).contains("Finding apps"));
            assert!(!screen_text(&app).contains("Clean caches"));

            app.handle_key(KeyCode::Esc);
            assert_main_menu(&app, 3);
        }

        #[test]
        fn the_search_ends_in_a_table_of_apps_with_nothing_selected() {
            let (_temp, mut app) = app_in_sandbox();
            let foo = installed(&app, "Foo", "com.acme.foo");

            app_list(&mut app, vec![foo]);

            let state = select(&app);
            assert_eq!(state.scan.id, apps::LIST_ID);
            assert_eq!(state.sort_order, Some(SortOrder::Size));
            assert!(state.scan.selected_items().is_empty());
            let text = screen_text(&app);
            assert!(text.contains("Uninstall apps"), "{text}");
            assert!(text.contains("LAST USED"), "{text}");
        }

        #[test]
        fn the_list_shows_apps_by_name_and_the_section_says_where() {
            let (_temp, mut app) = app_in_sandbox();
            let foo = installed(&app, "Foo Bar", "com.acme.foo");
            let folder = foo.path.parent().unwrap().display().to_string();

            app_list(&mut app, vec![foo]);

            let rows = text_rows(&app);
            let row = rows.iter().find(|row| row.contains("Foo Bar")).unwrap();
            assert!(!row.contains(".app"), "{row}");
            assert!(!row.contains(&folder), "{row}");
        }

        #[test]
        fn the_dialog_button_says_uninstall() {
            let (_temp, mut app) = app_in_sandbox();
            let foo = installed(&app, "Foo", "com.acme.foo");
            let plan = plan_for(&foo, &app);
            app_list(&mut app, vec![foo]);
            app.handle_key(KeyCode::Char('d'));
            deliver(&mut app, ScanMessage::Planned(plan));

            app.handle_key(KeyCode::Char('d'));

            let rows = text_rows(&app);
            assert!(
                rows.iter().any(|row| row.contains("Cancel    Uninstall")),
                "{rows:?}"
            );
        }

        fn text_rows(app: &App) -> Vec<String> {
            screen_rows(app, 100, 30)
        }

        #[test]
        fn the_apps_list_opens_the_review_with_d_and_not_enter() {
            let (_temp, mut app) = app_in_sandbox();
            let foo = installed(&app, "Foo", "com.acme.foo");
            let plan = plan_for(&foo, &app);
            app_list(&mut app, vec![foo]);

            assert_footer(&app, &["Review app  d"], &["↵"]);
            app.handle_key(KeyCode::Enter);
            assert!(matches!(app.screen, Screen::Select(_)));

            for key in [
                KeyCode::Char('d'),
                KeyCode::Char('D'),
                KeyCode::Backspace,
                KeyCode::Delete,
            ] {
                let (_temp, mut app) = app_in_sandbox();
                let foo = installed(&app, "Foo", "com.acme.foo");
                app_list(&mut app, vec![foo]);
                app.handle_key(key);
                assert!(matches!(app.screen, Screen::Scanning(..)), "{key:?}");
                assert!(screen_text(&app).contains("Foo"), "{key:?}");
            }

            app.handle_key(KeyCode::Char('d'));
            deliver(&mut app, ScanMessage::Planned(plan));
            assert_footer(&app, &["Uninstall 2 items  d"], &["↵"]);

            app.handle_key(KeyCode::Enter);
            assert!(matches!(select(&app).overlay, ModalOverlay::None));
        }

        #[test]
        fn the_review_opens_the_removal_question_with_every_removal_key() {
            for key in [
                KeyCode::Char('d'),
                KeyCode::Char('D'),
                KeyCode::Backspace,
                KeyCode::Delete,
            ] {
                let (_temp, mut app) = app_in_sandbox();
                let foo = installed(&app, "Foo", "com.acme.foo");
                let plan = plan_for(&foo, &app);
                app_list(&mut app, vec![foo]);
                app.handle_key(KeyCode::Char('d'));
                deliver(&mut app, ScanMessage::Planned(plan));

                app.handle_key(key);

                assert!(
                    matches!(
                        select(&app).overlay,
                        ModalOverlay::Confirm(ConfirmButton::Cancel)
                    ),
                    "{key:?}"
                );
            }
        }

        #[test]
        fn esc_during_the_search_for_leftovers_returns_to_the_list_as_it_was() {
            let (_temp, mut app) = app_in_sandbox();
            let foo = installed(&app, "Foo", "com.acme.foo");
            let bar = installed(&app, "Bar", "com.acme.bar");
            app_list(&mut app, vec![foo, bar]);
            app.handle_key(KeyCode::Char(' '));
            assert_footer(&app, &["Review 1 app"], &[]);

            press(&mut app, &[KeyCode::Char('d'), KeyCode::Esc]);

            assert_eq!(select(&app).scan.id, apps::LIST_ID);
            assert_eq!(select(&app).scan.selected_items().len(), 1);
        }

        #[test]
        fn the_review_keeps_its_selection_and_esc_returns_to_the_list() {
            let (_temp, mut app) = app_in_sandbox();
            let foo = installed(&app, "Foo", "com.acme.foo");
            let plan = plan_for(&foo, &app);
            app_list(&mut app, vec![foo]);
            app.handle_key(KeyCode::Char('d'));

            deliver(&mut app, ScanMessage::Planned(plan));

            assert_eq!(select(&app).scan.id, PLAN_ID);
            assert_eq!(select(&app).scan.selected_items().len(), 2);
            assert_footer(&app, &["Uninstall 2 items"], &[]);
            let text = screen_text(&app);
            assert!(text.contains("Review"), "{text}");

            app.handle_key(KeyCode::Esc);
            assert_eq!(select(&app).scan.id, apps::LIST_ID);
            app.handle_key(KeyCode::Esc);
            assert_main_menu(&app, 3);
        }

        #[test]
        fn a_plan_with_no_apps_keeps_the_plan_id() {
            let (_temp, mut app) = app_in_sandbox();
            let foo = installed(&app, "Foo", "com.acme.foo");
            let mut plan = plan_for(&foo, &app);
            plan.apps.clear();
            plan.scan.groups.clear();
            plan.notes
                .push("Foo needs the uninstaller from Acme".to_string());
            app_list(&mut app, vec![foo]);
            app.handle_key(KeyCode::Char('d'));

            deliver(&mut app, ScanMessage::Planned(plan));

            assert_eq!(select(&app).scan.id, PLAN_ID);
            let text = screen_text(&app);
            assert!(text.contains("Uninstall apps"), "{text}");
            assert!(text.contains("Review isn't available"), "{text}");
            assert!(text.contains("needs the uninstaller"), "{text}");
            assert_footer(&app, &["Uninstall"], &["Review items"]);

            app.handle_key(KeyCode::Esc);
            assert_eq!(select(&app).scan.id, apps::LIST_ID);
        }

        #[test]
        fn removing_starts_from_cancel_and_names_the_apps() {
            let (_temp, mut app) = app_in_sandbox();
            let foo = installed(&app, "Foo", "com.acme.foo");
            let plan = plan_for(&foo, &app);
            app_list(&mut app, vec![foo]);
            app.handle_key(KeyCode::Char('d'));
            deliver(&mut app, ScanMessage::Planned(plan));

            app.handle_key(KeyCode::Char('d'));

            assert!(matches!(
                select(&app).overlay,
                ModalOverlay::Confirm(ConfirmButton::Cancel)
            ));
            let text = screen_text(&app);
            assert!(text.contains("Uninstall 1 app?"), "{text}");
        }

        #[test]
        fn confirming_removes_the_app_and_leads_back_to_a_fresh_list() {
            let (_temp, mut app) = app_in_sandbox();
            let foo = installed(&app, "Foo", "com.acme.foo");
            let cache = app.layout.user("Library/Caches/com.acme.foo");
            let bundle = foo.path.clone();
            let plan = plan_for(&foo, &app);
            app_list(&mut app, vec![foo]);
            app.handle_key(KeyCode::Char('d'));
            deliver(&mut app, ScanMessage::Planned(plan));

            press(&mut app, &[KeyCode::Char('d'), KeyCode::Char('y')]);
            wait_for_removal(&mut app);

            assert!(!bundle.exists());
            assert!(!cache.exists());
            let text = screen_text(&app);
            assert!(text.contains("Uninstall Complete"), "{text}");

            app.handle_key(KeyCode::Enter);
            assert!(matches!(app.screen, Screen::Scanning(..)));

            app.handle_key(KeyCode::Esc);
            assert_main_menu(&app, 3);
        }

        #[test]
        fn an_uninstall_is_logged_in_the_history_of_the_layout() {
            let (_temp, mut app) = app_in_sandbox();
            let foo = installed(&app, "Foo", "com.acme.foo");
            let plan = plan_for(&foo, &app);
            app_list(&mut app, vec![foo]);
            app.handle_key(KeyCode::Char('d'));
            deliver(&mut app, ScanMessage::Planned(plan));

            press(&mut app, &[KeyCode::Char('d'), KeyCode::Char('y')]);
            wait_for_removal(&mut app);

            let runs = HistoryLog::new(app.layout.history_path())
                .load_runs()
                .unwrap();
            assert_eq!(runs.len(), 1);
            assert_eq!(runs[0].command, "uninstall Foo");
        }

        #[test]
        fn a_review_opened_from_the_command_line_runs_too() {
            let (_temp, mut app) = app_in_sandbox();
            let foo = installed(&app, "Foo", "com.acme.foo");
            let bundle = foo.path.clone();
            let plan = plan_for(&foo, &app);
            let (layout, host) = (app.layout.clone(), app.host.clone());
            app = offline(App::with_plan(
                plan,
                IgnoreStore::default(),
                Config::default(),
            ));
            app.layout = layout;
            app.host = host;

            press(&mut app, &[KeyCode::Char('d'), KeyCode::Char('y')]);
            wait_for_removal(&mut app);

            assert!(!bundle.exists());
        }

        #[test]
        fn the_review_of_a_cask_app_says_permanent() {
            let (_temp, app) = app_in_sandbox();
            let foo = installed(&app, "Foo", "com.acme.foo");
            let mut plan = plan_for(&foo, &app);
            plan.apps[0].cask = Some(uninstaller::Cask {
                token: "foo-app".to_string(),
                prefix: app.layout.system("opt/homebrew"),
            });

            let review = screen_text(&offline(App::with_plan(
                plan,
                IgnoreStore::default(),
                Config::default(),
            )));

            let line_with = |needle: &str| {
                review
                    .lines()
                    .find(|line| line.contains(needle))
                    .unwrap_or_else(|| panic!("no line with {needle}\n{review}"))
            };
            assert!(
                line_with("/Applications/").contains("(Permanent)"),
                "{review}"
            );
            assert!(
                line_with("/Library/Caches/").contains("(Trash)"),
                "{review}"
            );
            assert_eq!(review.matches("(Permanent)").count(), 1, "{review}");
            assert_eq!(review.matches("(Trash)").count(), 1, "{review}");
        }

        #[test]
        fn a_permanent_review_marks_its_rows_permanent() {
            let (_temp, app) = app_in_sandbox();
            let foo = installed(&app, "Foo", "com.acme.foo");
            let review = |permanent| {
                let plan = plan_for(&foo, &app);
                screen_text(&offline(
                    App::with_plan(plan, IgnoreStore::default(), Config::default())
                        .with_permanent(permanent),
                ))
            };

            assert!(review(false).contains("(Trash)"));
            let permanent = review(true);
            assert!(!permanent.contains("(Trash)"), "{permanent}");
            assert!(permanent.contains("(Permanent)"), "{permanent}");
        }

        #[test]
        fn help_names_what_d_does_on_each_list() {
            let (_temp, mut app) = app_in_sandbox();
            let foo = installed(&app, "Foo", "com.acme.foo");
            let plan = plan_for(&foo, &app);
            app_list(&mut app, vec![foo]);

            assert_eq!(help_row(&app, "Review"), Some("d"));

            app.handle_key(KeyCode::Char('d'));
            deliver(&mut app, ScanMessage::Planned(plan));

            assert_eq!(help_row(&app, "Uninstall"), Some("d"));
        }
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
        let mut app = new_app();

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

    mod clean_all {
        use super::*;
        use crate::core::apps::testing::sandbox;
        use crate::core::host::fake::FakeHost;
        use std::time::SystemTime;

        fn app_with_an_old_orphan() -> (tempfile::TempDir, App) {
            let (temp, layout) = sandbox();
            let orphan = layout.user("Library/Caches/com.gone.app");
            std::fs::create_dir_all(&orphan).unwrap();
            std::fs::write(orphan.join("data"), [1_u8; 8192]).unwrap();
            let then = SystemTime::now() - Duration::from_secs(40 * 24 * 60 * 60);
            for path in [orphan.join("data"), orphan.clone()] {
                std::fs::File::open(path)
                    .unwrap()
                    .set_modified(then)
                    .unwrap();
            }
            let host = FakeHost::default();
            host.reply("mdfind", "");
            let mut app = new_app();
            app.layout = layout;
            app.host = Arc::new(host);
            (temp, app)
        }

        /// Reads the finished scan off the channel, before the Select screen resets its rows.
        fn finished_scan(app: &App) -> CategoryScan {
            let Screen::Scanning(_, rx, _) = &app.screen else {
                panic!("expected the scanning screen");
            };
            loop {
                match rx.recv_timeout(Duration::from_secs(30)).unwrap() {
                    ScanMessage::Finished(scan) => return scan,
                    _ => continue,
                }
            }
        }

        fn orphan_rows(scan: &CategoryScan) -> Vec<bool> {
            scan.groups
                .iter()
                .flat_map(|group| &group.items)
                .filter(|item| item.role.is_some() && item.path.ends_with("com.gone.app"))
                .map(|item| item.selected)
                .collect()
        }

        #[test]
        fn the_orphans_row_starts_the_old_orphan_selected() {
            let (_temp, mut app) = app_with_an_old_orphan();

            app.start_clean_subcategory(CleanSubcategory::Orphans);

            assert_eq!(orphan_rows(&finished_scan(&app)), [true]);
        }

        #[test]
        fn the_all_row_leaves_orphan_rows_unselected() {
            let (_temp, mut app) = app_with_an_old_orphan();

            app.start_clean_subcategory(CleanSubcategory::All);

            assert_eq!(orphan_rows(&finished_scan(&app)), [false]);
        }

        #[test]
        fn the_all_row_counts_every_category() {
            let (_temp, mut app) = app_with_an_old_orphan();

            app.start_clean_subcategory(CleanSubcategory::All);

            let Screen::Scanning(progress, _, cancel) = &app.screen else {
                panic!("expected the scanning screen");
            };
            cancel.store(true, Ordering::Relaxed);
            assert_eq!(progress.headline, "Scanning Developer Caches, 1 of 5");
        }
    }

    mod slim_apps {
        use super::*;
        use crate::core::apps::testing::sandbox;
        use crate::core::bytes::format_bytes;
        use crate::core::history::{OutcomeKind, RunRecord};
        use crate::core::host::fake::FakeHost;
        use crate::core::slim::testing::{
            fake_ditto, holding_of, make_slim_save_space, write_test_app,
        };

        const TRASH_NOTE: &str =
            "Clones share blocks, so the space comes back when the Trash is emptied.";

        /// Slims the apps at `bundles` from the slim list and returns the finished app.
        fn run_slim(layout: Layout, bundles: &[&Path], host: FakeHost) -> App {
            let mut app = select_app(slim_scan(bundles));
            app.layout = layout;
            app.host = Arc::new(host);

            press(&mut app, &[KeyCode::Enter, KeyCode::Char('y')]);
            while app.removal_rx.is_some() {
                app.poll_removal();
                thread::sleep(Duration::from_millis(5));
            }
            app
        }

        fn report_of(app: &App) -> RemovalReport {
            let ModalOverlay::Outcome(report) = &select(app).overlay else {
                panic!("expected the outcome dialog");
            };
            report.clone()
        }

        /// Slims the app at `bundle` from the slim list and returns the outcome report.
        fn slim_in_list(layout: Layout, bundle: &Path, host: FakeHost) -> RemovalReport {
            let report = report_of(&run_slim(layout, &[bundle], host));
            assert_eq!(report.removed_count, 1, "{:?}", report.failures);
            report
        }

        fn slim_runs(layout: &Layout) -> Vec<RunRecord> {
            HistoryLog::new(layout.history_path()).load_runs().unwrap()
        }

        /// Gives the app at `bundle` an Intel slice large enough to free blocks.
        fn ready_host(bundle: &Path) -> FakeHost {
            let host = FakeHost::idle();
            host.reply("ditto", "");
            fake_ditto(&host, bundle);
            host.reply("codesign", "");
            host.reply("spctl", "");
            host
        }

        fn slim_scan(bundles: &[&Path]) -> CategoryScan {
            CategoryScan {
                id: slim::SCAN_ID.to_string(),
                title: "Slim Universal Apps".to_string(),
                notes: Vec::new(),
                groups: vec![ToolGroup {
                    tool: "Applications".to_string(),
                    items: bundles
                        .iter()
                        .map(|bundle| ScanItem::new(bundle.to_path_buf(), 11, true, false))
                        .collect(),
                }],
            }
        }

        #[test]
        fn the_slim_question_says_the_old_app_goes_to_the_trash() {
            let mut app = select_app(slim_scan(&[Path::new("/Applications/X.app")]));

            press(&mut app, &[KeyCode::Enter]);

            let screen = screen_text(&app);
            assert!(screen.contains("Slim 1 app?"), "{screen}");
            assert!(screen.contains("of Intel code"), "{screen}");
            assert!(screen.contains("goes to the Trash"), "{screen}");
            assert!(!screen.contains("permanently"), "{screen}");
            assert!(!screen.contains("cannot be undone"), "{screen}");
        }

        #[test]
        fn the_progress_title_names_the_run() {
            let (_temp, layout) = sandbox();
            let first = layout.user("Applications/A.app");
            let second = layout.user("Applications/B.app");
            write_test_app(&first, "com.vendor.a");
            write_test_app(&second, "com.vendor.b");
            let host = ready_host(&first);
            let release = hold(&host, "ditto");
            let mut app = select_app(slim_scan(&[&first, &second]));
            app.layout = layout;
            app.host = Arc::new(host);

            press(&mut app, &[KeyCode::Enter, KeyCode::Char('y')]);
            let screen = screen_text(&app);
            drop(release);
            wait_for_removal(&mut app);

            assert!(screen.contains("Slimming 2 apps…"), "{screen}");
            assert!(!screen.contains("Removing"), "{screen}");
        }

        #[test]
        fn a_refused_trash_names_the_old_app_in_the_report() {
            let (_temp, layout) = sandbox();
            let bundle = layout.user("Applications/X.app");
            write_test_app(&bundle, "com.vendor.x");
            let host = ready_host(&bundle);
            host.fail_trash("denied");

            let report = slim_in_list(layout, &bundle, host);

            let expected = format!(
                "Slimmed X, saved {}. The old app is still at {} (could not move it to the Trash: denied)",
                format_bytes(report.freed_bytes),
                holding_of(&bundle).join("X.app").display()
            );
            assert!(report.notes.contains(&expected), "{:?}", report.notes);
            assert!(
                !report.notes.contains(&TRASH_NOTE.to_string()),
                "{:?}",
                report.notes
            );
        }

        #[test]
        fn a_folder_left_after_the_trash_is_named_in_the_report() {
            let (_temp, layout) = sandbox();
            let bundle = layout.user("Applications/X.app");
            write_test_app(&bundle, "com.vendor.x");
            let host = ready_host(&bundle);

            let report = slim_in_list(layout, &bundle, host);

            let holding = holding_of(&bundle);
            let error = std::fs::remove_dir(&holding).unwrap_err().to_string();
            let done = slim::SlimDone {
                saved: report.freed_bytes,
                old_app: None,
                holding_left: Some((holding, error)),
            };
            assert!(
                report.notes.contains(&done.summary("X")),
                "{:?}",
                report.notes
            );
            assert!(
                report.notes.contains(&TRASH_NOTE.to_string()),
                "{:?}",
                report.notes
            );
        }

        #[test]
        fn an_old_app_in_the_trash_brings_the_trash_note() {
            let (_temp, layout) = sandbox();
            let bundle = layout.user("Applications/X.app");
            write_test_app(&bundle, "com.vendor.x");
            let host = ready_host(&bundle);
            host.simulate_trash();

            let report = slim_in_list(layout, &bundle, host);

            assert_eq!(report.notes, vec![TRASH_NOTE.to_string()]);
        }

        #[test]
        fn a_slim_reports_the_saved_bytes_and_nothing_trashed() {
            let (_temp, layout) = sandbox();
            let bundle = layout.user("Applications/X.app");
            write_test_app(&bundle, "com.vendor.x");
            make_slim_save_space(&bundle);
            let host = ready_host(&bundle);
            host.simulate_trash();

            let app = run_slim(layout.clone(), &[&bundle], host);

            let report = report_of(&app);
            let runs = slim_runs(&layout);
            let changed: Vec<_> = runs[0]
                .items
                .iter()
                .filter(|item| item.outcome == OutcomeKind::Changed)
                .collect();
            assert_eq!(report.trashed_bytes, 0);
            assert_eq!(changed.len(), 1);
            assert!(report.freed_bytes > 0);
            assert_eq!(report.freed_bytes, changed[0].bytes);
            let screen = screen_text(&app);
            assert!(screen.contains("Slim Complete"), "{screen}");
            assert!(
                screen.contains(&format!(
                    "Saved {} of Intel code.",
                    format_bytes(report.freed_bytes)
                )),
                "{screen}"
            );
            assert!(!screen.contains("Moved"), "{screen}");
            assert!(!screen.contains("permanently."), "{screen}");
        }

        #[test]
        fn a_refused_trash_still_says_slim_complete() {
            let (_temp, layout) = sandbox();
            let bundle = layout.user("Applications/X.app");
            write_test_app(&bundle, "com.vendor.x");
            let host = ready_host(&bundle);
            host.fail_trash("denied");

            let app = run_slim(layout, &[&bundle], host);

            let report = report_of(&app);
            let screen = screen_text(&app);
            assert_eq!(report.trashed_bytes, 0);
            assert!(screen.contains("Slim Complete"), "{screen}");
            assert!(
                screen.contains(&format!(
                    "Saved {} of Intel code.",
                    format_bytes(report.freed_bytes)
                )),
                "{screen}"
            );
            assert!(!screen.contains("Moved"), "{screen}");
        }

        #[test]
        fn only_the_slimmed_app_counts() {
            let (_temp, layout) = sandbox();
            let x = layout.user("Applications/X.app");
            write_test_app(&x, "com.vendor.x");
            make_slim_save_space(&x);
            let y = layout.user("Applications/Y.app");
            write_test_app(&y, "com.vendor.y");
            std::fs::create_dir(holding_of(&y)).unwrap();
            let host = ready_host(&x);
            host.simulate_trash();

            let app = run_slim(layout.clone(), &[&x, &y], host);

            let report = report_of(&app);
            let runs = slim_runs(&layout);
            assert_eq!(report.removed_count, 1);
            assert_eq!(report.failures.len(), 1);
            assert_eq!(report.failures[0].path, y);
            assert_eq!(report.trashed_bytes, 0);
            assert_eq!(runs[0].items.len(), 1);
            assert_eq!(runs[0].items[0].outcome, OutcomeKind::Changed);
            assert_eq!(runs[0].items[0].path, x.display().to_string());
            assert!(report.freed_bytes > 0);
            assert_eq!(runs[0].items[0].bytes, report.freed_bytes);
            let screen = screen_text(&app);
            assert!(
                screen.contains(&format!(
                    "Saved {} of Intel code.",
                    format_bytes(report.freed_bytes)
                )),
                "{screen}"
            );
            assert!(screen.contains("failed to slim:"), "{screen}");
        }

        #[test]
        fn nothing_slimmed_says_so() {
            let (_temp, layout) = sandbox();
            let y = layout.user("Applications/Y.app");
            write_test_app(&y, "com.vendor.y");
            std::fs::create_dir(holding_of(&y)).unwrap();

            let app = run_slim(layout, &[&y], ready_host(&y));

            let screen = screen_text(&app);
            assert_eq!(report_of(&app).removed_count, 0);
            assert!(screen.contains("Nothing was slimmed"), "{screen}");
            assert!(!screen.contains("Saved"), "{screen}");
            assert!(!screen.contains("0 B freed"), "{screen}");
        }

        #[test]
        fn a_slim_in_the_list_writes_the_saved_bytes_to_the_history() {
            let (_temp, layout) = sandbox();
            let bundle = layout.user("Applications/X.app");
            write_test_app(&bundle, "com.vendor.x");
            make_slim_save_space(&bundle);
            let host = ready_host(&bundle);
            host.simulate_trash();

            let report = slim_in_list(layout.clone(), &bundle, host);

            let runs = slim_runs(&layout);
            assert_eq!(runs.len(), 1);
            assert_eq!(runs[0].command, "slim");
            assert_eq!(runs[0].mode, "interactive");
            assert_eq!(runs[0].items.len(), 1);
            let item = &runs[0].items[0];
            assert_eq!(item.outcome, OutcomeKind::Changed);
            assert_eq!(item.path, bundle.display().to_string());
            assert!(report.freed_bytes > 0);
            assert_eq!(item.bytes, report.freed_bytes);
        }
    }

    mod compress {
        use super::*;
        use crate::core::apps::testing::sandbox;
        use crate::core::bytes::format_bytes;
        use crate::core::compress::UF_COMPRESSED;
        use crate::core::host::fake::FakeHost;
        use std::os::darwin::fs::MetadataExt;
        use std::path::PathBuf;
        use tempfile::TempDir;

        fn bundle_with_file(layout: &Layout, name: &str) -> (PathBuf, PathBuf) {
            let bundle = layout.system("Applications").join(format!("{name}.app"));
            let dir = bundle.join("Contents/Resources");
            std::fs::create_dir_all(bundle.join("Contents/MacOS")).unwrap();
            std::fs::create_dir_all(&dir).unwrap();
            let file = dir.join("strings.txt");
            let text = "Hello APFS transparent compression!\n".repeat(10_000);
            std::fs::write(&file, text).unwrap();
            (bundle, file)
        }

        fn is_compressed(path: &Path) -> bool {
            let meta = std::fs::symlink_metadata(path).unwrap();
            meta.st_flags() & UF_COMPRESSED != 0
        }

        /// A compress screen with every named app selected, on a fake Mac where `running` apps run.
        fn prepare(names: &[&str], running: &[&str]) -> (TempDir, App, Vec<(PathBuf, PathBuf)>) {
            let (temp, layout) = sandbox();
            let made: Vec<_> = names
                .iter()
                .map(|name| bundle_with_file(&layout, name))
                .collect();
            let mut ps = "    1     0 /sbin/launchd\n".to_string();
            for name in running {
                let bundle = layout.system("Applications").join(format!("{name}.app"));
                let program = format!("  500     1 {}/Contents/MacOS/{name}\n", bundle.display());
                ps.push_str(&program);
            }
            let host = FakeHost::default();
            host.reply("/bin/ps", &ps);
            host.reply("/usr/sbin/lsof", "");
            let items = made
                .iter()
                .map(|(bundle, _)| ScanItem::new(bundle.clone(), 1000, true, false))
                .collect();
            let scan = CategoryScan {
                id: "compress".to_string(),
                title: "Compress".to_string(),
                notes: Vec::new(),
                groups: vec![ToolGroup {
                    tool: "Applications".to_string(),
                    items,
                }],
            };
            let mut app = select_app(scan);
            app.layout = layout;
            app.host = Arc::new(host);
            (temp, app, made)
        }

        fn outcome(app: &App) -> &RemovalReport {
            let ModalOverlay::Outcome(report) = &select(app).overlay else {
                panic!("expected the outcome dialog");
            };
            report
        }

        fn run_selection(app: &mut App) {
            press(app, &[KeyCode::Enter, KeyCode::Char('y')]);
            while app.removal_rx.is_some() {
                app.poll_removal();
                thread::sleep(Duration::from_millis(5));
            }
        }

        #[test]
        fn the_tui_refuses_a_running_app_and_says_so() {
            let (_temp, mut app, made) = prepare(&["Run", "Idle"], &["Run"]);

            run_selection(&mut app);

            assert!(!is_compressed(&made[0].1));
            assert!(is_compressed(&made[1].1));
            let report = outcome(&app);
            assert_eq!(report.removed_count, 1);
            assert_eq!(report.failures.len(), 1);
            assert_eq!(report.failures[0].path, made[0].0);
            assert!(report.failures[0].error.contains("running"));
        }

        #[test]
        fn esc_during_a_compress_run_stops_the_worker_before_the_next_path() {
            let (_temp, mut app, made) = prepare(&["First", "Second"], &[]);
            let host = FakeHost::default();
            host.reply("/bin/ps", "    1     0 /sbin/launchd\n");
            host.reply("/usr/sbin/lsof", "");
            let release = hold(&host, "/usr/sbin/lsof");
            app.host = Arc::new(host);

            press(&mut app, &[KeyCode::Enter, KeyCode::Char('y')]);
            app.handle_key(KeyCode::Esc);
            drop(release);
            wait_for_removal(&mut app);

            assert!(!is_compressed(&made[1].1));
            assert!(outcome(&app).notes.iter().any(|n| n.contains("Cancelled")));
        }

        #[test]
        fn the_compress_question_says_files_stay() {
            let (_temp, mut app, _made) = prepare(&["Idle"], &[]);

            press(&mut app, &[KeyCode::Enter]);

            let screen = screen_text(&app);
            assert!(screen.contains("Compress 1 item?"), "{screen}");
            assert!(screen.contains("with APFS compression"), "{screen}");
            assert!(screen.contains("stay where they are."), "{screen}");
            assert!(screen.contains("Cancel    Compress"), "{screen}");
            assert!(!screen.contains("permanently"), "{screen}");
            assert!(!screen.contains("cannot be undone"), "{screen}");
            assert!(!screen.contains("Trash"), "{screen}");
            assert!(!screen.contains("Remove"), "{screen}");
        }

        #[test]
        fn the_progress_title_names_the_run() {
            let (_temp, mut app, _made) = prepare(&["First", "Second"], &[]);
            let host = FakeHost::default();
            host.reply("/bin/ps", "    1     0 /sbin/launchd\n");
            host.reply("/usr/sbin/lsof", "");
            let release = hold(&host, "/usr/sbin/lsof");
            app.host = Arc::new(host);

            press(&mut app, &[KeyCode::Enter, KeyCode::Char('y')]);
            let screen = screen_text(&app);
            drop(release);
            wait_for_removal(&mut app);

            assert!(screen.contains("Compressing 2 items…"), "{screen}");
            assert!(
                screen.contains("Esc stops after the current file."),
                "{screen}"
            );
            assert!(!screen.contains("Removing"), "{screen}");
        }

        #[test]
        fn a_compress_says_what_it_saved() {
            let (_temp, mut app, _made) = prepare(&["Idle"], &[]);

            run_selection(&mut app);

            let screen = screen_text(&app);
            assert!(screen.contains("Compress Complete"), "{screen}");
            assert!(
                screen.contains(&format!(
                    "Saved {} by compressing 1 file.",
                    format_bytes(outcome(&app).freed_bytes)
                )),
                "{screen}"
            );
            assert!(!screen.contains("permanently"), "{screen}");
            assert!(!screen.contains("Trash"), "{screen}");
            assert!(!screen.contains("Clean Complete"), "{screen}");
        }

        #[test]
        fn a_compress_of_only_running_apps_says_nothing_was_compressed() {
            let (_temp, mut app, _made) = prepare(&["Run"], &["Run"]);

            run_selection(&mut app);

            let screen = screen_text(&app);
            assert!(screen.contains("Nothing was compressed"), "{screen}");
            assert!(screen.contains("1 item failed to compress:"), "{screen}");
            assert!(!screen.contains("0 B freed"), "{screen}");
            assert!(!screen.contains("failed to remove"), "{screen}");
        }
    }

    #[test]
    fn a_failed_disable_in_doctor_keeps_the_finding_and_shows_the_error() {
        use crate::core::doctor::{DoctorReport, Finding, FindingSeverity};
        use crate::core::startup::{StartupItem, StartupKind, StartupState};
        let item = StartupItem {
            label: "com.test.daemon".into(),
            name: "Test".into(),
            app_name: None,
            app_bundle: None,
            kind: StartupKind::Daemon,
            state: StartupState::Running,
            program: None,
            plist_path: Some("/Library/LaunchDaemons/com.test.daemon.plist".into()),
            pid: Some(9),
            cpu_percent: None,
            memory_bytes: None,
            is_apple: false,
            is_security: false,
            is_ignored: false,
        };
        let finding = Finding {
            id: "x".into(),
            severity: FindingSeverity::Warning,
            title: "t".into(),
            evidence: "e".into(),
            action: "a".into(),
            action_target: None,
        };
        let report = DoctorReport {
            seconds: 1,
            findings: vec![finding],
            checks: Vec::new(),
        };
        let host = crate::core::host::fake::FakeHost::idle();
        *host.admin_result.lock().unwrap() = Some("denied".into());
        let mut app = new_app();
        app.host = Arc::new(host);
        let mut state = DoctorScreenState::new_done(report);
        state.confirm = Some(DoctorActionConfirm::DisableStartupItem { item });
        app.screen = Screen::Doctor(state);

        app.handle_key(KeyCode::Enter);

        let Screen::Doctor(state) = &app.screen else {
            panic!("expected the doctor screen");
        };
        assert_eq!(state.findings().len(), 1);
        assert!(matches!(&state.message, Some((m, true)) if m.contains("denied")));
    }

    fn doctor_finding(
        target: Option<crate::core::doctor::FindingAction>,
    ) -> crate::core::doctor::Finding {
        crate::core::doctor::Finding {
            id: "x".into(),
            severity: crate::core::doctor::FindingSeverity::Warning,
            title: "t".into(),
            evidence: "e".into(),
            action: "a".into(),
            action_target: target,
        }
    }

    fn doctor_app(
        target: Option<crate::core::doctor::FindingAction>,
    ) -> (App, Arc<crate::core::host::fake::FakeHost>) {
        let report = crate::core::doctor::DoctorReport {
            seconds: 1,
            findings: vec![doctor_finding(target)],
            checks: Vec::new(),
        };
        let host = Arc::new(crate::core::host::fake::FakeHost::idle());
        let mut app = new_app();
        app.host = host.clone();
        app.screen = Screen::Doctor(DoctorScreenState::new_done(report));
        (app, host)
    }

    fn kill_dialog(app: &mut App) {
        let Screen::Doctor(state) = &mut app.screen else {
            panic!("expected the doctor screen");
        };
        state.confirm = Some(DoctorActionConfirm::KillProcess {
            pid: 4242,
            name: "node".into(),
        });
        state.confirm_button = 0;
    }

    #[test]
    fn a_sent_signal_removes_the_finding() {
        let (mut app, host) = doctor_app(None);
        kill_dialog(&mut app);

        app.handle_key(KeyCode::Enter);

        let Screen::Doctor(state) = &app.screen else {
            panic!("expected the doctor screen");
        };
        assert_eq!(*host.signals.lock().unwrap(), [(4242, libc::SIGTERM)]);
        assert!(state.findings().is_empty());
        assert!(matches!(&state.message, Some((m, false)) if m.contains("Sent SIGTERM to node")));
    }

    #[test]
    fn confirming_pid_0_or_1_sends_nothing_and_shows_an_error() {
        for pid in [0, 1] {
            let (mut app, host) = doctor_app(None);
            kill_dialog(&mut app);
            let Screen::Doctor(state) = &mut app.screen else {
                panic!("expected the doctor screen");
            };
            state.confirm = Some(DoctorActionConfirm::KillProcess {
                pid,
                name: "x".into(),
            });

            app.handle_key(KeyCode::Enter);

            let Screen::Doctor(state) = &app.screen else {
                panic!("expected the doctor screen");
            };
            assert!(host.signals.lock().unwrap().is_empty());
            assert!(matches!(&state.message, Some((_, true))));
        }
    }

    #[test]
    fn a_failed_signal_keeps_the_finding_and_shows_the_error() {
        let (mut app, host) = doctor_app(None);
        *host.signal_error.lock().unwrap() = Some("Operation not permitted".into());
        kill_dialog(&mut app);

        app.handle_key(KeyCode::Enter);

        let Screen::Doctor(state) = &app.screen else {
            panic!("expected the doctor screen");
        };
        assert_eq!(state.findings().len(), 1);
        assert!(
            matches!(&state.message, Some((m, true)) if m.contains("Operation not permitted") && m.contains("node"))
        );
    }

    #[test]
    fn kill_and_disable_dialogs_start_on_cancel() {
        use crate::core::doctor::FindingAction;
        let item = crate::core::startup::StartupItem {
            label: "com.test.daemon".into(),
            name: "Test".into(),
            app_name: None,
            app_bundle: None,
            kind: crate::core::startup::StartupKind::Daemon,
            state: crate::core::startup::StartupState::Broken,
            program: None,
            plist_path: None,
            pid: None,
            cpu_percent: None,
            memory_bytes: None,
            is_apple: false,
            is_security: false,
            is_ignored: false,
        };
        let focus_after_request = |target: FindingAction| {
            let (mut app, host) = doctor_app(Some(target));
            let Screen::Doctor(state) = &mut app.screen else {
                panic!("expected the doctor screen");
            };
            state.request_action();
            (state.confirm_button, app, host)
        };

        let (button, mut app, host) = focus_after_request(FindingAction::KillProcess {
            pid: 4242,
            name: "node".into(),
        });
        assert_eq!(button, 1);
        app.handle_key(KeyCode::Enter);
        let Screen::Doctor(state) = &app.screen else {
            panic!("expected the doctor screen");
        };
        assert!(host.signals.lock().unwrap().is_empty());
        assert!(state.confirm.is_none());

        let (button, _, _) = focus_after_request(FindingAction::DisableStartupItem { item });
        assert_eq!(button, 1);
        let (button, _, _) = focus_after_request(FindingAction::ExploreStorage);
        assert_eq!(button, 0);
    }

    mod brew {
        use super::*;
        use crate::core::apps::testing::sandbox;
        use crate::core::homebrew::testing::{formula, info, outdated};
        use crate::core::homebrew::{self, Inventory, Package, PackageKind};
        use crate::core::host::fake::FakeHost;
        use crate::core::sizer::DirSizer;
        use crate::ui::components::toast::ToastKind;
        use crate::ui::select::RowKind;
        use crate::ui::select::testing::screen_words;
        use std::time::SystemTime;

        const BREW_START: &str =
            "/usr/bin/env HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_ENV_HINTS=1 NONINTERACTIVE=1";

        fn package(kind: PackageKind, name: &str, bytes: u64) -> Package {
            homebrew::testing::package(kind, name, "1.0", bytes)
        }

        fn brew_list_app(inventory: &Inventory) -> App {
            let mut app = new_app();
            app.screen = Screen::Select(Box::new(SelectState::homebrew_list(inventory)));
            app
        }

        fn brew_app() -> App {
            brew_list_app(&Inventory {
                packages: vec![
                    package(PackageKind::Formula, "fd", 3_000),
                    package(PackageKind::Formula, "go", 5_000),
                    package(PackageKind::Cask, "ghostty", 9_000),
                ],
                brew: Some("/fake/bin/brew".into()),
                ..Inventory::default()
            })
        }

        /// A plain formula, a pinned one, one that others use, and a cask.
        fn refusal_app() -> App {
            let mut pinned = package(PackageKind::Formula, "pin", 3_000);
            pinned.pinned = true;
            let mut used = package(PackageKind::Formula, "lib", 2_000);
            used.used_by = vec!["app".to_string(), "tool".to_string()];
            brew_list_app(&Inventory {
                packages: vec![
                    package(PackageKind::Formula, "fd", 5_000),
                    pinned,
                    used,
                    package(PackageKind::Cask, "ghostty", 9_000),
                ],
                brew: Some("/fake/bin/brew".into()),
                ..Inventory::default()
            })
        }

        fn focus_on(app: &mut App, name: &str) {
            let Screen::Select(state) = &mut app.screen else {
                panic!("expected the select screen");
            };
            let prefix = format!("{name} ");
            state.focused_index = state
                .flattened_rows()
                .iter()
                .position(|row| row.is_item() && row.display_path.starts_with(&prefix))
                .unwrap_or_else(|| panic!("no row {name}"));
        }

        fn selected(app: &App) -> usize {
            select(app).scan.selected_items().len()
        }

        #[test]
        fn the_menu_detail_says_a_formula_nothing_uses_can_be_removed() {
            let mut app = new_app();

            press(&mut app, &[KeyCode::Down; 4]);

            let mut terminal = Terminal::new(TestBackend::new(160, 40)).unwrap();
            terminal.draw(|frame| app.render_screen(frame)).unwrap();
            let text = format!("{:?}", terminal.backend().buffer());
            assert!(
                text.contains("Remove a formula that nothing uses."),
                "{text}"
            );
            assert!(!text.contains("Nothing is removed."), "{text}");
        }

        const CHECK_INFO: &str = r#"{"formulae": [
            {"name": "bun", "installed": [{"version": "1.4.2", "installed_on_request": true}]},
            {"name": "app", "dependencies": ["lib"],
             "installed": [{"version": "1", "installed_on_request": true}]},
            {"name": "lib", "installed": [{"version": "1"}]},
            {"name": "zed", "installed": [{"version": "2", "installed_on_request": true}]}],
            "casks": []}"#;
        const NOTHING_NEW: &str = r#"{"formulae": [], "casks": []}"#;

        /// A Mac whose brew knows four formulae. `outdated` first says nothing is new, then that
        /// bun has 1.4.4_1. `update` succeeds.
        fn check_mac() -> (tempfile::TempDir, App, Arc<FakeHost>, String) {
            check_mac_with(
                CHECK_INFO,
                NOTHING_NEW,
                Some(homebrew::testing::BREW_OUTDATED_BUN),
                None,
            )
        }

        /// A Mac whose brew answers `info` with `info`, and `outdated` with `first` and then with
        /// `second`, when there is one. `update` succeeds, or fails with `update_error`.
        fn check_mac_with(
            info: &str,
            first: &str,
            second: Option<&str>,
            update_error: Option<&str>,
        ) -> (tempfile::TempDir, App, Arc<FakeHost>, String) {
            let (temp, layout) = homebrew::testing::with_brew("opt/homebrew");
            let brew = layout.system("opt/homebrew/bin/brew").display().to_string();
            let host = Arc::new(FakeHost::default());
            host.reply(
                &format!("{BREW_START} {brew} info --json=v2 --installed"),
                info,
            );
            let outdated = format!("{BREW_START} {brew} outdated --json=v2");
            host.reply(&outdated, first);
            if let Some(second) = second {
                host.reply(&outdated, second);
            }
            let update = format!("{BREW_START} {brew} update");
            match update_error {
                Some(error) => host.fail(&update, error),
                None => host.reply(&update, ""),
            }
            let mut app = new_app();
            app.layout = layout;
            app.host = host.clone();
            (temp, app, host, brew)
        }

        fn open_the_list(app: &mut App) {
            app.handle_key(KeyCode::Char('5'));
            while matches!(app.screen, Screen::Scanning(..)) {
                app.poll_scan();
                thread::sleep(Duration::from_millis(5));
            }
        }

        fn finish_the_check(app: &mut App) {
            for _ in 0..400 {
                app.poll_brew_check();
                if !select(app).checking_updates {
                    return;
                }
                thread::sleep(Duration::from_millis(5));
            }
            panic!("the check did not end");
        }

        fn ran_times(host: &FakeHost, brew: &str, command: &str) -> usize {
            let line = format!("{BREW_START} {brew} {command}");
            host.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.starts_with(&line))
                .count()
        }

        /// The line of the Homebrew list on a 140-column screen that holds `needle`.
        fn row_on_screen(app: &App, needle: &str) -> String {
            screen_rows(app, 140, 30)
                .into_iter()
                .find(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("no row with {needle}"))
        }

        fn row_labels(app: &App) -> Vec<String> {
            select(app)
                .scan
                .groups
                .iter()
                .flat_map(|group| &group.items)
                .map(|item| item.path.to_string_lossy().into_owned())
                .collect()
        }

        #[test]
        fn opening_the_homebrew_list_checks_for_updates() {
            let (_temp, mut app, host, brew) = check_mac();

            open_the_list(&mut app);
            assert!(screen_text(&app).contains("Checking for updates…"));
            assert!(row_labels(&app).contains(&"bun 1.4.2".to_string()));
            assert!(!row_on_screen(&app, "bun 1.4.2").contains('↑'));
            finish_the_check(&mut app);

            assert!(row_labels(&app).contains(&"bun 1.4.2".to_string()));
            assert!(row_on_screen(&app, "bun 1.4.2").contains("↑ 1.4.4_1"));
            assert!(screen_text(&app).contains("1 update"));
            assert!(!screen_text(&app).contains("Checking for updates"));
            assert_eq!(ran_times(&host, &brew, "update"), 1);
            assert_eq!(ran_times(&host, &brew, "info"), 1);
        }

        #[test]
        fn the_list_opened_by_taert_brew_checks_when_the_run_starts() {
            let (_temp, app, host, brew) = check_mac();
            let inventory = homebrew::scan(&app.layout, &*host).unwrap();
            let layout = app.layout.clone();
            let mut app =
                App::with_brew_list(&inventory, IgnoreStore::default(), Config::default());
            app.layout = layout;
            app.host = host.clone();
            assert_eq!(ran_times(&host, &brew, "update"), 0);

            app.start_pending_brew_check();
            assert!(select(&app).checking_updates);
            finish_the_check(&mut app);

            assert_eq!(ran_times(&host, &brew, "update"), 1);
            assert!(row_labels(&app).contains(&"bun 1.4.2".to_string()));
            assert!(row_on_screen(&app, "bun 1.4.2").contains("↑ 1.4.4_1"));
            app.start_pending_brew_check();
            assert_eq!(ran_times(&host, &brew, "update"), 1);
        }

        #[test]
        fn the_check_runs_once_per_run() {
            let (_temp, mut app, host, brew) = check_mac();

            open_the_list(&mut app);
            finish_the_check(&mut app);
            app.handle_key(KeyCode::Esc);
            open_the_list(&mut app);

            assert!(!select(&app).checking_updates);
            assert_eq!(ran_times(&host, &brew, "update"), 1);
        }

        #[test]
        fn a_list_opened_again_while_the_check_runs_waits_for_it_and_takes_its_result() {
            let (_temp, mut app, host, brew) =
                check_mac_with(CHECK_INFO, NOTHING_NEW, Some(NOTHING_NEW), None);
            host.reply(
                &format!("{BREW_START} {brew} outdated --json=v2"),
                BUN_OUTDATED,
            );
            let release = hold(&host, &format!("{BREW_START} {brew} update"));
            open_the_list(&mut app);
            app.handle_key(KeyCode::Esc);
            assert!(matches!(app.screen, Screen::Workspace(_)));

            open_the_list(&mut app);
            focus_on(&mut app, "bun");
            for key in ['u', 'U'] {
                app.handle_key(KeyCode::Char(key));

                let state = select(&app);
                let toast = state.toast.as_ref().expect("a toast");
                assert_eq!(toast.text, "Checking for updates. Try again when it ends.");
                assert!(matches!(state.overlay, ModalOverlay::None), "{key}");
            }
            assert!(screen_text(&app).contains("Checking for updates…"));
            let calls = host.calls.lock().unwrap().clone();
            assert!(!calls.iter().any(|call| call.contains(" upgrade ")));
            drop(release);
            finish_the_check(&mut app);

            assert!(row_labels(&app).contains(&"bun 1.4.2".to_string()));
            assert!(row_on_screen(&app, "bun 1.4.2").contains("↑ 1.4.4_1"));
            assert_eq!(ran_times(&host, &brew, "update"), 1);
            app.handle_key(KeyCode::Char('u'));
            assert!(screen_text(&app).contains("Upgrade bun?"));
        }

        #[test]
        fn a_check_that_ends_off_the_list_is_dropped_and_the_next_opening_reads_its_own() {
            let (_temp, mut app, host, brew) = check_mac();
            let release = hold(&host, &format!("{BREW_START} {brew} update"));
            open_the_list(&mut app);
            app.handle_key(KeyCode::Esc);
            assert!(app.brew_check.is_some());
            drop(release);
            for _ in 0..400 {
                app.poll_brew_check();
                if app.brew_check.is_none() {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
            assert!(app.brew_check.is_none());
            assert!(matches!(app.screen, Screen::Workspace(_)));

            open_the_list(&mut app);

            assert!(!select(&app).checking_updates);
            assert!(select(&app).toast.is_none());
            assert_eq!(ran_times(&host, &brew, "update"), 1);
            assert_eq!(ran_times(&host, &brew, "outdated"), 3);
        }

        #[test]
        fn a_failed_update_says_so_and_keeps_the_list() {
            let (_temp, mut app, host, brew) = check_mac_with(
                CHECK_INFO,
                homebrew::testing::BREW_OUTDATED_BUN,
                None,
                Some("Error: no network"),
            );
            open_the_list(&mut app);
            let before = row_labels(&app);

            finish_the_check(&mut app);

            let toast = select(&app).toast.as_ref().expect("a toast");
            assert_eq!(toast.text, "Could not check for updates: Error: no network");
            assert_eq!(toast.kind, ToastKind::Error);
            assert!(row_labels(&app).contains(&"bun 1.4.2".to_string()));
            assert!(row_on_screen(&app, "bun 1.4.2").contains("↑ 1.4.4_1"));
            assert_eq!(row_labels(&app), before);
            assert_eq!(ran_times(&host, &brew, "outdated"), 2);
        }

        #[test]
        fn after_a_failed_update_the_list_shows_what_outdated_reports() {
            let (_temp, mut app, _host, _brew) = check_mac_with(
                CHECK_INFO,
                NOTHING_NEW,
                Some(homebrew::testing::BREW_OUTDATED_BUN),
                Some("Error: no network"),
            );
            open_the_list(&mut app);
            assert!(row_labels(&app).contains(&"bun 1.4.2".to_string()));
            assert!(!row_on_screen(&app, "bun 1.4.2").contains('↑'));

            finish_the_check(&mut app);

            assert!(row_labels(&app).contains(&"bun 1.4.2".to_string()));
            assert!(row_on_screen(&app, "bun 1.4.2").contains("↑ 1.4.4_1"));
            assert_eq!(
                select(&app).toast.as_ref().expect("a toast").text,
                "Could not check for updates: Error: no network"
            );
        }

        #[test]
        fn the_check_keeps_the_focus_and_the_open_rows() {
            let (_temp, mut app, _host, _brew) = check_mac();
            open_the_list(&mut app);
            focus_on(&mut app, "app");
            app.handle_key(KeyCode::Right);
            focus_on(&mut app, "bun");
            let focused = select(&app).focused_index;
            let rows_before = select(&app).flattened_rows().len();

            finish_the_check(&mut app);

            let state = select(&app);
            assert_eq!(state.focused_index, focused);
            let row = &state.flattened_rows()[focused];
            assert!(row.display_path.starts_with("bun "), "{}", row.display_path);
            assert_eq!(row.display_path, "bun 1.4.2");
            assert!(row_on_screen(&app, "bun 1.4.2").contains("↑ 1.4.4_1"));
            assert!(state.expanded.contains(&vec!["app".to_string()]));
            assert_eq!(state.flattened_rows().len(), rows_before);
        }

        const SEARCH_INFO: &str = r#"{"formulae": [], "casks": [
            {"token": "aaa", "installed": "1"},
            {"token": "bbb", "installed": "1"}]}"#;
        const AAA_PINNED: &str = r#"{"formulae": [], "casks": [
            {"name": "aaa", "current_version": "2", "pinned": true}]}"#;
        const AAA_BBB_PINNED: &str = r#"{"formulae": [], "casks": [
            {"name": "aaa", "current_version": "2", "pinned": true},
            {"name": "bbb", "current_version": "3", "pinned": true}]}"#;

        #[test]
        fn the_check_keeps_the_focus_on_its_package_while_a_search_is_active() {
            let (_temp, mut app, _host, _brew) =
                check_mac_with(SEARCH_INFO, AAA_PINNED, Some(AAA_BBB_PINNED), None);
            for (name, bytes) in [("aaa", 100_000), ("bbb", 300_000)] {
                let room = app.layout.system(&format!("opt/homebrew/Caskroom/{name}"));
                std::fs::create_dir_all(&room).unwrap();
                std::fs::write(room.join("file"), vec![0; bytes]).unwrap();
            }
            open_the_list(&mut app);
            let mut keys = vec![KeyCode::Char('/')];
            keys.extend("pinned".chars().map(KeyCode::Char));
            keys.push(KeyCode::Enter);
            press(&mut app, &keys);
            focus_on(&mut app, "aaa");
            let before = select(&app).focused_index;

            finish_the_check(&mut app);

            let state = select(&app);
            let labels: Vec<String> = state
                .flattened_rows()
                .iter()
                .filter(|row| row.is_item())
                .map(|row| row.display_path.clone())
                .collect();
            assert_eq!(labels, ["bbb 1 · pinned", "aaa 1 · pinned"]);
            assert_ne!(state.focused_index, before);
            let row = &state.flattened_rows()[state.focused_index];
            assert_eq!(row.display_path, "aaa 1 · pinned");
        }

        #[test]
        fn a_pinned_formula_keeps_its_refusal_after_the_check_gives_it_a_version() {
            let info = r#"{"formulae": [
                {"name": "pin", "pinned": true,
                 "installed": [{"version": "1", "installed_on_request": true}]}],
                "casks": []}"#;
            let outdated =
                r#"{"formulae": [{"name": "pin", "current_version": "2"}], "casks": []}"#;
            let (_temp, mut app, _host, _brew) =
                check_mac_with(info, NOTHING_NEW, Some(outdated), None);
            open_the_list(&mut app);
            finish_the_check(&mut app);
            assert!(row_on_screen(&app, "pin 1").contains("↑ 2"));
            focus_on(&mut app, "pin");

            app.handle_key(KeyCode::Char('d'));

            let state = select(&app);
            assert_eq!(
                state.toast.as_ref().expect("a toast").text,
                "pin is pinned. Run brew unpin pin first."
            );
            assert_eq!(selected(&app), 0);
            assert!(matches!(state.overlay, ModalOverlay::None));
        }

        #[test]
        fn a_failed_outdated_after_the_update_says_so_and_keeps_the_versions() {
            let (_temp, mut app, host, brew) =
                check_mac_with(CHECK_INFO, homebrew::testing::BREW_OUTDATED_BUN, None, None);
            host.fail(
                &format!("{BREW_START} {brew} outdated --json=v2"),
                "Error: outdated broke",
            );
            open_the_list(&mut app);
            let before = row_labels(&app);

            finish_the_check(&mut app);

            let toast = select(&app).toast.as_ref().expect("a toast");
            assert_eq!(
                toast.text,
                "Could not check for updates: Homebrew did not list the outdated packages: Error: outdated broke"
            );
            assert!(row_labels(&app).contains(&"bun 1.4.2".to_string()));
            assert!(row_on_screen(&app, "bun 1.4.2").contains("↑ 1.4.4_1"));
            assert_eq!(row_labels(&app), before);
        }

        #[test]
        fn a_check_that_dies_stops_checking() {
            let (_temp, mut app, host, brew) = check_mac();
            host.when(&format!("{BREW_START} {brew} update"), || {
                panic!("the check thread dies")
            });
            open_the_list(&mut app);

            finish_the_check(&mut app);

            assert!(select(&app).toast.is_none());
            assert!(!screen_text(&app).contains("Checking for updates"));
        }

        #[test]
        fn the_check_keeps_a_row_selected_for_removal() {
            let (_temp, mut app, _host, _brew) = check_mac();
            open_the_list(&mut app);
            focus_on(&mut app, "bun");
            app.handle_key(KeyCode::Char('d'));
            assert_eq!(selected(&app), 1);

            finish_the_check(&mut app);

            assert_eq!(selected(&app), 1);
            assert!(matches!(select(&app).overlay, ModalOverlay::Confirm(_)));
        }

        #[test]
        fn a_removal_during_the_check_keeps_the_status_line() {
            let (_temp, mut app, _host, _brew) = check_mac();
            open_the_list(&mut app);
            focus_on(&mut app, "bun");
            let Screen::Select(state) = &mut app.screen else {
                panic!("expected the select screen");
            };
            state.uninstall_focused();
            state.overlay = ModalOverlay::Outcome(RemovalReport::default());

            state.close_homebrew_outcome();

            assert!(state.checking_updates);
        }

        #[test]
        fn the_homebrew_row_reads_brew_and_shows_the_list() {
            let (_temp, layout) = sandbox();
            let mut app = new_app();
            app.layout = layout;

            app.handle_key(KeyCode::Char('5'));
            assert!(screen_text(&app).contains("Reading Homebrew"));

            while matches!(app.screen, Screen::Scanning(..)) {
                app.poll_scan();
                thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(select(&app).scan.id, homebrew::SCAN_ID);
            assert_eq!(select(&app).scan.notes, ["Homebrew is not installed."]);
        }

        #[test]
        fn a_failed_read_shows_its_message_as_a_note() {
            let (_temp, layout) = homebrew::testing::with_brew("opt/homebrew");
            let host = FakeHost::default();
            host.fail("/usr/bin/env", "Error: broken tap");
            let mut app = new_app();
            app.layout = layout;
            app.host = Arc::new(host);

            app.handle_key(KeyCode::Char('5'));
            while matches!(app.screen, Screen::Scanning(..)) {
                app.poll_scan();
                thread::sleep(Duration::from_millis(5));
            }

            let scan = &select(&app).scan;
            assert_eq!(scan.id, homebrew::SCAN_ID);
            assert!(scan.groups.is_empty());
            assert_eq!(
                scan.notes,
                ["Homebrew did not list its packages: Error: broken tap"]
            );
        }

        #[test]
        fn the_list_starts_as_a_table_sorted_by_size() {
            let app = brew_app();

            assert_eq!(select(&app).sort_order, Some(SortOrder::Size));
            assert_eq!(shown(&app), ["  go", "  fd", "  ghostty"]);
            assert_eq!(focused_path(&app), ["go"]);
        }

        fn days_ago(days: u64) -> Option<SystemTime> {
            Some(SystemTime::now() - Duration::from_secs(days * 24 * 3600))
        }

        fn dated(kind: PackageKind, name: &str, bytes: u64, days: Option<u64>) -> Package {
            let mut package = package(kind, name, bytes);
            package.installed_at = days.and_then(days_ago);
            package
        }

        fn sorted_app() -> App {
            brew_list_app(&Inventory {
                packages: vec![
                    dated(PackageKind::Formula, "bun", 5_000, Some(1)),
                    dated(PackageKind::Formula, "zed", 9_000, Some(30)),
                    dated(PackageKind::Formula, "act", 1_000, None),
                    dated(PackageKind::Cask, "zcask", 9_000, Some(2)),
                    dated(PackageKind::Cask, "acask", 100, Some(10)),
                ],
                brew: Some("/fake/bin/brew".into()),
                ..Inventory::default()
            })
        }

        #[test]
        fn shift_letters_sort_the_homebrew_list() {
            let mut app = sorted_app();

            app.handle_key(KeyCode::Char('N'));
            assert_eq!(
                shown(&app),
                ["  act", "  bun", "  zed", "  acask", "  zcask"]
            );
            app.handle_key(KeyCode::Char('T'));
            assert_eq!(
                shown(&app),
                ["  bun", "  zed", "  act", "  zcask", "  acask"]
            );
            app.handle_key(KeyCode::Char('S'));
            assert_eq!(
                shown(&app),
                ["  zed", "  bun", "  act", "  zcask", "  acask"]
            );
        }

        #[test]
        fn homebrew_shift_d_uninstalls_and_shift_t_sorts_by_date() {
            let mut app = refusal_app();
            focus_on(&mut app, "fd");

            app.handle_key(KeyCode::Char('T'));

            let state = select(&app);
            assert!(matches!(state.overlay, ModalOverlay::None));
            assert!(state.toast.is_none());
            assert_eq!(selected(&app), 0);
            assert_eq!(state.sort_order, Some(SortOrder::Date));

            let mut app = refusal_app();
            focus_on(&mut app, "fd");
            app.handle_key(KeyCode::Char('D'));
            assert!(matches!(select(&app).overlay, ModalOverlay::Confirm(_)));
            assert_eq!(selected(&app), 1);
            assert_eq!(select(&app).sort_order, Some(SortOrder::Size));
        }

        #[test]
        fn searching_the_homebrew_list_types_d_and_shift_d() {
            let mut app = refusal_app();
            focus_on(&mut app, "fd");

            press(
                &mut app,
                &[KeyCode::Char('/'), KeyCode::Char('d'), KeyCode::Char('D')],
            );

            let state = select(&app);
            assert_eq!(state.search_query, "dD");
            assert!(matches!(state.overlay, ModalOverlay::None));
            assert_eq!(selected(&app), 0);
        }

        #[test]
        fn the_homebrew_list_names_d_and_shift_t() {
            let app = refusal_app();

            assert_footer(
                &app,
                &["d  Uninstall", "⇧S/N/T  Sort: Size"],
                &["D  Uninstall"],
            );
            assert_eq!(help_row(&app, "Uninstall"), Some("d"));
            assert_eq!(help_row(&app, "Date installed"), Some("⇧T"));
        }

        #[test]
        fn the_sort_keys_do_not_work_while_typing_a_search() {
            let mut app = sorted_app();

            press(&mut app, &[KeyCode::Char('/'), KeyCode::Char('N')]);

            assert_eq!(select(&app).sort_order, Some(SortOrder::Size));
            assert_eq!(select(&app).search_query, "N");
        }

        #[test]
        fn children_sort_by_the_same_key() {
            let mut tool = package(PackageKind::Formula, "tool", 100);
            tool.dependencies = vec!["zlib".to_string(), "abc".to_string()];
            let mut zlib = package(PackageKind::Formula, "zlib", 9_000);
            zlib.on_request = false;
            let mut abc = package(PackageKind::Formula, "abc", 10);
            abc.on_request = false;
            let mut app = brew_list_app(&Inventory {
                packages: vec![tool, zlib, abc],
                brew: Some("/fake/bin/brew".into()),
                ..Inventory::default()
            });
            app.handle_key(KeyCode::Enter);

            app.handle_key(KeyCode::Char('N'));
            assert_eq!(shown(&app), ["▾ tool", "    abc", "    zlib"]);
            app.handle_key(KeyCode::Char('S'));
            assert_eq!(shown(&app), ["▾ tool", "    zlib", "    abc"]);
        }

        #[test]
        fn sorting_keeps_the_open_rows_and_the_focused_row() {
            let mut go = package(PackageKind::Formula, "go", 250_000);
            go.used_by = vec!["golangci-lint".to_string()];
            let mut lint = package(PackageKind::Formula, "golangci-lint", 120_000);
            lint.dependencies = vec!["go".to_string()];
            let bun = package(PackageKind::Formula, "bun", 60_000);
            let mut app = brew_list_app(&Inventory {
                packages: vec![go, lint, bun],
                brew: Some("/fake/bin/brew".into()),
                ..Inventory::default()
            });
            focus_path(&mut app, &["golangci-lint"]);
            app.handle_key(KeyCode::Enter);
            focus_path(&mut app, &["golangci-lint", "go"]);
            let expanded = select(&app).expanded.clone();

            app.handle_key(KeyCode::Char('N'));

            assert_eq!(focused_path(&app), ["golangci-lint", "go"]);
            assert_eq!(select(&app).expanded, expanded);
            assert_eq!(shown(&app)[0], "  bun");
            assert!(shown(&app).contains(&"▾ golangci-lint".to_string()));
            app.handle_key(KeyCode::Char('S'));
            assert_eq!(focused_path(&app), ["golangci-lint", "go"]);
        }

        #[test]
        fn search_results_sort_by_the_same_key() {
            let mut app = brew_list_app(&Inventory {
                packages: vec![
                    package(PackageKind::Formula, "gamma", 1_000),
                    package(PackageKind::Formula, "golf", 9_000),
                    package(PackageKind::Formula, "gnu", 5_000),
                ],
                brew: Some("/fake/bin/brew".into()),
                ..Inventory::default()
            });
            press(
                &mut app,
                &[KeyCode::Char('/'), KeyCode::Char('g'), KeyCode::Enter],
            );
            assert_eq!(shown(&app), ["golf", "gnu", "gamma"]);

            app.handle_key(KeyCode::Char('N'));
            assert_eq!(shown(&app), ["gamma", "gnu", "golf"]);
            let state = select(&app);
            let focused = &state.flattened_rows()[state.focused_index];
            assert!(
                focused.display_path.starts_with("golf "),
                "{}",
                focused.display_path
            );
            app.handle_key(KeyCode::Char('S'));
            assert_eq!(shown(&app), ["golf", "gnu", "gamma"]);
            app.handle_key(KeyCode::Char('N'));
            press(&mut app, &[KeyCode::Char('/'), KeyCode::Esc]);
            assert_eq!(shown(&app), ["  gamma", "  gnu", "  golf"]);
        }

        #[test]
        fn the_date_column_shows_when_a_package_was_installed() {
            let app = brew_list_app(&Inventory {
                packages: vec![
                    dated(PackageKind::Formula, "recent", 2_000, Some(3)),
                    dated(PackageKind::Formula, "undated", 1_000, None),
                ],
                brew: Some("/fake/bin/brew".into()),
                ..Inventory::default()
            });

            let text = screen_text(&app);

            assert!(text.contains("INSTALLED"), "{text}");
            let line = |name: &str| text.lines().find(|l| l.contains(name)).unwrap();
            assert!(line("recent").contains("3 days ago"), "{text}");
            assert!(line("undated").contains('—'), "{text}");
        }

        #[test]
        fn a_removal_keeps_the_sort() {
            let mut run = Removal::open(&[("aaa", &[]), ("bbb", &[]), ("ccc", &[])], false);
            run.app.handle_key(KeyCode::Char('N'));

            run.remove("bbb");
            run.close_outcome();

            assert_eq!(select(&run.app).sort_order, Some(SortOrder::Name));
            assert_eq!(shown(&run.app), ["  aaa", "  ccc"]);
        }

        #[test]
        fn no_row_has_a_path_to_open() {
            let app = brew_app();

            assert!(select(&app).current_focused_path().is_none());
        }

        #[test]
        fn selecting_does_nothing() {
            let mut app = brew_app();

            press(
                &mut app,
                &[
                    KeyCode::Char(' '),
                    KeyCode::Down,
                    KeyCode::Char(' '),
                    KeyCode::Char('a'),
                    KeyCode::Char('A'),
                ],
            );

            assert_eq!(selected(&app), 0);
            assert!(matches!(select(&app).overlay, ModalOverlay::None));

            press(&mut app, &[KeyCode::Char('n'), KeyCode::Char('N')]);
            assert_eq!(selected(&app), 0);
            assert_eq!(select(&app).sort_order, Some(SortOrder::Name));
        }

        #[test]
        fn the_footer_has_no_review_or_selection_hints() {
            let app = brew_app();

            assert_footer(&app, &["↑↓  Move", "/  Search", "⎋  Back", "?  More"], &[]);
            assert_footer(
                &app,
                &[],
                &["Select", "All", "None", "Review", "Remove", "Reveal"],
            );
            assert_footer(&app, &["⇧S/N/T  Sort: Size"], &[]);
        }

        #[test]
        fn the_footer_names_the_current_sort() {
            let mut app = brew_app();

            app.handle_key(KeyCode::Char('N'));

            assert_footer(&app, &["⇧S/N/T  Sort: Name"], &[]);
        }

        #[test]
        fn the_help_sheet_names_the_sort_keys() {
            let app = brew_app();

            assert_eq!(help_row(&app, "Size"), Some("⇧S"));
            assert_eq!(help_row(&app, "Name"), Some("⇧N"));
            assert_eq!(help_row(&app, "Date installed"), Some("⇧T"));
        }

        #[test]
        fn the_help_sheet_has_no_review_or_selection_hints() {
            let app = brew_app();

            for desc in [
                "Toggle item",
                "Select all",
                "Select none",
                "Review & clean",
                "Reveal in Finder",
            ] {
                assert_eq!(help_row(&app, desc), None, "{desc}");
            }
            assert_eq!(help_row(&app, "Search"), Some("/"));
        }

        #[test]
        fn the_crumbs_and_status_name_homebrew_without_a_selection_count() {
            let app = brew_app();

            let text = screen_text(&app);

            assert!(text.contains("taert › Homebrew"));
            assert!(text.contains("3 packages"));
            assert!(!text.contains("selected"));
            assert!(!text.contains("Clean caches"));
        }

        #[test]
        fn a_long_label_uses_the_whole_row_at_80_columns() {
            let mut python = homebrew::testing::package(
                PackageKind::Formula,
                "python@3.14",
                "3.14.8_1",
                180_000_000,
            );
            python.on_request = false;
            python.latest_version = Some("3.14.9".to_string());
            python.pinned = true;
            python.used_by = vec!["azure-cli".to_string()];
            let app = brew_list_app(&Inventory {
                packages: vec![python],
                ..Inventory::default()
            });

            let line = screen_rows(&app, 80, 24)
                .into_iter()
                .find(|line| line.contains("python@3.14"))
                .unwrap();

            assert!(
                line.contains("python@3.14 3.14.8_1 · used by 1 · pinned"),
                "{line}"
            );
            assert!(line.contains("↑ 3.14.9"), "{line}");
        }

        #[test]
        fn an_empty_list_says_there_is_nothing_to_show() {
            let app = brew_list_app(&Inventory::default());

            let text = screen_text(&app);

            assert!(text.contains("No Homebrew packages"));
            assert!(!text.contains("Nothing to clean"));
        }

        #[test]
        fn an_empty_list_with_a_note_shows_the_note_once_as_the_reason() {
            let app = brew_list_app(&Inventory {
                notes: vec!["Homebrew is not installed.".to_string()],
                ..Inventory::default()
            });

            let text = screen_text(&app);

            assert!(text.contains("Homebrew isn't available"));
            assert_eq!(text.matches("Homebrew is not installed.").count(), 1);
            assert!(!text.contains("No Homebrew packages"));
        }

        #[test]
        fn an_empty_list_has_no_primary_action() {
            for (notes, title) in [
                (Vec::new(), "No Homebrew packages"),
                (
                    vec!["Homebrew is not installed.".to_string()],
                    "Homebrew isn't available",
                ),
            ] {
                let app = brew_list_app(&Inventory {
                    notes,
                    ..Inventory::default()
                });

                assert!(screen_text(&app).contains(title), "{title}");
                assert_footer(&app, &["⎋  Back"], &["Uninstall", "↵"]);
                assert_eq!(help_row(&app, "Uninstall"), None, "{title}");
                assert_eq!(help_row(&app, "Size"), None, "{title}");
            }
        }

        const CLEANUP_LINE: &str = "/usr/bin/env HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_ENV_HINTS=1 NONINTERACTIVE=1 HOMEBREW_NO_AUTOREMOVE=1 /fake/bin/brew cleanup lib";

        fn cleanup_app(cellar: &Path) -> App {
            let mut item = ScanItem::new("lib 0.9".into(), 50_000, true, false);
            item.brew = Some(homebrew::BrewTask::Cleanup {
                name: "lib".to_string(),
                brew: "/fake/bin/brew".into(),
                cellar: cellar.to_path_buf(),
            });
            select_app(CategoryScan {
                notes: Vec::new(),
                id: homebrew::CLEANUP_ID.to_string(),
                title: "Homebrew Cleanup".to_string(),
                groups: vec![ToolGroup {
                    tool: "Old versions".to_string(),
                    items: vec![item],
                }],
            })
        }

        #[test]
        fn a_cleanup_without_homebrew_shows_the_note_once_as_the_reason() {
            let (_temp, layout) = sandbox();
            let scan = homebrew::cleanup_scan(&layout, &FakeHost::default()).unwrap();
            let app = select_app(scan);

            let text = screen_text(&app);

            assert!(text.contains("Homebrew isn't available"), "{text}");
            assert_eq!(text.matches("Homebrew is not installed.").count(), 1);
            assert!(!text.contains("Nothing to clean"), "{text}");
        }

        #[test]
        fn an_empty_cleanup_says_homebrew_has_nothing_to_clean() {
            let app = select_app(CategoryScan {
                notes: Vec::new(),
                id: homebrew::CLEANUP_ID.to_string(),
                title: "Homebrew Cleanup".to_string(),
                groups: Vec::new(),
            });

            let text = screen_text(&app);

            assert!(text.contains("Homebrew has nothing to clean"), "{text}");
            assert!(!text.contains("Nothing to clean"), "{text}");
            assert!(!text.contains("Your caches are already tidy."), "{text}");
        }

        #[test]
        fn the_cleanup_crumbs_read_homebrew_then_cleanup() {
            let app = cleanup_app(Path::new("/fake/Cellar/lib"));

            let text = screen_text(&app);

            assert!(text.contains("taert › Homebrew › Cleanup"), "{text}");
            assert!(!text.contains("Clean caches"));
        }

        #[test]
        fn a_cleanup_row_has_no_path_to_reveal_and_no_reveal_hint() {
            let app = cleanup_app(Path::new("/fake/Cellar/lib"));

            assert!(select(&app).current_focused_path().is_none());
            assert_footer(&app, &["/  Search"], &["Reveal"]);
        }

        #[test]
        fn a_list_with_a_file_row_next_to_a_brew_row_keeps_reveal() {
            let mut app = cleanup_app(Path::new("/fake/Cellar/lib"));
            let Screen::Select(state) = &mut app.screen else {
                panic!("expected the select screen");
            };
            state.scan.groups[0]
                .items
                .push(ScanItem::new("/tmp/file".into(), 1, true, false));

            assert_footer(&app, &["O  Reveal"], &[]);
        }

        #[test]
        fn a_docker_row_still_has_a_path_to_reveal() {
            let mut scan = scan_with_id(docker::SCAN_ID);
            scan.groups[0].items[0].docker = Some(docker::DockerObject::BuildCache);
            let app = select_app(scan);

            assert!(select(&app).current_focused_path().is_some());
            assert_footer(&app, &["O  Reveal"], &[]);
        }

        #[test]
        fn a_cleanup_removal_goes_through_brew_and_is_logged_as_brew_cleanup() {
            let (_temp, layout) = sandbox();
            let cellar = layout.system("opt/homebrew/Cellar/lib");
            std::fs::create_dir_all(cellar.join("0.9")).unwrap();
            std::fs::write(cellar.join("0.9/data"), vec![1_u8; 50_000]).unwrap();
            let host = FakeHost::default();
            host.reply(CLEANUP_LINE, "");
            let old = cellar.join("0.9");
            host.when(CLEANUP_LINE, move || std::fs::remove_dir_all(&old).unwrap());
            let mut app = cleanup_app(&cellar);
            app.layout = layout;
            app.host = Arc::new(host);

            app.handle_key(KeyCode::Char('d'));
            let confirm = screen_text(&app);
            assert!(confirm.contains("permanently deleted"), "{confirm}");
            assert!(!confirm.contains("Trash"), "{confirm}");

            app.handle_key(KeyCode::Char('y'));
            while app.removal_rx.is_some() {
                app.poll_removal();
                thread::sleep(Duration::from_millis(5));
            }

            let outcome = screen_text(&app);
            assert!(outcome.contains("Clean Complete"), "{outcome}");
            assert!(outcome.contains("permanently"), "{outcome}");
            assert!(!outcome.contains("Trash"), "{outcome}");
            assert!(!cellar.join("0.9").exists());
            let runs = HistoryLog::new(app.layout.history_path())
                .load_runs()
                .unwrap();
            assert_eq!(runs.len(), 1);
            assert_eq!(runs[0].command, "brew cleanup");
            assert_eq!(runs[0].items[0].path, "lib 0.9");
            assert!(runs[0].items[0].bytes >= 50_000);
        }

        #[test]
        fn searching_keeps_the_apply_action_and_browsing_has_uninstall() {
            let mut app = brew_app();
            assert!(!screen_text(&app).contains("Apply"));
            assert!(footer(&app).contains("Uninstall"), "{}", footer(&app));

            press(&mut app, &[KeyCode::Char('/')]);

            assert!(screen_text(&app).contains("Apply"));
            assert!(!footer(&app).contains("Uninstall"));
        }

        #[test]
        fn d_on_a_refused_row_shows_why_and_changes_nothing() {
            let refusals = [
                (
                    "ghostty",
                    "ghostty is a cask. Remove it with taert uninstall.",
                ),
                ("pin", "pin is pinned. Run brew unpin pin first."),
                ("lib", "lib stays: app, tool use it."),
            ];

            for (name, text) in refusals {
                let mut app = refusal_app();
                let host = Arc::new(FakeHost::default());
                app.host = host.clone();
                focus_on(&mut app, name);

                app.handle_key(KeyCode::Char('d'));

                let state = select(&app);
                let toast = state.toast.as_ref().expect(name);
                assert_eq!(toast.text, text);
                assert_eq!(toast.kind, ToastKind::Error);
                assert!(matches!(state.overlay, ModalOverlay::None), "{name}");
                assert_eq!(selected(&app), 0, "{name}");
                assert!(screen_text(&app).contains(text), "{name}");
                assert!(host.calls.lock().unwrap().is_empty(), "{name}");
            }
        }

        #[test]
        fn d_on_a_plain_formula_asks_to_remove_only_that_row_and_says_homebrew_does_it() {
            let mut app = refusal_app();
            focus_on(&mut app, "fd");

            app.handle_key(KeyCode::Char('d'));

            let state = select(&app);
            assert!(matches!(state.overlay, ModalOverlay::Confirm(_)));
            assert!(state.toast.is_none());
            let chosen = state.scan.selected_items();
            assert_eq!(chosen.len(), 1);
            assert!(chosen[0].path.to_string_lossy().starts_with("fd "));
            let text = screen_text(&app);
            assert!(text.contains("Uninstall fd?"), "{text}");
            assert!(text.contains("Homebrew will remove fd"), "{text}");
            assert!(!text.contains("Trash"), "{text}");
            assert!(!text.contains("permanently deleted"), "{text}");
        }

        #[test]
        fn a_cancelled_question_does_not_carry_its_row_into_the_next_d() {
            let mut app = refusal_app();
            focus_on(&mut app, "fd");
            app.handle_key(KeyCode::Char('d'));
            app.handle_key(KeyCode::Esc);

            focus_on(&mut app, "lib");
            app.handle_key(KeyCode::Char('d'));

            assert_eq!(selected(&app), 0);
            assert!(select(&app).toast.is_some());
        }

        #[test]
        fn the_list_opened_from_the_command_line_has_the_refusals_too() {
            let inventory = Inventory {
                packages: vec![package(PackageKind::Cask, "ghostty", 9_000)],
                brew: Some("/fake/bin/brew".into()),
                ..Inventory::default()
            };
            let mut app = offline(App::with_brew_list(
                &inventory,
                IgnoreStore::default(),
                Config::default(),
            ));

            app.handle_key(KeyCode::Char('d'));

            assert_eq!(
                select(&app).toast.as_ref().unwrap().text,
                "ghostty is a cask. Remove it with taert uninstall."
            );
        }

        #[test]
        fn the_list_opened_from_the_menu_has_the_refusals_too() {
            let (_temp, layout) = homebrew::testing::with_brew("opt/homebrew");
            let brew = layout.system("opt/homebrew/bin/brew");
            let info = serde_json::json!({
                "formulae": [
                    {"name": "pin", "pinned": true, "installed": [{"version": "1"}]},
                    {"name": "lib", "installed": [{"version": "1"}]},
                    {"name": "app", "dependencies": ["lib"], "installed": [{"version": "1"}]},
                    {"name": "tool", "dependencies": ["lib"], "installed": [{"version": "1"}]},
                ],
                "casks": [{"token": "ghostty", "installed": "1.0"}],
            });
            let host = Arc::new(FakeHost::default());
            host.reply(
                &format!("{BREW_START} {} info --json=v2 --installed", brew.display()),
                &info.to_string(),
            );
            let mut app = new_app();
            app.layout = layout;
            app.host = host.clone();
            app.brew_checked = true;

            app.handle_key(KeyCode::Char('5'));
            while matches!(app.screen, Screen::Scanning(..)) {
                app.poll_scan();
                thread::sleep(Duration::from_millis(5));
            }
            let calls_after_scan = host.calls.lock().unwrap().len();

            let refusals = [
                (
                    "ghostty",
                    "ghostty is a cask. Remove it with taert uninstall.",
                ),
                ("pin", "pin is pinned. Run brew unpin pin first."),
                ("lib", "lib stays: app, tool use it."),
            ];
            for (name, text) in refusals {
                focus_on(&mut app, name);

                app.handle_key(KeyCode::Char('d'));

                let state = select(&app);
                let toast = state.toast.as_ref().expect(name);
                assert_eq!(toast.text, text);
                assert_eq!(toast.kind, ToastKind::Error);
                assert!(matches!(state.overlay, ModalOverlay::None), "{name}");
                assert_eq!(selected(&app), 0, "{name}");
            }
            assert_eq!(host.calls.lock().unwrap().len(), calls_after_scan);
            assert!(!host.ran(&format!("{BREW_START} HOMEBREW_NO_AUTOREMOVE=1")));
        }

        /// Two formulae that use `go`, one that uses `shellcheck`, which needs `gmp`, a plain one,
        /// and one that nothing needs.
        fn tree_inventory() -> Inventory {
            let formula = |name: &str, bytes: u64, on_request: bool, deps: &[&str], by: &[&str]| {
                let mut package = package(PackageKind::Formula, name, bytes);
                package.on_request = on_request;
                package.dependencies = deps.iter().map(|d| d.to_string()).collect();
                package.used_by = by.iter().map(|u| u.to_string()).collect();
                package
            };
            Inventory {
                packages: vec![
                    formula("actionlint", 9_000, true, &["shellcheck"], &[]),
                    formula("golangci-lint", 8_000, true, &["go"], &[]),
                    formula("go", 6_000, true, &[], &["golangci-lint", "gosec"]),
                    formula("gosec", 4_000, true, &["go"], &[]),
                    formula("shellcheck", 3_000, true, &["gmp"], &["actionlint"]),
                    formula("fd", 2_000, true, &[], &[]),
                    formula("gmp", 500, false, &[], &["shellcheck"]),
                    formula("orphan", 100, false, &[], &[]),
                    package(PackageKind::Cask, "ghostty", 9_000),
                ],
                brew: Some("/fake/bin/brew".into()),
                ..Inventory::default()
            }
        }

        fn tree_app() -> App {
            brew_list_app(&tree_inventory())
        }

        /// The item rows as the tree draws them: indent, `▸` or `▾` when it can open, the name.
        fn shown(app: &App) -> Vec<String> {
            select(app)
                .flattened_rows()
                .iter()
                .filter(|row| row.is_item())
                .map(|row| {
                    let name = row.display_path.split(' ').next().unwrap();
                    let Some(tree) = &row.tree else {
                        return name.to_string();
                    };
                    let mark = match (tree.expandable, tree.expanded) {
                        (false, _) => " ",
                        (true, false) => "▸",
                        (true, true) => "▾",
                    };
                    format!("{}{mark} {name}", "  ".repeat(tree.depth()))
                })
                .collect()
        }

        fn focus_path(app: &mut App, path: &[&str]) {
            let Screen::Select(state) = &mut app.screen else {
                panic!("expected the select screen");
            };
            let path: Vec<String> = path.iter().map(|name| name.to_string()).collect();
            state.focused_index = state
                .flattened_rows()
                .iter()
                .position(|row| row.tree.as_ref().is_some_and(|tree| tree.path == path))
                .unwrap_or_else(|| panic!("no row {path:?}"));
        }

        fn focused_path(app: &App) -> Vec<String> {
            let state = select(app);
            state.flattened_rows()[state.focused_index]
                .tree
                .as_ref()
                .expect("a tree row")
                .path
                .clone()
        }

        #[test]
        fn the_list_starts_collapsed_with_the_formulae_you_installed_first() {
            let app = tree_app();

            assert_eq!(
                shown(&app),
                [
                    "▸ actionlint",
                    "▸ golangci-lint",
                    "  go",
                    "▸ gosec",
                    "▸ shellcheck",
                    "  fd",
                    "  orphan",
                    "  ghostty"
                ]
            );
            let sections: Vec<String> = select(&app)
                .flattened_rows()
                .iter()
                .filter(|row| row.kind == RowKind::Section)
                .map(|row| row.group_name.clone())
                .collect();
            assert_eq!(
                sections,
                [
                    "Formulae",
                    "Not needed by anything you installed",
                    "Casks · Remove with taert uninstall"
                ]
            );
        }

        #[test]
        fn enter_and_right_expand_and_enter_and_left_collapse_without_moving_the_focus() {
            let mut app = tree_app();
            let row = vec!["actionlint".to_string()];

            app.handle_key(KeyCode::Enter);
            assert_eq!(
                &shown(&app)[..3],
                ["▾ actionlint", "  ▸ shellcheck", "▸ golangci-lint"]
            );
            assert_eq!(focused_path(&app), row);

            app.handle_key(KeyCode::Enter);
            assert_eq!(&shown(&app)[..2], ["▸ actionlint", "▸ golangci-lint"]);
            assert_eq!(focused_path(&app), row);

            app.handle_key(KeyCode::Right);
            assert_eq!(&shown(&app)[..2], ["▾ actionlint", "  ▸ shellcheck"]);
            assert_eq!(focused_path(&app), row);

            app.handle_key(KeyCode::Left);
            assert_eq!(&shown(&app)[..2], ["▸ actionlint", "▸ golangci-lint"]);
            assert_eq!(focused_path(&app), row);
            assert!(matches!(select(&app).overlay, ModalOverlay::None));
            assert_eq!(selected(&app), 0);
        }

        #[test]
        fn left_on_a_child_goes_to_its_parent_and_at_the_top_does_nothing() {
            let mut app = tree_app();
            app.handle_key(KeyCode::Enter);
            focus_path(&mut app, &["actionlint", "shellcheck"]);

            app.handle_key(KeyCode::Left);

            assert_eq!(focused_path(&app), ["actionlint"]);
            assert_eq!(shown(&app)[1], "  ▸ shellcheck");

            focus_path(&mut app, &["golangci-lint"]);
            let before = shown(&app);
            app.handle_key(KeyCode::Left);
            assert_eq!(focused_path(&app), ["golangci-lint"]);
            assert_eq!(shown(&app), before);
        }

        #[test]
        fn left_on_an_open_child_closes_it_before_it_goes_to_the_parent() {
            let mut app = tree_app();
            app.handle_key(KeyCode::Enter);
            focus_path(&mut app, &["actionlint", "shellcheck"]);
            app.handle_key(KeyCode::Enter);
            assert_eq!(
                &shown(&app)[..3],
                ["▾ actionlint", "  ▾ shellcheck", "      gmp"]
            );

            app.handle_key(KeyCode::Left);
            assert_eq!(focused_path(&app), ["actionlint", "shellcheck"]);
            assert_eq!(shown(&app)[1], "  ▸ shellcheck");

            app.handle_key(KeyCode::Left);
            assert_eq!(focused_path(&app), ["actionlint"]);
        }

        #[test]
        fn enter_on_a_row_with_no_dependencies_does_nothing() {
            let mut app = tree_app();
            let before = shown(&app);
            focus_on(&mut app, "fd");

            app.handle_key(KeyCode::Enter);

            assert_eq!(shown(&app), before);
            assert!(matches!(select(&app).overlay, ModalOverlay::None));
            assert!(select(&app).toast.is_none());
            assert_eq!(selected(&app), 0);

            app.handle_key(KeyCode::Enter);
            focus_path(&mut app, &["actionlint"]);
            app.handle_key(KeyCode::Enter);
            focus_path(&mut app, &["actionlint", "shellcheck"]);
            app.handle_key(KeyCode::Enter);
            focus_path(&mut app, &["actionlint", "shellcheck", "gmp"]);
            let before = shown(&app);
            app.handle_key(KeyCode::Enter);
            assert_eq!(shown(&app), before);
        }

        #[test]
        fn a_dependency_shows_under_every_formula_that_uses_it() {
            let mut app = tree_app();
            focus_path(&mut app, &["golangci-lint"]);
            app.handle_key(KeyCode::Enter);

            assert_eq!(
                &shown(&app)[..5],
                [
                    "▸ actionlint",
                    "▾ golangci-lint",
                    "    go",
                    "  go",
                    "▸ gosec"
                ]
            );

            focus_path(&mut app, &["gosec"]);
            app.handle_key(KeyCode::Enter);

            assert_eq!(
                &shown(&app)[..6],
                [
                    "▸ actionlint",
                    "▾ golangci-lint",
                    "    go",
                    "  go",
                    "▾ gosec",
                    "    go"
                ]
            );
            focus_path(&mut app, &["gosec", "go"]);
            app.handle_key(KeyCode::Left);
            assert_eq!(focused_path(&app), ["gosec"]);
        }

        #[test]
        fn a_dependency_cycle_stops_at_the_formula_that_is_already_open() {
            let mut a = package(PackageKind::Formula, "a", 9_000);
            a.dependencies = vec!["b".to_string()];
            let mut b = package(PackageKind::Formula, "b", 3_000);
            b.dependencies = vec!["a".to_string()];
            let mut app = brew_list_app(&Inventory {
                packages: vec![a, b],
                brew: Some("/fake/bin/brew".into()),
                ..Inventory::default()
            });

            app.handle_key(KeyCode::Enter);
            focus_path(&mut app, &["a", "b"]);
            app.handle_key(KeyCode::Enter);

            assert_eq!(shown(&app), ["▾ a", "  ▾ b", "      a", "▸ b"]);
            focus_path(&mut app, &["a", "b", "a"]);
            app.handle_key(KeyCode::Enter);
            app.handle_key(KeyCode::Right);
            assert_eq!(shown(&app), ["▾ a", "  ▾ b", "      a", "▸ b"]);
        }

        #[test]
        fn d_backspace_and_delete_ask_to_remove_a_formula_and_enter_does_not() {
            for key in [KeyCode::Char('d'), KeyCode::Backspace, KeyCode::Delete] {
                let mut app = refusal_app();
                focus_on(&mut app, "fd");

                app.handle_key(key);

                let state = select(&app);
                assert!(matches!(state.overlay, ModalOverlay::Confirm(_)), "{key:?}");
                assert!(state.toast.is_none(), "{key:?}");
                assert_eq!(selected(&app), 1, "{key:?}");
            }

            let mut app = refusal_app();
            focus_on(&mut app, "fd");
            app.handle_key(KeyCode::Enter);
            assert!(matches!(select(&app).overlay, ModalOverlay::None));
            assert!(select(&app).toast.is_none());
            assert_eq!(selected(&app), 0);
        }

        #[test]
        fn every_removal_key_shows_the_refusal_and_calls_nothing() {
            let refusals = [
                (
                    "ghostty",
                    "ghostty is a cask. Remove it with taert uninstall.",
                ),
                ("pin", "pin is pinned. Run brew unpin pin first."),
                ("lib", "lib stays: app, tool use it."),
            ];
            for key in [KeyCode::Char('d'), KeyCode::Backspace, KeyCode::Delete] {
                for (name, text) in refusals {
                    let mut app = refusal_app();
                    let host = Arc::new(FakeHost::default());
                    app.host = host.clone();
                    focus_on(&mut app, name);

                    app.handle_key(key);

                    let state = select(&app);
                    assert_eq!(state.toast.as_ref().expect(name).text, text);
                    assert!(matches!(state.overlay, ModalOverlay::None), "{name}");
                    assert_eq!(selected(&app), 0, "{name}");
                    assert!(host.calls.lock().unwrap().is_empty(), "{name}");
                }
            }
        }

        #[test]
        fn a_nested_row_is_refused_the_same_way_as_its_top_level_row() {
            let mut app = tree_app();
            focus_path(&mut app, &["golangci-lint"]);
            app.handle_key(KeyCode::Enter);
            focus_path(&mut app, &["golangci-lint", "go"]);

            app.handle_key(KeyCode::Char('d'));

            let state = select(&app);
            assert_eq!(
                state.toast.as_ref().unwrap().text,
                "go stays: golangci-lint, gosec use it."
            );
            assert!(matches!(state.overlay, ModalOverlay::None));
            assert_eq!(selected(&app), 0);
        }

        #[test]
        fn d_on_a_nested_row_asks_to_remove_that_formula() {
            let mut app_formula = package(PackageKind::Formula, "app", 9_000);
            app_formula.dependencies = vec!["lib".to_string()];
            let mut lib = package(PackageKind::Formula, "lib", 3_000);
            lib.on_request = false;
            let mut app = brew_list_app(&Inventory {
                packages: vec![app_formula, lib],
                brew: Some("/fake/bin/brew".into()),
                ..Inventory::default()
            });
            app.handle_key(KeyCode::Enter);
            focus_path(&mut app, &["app", "lib"]);

            app.handle_key(KeyCode::Char('d'));

            let state = select(&app);
            assert!(matches!(state.overlay, ModalOverlay::Confirm(_)));
            let chosen = state.scan.selected_items();
            assert_eq!(chosen.len(), 1);
            assert!(chosen[0].path.to_string_lossy().starts_with("lib "));
            assert!(screen_text(&app).contains("Homebrew will remove lib"));
        }

        #[test]
        fn after_a_removal_the_rows_that_were_open_stay_open() {
            let mut app = tree_app();
            app.handle_key(KeyCode::Enter);
            focus_path(&mut app, &["actionlint", "shellcheck"]);
            app.handle_key(KeyCode::Enter);
            focus_on(&mut app, "fd");
            app.handle_key(KeyCode::Char('d'));
            let Screen::Select(state) = &mut app.screen else {
                panic!("expected the select screen");
            };
            state.overlay = ModalOverlay::Outcome(RemovalReport {
                removed_count: 1,
                ..RemovalReport::default()
            });

            state.close_homebrew_outcome();

            assert_eq!(
                shown(&app),
                [
                    "▾ actionlint",
                    "  ▾ shellcheck",
                    "      gmp",
                    "▸ golangci-lint",
                    "  go",
                    "▸ gosec",
                    "▸ shellcheck",
                    "  orphan",
                    "  ghostty"
                ]
            );
        }

        #[test]
        fn a_search_lists_every_match_flat_and_clearing_it_brings_the_open_rows_back() {
            let mut app = tree_app();
            focus_path(&mut app, &["golangci-lint"]);
            app.handle_key(KeyCode::Enter);

            press(
                &mut app,
                &[KeyCode::Char('/'), KeyCode::Char('g'), KeyCode::Char('m')],
            );
            assert_eq!(shown(&app), ["gmp"]);

            press(&mut app, &[KeyCode::Backspace, KeyCode::Char('o')]);
            assert_eq!(shown(&app), ["golangci-lint", "go", "gosec"]);
            assert!(
                select(&app)
                    .flattened_rows()
                    .iter()
                    .all(|row| row.tree.is_none())
            );

            app.handle_key(KeyCode::Esc);

            assert_eq!(select(&app).search_query, "");
            assert_eq!(
                &shown(&app)[..5],
                [
                    "▸ actionlint",
                    "▾ golangci-lint",
                    "    go",
                    "  go",
                    "▸ gosec"
                ]
            );
        }

        #[test]
        fn a_search_shows_a_dependency_you_need_flat_under_needed_by_what_you_installed() {
            let mut app = tree_app();

            press(
                &mut app,
                &[
                    KeyCode::Char('/'),
                    KeyCode::Char('g'),
                    KeyCode::Char('m'),
                    KeyCode::Char('p'),
                ],
            );

            let rows = select(&app).flattened_rows();
            let sections: Vec<&str> = rows
                .iter()
                .filter(|row| row.kind == RowKind::Section)
                .map(|row| row.group_name.as_str())
                .collect();
            assert_eq!(sections, ["Needed by what you installed"]);
            let items: Vec<_> = rows.iter().filter(|row| row.is_item()).collect();
            assert_eq!(items.len(), 1);
            assert!(items[0].display_path.starts_with("gmp "));
            assert!(items[0].tree.is_none());
            let screen = screen_text(&app);
            let line = screen
                .lines()
                .find(|line| line.contains("gmp 1.0"))
                .expect("the gmp row on the screen");
            assert!(!line.contains('▸') && !line.contains('▾'), "{line}");
        }

        #[test]
        fn the_footer_and_the_primary_action_name_expand_collapse_and_uninstall() {
            let mut app = tree_app();

            assert_footer(&app, &["↵", "Expand", "d  Uninstall"], &["Collapse"]);
            let action = crate::ui::select::homebrew_primary(select(&app), "↵");
            assert!(action.enabled);
            assert_eq!(action.label, "Expand");

            app.handle_key(KeyCode::Enter);
            assert_footer(&app, &["Collapse", "d  Uninstall"], &["Expand"]);
            let action = crate::ui::select::homebrew_primary(select(&app), "↵");
            assert!(action.enabled);
            assert_eq!(action.label, "Collapse");

            focus_on(&mut app, "fd");
            let action = crate::ui::select::homebrew_primary(select(&app), "↵");
            assert!(!action.enabled);
            assert_footer(&app, &["d  Uninstall"], &[]);
        }

        #[test]
        fn the_help_sheet_names_the_tree_keys_and_uninstall() {
            let app = tree_app();

            assert_eq!(help_row(&app, "Expand"), Some("↵ / →"));
            assert_eq!(help_row(&app, "Collapse"), Some("↵ / ←"));
            assert_eq!(help_row(&app, "Uninstall"), Some("d"));
        }

        #[test]
        fn a_row_with_no_task_and_no_refusal_cannot_be_uninstalled() {
            let mut app = brew_list_app(&Inventory {
                packages: vec![package(PackageKind::Formula, "fd", 3_000)],
                ..Inventory::default()
            });

            app.handle_key(KeyCode::Char('d'));

            assert!(matches!(select(&app).overlay, ModalOverlay::None));
            assert!(select(&app).toast.is_none());
        }

        fn info_deps(formulae: &[(&str, &[&str])]) -> String {
            let formulae: Vec<_> = formulae
                .iter()
                .map(|(name, dependencies)| {
                    serde_json::json!({
                        "name": name,
                        "dependencies": dependencies,
                        "installed": [{"version": "1", "installed_on_request": true}],
                    })
                })
                .collect();
            serde_json::json!({"formulae": formulae, "casks": []}).to_string()
        }

        struct Removal {
            _temp: tempfile::TempDir,
            app: App,
            host: Arc<FakeHost>,
            brew: std::path::PathBuf,
            info_line: String,
        }

        impl Removal {
            /// The list is read from `formulae`; removing `lib` succeeds or fails as asked.
            fn open(formulae: &[(&str, &[&str])], uninstall_fails: bool) -> Self {
                let (temp, layout) = homebrew::testing::with_brew("opt/homebrew");
                let brew = layout.system("opt/homebrew/bin/brew");
                let info_line =
                    format!("{BREW_START} {} info --json=v2 --installed", brew.display());
                let host = FakeHost::default();
                host.reply(&info_line, &info_deps(formulae));
                let mut cellars = Vec::new();
                for (i, (name, _)) in formulae.iter().enumerate() {
                    let cellar = layout.system(&format!("opt/homebrew/Cellar/{name}"));
                    std::fs::create_dir_all(cellar.join("1")).unwrap();
                    std::fs::write(cellar.join("1/data"), vec![1_u8; 20_000 * (i + 1)]).unwrap();
                    cellars.push((name.to_string(), cellar));
                }
                for (name, cellar) in cellars {
                    let line = format!(
                        "{BREW_START} HOMEBREW_NO_AUTOREMOVE=1 {} uninstall {name}",
                        brew.display()
                    );
                    if uninstall_fails {
                        host.fail(&line, "Error: boom");
                    } else {
                        host.reply(&line, "");
                        host.when(&line, move || {
                            std::fs::remove_dir_all(&cellar).unwrap();
                        });
                    }
                }
                let inventory = homebrew::scan(&layout, &host).unwrap();
                let mut app = brew_list_app(&inventory);
                app.layout = layout;
                let host = Arc::new(host);
                app.host = host.clone();
                Self {
                    _temp: temp,
                    app,
                    host,
                    brew,
                    info_line,
                }
            }

            fn uninstall_line(&self, name: &str) -> String {
                format!(
                    "{BREW_START} HOMEBREW_NO_AUTOREMOVE=1 {} uninstall {name}",
                    self.brew.display()
                )
            }

            fn info_calls(&self) -> usize {
                self.host
                    .calls
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|call| **call == self.info_line)
                    .count()
            }

            /// `d` and `y` on the focused row, then waits for the outcome.
            fn remove(&mut self, name: &str) {
                focus_on(&mut self.app, name);
                self.app.handle_key(KeyCode::Char('d'));
                self.app.handle_key(KeyCode::Char('y'));
                while self.app.removal_rx.is_some() {
                    self.app.poll_removal();
                    thread::sleep(Duration::from_millis(5));
                }
            }

            fn close_outcome(&mut self) {
                self.app.handle_key(KeyCode::Enter);
                assert!(!screen_text(&self.app).contains("Reading Homebrew"));
            }
        }

        fn labels(app: &App) -> Vec<String> {
            select(app)
                .scan
                .groups
                .iter()
                .flat_map(|group| &group.items)
                .map(|item| item.path.to_string_lossy().into_owned())
                .collect()
        }

        fn size_of(app: &App, name: &str) -> u64 {
            select(app)
                .scan
                .groups
                .iter()
                .flat_map(|group| &group.items)
                .find(|item| item.path.to_string_lossy().starts_with(&format!("{name} ")))
                .unwrap()
                .size_bytes
        }

        #[test]
        fn a_removal_from_the_list_goes_through_brew_is_logged_and_the_list_is_not_read_again() {
            let mut run = Removal::open(&[("lib", &[]), ("keep", &[])], false);
            let cellar = run.app.layout.system("opt/homebrew/Cellar/lib");
            let before = DirSizer::size_of(&cellar);
            let keep_size = size_of(&run.app, "keep");

            run.remove("lib");

            let outcome = screen_text(&run.app);
            assert!(outcome.contains("Uninstall Complete"), "{outcome}");
            assert!(!outcome.contains("Trash"), "{outcome}");
            let runs = HistoryLog::new(run.app.layout.history_path())
                .load_runs()
                .unwrap();
            assert_eq!(runs.len(), 1);
            assert_eq!(runs[0].command, "brew uninstall");
            assert_eq!(runs[0].items.len(), 1);
            assert_eq!(runs[0].items[0].path, "lib 1");
            assert!(before >= 20_000);
            assert_eq!(runs[0].items[0].bytes, before);
            let removals: Vec<String> = run
                .host
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.contains("HOMEBREW_NO_AUTOREMOVE=1"))
                .cloned()
                .collect();
            assert_eq!(removals, [run.uninstall_line("lib")]);
            // The list, then the check before the removal.
            assert_eq!(run.info_calls(), 2);

            run.close_outcome();

            assert_eq!(run.info_calls(), 2);
            assert!(matches!(run.app.screen, Screen::Select(..)));
            let labels = labels(&run.app);
            assert_eq!(labels.len(), 1);
            assert!(labels[0].starts_with("keep "), "{labels:?}");
            assert_eq!(size_of(&run.app, "keep"), keep_size);
        }

        #[test]
        fn a_dependency_only_the_removed_formula_used_becomes_removable() {
            let mut run = Removal::open(&[("app", &["lib"]), ("lib", &[])], false);
            focus_on(&mut run.app, "lib");
            assert!(labels(&run.app).iter().any(|l| l.contains("used by 1")));
            run.app.handle_key(KeyCode::Char('d'));
            assert_eq!(
                select(&run.app).toast.as_ref().unwrap().text,
                "lib stays: app use it."
            );

            run.remove("app");
            run.close_outcome();

            assert_eq!(labels(&run.app).len(), 1);
            assert!(!labels(&run.app)[0].contains("used by"));
            focus_on(&mut run.app, "lib");
            run.app.handle_key(KeyCode::Char('d'));
            let state = select(&run.app);
            assert!(matches!(state.overlay, ModalOverlay::Confirm(_)));
            assert!(state.toast.is_none());
            assert_eq!(selected(&run.app), 1);
        }

        #[test]
        fn the_total_drops_by_the_size_of_the_removed_formula() {
            let mut run = Removal::open(&[("lib", &[]), ("keep", &[])], false);
            let total = select(&run.app).scan.total_all_bytes();
            let lib = size_of(&run.app, "lib");

            run.remove("lib");
            run.close_outcome();

            assert_eq!(select(&run.app).scan.total_all_bytes(), total - lib);
        }

        #[test]
        fn a_failed_removal_leaves_the_list_as_it_is_and_reads_nothing() {
            let mut run = Removal::open(&[("lib", &[]), ("keep", &[])], true);
            let before = labels(&run.app);

            run.remove("lib");
            assert!(screen_text(&run.app).contains("Nothing was removed"));
            run.close_outcome();

            assert_eq!(labels(&run.app), before);
            assert_eq!(selected(&run.app), 0);
            assert_eq!(run.info_calls(), 2);
        }

        #[test]
        fn removing_the_only_formula_leaves_an_empty_list() {
            let mut run = Removal::open(&[("lib", &[])], false);

            run.remove("lib");
            run.close_outcome();

            assert!(labels(&run.app).is_empty());
        }

        #[test]
        fn removing_the_only_formula_the_search_matches_clears_the_search() {
            let mut run = Removal::open(&[("lib", &[]), ("keep", &[])], false);
            let Screen::Select(state) = &mut run.app.screen else {
                panic!("expected the select screen");
            };
            state.search_query = "lib".to_string();

            run.remove("lib");
            run.close_outcome();

            let state = select(&run.app);
            assert_eq!(state.search_query, "");
            assert_eq!(labels(&run.app), ["keep 1"]);
            let rows = state.flattened_rows();
            assert!(rows[state.focused_index].display_path.starts_with("keep "));
        }

        #[test]
        fn keys_on_a_search_with_no_match_change_nothing() {
            let mut run = Removal::open(&[("lib", &[]), ("keep", &[])], false);
            run.app.handle_key(KeyCode::Char('/'));
            for c in "zzz".chars() {
                run.app.handle_key(KeyCode::Char(c));
            }
            run.app.handle_key(KeyCode::Enter);
            assert!(select(&run.app).flattened_rows().is_empty());

            for key in [
                KeyCode::Up,
                KeyCode::Down,
                KeyCode::PageUp,
                KeyCode::PageDown,
                KeyCode::Home,
                KeyCode::End,
                KeyCode::Left,
                KeyCode::Right,
                KeyCode::Enter,
                KeyCode::Char('d'),
                KeyCode::Delete,
                KeyCode::Backspace,
                KeyCode::Char(' '),
                KeyCode::Char('a'),
                KeyCode::Char('n'),
            ] {
                run.app.handle_key(key);
                let _ = screen_text(&run.app);
            }

            let state = select(&run.app);
            assert!(matches!(state.overlay, ModalOverlay::None));
            assert_eq!(selected(&run.app), 0);
            assert!(!run.host.ran(&run.uninstall_line("lib")));
        }

        #[test]
        fn removing_the_last_formula_moves_the_focus_to_the_formula_above() {
            let mut app = brew_list_app(&Inventory {
                packages: vec![
                    package(PackageKind::Formula, "big", 9_000),
                    package(PackageKind::Formula, "small", 3_000),
                    package(PackageKind::Cask, "ghostty", 5_000),
                ],
                brew: Some("/fake/bin/brew".into()),
                ..Inventory::default()
            });
            focus_on(&mut app, "small");
            let Screen::Select(state) = &mut app.screen else {
                panic!("expected the select screen");
            };
            for item in state.scan.groups.iter_mut().flat_map(|g| &mut g.items) {
                item.selected = item.path.to_string_lossy().starts_with("small ");
            }
            state.overlay = ModalOverlay::Outcome(RemovalReport {
                removed_count: 1,
                ..RemovalReport::default()
            });

            state.close_homebrew_outcome();

            let rows = state.flattened_rows();
            assert!(
                rows[state.focused_index].display_path.starts_with("big "),
                "{}",
                rows[state.focused_index].display_path
            );
        }

        #[test]
        fn the_search_and_the_focus_survive_the_update() {
            let mut run = Removal::open(
                &[("go", &[]), ("fd3", &[]), ("fd2", &[]), ("fd1", &[])],
                false,
            );
            let Screen::Select(state) = &mut run.app.screen else {
                panic!("expected the select screen");
            };
            state.search_query = "fd".to_string();

            run.remove("fd2");
            let focused = select(&run.app).focused_index;
            run.close_outcome();

            let state = select(&run.app);
            assert_eq!(state.search_query, "fd");
            assert_eq!(state.focused_index, focused);
            let rows = state.flattened_rows();
            assert!(rows[state.focused_index].display_path.starts_with("fd3 "));
            assert!(!rows.iter().any(|row| row.display_path.starts_with("go ")));

            run.remove("fd3");
            run.close_outcome();

            let state = select(&run.app);
            let rows = state.flattened_rows();
            assert!(rows[state.focused_index].is_item());
            assert!(rows[state.focused_index].display_path.starts_with("fd1 "));
        }

        const BUN_OUTDATED: &str = homebrew::testing::BREW_OUTDATED_BUN;

        /// The Homebrew list on a Mac whose brew lists `before`, and `after` once an upgrade ran.
        /// `brew outdated` says `says[0]` for the list, `says[1]` for the check, and `says[2]`
        /// afterwards.
        struct Upgrades {
            _temp: tempfile::TempDir,
            app: App,
            host: Arc<FakeHost>,
            brew: String,
        }

        impl Upgrades {
            fn open(before: &str, after: &str, says: [&str; 3]) -> Self {
                Self::open_failing(before, after, says, &[])
            }

            /// Like `open`, and `brew upgrade` fails for the packages in `failing`.
            fn open_failing(
                before: &str,
                after: &str,
                says: [&str; 3],
                failing: &[(&str, &str)],
            ) -> Self {
                let (temp, layout) = homebrew::testing::with_brew("opt/homebrew");
                let brew = layout.system("opt/homebrew/bin/brew").display().to_string();
                let host = Arc::new(FakeHost::default());
                let read = |args: &str| format!("{BREW_START} {brew} {args}");
                host.reply(&read("info --json=v2 --installed"), before);
                host.reply(&read("info --json=v2 --installed"), after);
                for said in says {
                    host.reply(&read("outdated --json=v2"), said);
                }
                host.reply(&read("update"), "");
                let listed: serde_json::Value = serde_json::from_str(before).unwrap();
                for (flag, key, field) in
                    [("formula", "formulae", "name"), ("cask", "casks", "token")]
                {
                    for name in listed[key]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter_map(|p| p[field].as_str())
                    {
                        let line = format!(
                            "{BREW_START} HOMEBREW_NO_AUTOREMOVE=1 {brew} upgrade --{flag} {name}"
                        );
                        match failing.iter().find(|(failing, _)| *failing == name) {
                            Some((_, error)) => host.fail(&line, error),
                            None => host.reply(&line, ""),
                        }
                    }
                }
                for text in [before, after] {
                    let listed: serde_json::Value = serde_json::from_str(text).unwrap();
                    let names = listed["formulae"].as_array().unwrap().iter();
                    for name in names.filter_map(|f| f["name"].as_str()) {
                        let keg = layout.system(&format!("opt/homebrew/Cellar/{name}"));
                        std::fs::create_dir_all(&keg).unwrap();
                        std::fs::write(keg.join("data"), vec![1_u8; 20_000]).unwrap();
                    }
                }
                let mut app = new_app();
                app.layout = layout;
                app.host = host.clone();
                open_the_list(&mut app);
                finish_the_check(&mut app);
                Self {
                    _temp: temp,
                    app,
                    host,
                    brew,
                }
            }

            fn upgrade_line(&self, args: &str) -> String {
                format!(
                    "{BREW_START} HOMEBREW_NO_AUTOREMOVE=1 {} upgrade {args}",
                    self.brew
                )
            }

            fn upgrades_run(&self) -> Vec<String> {
                let mut lines: Vec<String> = self
                    .host
                    .calls
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|call| call.contains(" upgrade "))
                    .cloned()
                    .collect();
                lines.sort();
                lines
            }

            fn wait_for_the_upgrades(&mut self) {
                wait_for_removal(&mut self.app);
            }

            fn wait_for_the_list(&mut self) {
                for _ in 0..400 {
                    self.app.poll_brew_refresh();
                    if self.app.brew_refresh.is_none() {
                        return;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                panic!("the list was not read again");
            }

            /// `y` on the question, then waits for the outcome.
            fn confirm(&mut self) {
                self.app.handle_key(KeyCode::Char('y'));
                self.wait_for_the_upgrades();
            }

            fn close_outcome(&mut self) {
                self.app.handle_key(KeyCode::Enter);
                self.wait_for_the_list();
            }

            fn calls_of(&self, command: &str) -> usize {
                ran_times(&self.host, &self.brew, command)
            }

            /// Waits until brew `command` has started `times` times.
            fn wait_for_calls(&self, command: &str, times: usize) {
                for _ in 0..400 {
                    if self.calls_of(command) >= times {
                        return;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                panic!("brew {command} did not start {times} times");
            }

            fn info_line(&self) -> String {
                format!("{BREW_START} {} info --json=v2 --installed", self.brew)
            }

            fn uninstalls_run(&self) -> usize {
                let calls = self.host.calls.lock().unwrap();
                calls
                    .iter()
                    .filter(|call| call.contains(" uninstall "))
                    .count()
            }

            /// Upgrades bun and closes the outcome, while the read after it waits for the
            /// returned sender to be dropped.
            fn upgrade_bun_and_hold_the_read(&mut self) -> mpsc::Sender<()> {
                let release = hold(&self.host, &self.info_line());
                focus_on(&mut self.app, "bun");
                self.app.handle_key(KeyCode::Char('u'));
                self.confirm();
                self.app.handle_key(KeyCode::Enter);
                release
            }
        }

        fn bun_and_zed() -> (String, String) {
            (
                info(
                    &[formula("bun", "1.4.2", false), formula("zed", "2", false)],
                    &[],
                ),
                info(
                    &[formula("bun", "1.4.4_1", false), formula("zed", "2", false)],
                    &[],
                ),
            )
        }

        #[test]
        fn u_on_an_outdated_formula_asks_and_upgrades_it() {
            let (before, after) = bun_and_zed();
            let mut run =
                Upgrades::open(&before, &after, [BUN_OUTDATED, BUN_OUTDATED, NOTHING_NEW]);
            focus_on(&mut run.app, "bun");

            run.app.handle_key(KeyCode::Char('u'));

            assert!(matches!(select(&run.app).overlay, ModalOverlay::Confirm(_)));
            let text = screen_text(&run.app);
            assert!(text.contains("Upgrade bun?"), "{text}");
            assert!(text.contains("bun 1.4.2 → 1.4.4_1"), "{text}");
            let words = screen_words(100, 30, |frame| run.app.render_screen(frame));
            assert!(
                words.contains("Homebrew may also upgrade what these need or what needs them."),
                "{words}"
            );
            assert!(!text.contains("Trash"), "{text}");
            assert!(!text.contains("delete"), "{text}");
            assert!(run.upgrades_run().is_empty());

            run.confirm();

            let text = screen_text(&run.app);
            assert!(text.contains("Upgrade Complete"), "{text}");
            assert!(text.contains("Upgraded 1 package."), "{text}");
            assert_eq!(run.upgrades_run(), [run.upgrade_line("--formula bun")]);
            let history = HistoryLog::new(run.app.layout.history_path());
            assert!(history.load_runs().unwrap().is_empty());
            assert!(row_labels(&run.app).contains(&"bun 1.4.2".to_string()));
            assert!(row_on_screen(&run.app, "bun 1.4.2").contains("↑ 1.4.4_1"));

            run.close_outcome();

            let labels = row_labels(&run.app);
            assert!(labels.contains(&"bun 1.4.4_1".to_string()), "{labels:?}");
            assert!(!row_on_screen(&run.app, "bun 1.4.4_1").contains('↑'));
            assert!(!screen_text(&run.app).contains("1 update"));
            assert_eq!(run.calls_of("update"), 1);
            assert_eq!(run.calls_of("info"), 2);
            assert_eq!(run.calls_of("outdated"), 3);
        }

        #[test]
        fn the_upgrade_shows_its_progress() {
            let (before, after) = bun_and_zed();
            let mut run =
                Upgrades::open(&before, &after, [BUN_OUTDATED, BUN_OUTDATED, NOTHING_NEW]);
            let release = hold(&run.host, &run.upgrade_line("--formula bun"));
            focus_on(&mut run.app, "bun");
            press(&mut run.app, &[KeyCode::Char('u'), KeyCode::Char('y')]);

            let text = screen_text(&run.app);
            assert!(text.contains("Upgrading 1 package…"), "{text}");
            assert!(text.contains("1 of 1"), "{text}");
            drop(release);
            run.wait_for_the_upgrades();
        }

        #[test]
        fn a_cancelled_upgrade_does_not_change_the_next_question() {
            let (before, after) = bun_and_zed();
            let mut run =
                Upgrades::open(&before, &after, [BUN_OUTDATED, BUN_OUTDATED, NOTHING_NEW]);
            focus_on(&mut run.app, "bun");
            press(&mut run.app, &[KeyCode::Char('u'), KeyCode::Esc]);
            assert!(matches!(select(&run.app).overlay, ModalOverlay::None));

            focus_on(&mut run.app, "zed");
            run.app.handle_key(KeyCode::Char('d'));

            let text = screen_text(&run.app);
            assert!(text.contains("Uninstall zed?"), "{text}");
            assert!(!text.contains("Upgrade"), "{text}");
            assert!(run.upgrades_run().is_empty());
        }

        #[test]
        fn u_on_an_up_to_date_or_pinned_row_shows_why_and_runs_nothing() {
            let mut pinned = package(PackageKind::Formula, "pin", 3_000);
            pinned.pinned = true;
            pinned.latest_version = Some("2".to_string());
            let mut cask = package(PackageKind::Cask, "ghostty", 9_000);
            cask.pinned = true;
            cask.latest_version = Some("2".to_string());
            let mut app = brew_list_app(&Inventory {
                packages: vec![package(PackageKind::Formula, "fd", 5_000), pinned, cask],
                brew: Some("/fake/bin/brew".into()),
                ..Inventory::default()
            });
            let host = Arc::new(FakeHost::default());
            app.host = host.clone();

            for (name, text) in [
                ("fd", "fd is up to date."),
                ("pin", "pin is pinned. Run brew unpin pin first."),
                (
                    "ghostty",
                    "ghostty is pinned. Run brew unpin ghostty first.",
                ),
            ] {
                focus_on(&mut app, name);

                app.handle_key(KeyCode::Char('u'));

                let state = select(&app);
                let toast = state.toast.as_ref().expect(name);
                assert_eq!(toast.text, text);
                assert_eq!(toast.kind, ToastKind::Error);
                assert!(matches!(state.overlay, ModalOverlay::None), "{name}");
                assert!(screen_text(&app).contains(text), "{name}");
            }
            assert!(host.calls.lock().unwrap().is_empty());
            assert!(app.removal_rx.is_none());
        }

        fn mixed_mac() -> Upgrades {
            let before = info(
                &[
                    formula("bun", "1.4.2", false),
                    formula("zed", "2", false),
                    formula("pin", "1", true),
                    formula("lib", "1", false),
                ],
                &[("ghostty", "1.0")],
            );
            let after = info(
                &[
                    formula("bun", "1.4.4_1", false),
                    formula("zed", "3", false),
                    formula("pin", "1", true),
                    formula("lib", "1", false),
                ],
                &[("ghostty", "1.1")],
            );
            let said = outdated(
                &[("bun", "1.4.4_1"), ("zed", "3"), ("pin", "2")],
                &[("ghostty", "1.1")],
            );
            Upgrades::open(&before, &after, [&said, &said, NOTHING_NEW])
        }

        #[test]
        fn shift_u_upgrades_every_outdated_package_but_a_pinned_one() {
            let mut run = mixed_mac();

            run.app.handle_key(KeyCode::Char('U'));

            let text = screen_text(&run.app);
            assert!(text.contains("Upgrade 3 packages?"), "{text}");
            for line in ["bun 1.4.2 → 1.4.4_1", "zed 2 → 3", "ghostty 1.0 → 1.1"] {
                assert!(text.contains(line), "{line} {text}");
            }
            assert!(!text.contains("pin 1 → 2"), "{text}");
            assert!(run.upgrades_run().is_empty());

            run.confirm();

            let text = screen_text(&run.app);
            assert!(text.contains("Upgraded 3 packages."), "{text}");
            assert_eq!(
                run.upgrades_run(),
                [
                    run.upgrade_line("--cask ghostty"),
                    run.upgrade_line("--formula bun"),
                    run.upgrade_line("--formula zed"),
                ]
            );
        }

        #[test]
        fn shift_u_with_nothing_to_upgrade_says_so() {
            let (before, _) = bun_and_zed();
            let mut run = Upgrades::open(&before, &before, [NOTHING_NEW; 3]);

            run.app.handle_key(KeyCode::Char('U'));

            let state = select(&run.app);
            let toast = state.toast.as_ref().expect("a toast");
            assert_eq!(toast.text, "Everything is up to date.");
            assert!(matches!(state.overlay, ModalOverlay::None));
            assert!(run.upgrades_run().is_empty());
        }

        #[test]
        fn u_and_shift_u_work_on_search_results_and_are_text_while_typing() {
            let mut run = mixed_mac();
            press(
                &mut run.app,
                &[KeyCode::Char('/'), KeyCode::Char('u'), KeyCode::Char('U')],
            );
            let state = select(&run.app);
            assert_eq!(state.search_query, "uU");
            assert!(matches!(state.overlay, ModalOverlay::None));

            press(
                &mut run.app,
                &[
                    KeyCode::Esc,
                    KeyCode::Char('/'),
                    KeyCode::Char('z'),
                    KeyCode::Enter,
                ],
            );
            assert_eq!(select(&run.app).search_query, "z");
            focus_on(&mut run.app, "zed");
            run.app.handle_key(KeyCode::Char('u'));
            let text = screen_text(&run.app);
            assert!(text.contains("Upgrade zed?"), "{text}");
            assert!(text.contains("zed 2 → 3"), "{text}");
            run.app.handle_key(KeyCode::Esc);

            run.app.handle_key(KeyCode::Char('U'));
            let text = screen_text(&run.app);
            assert!(text.contains("Upgrade 3 packages?"), "{text}");
        }

        #[test]
        fn a_long_question_lists_what_fits_and_counts_the_rest() {
            let names: Vec<String> = (0..30).map(|i| format!("pkg{i:02}")).collect();
            let formulae: Vec<_> = names.iter().map(|name| formula(name, "1", false)).collect();
            let said: Vec<_> = names.iter().map(|name| (name.as_str(), "2")).collect();
            let said = outdated(&said, &[]);
            let before = info(&formulae, &[]);
            let mut run = Upgrades::open(&before, &before, [&said, &said, NOTHING_NEW]);

            run.app.handle_key(KeyCode::Char('U'));

            let text = screen_text(&run.app);
            assert!(text.contains("Upgrade 30 packages?"), "{text}");
            assert!(text.contains("and 14 more"), "{text}");
            assert!(text.contains("Homebrew may also upgrade"), "{text}");
        }

        #[test]
        fn after_an_upgrade_only_changed_packages_are_measured_again() {
            let before = info(
                &[
                    formula("bun", "1.4.2", false),
                    formula("zed", "2", false),
                    formula("old", "1", false),
                ],
                &[],
            );
            let after = info(
                &[
                    formula("bun", "1.4.4_1", false),
                    formula("zed", "2", false),
                    formula("fresh", "1", false),
                ],
                &[],
            );
            let mut run =
                Upgrades::open(&before, &after, [BUN_OUTDATED, BUN_OUTDATED, NOTHING_NEW]);
            let cellar = run.app.layout.system("opt/homebrew/Cellar");
            let zed_before = size_of(&run.app, "zed");
            let bun_before = size_of(&run.app, "bun");
            let grow = |name: &str| {
                let folder = cellar.join(name);
                move || std::fs::write(folder.join("more"), vec![1_u8; 80_000]).unwrap()
            };
            run.host
                .when(&run.upgrade_line("--formula bun"), grow("bun"));
            run.host
                .when(&run.upgrade_line("--formula bun"), grow("zed"));
            run.host.when(&run.upgrade_line("--formula bun"), {
                let folder = cellar.join("fresh");
                move || {
                    std::fs::create_dir_all(&folder).unwrap();
                    std::fs::write(folder.join("data"), vec![1_u8; 40_000]).unwrap();
                }
            });
            focus_on(&mut run.app, "bun");
            run.app.handle_key(KeyCode::Char('u'));
            run.confirm();

            run.close_outcome();

            let labels = labels(&run.app);
            assert_eq!(labels.len(), 3, "{labels:?}");
            assert!(!labels.iter().any(|label| label.starts_with("old ")));
            assert_eq!(size_of(&run.app, "zed"), zed_before);
            let bun_now = size_of(&run.app, "bun");
            assert_eq!(bun_now, DirSizer::size_of(&cellar.join("bun")));
            assert!(bun_now > bun_before);
            assert_eq!(
                size_of(&run.app, "fresh"),
                DirSizer::size_of(&cellar.join("fresh"))
            );
            assert!(size_of(&run.app, "fresh") >= 40_000);
            assert_eq!(run.calls_of("info"), 2);
        }

        #[test]
        fn the_list_keeps_its_sort_search_and_focus_after_an_upgrade() {
            let (before, after) = bun_and_zed();
            let mut run =
                Upgrades::open(&before, &after, [BUN_OUTDATED, BUN_OUTDATED, NOTHING_NEW]);
            press(&mut run.app, &[KeyCode::Char('N')]);
            press(
                &mut run.app,
                &[KeyCode::Char('/'), KeyCode::Char('u'), KeyCode::Enter],
            );
            focus_on(&mut run.app, "bun");
            run.app.handle_key(KeyCode::Char('u'));
            run.confirm();

            run.close_outcome();

            let state = select(&run.app);
            assert_eq!(state.sort_order, Some(SortOrder::Name));
            assert_eq!(state.search_query, "u");
            let row = &state.flattened_rows()[state.focused_index];
            assert!(
                row.display_path.starts_with("bun 1.4.4_1"),
                "{}",
                row.display_path
            );
            assert!(!state.refreshing);
        }

        fn needing(mut formula: serde_json::Value, dependencies: &[&str]) -> serde_json::Value {
            formula["dependencies"] = serde_json::json!(dependencies);
            formula
        }

        #[test]
        fn u_on_a_nested_row_asks_to_upgrade_that_package() {
            let before = info(
                &[
                    needing(formula("app", "1", false), &["lib"]),
                    formula("lib", "1", false),
                ],
                &[],
            );
            let said = outdated(&[("lib", "2")], &[]);
            let mut run = Upgrades::open(&before, &before, [&said, &said, &said]);
            focus_on(&mut run.app, "app");
            run.app.handle_key(KeyCode::Enter);
            focus_path(&mut run.app, &["app", "lib"]);

            run.app.handle_key(KeyCode::Char('u'));

            let text = screen_text(&run.app);
            assert!(text.contains("Upgrade lib?"), "{text}");
            assert!(text.contains("lib 1 → 2"), "{text}");
            run.confirm();
            assert_eq!(run.upgrades_run(), [run.upgrade_line("--formula lib")]);
        }

        #[test]
        fn the_focus_and_the_open_rows_follow_the_package_after_an_upgrade() {
            let before = info(
                &[
                    formula("aaa", "1", false),
                    needing(formula("app", "1", false), &["lib"]),
                    formula("bun", "1.4.2", false),
                    formula("lib", "1", false),
                    formula("zed", "2", false),
                ],
                &[],
            );
            let after = info(
                &[
                    needing(formula("app", "1", false), &["lib"]),
                    formula("bun", "1.4.4_1", false),
                    formula("lib", "1", false),
                    formula("zed", "2", false),
                ],
                &[],
            );
            let mut run =
                Upgrades::open(&before, &after, [BUN_OUTDATED, BUN_OUTDATED, NOTHING_NEW]);
            press(&mut run.app, &[KeyCode::Char('N')]);
            focus_on(&mut run.app, "app");
            run.app.handle_key(KeyCode::Enter);
            focus_on(&mut run.app, "bun");
            let before_index = select(&run.app).focused_index;
            run.app.handle_key(KeyCode::Char('u'));
            run.confirm();

            run.close_outcome();

            let state = select(&run.app);
            assert!(state.focused_index < before_index);
            let rows = state.flattened_rows();
            assert!(
                rows[state.focused_index]
                    .display_path
                    .starts_with("bun 1.4.4_1")
            );
            assert!(
                state
                    .expanded
                    .iter()
                    .any(|path| path == &["app".to_string()])
            );
            let app_lib = ["app".to_string(), "lib".to_string()];
            assert!(
                rows.iter()
                    .any(|row| { row.tree.as_ref().is_some_and(|tree| tree.path == app_lib) })
            );
        }

        #[test]
        fn the_focus_stays_on_the_cask_when_a_formula_has_its_name() {
            let before = info(&[formula("docker", "1.0", false)], &[("docker", "1.0")]);
            // The new formula moves the cask row, so its old index points at a formula.
            let after = info(
                &[formula("docker", "1.0", false), formula("bat", "1", false)],
                &[("docker", "1.1")],
            );
            let said = outdated(&[], &[("docker", "1.1")]);
            let mut run = Upgrades::open(&before, &after, [&said, &said, NOTHING_NEW]);
            let cask_row = |app: &App| {
                let state = select(app);
                state
                    .flattened_rows()
                    .iter()
                    .position(|row| row.is_item() && row.group_name.starts_with("Casks"))
                    .expect("a cask row")
            };
            let at = cask_row(&run.app);
            let Screen::Select(state) = &mut run.app.screen else {
                panic!("expected the select screen");
            };
            state.focused_index = at;
            run.app.handle_key(KeyCode::Char('u'));
            run.confirm();

            run.close_outcome();

            let state = select(&run.app);
            assert_eq!(state.focused_index, cask_row(&run.app));
            let row = &state.flattened_rows()[state.focused_index];
            assert!(
                row.display_path.starts_with("docker 1.1"),
                "{}",
                row.display_path
            );
        }

        #[test]
        fn the_focus_stays_on_the_cask_when_the_upgrade_adds_a_group_above_it() {
            let dependency =
                |name: &str| serde_json::json!({"name": name, "installed": [{"version": "1"}]});
            let before = info(
                &[
                    needing(formula("app", "1", false), &["lib", "zlib"]),
                    dependency("lib"),
                    dependency("zlib"),
                ],
                &[("ghostty", "1.0")],
            );
            // The upgraded app no longer needs lib, so lib gets a group above the casks.
            let after = info(
                &[
                    needing(formula("app", "2", false), &["zlib"]),
                    dependency("lib"),
                    dependency("zlib"),
                ],
                &[("ghostty", "1.1")],
            );
            let said = outdated(&[("app", "2")], &[("ghostty", "1.1")]);
            let mut run = Upgrades::open(&before, &after, [&said, &said, NOTHING_NEW]);
            focus_on(&mut run.app, "ghostty");
            run.app.handle_key(KeyCode::Char('U'));
            run.confirm();

            run.close_outcome();

            let state = select(&run.app);
            let groups: Vec<&str> = state.scan.groups.iter().map(|g| g.tool.as_str()).collect();
            assert_eq!(
                groups,
                [
                    "Formulae",
                    "Not needed by anything you installed",
                    "Needed by what you installed",
                    "Casks · Remove with taert uninstall"
                ]
            );
            let row = &state.flattened_rows()[state.focused_index];
            assert!(
                row.display_path.starts_with("ghostty 1.1"),
                "{}",
                row.display_path
            );
        }

        #[test]
        fn the_list_says_it_is_updating_while_brew_is_read_again() {
            let (before, after) = bun_and_zed();
            let mut run =
                Upgrades::open(&before, &after, [BUN_OUTDATED, BUN_OUTDATED, NOTHING_NEW]);
            let info_line = run.info_line();
            let release = hold(&run.host, &info_line);
            focus_on(&mut run.app, "bun");
            press(&mut run.app, &[KeyCode::Char('u'), KeyCode::Char('y')]);
            run.wait_for_the_upgrades();

            run.app.handle_key(KeyCode::Enter);

            assert!(screen_text(&run.app).contains("Updating the list…"));
            assert!(select(&run.app).refreshing);
            drop(release);
            run.wait_for_the_list();
            assert!(!screen_text(&run.app).contains("Updating the list…"));
        }

        #[test]
        fn a_failed_refresh_shows_the_error_and_keeps_the_rows() {
            let (before, after) = bun_and_zed();
            let mut run =
                Upgrades::open(&before, &after, [BUN_OUTDATED, BUN_OUTDATED, NOTHING_NEW]);
            let info_line = run.info_line();
            run.host
                .replies
                .lock()
                .unwrap()
                .retain(|(command, _)| command != &info_line);
            run.host.fail(&info_line, "Error: boom");
            focus_on(&mut run.app, "bun");
            run.app.handle_key(KeyCode::Char('u'));
            run.confirm();

            run.close_outcome();

            let state = select(&run.app);
            let toast = state.toast.as_ref().expect("a toast");
            assert!(
                toast.text.starts_with("Could not update the list:"),
                "{}",
                toast.text
            );
            assert_eq!(toast.kind, ToastKind::Error);
            assert!(!state.refreshing);
            assert!(
                row_labels(&run.app)
                    .iter()
                    .any(|label| label.starts_with("bun 1.4.2"))
            );
            focus_on(&mut run.app, "bun");
            run.app.handle_key(KeyCode::Char('u'));
            assert!(screen_text(&run.app).contains("Upgrade bun?"));
        }

        #[test]
        fn a_read_that_stops_without_an_answer_says_so() {
            let mut app = brew_app();
            let (tx, rx) = mpsc::channel();
            drop(tx);
            app.brew_refresh = Some(rx);
            let Screen::Select(state) = &mut app.screen else {
                panic!("expected the select screen");
            };
            state.refreshing = true;

            app.poll_brew_refresh();

            let state = select(&app);
            let toast = state.toast.as_ref().expect("a toast");
            assert_eq!(toast.text, "Could not update the list: the read stopped");
            assert_eq!(toast.kind, ToastKind::Error);
            assert!(!state.refreshing);
            assert!(app.brew_refresh.is_none());
        }

        #[test]
        fn a_refresh_waits_while_a_question_is_open() {
            let (before, after) = bun_and_zed();
            let mut run =
                Upgrades::open(&before, &after, [BUN_OUTDATED, BUN_OUTDATED, NOTHING_NEW]);
            let old = select(&run.app).homebrew.clone().expect("the list");
            let read = homebrew::refresh(&*run.host, &old);
            let (tx, rx) = mpsc::channel();
            tx.send(read).unwrap();
            run.app.brew_refresh = Some(rx);
            let Screen::Select(state) = &mut run.app.screen else {
                panic!("expected the select screen");
            };
            state.refreshing = true;
            focus_on(&mut run.app, "zed");
            run.app.handle_key(KeyCode::Char('d'));

            run.app.poll_brew_refresh();

            assert!(matches!(select(&run.app).overlay, ModalOverlay::Confirm(_)));
            assert!(run.app.brew_refresh.is_some());
            assert!(select(&run.app).refreshing);
            run.app.handle_key(KeyCode::Esc);
            run.app.poll_brew_refresh();
            assert!(run.app.brew_refresh.is_none());
            assert!(!select(&run.app).refreshing);
            let labels = row_labels(&run.app);
            assert!(
                labels.iter().any(|label| label.starts_with("bun 1.4.4_1")),
                "{labels:?}"
            );
        }

        #[test]
        fn an_uninstall_during_the_refresh_is_not_undone_by_it() {
            let (before, after) = bun_and_zed();
            let mut run =
                Upgrades::open(&before, &after, [BUN_OUTDATED, BUN_OUTDATED, NOTHING_NEW]);
            // The uninstall checks that brew still lists zed; the read after it does not.
            run.host.reply(&run.info_line(), &after);
            run.host.reply(
                &run.info_line(),
                &info(&[formula("bun", "1.4.4_1", false)], &[]),
            );
            let keg = run.app.layout.system("opt/homebrew/Cellar/zed/2");
            std::fs::create_dir_all(&keg).unwrap();
            let uninstall = format!(
                "{BREW_START} HOMEBREW_NO_AUTOREMOVE=1 {} uninstall zed",
                run.brew
            );
            run.host.reply(&uninstall, "");
            run.host
                .when(&uninstall, move || std::fs::remove_dir_all(&keg).unwrap());
            let outdated_line = format!("{BREW_START} {} outdated --json=v2", run.brew);
            let release = hold(&run.host, &outdated_line);
            focus_on(&mut run.app, "bun");
            press(&mut run.app, &[KeyCode::Char('u'), KeyCode::Char('y')]);
            run.wait_for_the_upgrades();
            run.app.handle_key(KeyCode::Enter);
            // The refresh has read brew info and waits on brew outdated.
            run.wait_for_calls("outdated", 3);
            focus_on(&mut run.app, "zed");
            press(&mut run.app, &[KeyCode::Char('d'), KeyCode::Char('y')]);
            wait_for_removal(&mut run.app);
            assert!(run.host.ran(&uninstall));
            let text = screen_text(&run.app);
            assert!(text.contains("Uninstall Complete"), "{text}");
            drop(release);

            run.app.handle_key(KeyCode::Enter);
            run.wait_for_the_list();

            let labels = row_labels(&run.app);
            assert!(
                !labels.iter().any(|label| label.starts_with("zed ")),
                "{labels:?}"
            );
            assert!(select(&run.app).homebrew.as_ref().is_some_and(|list| {
                !list.packages.iter().any(|package| package.name == "zed")
            }));
        }

        #[test]
        fn a_search_being_typed_goes_on_when_the_list_is_read_again() {
            let (before, after) = bun_and_zed();
            let mut run =
                Upgrades::open(&before, &after, [BUN_OUTDATED, BUN_OUTDATED, NOTHING_NEW]);
            let release = run.upgrade_bun_and_hold_the_read();
            press(&mut run.app, &[KeyCode::Char('/'), KeyCode::Char('z')]);
            drop(release);
            run.wait_for_the_list();

            press(&mut run.app, &[KeyCode::Char('u'), KeyCode::Char('d')]);

            let state = select(&run.app);
            assert_eq!(state.search_query, "zud");
            assert!(matches!(state.overlay, ModalOverlay::None));
            assert_eq!(run.upgrades_run(), [run.upgrade_line("--formula bun")]);
            assert_eq!(run.uninstalls_run(), 0);
        }

        #[test]
        fn a_search_being_typed_keeps_its_text_when_the_list_loses_what_it_matched() {
            let (before, _) = bun_and_zed();
            let after = info(&[formula("bun", "1.4.4_1", false)], &[]);
            let mut run =
                Upgrades::open(&before, &after, [BUN_OUTDATED, BUN_OUTDATED, NOTHING_NEW]);
            let release = run.upgrade_bun_and_hold_the_read();
            press(&mut run.app, &[KeyCode::Char('/'), KeyCode::Char('z')]);
            drop(release);

            run.wait_for_the_list();

            let state = select(&run.app);
            assert!(state.is_searching);
            assert_eq!(state.search_query, "z");
        }

        #[test]
        fn a_toast_stays_when_the_list_is_read_again() {
            let before = info(
                &[formula("bun", "1.4.2", false), formula("pin", "1", true)],
                &[],
            );
            let after = info(
                &[formula("bun", "1.4.4_1", false), formula("pin", "1", true)],
                &[],
            );
            let mut run =
                Upgrades::open(&before, &after, [BUN_OUTDATED, BUN_OUTDATED, NOTHING_NEW]);
            let release = run.upgrade_bun_and_hold_the_read();
            focus_on(&mut run.app, "pin");
            run.app.handle_key(KeyCode::Char('d'));
            drop(release);

            run.wait_for_the_list();

            let state = select(&run.app);
            assert!(!state.refreshing);
            let toast = state.toast.as_ref().expect("a toast");
            assert_eq!(toast.text, "pin is pinned. Run brew unpin pin first.");
        }

        #[test]
        fn u_and_shift_u_wait_while_the_list_is_read_again() {
            let (before, after) = bun_and_zed();
            let mut run =
                Upgrades::open(&before, &after, [BUN_OUTDATED, BUN_OUTDATED, NOTHING_NEW]);
            let release = run.upgrade_bun_and_hold_the_read();
            focus_on(&mut run.app, "bun");

            for key in ['u', 'U'] {
                run.app.handle_key(KeyCode::Char(key));

                let state = select(&run.app);
                let toast = state.toast.as_ref().expect("a toast");
                assert_eq!(toast.text, "Updating the list. Try again when it ends.");
                assert_eq!(toast.kind, ToastKind::Info);
                assert!(matches!(state.overlay, ModalOverlay::None), "{key}");
            }
            assert_eq!(run.upgrades_run(), [run.upgrade_line("--formula bun")]);
            drop(release);
            run.wait_for_the_list();
        }

        #[test]
        fn u_and_shift_u_wait_for_the_update_check() {
            let (_temp, mut app, host, _brew) =
                check_mac_with(CHECK_INFO, BUN_OUTDATED, Some(BUN_OUTDATED), None);
            open_the_list(&mut app);
            assert!(select(&app).checking_updates);
            focus_on(&mut app, "bun");

            for key in ['u', 'U'] {
                app.handle_key(KeyCode::Char(key));

                let state = select(&app);
                let toast = state.toast.as_ref().expect("a toast");
                assert_eq!(toast.text, "Checking for updates. Try again when it ends.");
                assert_eq!(toast.kind, ToastKind::Info);
                assert!(matches!(state.overlay, ModalOverlay::None), "{key}");
            }
            let calls = host.calls.lock().unwrap().clone();
            assert!(!calls.iter().any(|call| call.contains(" upgrade ")));

            finish_the_check(&mut app);
            app.handle_key(KeyCode::Char('u'));
            assert!(screen_text(&app).contains("Upgrade bun?"));
        }

        #[test]
        fn a_failed_cask_upgrade_shows_the_terminal_hint() {
            let before = info(&[], &[("ghostty", "1.0")]);
            let said = outdated(&[], &[("ghostty", "1.1")]);
            let mut run = Upgrades::open_failing(
                &before,
                &before,
                [&said, &said, &said],
                &[("ghostty", "Error: sudo needed")],
            );
            focus_on(&mut run.app, "ghostty");
            run.app.handle_key(KeyCode::Char('u'));

            run.confirm();

            let text = screen_text(&run.app);
            assert!(text.contains("Nothing was upgraded"), "{text}");
            assert!(text.contains("1 item failed to upgrade:"), "{text}");
            assert!(text.contains("ghostty"), "{text}");
            assert!(text.contains("Error: sudo needed"), "{text}");
            let words = screen_words(100, 30, |frame| run.app.render_screen(frame));
            assert!(
                words.contains("run brew upgrade --cask ghostty in a terminal."),
                "{words}"
            );
        }

        #[test]
        fn the_homebrew_list_names_u_and_shift_u() {
            let mut run = mixed_mac();
            focus_on(&mut run.app, "bun");
            assert_footer(&run.app, &["u  Upgrade", "d  Uninstall"], &[]);
            focus_on(&mut run.app, "lib");
            assert_footer(&run.app, &["d  Uninstall"], &["u  Upgrade"]);
            assert_eq!(help_row(&run.app, "Upgrade"), Some("u"));
            assert_eq!(help_row(&run.app, "Upgrade all"), Some("⇧U"));
        }
    }

    mod removal_keys {
        use super::*;
        use crate::core::apps::testing::sandbox;
        use crate::core::startup::{StartupItem, StartupKind, StartupState};

        const REMOVAL_KEYS: [KeyCode; 4] = [
            KeyCode::Char('d'),
            KeyCode::Char('D'),
            KeyCode::Backspace,
            KeyCode::Delete,
        ];

        const REMOVING_LISTS: [&str; 6] = [
            "dev",
            docker::SCAN_ID,
            large_files::SCAN_ID,
            purger::SCAN_ID,
            crate::core::homebrew::CLEANUP_ID,
            crate::core::orphans::SCAN_ID,
        ];

        #[test]
        fn enter_does_not_open_the_removal_question() {
            for id in REMOVING_LISTS {
                let mut app = select_app(scan_with_id(id));
                app.handle_key(KeyCode::Enter);
                assert!(matches!(select(&app).overlay, ModalOverlay::None), "{id}");

                for key in REMOVAL_KEYS {
                    let mut app = select_app(scan_with_id(id));
                    app.handle_key(key);
                    assert!(
                        matches!(select(&app).overlay, ModalOverlay::Confirm(_)),
                        "{id} {key:?}"
                    );
                }
            }
        }

        #[test]
        fn d_and_shift_d_are_typed_into_a_search_and_backspace_deletes_a_letter() {
            let mut app = select_app(scan_with_id(docker::SCAN_ID));

            press(
                &mut app,
                &[KeyCode::Char('/'), KeyCode::Char('d'), KeyCode::Char('D')],
            );
            assert_eq!(select(&app).search_query, "dD");
            assert!(matches!(select(&app).overlay, ModalOverlay::None));

            app.handle_key(KeyCode::Backspace);
            assert_eq!(select(&app).search_query, "d");
            assert!(matches!(select(&app).overlay, ModalOverlay::None));
        }

        #[test]
        fn slim_and_compress_start_with_enter_and_ignore_d() {
            for id in [slim::SCAN_ID, crate::core::compress::SCAN_ID] {
                let mut app = select_app(scan_with_id(id));
                app.handle_key(KeyCode::Enter);
                assert!(
                    matches!(select(&app).overlay, ModalOverlay::Confirm(_)),
                    "{id}"
                );

                for key in REMOVAL_KEYS {
                    let mut app = select_app(scan_with_id(id));
                    app.handle_key(key);
                    assert!(
                        matches!(select(&app).overlay, ModalOverlay::None),
                        "{id} {key:?}"
                    );
                }
            }
        }

        #[test]
        fn slim_and_compress_keep_enter_in_the_footer_and_help() {
            let app = select_app(scan_with_id(slim::SCAN_ID));
            assert_footer(&app, &["Slim 1 app  ↵"], &[]);

            assert_eq!(help_row(&app, "Review & clean"), Some("↵"));

            let app = select_app(scan_with_id(crate::core::compress::SCAN_ID));
            assert_footer(&app, &["Review 1 items  ↵"], &["items  d"]);
            assert_eq!(help_row(&app, "Review & clean"), Some("↵"));
        }

        #[test]
        fn a_removing_list_names_d_in_its_footer_and_help() {
            let app = select_app(scan_with_id("dev"));

            assert_footer(&app, &["Review 1 items  d"], &["↵"]);
            assert_eq!(help_row(&app, "Review & clean"), Some("d"));
        }

        #[test]
        fn the_date_sort_is_shift_t_in_the_footer_and_help() {
            let app = select_app(scan_with_id(docker::SCAN_ID));
            assert_footer(&app, &["⇧S/N/T  Sort: Size"], &["⇧S/N/D"]);
            assert_eq!(help_row(&app, "Date created"), Some("⇧T"));
        }

        #[test]
        fn explore_shift_d_deletes_and_shift_t_sorts_by_date() {
            let dir = tempdir().unwrap();
            std::fs::write(dir.path().join("file.txt"), "x").unwrap();

            for key in REMOVAL_KEYS {
                let mut app = analyze_app(dir.path());
                app.handle_key(KeyCode::Char(' '));
                app.handle_key(key);
                assert!(
                    matches!(analyze(&app).overlay, AnalyzeOverlay::ConfirmDelete { .. }),
                    "{key:?}"
                );
                assert_eq!(analyze(&app).sort_order, SortOrder::Size, "{key:?}");
            }

            let mut app = analyze_app(dir.path());
            app.handle_key(KeyCode::Char('T'));
            assert_eq!(analyze(&app).sort_order, SortOrder::Date);
            assert!(matches!(analyze(&app).overlay, AnalyzeOverlay::None));
        }

        #[test]
        fn explore_names_d_in_its_footer_and_help() {
            let dir = tempdir().unwrap();
            std::fs::write(dir.path().join("file.txt"), "x").unwrap();
            let mut app = analyze_app(dir.path());

            assert_footer(&app, &["d  Delete", "⇧S/N/T  Sort: Size"], &["D  Delete"]);
            assert_eq!(help_row(&app, "Delete selected"), Some("d"));
            assert_eq!(help_row(&app, "Date created"), Some("⇧T"));

            app.handle_key(KeyCode::Char(' '));
            assert_footer(&app, &["Delete 1 items  d"], &[]);
        }

        fn startup_item(state: StartupState) -> StartupItem {
            StartupItem {
                label: "com.example.item".to_string(),
                name: "Item".to_string(),
                app_name: None,
                app_bundle: None,
                kind: StartupKind::UserAgent,
                state,
                program: None,
                plist_path: Some("/nowhere/com.example.item.plist".into()),
                pid: None,
                cpu_percent: None,
                memory_bytes: None,
                is_apple: false,
                is_security: false,
                is_ignored: false,
            }
        }

        fn startup_app(state: StartupState) -> App {
            let mut app = new_app();
            app.screen = Screen::Startup(StartupScreenState::new(vec![startup_item(state)], false));
            app
        }

        fn startup(app: &App) -> &StartupScreenState {
            match &app.screen {
                Screen::Startup(state) => state,
                _ => panic!("expected the startup screen"),
            }
        }

        #[test]
        fn a_broken_startup_item_is_removed_with_d_not_enter() {
            for key in REMOVAL_KEYS {
                let mut app = startup_app(StartupState::Broken);
                app.handle_key(key);
                assert_eq!(
                    startup(&app).confirm,
                    Some(StartupConfirm::RemoveBroken { index: 0 }),
                    "{key:?}"
                );
            }

            for key in [KeyCode::Enter, KeyCode::Char(' ')] {
                let mut app = startup_app(StartupState::Broken);
                app.handle_key(key);
                assert_eq!(startup(&app).confirm, None, "{key:?}");
                assert_eq!(startup(&app).message, None, "{key:?}");
            }
        }

        #[test]
        fn d_on_a_broken_item_taert_cannot_remove_says_why_and_asks_nothing() {
            let daemon = StartupItem {
                kind: StartupKind::Daemon,
                ..startup_item(StartupState::Broken)
            };
            let apple = StartupItem {
                is_apple: true,
                ..startup_item(StartupState::Broken)
            };
            for (item, reason) in [
                (
                    daemon,
                    "System-level broken plists cannot be removed without root privileges",
                ),
                (apple, "Apple system services cannot be modified"),
            ] {
                let mut app = new_app();
                app.screen = Screen::Startup(StartupScreenState::new(vec![item], false));
                app.handle_key(KeyCode::Char('d'));
                assert_eq!(startup(&app).confirm, None, "{reason}");
                assert_eq!(
                    startup(&app).message,
                    Some((reason.to_string(), true)),
                    "{reason}"
                );
            }
        }

        #[test]
        fn a_working_startup_item_still_toggles_with_enter_and_space_and_ignores_d() {
            for key in [KeyCode::Enter, KeyCode::Char(' ')] {
                let mut app = startup_app(StartupState::Enabled);
                app.handle_key(key);
                assert_eq!(
                    startup(&app).confirm,
                    Some(StartupConfirm::Toggle {
                        index: 0,
                        enable: false
                    }),
                    "{key:?}"
                );
            }

            for key in REMOVAL_KEYS {
                let mut app = startup_app(StartupState::Enabled);
                app.handle_key(key);
                assert_eq!(startup(&app).confirm, None, "{key:?}");
                assert_eq!(startup(&app).message, None, "{key:?}");
            }
        }

        #[test]
        fn startup_help_names_d_for_a_broken_item() {
            let app = startup_app(StartupState::Broken);

            assert_eq!(help_row(&app, "Remove broken item"), Some("d"));
            assert_footer(&app, &["d  Remove broken item"], &["Toggle"]);
            let app = startup_app(StartupState::Enabled);
            assert_footer(&app, &["Toggle"], &["Remove"]);
        }

        fn path_list_app() -> (tempfile::TempDir, App) {
            let (temp, layout) = sandbox();
            let mut app = new_app();
            app.layout = layout;
            app.ignore.roots.push("/nowhere/protected".to_string());
            app.screen = Screen::PathListEditor(PathListEditorState::new(
                PathListKind::ProtectedPaths,
                &app.ignore,
                &app.layout,
            ));
            (temp, app)
        }

        #[test]
        fn the_path_list_removes_with_delete_too() {
            for key in REMOVAL_KEYS {
                let (_temp, mut app) = path_list_app();
                app.handle_key(key);
                let Screen::PathListEditor(state) = &app.screen else {
                    panic!("expected the path list");
                };
                assert!(
                    matches!(state.mode, PathListMode::ConfirmRemove(0)),
                    "{key:?}"
                );
            }
        }

        #[test]
        fn the_path_list_names_d_in_its_footer_and_help() {
            let (_temp, app) = path_list_app();

            assert_footer(&app, &["d  Remove"], &["D  Remove"]);
            assert_eq!(help_row(&app, "Remove path"), Some("d"));
        }
    }
}

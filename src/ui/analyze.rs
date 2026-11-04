//! Full-featured Read-Only Explore Storage Browser.
//!
//! Features:
//! - Multi-level breadcrumb navigation stack with remembered scroll & focus offsets.
//! - 'O' key to reveal item in Finder (`/usr/bin/open -R`).
//! - Gauge proportion relative to current folder's total allocated size.
//! - Accurate folder `/` and package ` [package]` suffixes.
//! - Live measuring background progress dialog with cancellation.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::components::button::Button;
use super::components::chrome::AppChrome;
use super::components::frame::{FRAME_ROWS, FRAME_SIDES, card};
use super::components::gauge::bar;
use super::components::hints::{Hint, PrimaryAction};
use super::components::list::{ColumnTitles, ListRow};
use super::components::modal::Modal;
use super::components::progress::ProgressView;
use super::components::toast::Toast;
use super::folder_preview::{FolderPreview, Job, preview_lines};
use super::theme::Theme;
use crate::core::bytes::format_bytes;
pub use crate::core::cache::{CachedSnapshot, ChildItem, FolderSnapshot, StorageCache};
use crate::core::cleaner::{RemovalProgress, RemovalReport};
use crate::core::homebrew::Package;
use crate::core::path_utils::prettify_path;
use crate::core::safety::Safety;
use crate::core::scanner::ScanItem;
use crate::core::sizer::DirSizer;
use crate::core::stores::IgnoreStore;

// Below this the panel would squeeze the list or itself.
const PREVIEW_MIN_WIDTH: u16 = 150;
const LIST_WIDTH_WITH_PREVIEW: u16 = 104;
// "2026-10-30 11:01".
const DATE_W: usize = 16;
const PREVIEW_GAP: u16 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalyzeConfirmButton {
    Cancel,
    DeletePermanently,
    MoveToTrash,
}

#[derive(Debug, Clone)]
pub enum AnalyzeOverlay {
    None,
    ConfirmDelete {
        items: Vec<ChildItem>,
        button: AnalyzeConfirmButton,
    },
    Deleting {
        progress: RemovalProgress,
        spinner_idx: usize,
    },
    Outcome(RemovalReport),
}

#[derive(Debug, Clone)]
pub struct BrowserLevel {
    pub snapshot: FolderSnapshot,
    pub selected: usize,
    pub offset: usize,
    pub stale: bool,
    /// The background update of this stale level ended without new sizes.
    pub update_failed: bool,
}

impl BrowserLevel {
    /// Stale, and its background update has not failed.
    fn wants_update(&self) -> bool {
        self.stale && !self.update_failed
    }
}

#[derive(Debug, Clone)]
pub struct MeasuringProgress {
    pub folder_path: PathBuf,
    pub position: usize,
    pub total: usize,
    pub current_name: String,
    pub current_subpath: Option<String>,
    pub scanned_files: usize,
    pub scanned_bytes: u64,
}

pub enum MeasureMessage {
    Progress(MeasuringProgress),
    Finished(FolderSnapshot),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortOrder {
    #[default]
    Size,
    Date,
    Name,
}

impl SortOrder {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Size => "Size",
            Self::Date => "Date",
            Self::Name => "Name",
        }
    }

    pub fn hint_label(&self) -> &'static str {
        match self {
            Self::Size => "Sort: Size",
            Self::Date => "Sort: Date",
            Self::Name => "Sort: Name",
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            Self::Size => "size (largest first)",
            Self::Date => "date created (newest first)",
            Self::Name => "name (A–Z)",
        }
    }

    pub fn sort_children<T: Sortable>(&self, children: &mut [T]) {
        let by_name = |a: &T, b: &T| a.sort_name().cmp(&b.sort_name());
        match self {
            Self::Size => {
                children.sort_by(|a, b| {
                    b.sort_bytes()
                        .cmp(&a.sort_bytes())
                        .then_with(|| by_name(a, b))
                });
            }
            Self::Date => {
                children.sort_by(|a, b| {
                    b.sort_date()
                        .cmp(&a.sort_date())
                        .then_with(|| b.sort_bytes().cmp(&a.sort_bytes()))
                        .then_with(|| by_name(a, b))
                });
            }
            Self::Name => {
                children.sort_by(|a, b| {
                    by_name(a, b).then_with(|| b.sort_bytes().cmp(&a.sort_bytes()))
                });
            }
        }
    }
}

/// What a list row offers to [`SortOrder`]. An item without a date sorts last.
pub trait Sortable {
    fn sort_bytes(&self) -> u64;
    fn sort_date(&self) -> Option<SystemTime>;
    fn sort_name(&self) -> String;
}

impl Sortable for ChildItem {
    fn sort_bytes(&self) -> u64 {
        self.size_bytes
    }

    fn sort_date(&self) -> Option<SystemTime> {
        self.created
    }

    fn sort_name(&self) -> String {
        self.name.to_lowercase()
    }
}

impl Sortable for ScanItem {
    fn sort_bytes(&self) -> u64 {
        self.size_bytes
    }

    fn sort_date(&self) -> Option<SystemTime> {
        self.modified
    }

    fn sort_name(&self) -> String {
        self.path.display().to_string().to_lowercase()
    }
}

impl Sortable for &Package {
    fn sort_bytes(&self) -> u64 {
        self.size_bytes
    }

    fn sort_date(&self) -> Option<SystemTime> {
        self.installed_at
    }

    fn sort_name(&self) -> String {
        self.name.to_lowercase()
    }
}

/// A scan that ends sooner opens its folder with no dialog.
const MEASURING_DIALOG_DELAY: Duration = Duration::from_millis(150);

pub struct AnalyzeState {
    pub root: PathBuf,
    pub levels: Vec<BrowserLevel>,
    pub ignore: IgnoreStore,
    pub measuring_rx: Option<Receiver<MeasureMessage>>,
    pub cancel_flag: Arc<AtomicBool>,
    pub current_progress: Option<MeasuringProgress>,
    pub spinner_idx: usize,
    pub status_notice: Option<String>,
    pub started_at: Instant,
    pub overlay: AnalyzeOverlay,
    pub cache: Arc<RwLock<StorageCache>>,
    pub sort_order: SortOrder,
    pub preview: FolderPreview,
    pub update: Option<Job>,
}

impl AnalyzeState {
    pub fn new(root: PathBuf, ignore: IgnoreStore) -> Self {
        Self::with_cache(root, ignore, Arc::new(RwLock::new(StorageCache::new())))
    }

    pub fn with_cache(
        root: PathBuf,
        ignore: IgnoreStore,
        cache: Arc<RwLock<StorageCache>>,
    ) -> Self {
        let mut state = Self {
            root: root.clone(),
            levels: Vec::new(),
            ignore,
            measuring_rx: None,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            current_progress: None,
            spinner_idx: 0,
            status_notice: None,
            started_at: Instant::now(),
            overlay: AnalyzeOverlay::None,
            cache,
            sort_order: SortOrder::default(),
            preview: FolderPreview::default(),
            update: None,
        };
        state.start_measuring(root);
        state
    }

    pub fn new_sync(root: PathBuf, ignore: IgnoreStore) -> Self {
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let cache = Arc::new(RwLock::new(StorageCache::new()));
        let mut snapshot =
            Self::scan_folder(&root, &ignore, None, cancel_flag.clone(), cache.clone());
        let sort_order = SortOrder::default();
        sort_order.sort_children(&mut snapshot.children);
        let initial_level = BrowserLevel {
            snapshot,
            selected: 0,
            offset: 0,
            stale: false,
            update_failed: false,
        };

        Self {
            root,
            levels: vec![initial_level],
            ignore,
            measuring_rx: None,
            cancel_flag,
            current_progress: None,
            spinner_idx: 0,
            status_notice: None,
            started_at: Instant::now(),
            overlay: AnalyzeOverlay::None,
            cache,
            sort_order,
            preview: FolderPreview::default(),
            update: None,
        }
    }

    pub fn toggle_current(&mut self) {
        if let Some(level) = self.current_level_mut()
            && let Some(child) = level.snapshot.children.get_mut(level.selected)
        {
            child.selected = !child.selected;
        }
    }

    pub fn select_all(&mut self) {
        if let Some(level) = self.current_level_mut() {
            for child in &mut level.snapshot.children {
                child.selected = true;
            }
        }
    }

    pub fn select_none(&mut self) {
        if let Some(level) = self.current_level_mut() {
            for child in &mut level.snapshot.children {
                child.selected = false;
            }
        }
    }

    pub fn set_sort(&mut self, sort_order: SortOrder) {
        self.sort_order = sort_order;
        self.status_notice = Some(format!("Sorted by {}", sort_order.description()));
        if let Some(level) = self.current_level_mut() {
            let selected_path = level
                .snapshot
                .children
                .get(level.selected)
                .map(|c| c.path.clone());
            sort_order.sort_children(&mut level.snapshot.children);
            if let Some(path) = selected_path
                && let Some(new_idx) = level.snapshot.children.iter().position(|c| c.path == path)
            {
                level.selected = new_idx;
            }
        }
    }

    pub fn items_to_delete(&self) -> Vec<ChildItem> {
        if let Some(level) = self.current_level() {
            let selected: Vec<ChildItem> = level
                .snapshot
                .children
                .iter()
                .filter(|c| c.selected)
                .cloned()
                .collect();
            if !selected.is_empty() {
                return selected;
            }
            if let Some(focused) = level.snapshot.children.get(level.selected) {
                return vec![focused.clone()];
            }
        }
        Vec::new()
    }

    pub fn open_delete_confirmation(&mut self, permanent: bool) {
        let items = self.items_to_delete();
        if !items.is_empty() {
            let button = if permanent {
                AnalyzeConfirmButton::DeletePermanently
            } else {
                AnalyzeConfirmButton::MoveToTrash
            };
            self.overlay = AnalyzeOverlay::ConfirmDelete { items, button };
        }
    }

    pub fn handle_removal_finished(&mut self, report: &RemovalReport) {
        if report.removed_count == 0 {
            return;
        }
        let Some(level) = self.current_level_mut() else {
            return;
        };
        level.snapshot.children.retain(|child| child.path.exists());
        level.snapshot.total_bytes = level.snapshot.children.iter().map(|c| c.size_bytes).sum();
        if level.selected >= level.snapshot.children.len() && !level.snapshot.children.is_empty() {
            level.selected = level.snapshot.children.len() - 1;
        }
        let stale = level.stale;
        let snapshot = level.snapshot.clone();
        let mut cache = self.cache.write().unwrap();
        cache.invalidate_prefix(&snapshot.path);
        if !stale {
            cache.put_snapshot(snapshot.path.clone(), snapshot);
        }
        drop(cache);
        self.preview = FolderPreview::default();
    }

    pub fn current_level(&self) -> Option<&BrowserLevel> {
        self.levels.last()
    }

    pub fn current_level_mut(&mut self) -> Option<&mut BrowserLevel> {
        self.levels.last_mut()
    }

    pub fn scan_folder(
        dir: &Path,
        ignore: &IgnoreStore,
        progress_tx: Option<mpsc::Sender<MeasureMessage>>,
        cancel: Arc<AtomicBool>,
        cache: Arc<RwLock<StorageCache>>,
    ) -> FolderSnapshot {
        let mut children = Vec::new();
        let mut inaccessible = 0;
        let mut skipped = 0;

        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => {
                return FolderSnapshot {
                    path: dir.to_path_buf(),
                    children: Vec::new(),
                    total_bytes: 0,
                    inaccessible_count: 1,
                    skipped_count: 0,
                };
            }
        };

        let mut raw_paths = Vec::new();
        for entry in entries.flatten() {
            raw_paths.push(entry.path());
        }
        raw_paths.sort();

        let total_count = raw_paths.len();
        let mut total_files_count = 0;
        let mut total_bytes_accum = 0u64;

        for (idx, p) in raw_paths.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                break;
            }

            let name = p
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();

            if let Some(ref tx) = progress_tx {
                let _ = tx.send(MeasureMessage::Progress(MeasuringProgress {
                    folder_path: dir.to_path_buf(),
                    position: idx + 1,
                    total: total_count,
                    current_name: name.clone(),
                    current_subpath: None,
                    scanned_files: total_files_count,
                    scanned_bytes: total_bytes_accum,
                }));
            }

            if Safety::has_symlink_component(p) || Safety::is_cloud_or_dataless(p) {
                skipped += 1;
                continue;
            }

            if Safety::is_ignored_or_protected(p, ignore) {
                skipped += 1;
                continue;
            }

            let is_dir = p.is_dir();
            let is_package = is_dir
                && (name.ends_with(".app")
                    || name.ends_with(".framework")
                    || name.ends_with(".bundle"));

            let size = if is_dir {
                let cached_size = cache.read().unwrap().get_size(p);
                if let Some(s) = cached_size {
                    total_bytes_accum += s;
                    total_files_count += 1;
                    if let Some(ref tx) = progress_tx {
                        let _ = tx.send(MeasureMessage::Progress(MeasuringProgress {
                            folder_path: dir.to_path_buf(),
                            position: idx + 1,
                            total: total_count,
                            current_name: name.clone(),
                            current_subpath: None,
                            scanned_files: total_files_count,
                            scanned_bytes: total_bytes_accum,
                        }));
                    }
                    s
                } else {
                    let tx_clone = progress_tx.clone();
                    let dir_buf = dir.to_path_buf();
                    let name_clone = name.clone();
                    let cancel_clone = cancel.clone();
                    let cache_clone = cache.clone();
                    let mut child_files = 0;
                    let meas = DirSizer::measure_with_progress_and_cache(
                        p,
                        Some(&cancel_clone),
                        Some(&cache_clone),
                        |subpath, bytes, files| {
                            child_files = files;
                            if let Some(ref tx) = tx_clone {
                                let _ = tx.send(MeasureMessage::Progress(MeasuringProgress {
                                    folder_path: dir_buf.clone(),
                                    position: idx + 1,
                                    total: total_count,
                                    current_name: name_clone.clone(),
                                    current_subpath: Some(subpath.display().to_string()),
                                    scanned_files: total_files_count + files,
                                    scanned_bytes: total_bytes_accum + bytes,
                                }));
                            }
                        },
                    );
                    total_files_count += child_files;
                    total_bytes_accum += meas.bytes;
                    meas.bytes
                }
            } else {
                let s = DirSizer::size_of(p);
                total_files_count += 1;
                total_bytes_accum += s;
                s
            };

            if size == 0 && !p.exists() {
                inaccessible += 1;
            }

            let created = std::fs::symlink_metadata(p)
                .or_else(|_| std::fs::metadata(p))
                .ok()
                .and_then(|m| m.created().or_else(|_| m.modified()).ok());

            children.push(ChildItem {
                name,
                path: p.clone(),
                size_bytes: size,
                is_dir,
                is_package,
                selected: false,
                created,
            });
        }

        // Rank largest first, then alphabetically
        children.sort_by(|a, b| {
            b.size_bytes
                .cmp(&a.size_bytes)
                .then_with(|| a.name.cmp(&b.name))
        });

        let total_bytes = children.iter().map(|c| c.size_bytes).sum();

        let snapshot = FolderSnapshot {
            path: dir.to_path_buf(),
            children,
            total_bytes,
            inaccessible_count: inaccessible,
            skipped_count: skipped,
        };

        if !cancel.load(Ordering::Relaxed) {
            cache
                .write()
                .unwrap()
                .put_snapshot(dir.to_path_buf(), snapshot.clone());
        }

        snapshot
    }

    pub fn next(&mut self) {
        if let Some(level) = self.current_level_mut()
            && level.selected + 1 < level.snapshot.children.len()
        {
            level.selected += 1;
        }
    }

    pub fn previous(&mut self) {
        if let Some(level) = self.current_level_mut()
            && level.selected > 0
        {
            level.selected -= 1;
        }
    }

    pub fn page_down(&mut self) {
        if let Some(level) = self.current_level_mut() {
            let count = level.snapshot.children.len();
            if count > 0 {
                level.selected = (level.selected + 10).min(count - 1);
            }
        }
    }

    pub fn page_up(&mut self) {
        if let Some(level) = self.current_level_mut() {
            level.selected = level.selected.saturating_sub(10);
        }
    }

    pub fn move_to_start(&mut self) {
        if let Some(level) = self.current_level_mut() {
            level.selected = 0;
        }
    }

    pub fn move_to_end(&mut self) {
        if let Some(level) = self.current_level_mut() {
            let count = level.snapshot.children.len();
            if count > 0 {
                level.selected = count - 1;
            }
        }
    }

    pub fn enter_selected(&mut self) {
        let child = match self.current_level() {
            Some(level) if level.selected < level.snapshot.children.len() => {
                level.snapshot.children[level.selected].clone()
            }
            _ => return,
        };

        if child.is_dir && !child.is_package {
            self.start_measuring(child.path);
        }
    }

    pub fn refresh_current(&mut self) {
        if let Some(level) = self.current_level() {
            let path = level.snapshot.path.clone();
            self.cache.write().unwrap().invalidate_prefix(&path);
            self.preview = FolderPreview::default();
            self.levels.pop();
            self.start_measuring(path);
        }
    }

    pub fn go_up(&mut self) -> bool {
        if self.levels.len() > 1 {
            self.levels.pop();
            self.cancel_update();
            true
        } else {
            false
        }
    }

    pub fn reveal_in_finder(&mut self) {
        if let Some(level) = self.current_level()
            && let Some(child) = level.snapshot.children.get(level.selected)
        {
            let _ = Command::new("open").arg("-R").arg(&child.path).spawn();
            self.status_notice = Some(format!("Revealed in Finder: {}", child.name));
        }
    }

    pub fn start_measuring(&mut self, path: PathBuf) {
        self.cancel_update();
        let cached = self.cache.read().unwrap().lookup_snapshot(&path);
        if let Some(cached) = cached {
            let (mut snapshot, stale) = match cached {
                CachedSnapshot::Fresh(snapshot) => (snapshot, false),
                CachedSnapshot::Stale(snapshot) => (snapshot, true),
            };
            self.sort_order.sort_children(&mut snapshot.children);
            self.levels.push(BrowserLevel {
                snapshot,
                selected: 0,
                offset: 0,
                stale,
                update_failed: false,
            });
            if !stale {
                self.measuring_rx = None;
                self.current_progress = None;
            }
            self.start_update_if_stale();
            return;
        }

        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel_flag = cancel.clone();
        let ignore = self.ignore.clone();
        let cache = self.cache.clone();

        let path_clone = path.clone();
        thread::spawn(move || {
            let snapshot = Self::scan_folder(&path_clone, &ignore, Some(tx.clone()), cancel, cache);
            let _ = tx.send(MeasureMessage::Finished(snapshot));
        });

        self.measuring_rx = Some(rx);
        self.started_at = Instant::now();
        self.current_progress = Some(MeasuringProgress {
            folder_path: path,
            position: 0,
            total: 0,
            current_name: "Reading directory…".to_string(),
            current_subpath: None,
            scanned_files: 0,
            scanned_bytes: 0,
        });
    }

    pub fn cancel_update(&mut self) {
        self.update = None;
    }

    fn start_update_if_stale(&mut self) {
        let Some(level) = self.levels.last().filter(|level| level.wants_update()) else {
            return;
        };
        if self.update.is_none()
            && self.measuring_rx.is_none()
            && !matches!(self.overlay, AnalyzeOverlay::Deleting { .. })
        {
            self.update = Some(Job::start(&level.snapshot.path, &self.ignore, &self.cache));
        }
    }

    fn swap_in_update(&mut self) {
        if !matches!(self.overlay, AnalyzeOverlay::None) {
            return;
        }
        let Some(job) = &self.update else {
            return;
        };
        let mut snapshot = match job.rx.try_recv() {
            Ok(snapshot) => snapshot,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                self.update = None;
                if let Some(level) = self.levels.last_mut() {
                    level.update_failed = true;
                }
                return;
            }
        };
        self.sort_order.sort_children(&mut snapshot.children);
        if let Some(level) = self.levels.last_mut() {
            let focused = level.snapshot.children.get(level.selected).map(|c| &c.path);
            let marked: HashSet<&PathBuf> = level
                .snapshot
                .children
                .iter()
                .filter(|c| c.selected)
                .map(|c| &c.path)
                .collect();
            for child in &mut snapshot.children {
                child.selected = marked.contains(&child.path);
            }
            level.selected = focused
                .and_then(|path| snapshot.children.iter().position(|c| &c.path == path))
                .unwrap_or_else(|| {
                    level
                        .selected
                        .min(snapshot.children.len().saturating_sub(1))
                });
            level.snapshot = snapshot;
            level.stale = false;
        }
        self.update = None;
    }

    pub fn cancel_measuring(&mut self) {
        self.cancel_flag.store(true, Ordering::Relaxed);
        self.measuring_rx = None;
        self.current_progress = None;
    }

    fn dialog_open(&self) -> bool {
        self.measuring_rx.is_some() || !matches!(self.overlay, AnalyzeOverlay::None)
    }

    fn preview_target(&self) -> Option<PathBuf> {
        if self.dialog_open() || !self.preview.visible.get() {
            return None;
        }

        let level = self.current_level()?;
        let child = level.snapshot.children.get(level.selected)?;
        (child.is_dir && !child.is_package).then(|| child.path.clone())
    }

    pub fn tick(&mut self) {
        self.spinner_idx = (self.spinner_idx + 1) % 4;

        if let Some(ref rx) = self.measuring_rx {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    MeasureMessage::Progress(prog) => {
                        self.current_progress = Some(prog);
                    }
                    MeasureMessage::Finished(mut snapshot) => {
                        self.sort_order.sort_children(&mut snapshot.children);
                        self.levels.push(BrowserLevel {
                            snapshot,
                            selected: 0,
                            offset: 0,
                            stale: false,
                            update_failed: false,
                        });
                        self.measuring_rx = None;
                        self.current_progress = None;
                        break;
                    }
                }
            }
        }

        self.swap_in_update();
        self.start_update_if_stale();

        let target = self.preview_target();
        self.preview
            .follow(target.as_deref(), Instant::now(), &self.ignore, &self.cache);
    }
}

pub fn render_analyze(frame: &mut Frame, state: &AnalyzeState, theme: &Theme) {
    if state.levels.is_empty() {
        let title_path = prettify_path(
            state
                .current_progress
                .as_ref()
                .map(|p| &p.folder_path)
                .unwrap_or(&state.root),
        );
        let chrome = AppChrome::new(vec!["taert", "Explore", &title_path])
            .full_width()
            .hints(vec![Hint::new(theme.g.key_esc, "Cancel")]);
        let content_rect = chrome.render(frame, theme);

        if let Some(ref prog) = state.current_progress {
            let spin_char = if theme.env.reduce_motion {
                "*"
            } else {
                match state.spinner_idx % 4 {
                    0 => "⠋",
                    1 => "⠙",
                    2 => "⠹",
                    _ => "⠸",
                }
            };
            let headline = format!("Exploring {title_path}");
            let step = if prog.total > 0 {
                format!(
                    "Checking {} ({}/{} items)",
                    prog.current_name, prog.position, prog.total
                )
            } else {
                format!("Scanning {}…", prog.current_name)
            };
            let fraction = if prog.total > 0 {
                Some(prog.position as f64 / prog.total as f64)
            } else {
                None
            };
            let counter = format!(
                "Scanned {} files · {}",
                prog.scanned_files,
                format_bytes(prog.scanned_bytes)
            );
            let elapsed = state.started_at.elapsed().as_secs();
            let pretty_sub = prog
                .current_subpath
                .as_deref()
                .map(|sub| prettify_path(Path::new(sub)));

            let mut view = ProgressView::new(spin_char, &headline)
                .step_info(&step)
                .fraction(fraction)
                .counter_info(&counter)
                .elapsed_seconds(elapsed);

            if let Some(ref sub) = pretty_sub {
                view = view.current_path(sub);
            }

            view.render(frame, content_rect, theme);
        }
        return;
    }

    let level = match state.current_level() {
        Some(l) => l,
        None => return,
    };

    let pretty_path = prettify_path(&level.snapshot.path);
    let mut crumbs = vec!["taert", "Explore"];
    let path_segments: Vec<&str> = pretty_path.split('/').filter(|s| !s.is_empty()).collect();
    for seg in path_segments {
        crumbs.push(seg);
    }

    let selected_items: Vec<&ChildItem> = level
        .snapshot
        .children
        .iter()
        .filter(|c| c.selected)
        .collect();
    let selected_count = selected_items.len();
    let selected_bytes: u64 = selected_items.iter().map(|c| c.size_bytes).sum();

    let right_status = if selected_count > 0 {
        Line::from(vec![
            Span::styled(format!("{selected_count}"), theme.headline()),
            Span::styled(
                format!(" of {} selected  ·  ", level.snapshot.children.len()),
                theme.callout(),
            ),
            Span::styled(format_bytes(selected_bytes), theme.size(selected_bytes)),
        ])
    } else {
        Line::from(vec![
            Span::styled(
                format_bytes(level.snapshot.total_bytes),
                theme.size(level.snapshot.total_bytes),
            ),
            Span::styled(
                format!("  ·  {} items", level.snapshot.children.len()),
                theme.callout(),
            ),
        ])
    };

    let can_open = match level.snapshot.children.get(level.selected) {
        Some(c) => c.is_dir && !c.is_package,
        None => false,
    };

    let primary_action = if selected_count > 0 {
        PrimaryAction::destructive(format!("Delete {selected_count} items"), "d")
    } else if can_open {
        PrimaryAction::new("Open", "↵")
    } else {
        PrimaryAction::disabled("Open", "↵")
    };

    let toast = state
        .status_notice
        .as_ref()
        .map(|msg| Toast::success(msg.clone()));

    let mut hints = Vec::new();
    if !level.snapshot.children.is_empty() {
        hints.extend([
            Hint::new("␣", "Select"),
            Hint::new("A", "All"),
            Hint::new("N", "None"),
            Hint::new("⇧S/N/T", state.sort_order.hint_label()),
        ]);
        if selected_count == 0 {
            hints.push(Hint::new("d", "Delete"));
        } else if can_open {
            hints.push(Hint::new("↵", "Open"));
        }
        hints.push(Hint::new("↑↓", "Move"));
    }
    hints.push(Hint::new("⎋", "Back"));
    hints.push(Hint::new("?", "More"));

    let chrome = AppChrome::new(crumbs)
        .full_width()
        .status(right_status)
        .hints(hints)
        .primary(primary_action)
        .toast(toast.as_ref())
        .behind_dialog(state.dialog_open());

    let content_rect = chrome.render(frame, theme);

    let show_preview = content_rect.width >= PREVIEW_MIN_WIDTH;
    state.preview.visible.set(show_preview);
    let list_rect = if show_preview {
        Rect {
            width: LIST_WIDTH_WITH_PREVIEW,
            ..content_rect
        }
    } else {
        content_rect
    };

    let frame_rows = (content_rect.height as usize).saturating_sub(FRAME_ROWS);
    let row_width = (list_rect.width as usize).saturating_sub(FRAME_SIDES);
    // The column titles take the first row.
    let list_area_height = frame_rows.saturating_sub(1);
    let scroll_offset = if level.snapshot.children.len() > list_area_height && list_area_height > 0
    {
        let half = list_area_height / 2;
        let max_offset = level
            .snapshot
            .children
            .len()
            .saturating_sub(list_area_height);
        level.selected.saturating_sub(half).min(max_offset)
    } else {
        0
    };

    let total_dir_bytes = level.snapshot.total_bytes.max(1);
    let titles = ColumnTitles {
        date: "CREATED",
        date_width: DATE_W,
        order: state.sort_order,
        share: true,
        chevron: true,
    };
    let mut list_lines = vec![titles.render(row_width, theme)];

    for (idx, child) in level
        .snapshot
        .children
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(list_area_height)
    {
        let is_selected = idx == level.selected;

        let (bullet, bullet_style) = if child.selected {
            (theme.g.check_on, theme.accent_text())
        } else {
            (theme.g.check_off, theme.footnote())
        };

        let icon = if child.is_package {
            theme.g.package
        } else if child.is_dir {
            theme.g.folder
        } else {
            theme.g.file
        };

        let icon_style = if child.is_dir || child.is_package {
            theme.accent_text()
        } else {
            theme.callout()
        };

        let display_name = format!("{}{}", child.name, child.suffix());
        let fraction = (child.size_bytes as f64 / total_dir_bytes as f64).clamp(0.0, 1.0);

        let size_style = theme.size(child.size_bytes);
        let detail_str = format!("{:>9}", format_bytes(child.size_bytes));

        let title_spans = vec![
            Span::styled(format!("{icon} "), icon_style),
            Span::styled(display_name, theme.body()),
        ];

        let date_str = crate::core::date_utils::format_system_time_opt(child.created);
        let date_str = format!("{date_str:<DATE_W$}");

        let row = ListRow::new(title_spans)
            .leading(Span::styled(bullet, bullet_style))
            .meta(Span::styled(date_str, theme.footnote()))
            .detail(Span::styled(detail_str, size_style))
            .chevron(child.is_dir && !child.is_package)
            .reserve_chevron(true)
            .bar(fraction)
            .selected(is_selected);

        list_lines.push(row.render_line(row_width, theme));
    }

    if level.snapshot.children.is_empty() {
        list_lines.push(Line::from(""));
        list_lines.push(Line::from(Span::styled(
            "Empty folder or no accessible files",
            theme.callout(),
        )));
    }

    let folder = level.snapshot.path.file_name().map_or_else(
        || level.snapshot.path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    let updating = if level.wants_update() {
        vec![Span::styled("Updating…", theme.footnote())]
    } else {
        Vec::new()
    };
    let list_frame = card(
        &folder,
        updating,
        list_lines,
        list_rect.width as usize,
        frame_rows,
        theme.p.accent,
        theme,
    );
    frame.render_widget(Paragraph::new(list_frame), list_rect);

    if show_preview && let Some(child) = level.snapshot.children.get(level.selected) {
        render_preview_panel(frame, content_rect, list_rect, child, state, theme);
    }

    // Measuring Modal
    if let Some(ref prog) = state.current_progress
        && state.started_at.elapsed() > MEASURING_DIALOG_DELAY
    {
        render_measuring_modal(frame, prog, state.spinner_idx, theme);
    }

    // Modal Overlays (Deletion confirmation, deletion progress, deletion outcome)
    match &state.overlay {
        AnalyzeOverlay::None => {}
        AnalyzeOverlay::ConfirmDelete { items, button } => {
            render_analyze_confirm_dialog(frame, items, *button, &state.ignore, theme);
        }
        AnalyzeOverlay::Deleting {
            progress,
            spinner_idx,
        } => {
            render_analyze_deleting_dialog(frame, progress, *spinner_idx, theme);
        }
        AnalyzeOverlay::Outcome(report) => {
            render_analyze_outcome_dialog(frame, report, theme);
        }
    }
}

fn render_preview_panel(
    frame: &mut Frame,
    content: Rect,
    list: Rect,
    child: &ChildItem,
    state: &AnalyzeState,
    theme: &Theme,
) {
    let panel_x = list.right() + PREVIEW_GAP;
    let panel = Rect {
        x: panel_x,
        width: content.right().saturating_sub(panel_x),
        ..content
    };

    let parent_bytes = state.current_level().map_or(0, |l| l.snapshot.total_bytes);
    let lines = preview_lines(
        child,
        parent_bytes,
        &state.preview,
        panel.width as usize,
        panel.height as usize,
        theme,
    );
    frame.render_widget(Paragraph::new(lines), panel);
}

fn render_measuring_modal(
    frame: &mut Frame,
    prog: &MeasuringProgress,
    spinner_idx: usize,
    theme: &Theme,
) {
    let pretty_folder = prettify_path(&prog.folder_path);
    let title = format!("Measuring {pretty_folder}…");

    let spinner_char = if theme.env.reduce_motion {
        "*"
    } else {
        match spinner_idx % 4 {
            0 => "⠋",
            1 => "⠙",
            2 => "⠹",
            _ => "⠸",
        }
    };

    let fraction = if prog.total > 0 {
        prog.position as f64 / prog.total as f64
    } else {
        0.0
    };

    let bar_spans = bar(fraction, 38, theme.p.accent, theme);

    let step_info = if prog.total > 0 {
        format!("{} of {} items", prog.position, prog.total)
    } else {
        "Scanning…".to_string()
    };

    let active_name = if let Some(ref sub) = prog.current_subpath {
        prettify_path(Path::new(sub))
    } else {
        prog.current_name.clone()
    };

    let message = vec![
        Line::from(vec![
            Span::styled(format!("{spinner_char} "), theme.accent_text()),
            Span::styled(active_name, theme.body()),
        ]),
        Line::from(bar_spans),
        Line::from(vec![
            Span::styled(step_info, theme.callout()),
            Span::raw("  ·  "),
            Span::styled(
                format!(
                    "{} files ({})",
                    prog.scanned_files,
                    format_bytes(prog.scanned_bytes)
                ),
                theme.footnote(),
            ),
        ]),
    ];

    Modal::new(&title)
        .tint(theme.p.accent)
        .message(message)
        .buttons(vec![Button::new("Cancel (esc)").focused(true)])
        .render(frame, frame.area(), theme);
}

fn render_analyze_confirm_dialog(
    frame: &mut Frame,
    items: &[ChildItem],
    focus_btn: AnalyzeConfirmButton,
    ignore: &IgnoreStore,
    theme: &Theme,
) {
    let cancel_focused = focus_btn == AnalyzeConfirmButton::Cancel;
    let perm_focused = focus_btn == AnalyzeConfirmButton::DeletePermanently;
    let trash_focused = focus_btn == AnalyzeConfirmButton::MoveToTrash;

    let (title, message_lines) = if items.len() == 1 {
        let item = &items[0];
        let pretty = prettify_path(&item.path);
        let title = format!("Delete {}?", item.name);
        let mut lines = vec![
            Line::from(vec![
                Span::styled("Delete ", theme.body()),
                Span::styled(pretty, theme.headline()),
                Span::styled(
                    format!(" ({})?", format_bytes(item.size_bytes)),
                    theme.body(),
                ),
            ]),
            Line::from(Span::styled(
                "Move to the Trash to recover later, or delete permanently to free space immediately.",
                theme.callout(),
            )),
        ];
        if let Err(e) = Safety::can_remove(&item.path, ignore) {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("Warning: ", theme.danger()),
                Span::styled(format!("Protected path: {e}"), theme.danger()),
            ]));
        }
        (title, lines)
    } else {
        let count = items.len();
        let total_bytes: u64 = items.iter().map(|i| i.size_bytes).sum();
        let title = format!("Delete {count} items?");
        let mut lines = vec![
            Line::from(vec![
                Span::styled("Delete ", theme.body()),
                Span::styled(format!("{count} items"), theme.headline()),
                Span::styled(format!(" ({})?", format_bytes(total_bytes)), theme.body()),
            ]),
            Line::from(Span::styled(
                "Move to the Trash to recover later, or delete permanently to free space immediately.",
                theme.callout(),
            )),
        ];
        let protected_count = items
            .iter()
            .filter(|i| Safety::can_remove(&i.path, ignore).is_err())
            .count();
        if protected_count > 0 {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("Warning: {protected_count} items are protected and will be skipped."),
                theme.danger(),
            )));
        }
        (title, lines)
    };

    let cancel_label = if cancel_focused {
        "Cancel (↵)"
    } else {
        "Cancel (esc)"
    };
    let perm_label = if perm_focused {
        "Delete Permanently (↵)"
    } else {
        "Delete Permanently"
    };
    let trash_label = if trash_focused {
        "Move to Trash (↵)"
    } else {
        "Move to Trash"
    };

    Modal::new(&title)
        .message(message_lines)
        .buttons(vec![
            Button::new(cancel_label).focused(cancel_focused),
            Button::destructive(perm_label).focused(perm_focused),
            Button::primary(trash_label).focused(trash_focused),
        ])
        .render(frame, frame.area(), theme);
}

fn render_analyze_deleting_dialog(
    frame: &mut Frame,
    progress: &RemovalProgress,
    spinner_idx: usize,
    theme: &Theme,
) {
    let count = progress.total_count;
    let noun = if count == 1 { "item" } else { "items" };
    let title = format!("Deleting {count} {noun}…");

    let spinner_char = if theme.env.reduce_motion {
        "*"
    } else {
        match spinner_idx % 4 {
            0 => "⠋",
            1 => "⠙",
            2 => "⠹",
            _ => "⠸",
        }
    };

    let pretty_path = prettify_path(&progress.path);
    let path_line = Line::from(vec![
        Span::styled(format!("{spinner_char} "), theme.accent_text()),
        Span::styled(pretty_path, theme.body()),
    ]);

    let fraction = if progress.total_count > 0 {
        progress.current_index as f64 / progress.total_count as f64
    } else {
        0.0
    };

    let bar_spans = bar(fraction, 40, theme.p.accent, theme);

    Modal::new(&title)
        .tint(theme.p.orange)
        .message(vec![
            path_line,
            Line::from(bar_spans),
            Line::from(vec![Span::styled(
                format!("{} of {}", progress.current_index, progress.total_count),
                theme.footnote(),
            )]),
        ])
        .buttons(vec![])
        .render(frame, frame.area(), theme);
}

fn render_analyze_outcome_dialog(frame: &mut Frame, report: &RemovalReport, theme: &Theme) {
    let title = if report.removed_count > 0 {
        "Deletion Complete"
    } else {
        "Nothing was deleted"
    };

    let mut message = Vec::new();
    if report.trashed_bytes > 0 {
        message.push(Line::from(vec![
            Span::styled("Moved ", theme.body()),
            Span::styled(format_bytes(report.trashed_bytes), theme.success()),
            Span::styled(
                " to the Trash. Empty it to get the space back.",
                theme.body(),
            ),
        ]));
    }
    if report.freed_bytes > 0 {
        message.push(Line::from(vec![
            Span::styled("Removed ", theme.body()),
            Span::styled(format_bytes(report.freed_bytes), theme.success()),
            Span::styled(" permanently.", theme.body()),
        ]));
    }
    if report.trashed_bytes == 0 && report.freed_bytes == 0 {
        message.push(Line::from(vec![Span::styled("0 B freed.", theme.body())]));
    }

    if !report.failures.is_empty() {
        message.push(Line::from(""));
        message.push(Line::from(Span::styled(
            format!("{} items could not be deleted:", report.failures.len()),
            theme.danger(),
        )));
        for fail in report.failures.iter().take(3) {
            let pretty = prettify_path(&fail.path);
            message.push(Line::from(vec![
                Span::styled("• ", theme.danger()),
                Span::styled(format!("{pretty}: "), theme.callout()),
                Span::styled(&fail.error, theme.footnote()),
            ]));
        }
        if report.failures.len() > 3 {
            message.push(Line::from(Span::styled(
                format!("… and {} more", report.failures.len() - 3),
                theme.footnote(),
            )));
        }
    }

    Modal::new(title)
        .message(message)
        .buttons(vec![Button::primary("Done (↵)").focused(true)])
        .render(frame, frame.area(), theme);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn test_render_analyze_initial_progress() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::new();

        let state = AnalyzeState {
            root: PathBuf::from("/Users/test"),
            levels: Vec::new(),
            ignore: IgnoreStore::default(),
            measuring_rx: None,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            current_progress: Some(MeasuringProgress {
                folder_path: PathBuf::from("/Users/test"),
                position: 5,
                total: 20,
                current_name: "Library".to_string(),
                current_subpath: Some("/Users/test/Library/Caches/Cargo".to_string()),
                scanned_files: 1250,
                scanned_bytes: 45_000_000,
            }),
            spinner_idx: 1,
            status_notice: None,
            started_at: Instant::now(),
            overlay: AnalyzeOverlay::None,
            cache: Arc::new(RwLock::new(StorageCache::new())),
            sort_order: SortOrder::default(),
            preview: FolderPreview::default(),
            update: None,
        };

        terminal
            .draw(|f| {
                render_analyze(f, &state, &theme);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let rendered = format!("{buffer:?}");
        assert!(rendered.contains("Exploring"));
        assert!(rendered.contains("Library"));
        assert!(rendered.contains("5/20"));
        assert!(rendered.contains("1250 files"));
    }

    #[test]
    fn test_render_analyze_modal_progress() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::new();

        let state = AnalyzeState {
            root: PathBuf::from("/Users/test"),
            levels: vec![BrowserLevel {
                snapshot: FolderSnapshot {
                    path: PathBuf::from("/Users/test"),
                    children: vec![ChildItem {
                        name: "Documents".to_string(),
                        path: PathBuf::from("/Users/test/Documents"),
                        size_bytes: 1024,
                        is_dir: true,
                        is_package: false,
                        selected: false,
                        created: None,
                    }],
                    total_bytes: 1024,
                    inaccessible_count: 0,
                    skipped_count: 0,
                },
                selected: 0,
                offset: 0,
                stale: false,
                update_failed: false,
            }],
            ignore: IgnoreStore::default(),
            measuring_rx: None,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            current_progress: Some(MeasuringProgress {
                folder_path: PathBuf::from("/Users/test/Documents"),
                position: 3,
                total: 10,
                current_name: "Work".to_string(),
                current_subpath: Some("/Users/test/Documents/Work/notes.md".to_string()),
                scanned_files: 45,
                scanned_bytes: 500_000,
            }),
            spinner_idx: 0,
            status_notice: None,
            started_at: Instant::now() - (MEASURING_DIALOG_DELAY + Duration::from_millis(50)),
            overlay: AnalyzeOverlay::None,
            cache: Arc::new(RwLock::new(StorageCache::new())),
            sort_order: SortOrder::default(),
            preview: FolderPreview::default(),
            update: None,
        };

        terminal
            .draw(|f| {
                render_analyze(f, &state, &theme);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let rendered = format!("{buffer:?}");
        assert!(rendered.contains("Measuring"));
        assert!(rendered.contains("Documents"));
        assert!(rendered.contains("3 of 10"));
        assert!(rendered.contains("45 files"));
    }

    #[test]
    fn test_analyze_selection_and_confirm_dialog() {
        let mut state = AnalyzeState {
            root: PathBuf::from("/Users/test"),
            levels: vec![BrowserLevel {
                snapshot: FolderSnapshot {
                    path: PathBuf::from("/Users/test"),
                    children: vec![
                        ChildItem {
                            name: "file1.bin".to_string(),
                            path: PathBuf::from("/Users/test/file1.bin"),
                            size_bytes: 1000,
                            is_dir: false,
                            is_package: false,
                            selected: false,
                            created: None,
                        },
                        ChildItem {
                            name: "file2.bin".to_string(),
                            path: PathBuf::from("/Users/test/file2.bin"),
                            size_bytes: 2000,
                            is_dir: false,
                            is_package: false,
                            selected: false,
                            created: None,
                        },
                    ],
                    total_bytes: 3000,
                    inaccessible_count: 0,
                    skipped_count: 0,
                },
                selected: 0,
                offset: 0,
                stale: false,
                update_failed: false,
            }],
            ignore: IgnoreStore::default(),
            measuring_rx: None,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            current_progress: None,
            spinner_idx: 0,
            status_notice: None,
            started_at: Instant::now(),
            overlay: AnalyzeOverlay::None,
            cache: Arc::new(RwLock::new(StorageCache::new())),
            sort_order: SortOrder::default(),
            preview: FolderPreview::default(),
            update: None,
        };

        // Initially nothing selected
        assert_eq!(state.items_to_delete().len(), 1); // focused item
        assert_eq!(state.items_to_delete()[0].name, "file1.bin");

        // Toggle selection with Space
        state.toggle_current();
        assert!(state.current_level().unwrap().snapshot.children[0].selected);

        // Select all
        state.select_all();
        assert_eq!(state.items_to_delete().len(), 2);

        // Select none
        state.select_none();
        assert_eq!(state.items_to_delete().len(), 1); // falls back to focused

        // Open delete confirmation
        state.open_delete_confirmation(false);
        assert!(matches!(
            state.overlay,
            AnalyzeOverlay::ConfirmDelete { .. }
        ));

        // Render with dialog open
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::new();

        terminal
            .draw(|f| {
                render_analyze(f, &state, &theme);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let rendered = format!("{buffer:?}");
        assert!(rendered.contains("Delete file1.bin?"));
        assert!(rendered.contains("Cancel"));
        assert!(rendered.contains("Move to Trash"));
        assert!(rendered.contains("Delete Permanently"));
    }

    fn screen(state: &AnalyzeState, width: u16, height: u16) -> String {
        let theme = Theme::with_env(crate::ui::env::UiEnv::default());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| render_analyze(f, state, &theme)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    // Holds a folder `sub` with `nested.bin`, and a file `other.bin` that is smaller.
    fn explore_fixture() -> (tempfile::TempDir, AnalyzeState) {
        let temp = tempfile::tempdir().unwrap();
        let sub = temp.path().join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("nested.bin"), vec![0_u8; 200_000]).unwrap();
        fs::write(temp.path().join("other.bin"), b"123").unwrap();
        let state = AnalyzeState::new_sync(temp.path().to_path_buf(), IgnoreStore::default());
        (temp, state)
    }

    fn screen_with(state: &mut AnalyzeState, width: u16, text: &str) -> String {
        for _ in 0..1000 {
            let rendered = screen(state, width, 40);
            if rendered.contains(text) {
                return rendered;
            }
            state.tick();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("{text} never appeared:\n{}", screen(state, width, 40));
    }

    #[test]
    fn a_wide_window_shows_what_the_highlighted_folder_holds() {
        let (_temp, mut state) = explore_fixture();

        let rendered = screen_with(&mut state, 205, "nested.bin");

        assert!(rendered.contains("Contents"), "{rendered}");
        assert!(rendered.contains("1 item"), "{rendered}");
    }

    #[test]
    fn a_highlighted_file_shows_its_details() {
        let (_temp, mut state) = explore_fixture();
        state.next();

        let rendered = screen_with(&mut state, 205, "of this folder");

        assert!(!rendered.contains("Contents"), "{rendered}");
    }

    #[test]
    fn the_list_keeps_its_width_beside_the_panel() {
        let (_temp, mut state) = explore_fixture();

        let rendered = screen_with(&mut state, 205, "nested.bin");

        let row = rendered.lines().nth(2).unwrap();
        let corners: Vec<usize> = row
            .chars()
            .enumerate()
            .filter(|(_, c)| *c == '╭')
            .map(|(x, _)| x)
            .collect();
        assert_eq!(corners.len(), 2, "{row}");
        assert_eq!(corners[1] - corners[0], 106, "{row}");
    }

    #[test]
    fn a_narrower_window_has_no_panel_and_measures_nothing() {
        let (temp, mut state) = explore_fixture();

        screen(&state, 120, 40);
        for _ in 0..3 {
            state.tick();
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        assert!(!screen(&state, 120, 40).contains("of this folder"));
        let sub = temp.path().join("sub");
        assert!(state.cache.read().unwrap().get_snapshot(&sub).is_none());
    }

    #[test]
    fn permanent_run_focuses_delete_permanently() {
        let (_temp, mut state) = explore_fixture();

        state.open_delete_confirmation(true);

        assert!(matches!(
            state.overlay,
            AnalyzeOverlay::ConfirmDelete {
                button: AnalyzeConfirmButton::DeletePermanently,
                ..
            }
        ));
        let rendered = screen(&state, 120, 40);
        assert!(rendered.contains("Delete Permanently (↵)"), "{rendered}");
        assert!(rendered.contains("Move to Trash"), "{rendered}");

        state.open_delete_confirmation(false);
        assert!(matches!(
            state.overlay,
            AnalyzeOverlay::ConfirmDelete {
                button: AnalyzeConfirmButton::MoveToTrash,
                ..
            }
        ));
    }

    #[test]
    fn a_dialog_pauses_the_measurement() {
        let (_temp, mut state) = explore_fixture();
        screen(&state, 205, 40);
        state.open_delete_confirmation(false);

        assert_eq!(state.preview_target(), None);
    }

    #[test]
    fn refreshing_forgets_the_preview() {
        let (temp, mut state) = explore_fixture();
        screen_with(&mut state, 205, "nested.bin");

        state.refresh_current();

        let sub = temp.path().join("sub");
        assert!(state.preview.snapshot_of(&sub).is_none());
    }

    #[test]
    fn test_drill_down_uses_cache() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let sub = root.join("subfolder");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("nested.bin"), b"12345678").unwrap();

        let mut state = AnalyzeState::new_sync(root.clone(), IgnoreStore::default());
        assert_eq!(state.levels.len(), 1);

        // Subfolder was traversed during initial scan; verify its size is in cache
        let cached_size = state.cache.read().unwrap().get_size(&sub);
        assert!(cached_size.is_some());

        // Enter selected subfolder; background measuring will process it
        state.enter_selected();
        while state.measuring_rx.is_some() {
            state.tick();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(state.levels.len(), 2);
        assert_eq!(state.current_level().unwrap().snapshot.path, sub);

        // Go up
        assert!(state.go_up());
        assert_eq!(state.levels.len(), 1);

        // Enter again: now snapshot is cached in cache! enter_selected should be instant without measuring_rx
        state.enter_selected();
        assert_eq!(state.levels.len(), 2);
        assert!(state.measuring_rx.is_none()); // Instant transition, no background thread needed!
        assert!(state.update.is_none());
        assert!(!state.current_level().unwrap().stale);
    }

    #[test]
    fn test_analyze_sort_order() {
        let t1 = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        let t2 = std::time::UNIX_EPOCH + std::time::Duration::from_secs(2_000_000);
        let t3 = std::time::UNIX_EPOCH + std::time::Duration::from_secs(3_000_000);

        let mut items = vec![
            ChildItem {
                name: "b_file.txt".to_string(),
                path: PathBuf::from("/test/b_file.txt"),
                size_bytes: 500,
                is_dir: false,
                is_package: false,
                selected: false,
                created: Some(t1),
            },
            ChildItem {
                name: "a_file.txt".to_string(),
                path: PathBuf::from("/test/a_file.txt"),
                size_bytes: 2000,
                is_dir: false,
                is_package: false,
                selected: false,
                created: Some(t2),
            },
            ChildItem {
                name: "c_file.txt".to_string(),
                path: PathBuf::from("/test/c_file.txt"),
                size_bytes: 100,
                is_dir: false,
                is_package: false,
                selected: false,
                created: Some(t3),
            },
        ];

        // 1. Sort by Size (default: largest first)
        SortOrder::Size.sort_children(&mut items);
        assert_eq!(items[0].name, "a_file.txt");
        assert_eq!(items[1].name, "b_file.txt");
        assert_eq!(items[2].name, "c_file.txt");

        // 2. Sort by Date (newest first)
        SortOrder::Date.sort_children(&mut items);
        assert_eq!(items[0].name, "c_file.txt"); // t3
        assert_eq!(items[1].name, "a_file.txt"); // t2
        assert_eq!(items[2].name, "b_file.txt"); // t1

        // 3. Sort by Name (A-Z)
        SortOrder::Name.sort_children(&mut items);
        assert_eq!(items[0].name, "a_file.txt");
        assert_eq!(items[1].name, "b_file.txt");
        assert_eq!(items[2].name, "c_file.txt");

        // Pick a sort order
        let mut state = AnalyzeState {
            root: PathBuf::from("/test"),
            levels: vec![BrowserLevel {
                snapshot: FolderSnapshot {
                    path: PathBuf::from("/test"),
                    children: items,
                    total_bytes: 2600,
                    inaccessible_count: 0,
                    skipped_count: 0,
                },
                selected: 0,
                offset: 0,
                stale: false,
                update_failed: false,
            }],
            ignore: IgnoreStore::default(),
            measuring_rx: None,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            current_progress: None,
            spinner_idx: 0,
            status_notice: None,
            started_at: Instant::now(),
            overlay: AnalyzeOverlay::None,
            cache: Arc::new(RwLock::new(StorageCache::new())),
            sort_order: SortOrder::Size,
            preview: FolderPreview::default(),
            update: None,
        };

        assert_eq!(state.sort_order, SortOrder::Size);
        state.set_sort(SortOrder::Date);
        assert_eq!(state.sort_order, SortOrder::Date);
        assert!(state.status_notice.as_ref().unwrap().contains("date"));
        state.set_sort(SortOrder::Name);
        assert_eq!(state.sort_order, SortOrder::Name);
        assert!(state.status_notice.as_ref().unwrap().contains("name"));
        state.set_sort(SortOrder::Size);
        assert_eq!(state.sort_order, SortOrder::Size);
    }

    fn folder_with_bundle() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let app = temp.path().join("X.app");
        fs::create_dir_all(app.join("Contents")).unwrap();
        fs::write(app.join("Contents/bin"), vec![1_u8; 65536]).unwrap();
        (temp, app)
    }

    fn scan_with(cache: &Arc<RwLock<StorageCache>>, dir: &Path) -> FolderSnapshot {
        AnalyzeState::scan_folder(
            dir,
            &IgnoreStore::default(),
            None,
            Arc::new(AtomicBool::new(false)),
            cache.clone(),
        )
    }

    #[test]
    fn a_bundle_size_in_the_cache_is_used_by_the_scan() {
        let (temp, app) = folder_with_bundle();
        let mtime = fs::metadata(&app).unwrap().modified().ok();
        let cache = Arc::new(RwLock::new(StorageCache::new()));
        cache
            .write()
            .unwrap()
            .put_size(app.clone(), 12345, 1, mtime);

        let snapshot = scan_with(&cache, temp.path());

        let item = snapshot.children.iter().find(|c| c.path == app).unwrap();
        assert!(item.is_package);
        assert_eq!(item.size_bytes, 12345);
    }

    #[test]
    fn a_scan_stores_the_size_of_each_bundle() {
        let (temp, app) = folder_with_bundle();
        let cache = Arc::new(RwLock::new(StorageCache::new()));

        let snapshot = scan_with(&cache, temp.path());

        let item = snapshot.children.iter().find(|c| c.path == app).unwrap();
        assert!(item.size_bytes >= 65536);
        assert_eq!(cache.read().unwrap().get_size(&app), Some(item.size_bytes));
    }

    fn measuring_state() -> (tempfile::TempDir, AnalyzeState) {
        let (temp, mut state) = explore_fixture();
        state.cache.write().unwrap().clear();
        state.start_measuring(state.root.join("sub"));
        (temp, state)
    }

    #[test]
    fn the_measuring_dialog_waits_before_it_shows() {
        let (_temp, mut state) = measuring_state();
        assert!(state.measuring_rx.is_some());
        assert!(!screen(&state, 100, 30).contains("Measuring"));

        state.started_at = Instant::now() - (MEASURING_DIALOG_DELAY + Duration::from_millis(50));
        assert!(screen(&state, 100, 30).contains("Measuring"));
    }

    #[test]
    fn a_quick_scan_opens_the_folder_with_no_dialog_drawn() {
        let (_temp, mut state) = measuring_state();
        let mut drawn = Vec::new();
        while state.measuring_rx.is_some() {
            drawn.push(screen(&state, 100, 30));
            state.tick();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(state.levels.len(), 2);
        assert!(drawn.iter().all(|s| !s.contains("Measuring")));
        assert!(!screen(&state, 100, 30).contains("Measuring"));
    }

    // Stores `rows` as the cached view of `dir`, then makes it old by moving the folder's mtime back.
    fn make_stale(state: &AnalyzeState, dir: &Path, rows: &[(&str, u64, bool)]) {
        let children: Vec<ChildItem> = rows
            .iter()
            .map(|(name, size, is_dir)| ChildItem {
                name: (*name).to_string(),
                path: dir.join(name),
                size_bytes: *size,
                is_dir: *is_dir,
                is_package: false,
                selected: false,
                created: None,
            })
            .collect();
        let snapshot = FolderSnapshot {
            path: dir.to_path_buf(),
            total_bytes: children.iter().map(|c| c.size_bytes).sum(),
            children,
            inaccessible_count: 0,
            skipped_count: 0,
        };
        state
            .cache
            .write()
            .unwrap()
            .put_snapshot(dir.to_path_buf(), snapshot);
        let old = SystemTime::now() - Duration::from_secs(3600);
        fs::File::open(dir).unwrap().set_modified(old).unwrap();
    }

    fn names(state: &AnalyzeState) -> Vec<String> {
        let level = state.current_level().unwrap();
        level
            .snapshot
            .children
            .iter()
            .map(|c| c.name.clone())
            .collect()
    }

    fn finish_update(state: &mut AnalyzeState) {
        for _ in 0..1000 {
            if state.update.is_none() {
                return;
            }
            state.tick();
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("the update never ended");
    }

    // A root with `sub` holding the given files; `sub` is shown stale with `rows`, and opened.
    fn stale_sub(
        files: &[(&str, usize)],
        rows: &[(&str, u64, bool)],
    ) -> (tempfile::TempDir, AnalyzeState, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let sub = temp.path().join("sub");
        fs::create_dir_all(&sub).unwrap();
        if rows.iter().any(|(name, ..)| *name == "inner") {
            fs::create_dir(sub.join("inner")).unwrap();
        }
        for (name, len) in files {
            fs::write(sub.join(name), vec![0_u8; *len]).unwrap();
        }
        let mut state = AnalyzeState::new_sync(temp.path().to_path_buf(), IgnoreStore::default());
        make_stale(&state, &sub, rows);
        state.enter_selected();
        (temp, state, sub)
    }

    #[test]
    fn a_stale_folder_opens_at_once_and_updates() {
        let (temp, mut state) = explore_fixture();
        let sub = temp.path().join("sub");
        make_stale(&state, &sub, &[("old.bin", 1, false)]);

        state.enter_selected();

        assert_eq!(state.levels.len(), 2);
        assert!(state.measuring_rx.is_none());
        assert_eq!(names(&state), ["old.bin"]);
        assert!(state.current_level().unwrap().stale);
        assert!(state.update.is_some());
        let rendered = screen(&state, 100, 30);
        assert!(rendered.contains("Updating…"), "{rendered}");
        assert!(!rendered.contains("Measuring"), "{rendered}");

        finish_update(&mut state);

        assert_eq!(names(&state), ["nested.bin"]);
        assert!(!state.current_level().unwrap().stale);
        assert!(!screen(&state, 100, 30).contains("Updating…"));
        let expected = AnalyzeState::scan_folder(
            &sub,
            &IgnoreStore::default(),
            None,
            Arc::new(AtomicBool::new(false)),
            Arc::new(RwLock::new(StorageCache::new())),
        );
        let rows = |s: &FolderSnapshot| -> Vec<(String, u64)> {
            s.children
                .iter()
                .map(|c| (c.name.clone(), c.size_bytes))
                .collect()
        };
        assert_eq!(
            rows(&state.current_level().unwrap().snapshot),
            rows(&expected)
        );
        assert!(matches!(
            state.cache.read().unwrap().lookup_snapshot(&sub),
            Some(CachedSnapshot::Fresh(_))
        ));
    }

    const MB: u64 = 1024 * 1024;

    #[test]
    fn the_update_keeps_the_focus_and_the_marks_by_path() {
        let files = [("a.bin", 4096), ("b.bin", 16384), ("c.bin", 65536)];
        let rows = [
            ("a.bin", 3 * MB, false),
            ("b.bin", 2 * MB, false),
            ("c.bin", MB, false),
        ];
        let (_temp, mut state, _sub) = stale_sub(&files, &rows);
        assert_eq!(names(&state), ["a.bin", "b.bin", "c.bin"]);
        state.next();
        state.next();
        state.toggle_current();
        state.previous();
        state.previous();

        finish_update(&mut state);

        assert_eq!(names(&state), ["c.bin", "b.bin", "a.bin"]);
        let level = state.current_level().unwrap();
        assert_eq!(level.selected, 2);
        let marked: Vec<bool> = level.snapshot.children.iter().map(|c| c.selected).collect();
        assert_eq!(marked, [true, false, false]);

        let rows = [
            ("a.bin", 3 * MB, false),
            ("b.bin", 2 * MB, false),
            ("c.bin", MB, false),
            ("gone.bin", 1, false),
        ];
        let (_temp, mut state, _sub) = stale_sub(&files, &rows);
        state.move_to_end();
        assert_eq!(state.current_level().unwrap().selected, 3);

        finish_update(&mut state);

        assert_eq!(state.current_level().unwrap().selected, 2);
    }

    #[test]
    fn opening_explore_at_a_stale_folder_shows_it_at_once() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        fs::write(root.join("a.bin"), b"12345").unwrap();
        let holder = AnalyzeState::new_sync(root.clone(), IgnoreStore::default());
        make_stale(&holder, &root, &[("old.bin", 1, false)]);

        let mut state =
            AnalyzeState::with_cache(root, IgnoreStore::default(), holder.cache.clone());

        assert_eq!(state.levels.len(), 1);
        assert!(state.measuring_rx.is_none());
        assert!(state.levels[0].stale);
        assert!(state.update.is_some());
        finish_update(&mut state);
        assert_eq!(names(&state), ["a.bin"]);
    }

    fn updating_sub() -> (tempfile::TempDir, AnalyzeState, PathBuf) {
        let rows = [("inner", 500, true), ("old.bin", 1, false)];
        stale_sub(&[("old.bin", 1)], &rows)
    }

    // Puts a hand-made update of `sub` in place of the running one and returns its sender.
    fn fake_update(state: &mut AnalyzeState, sub: &Path) -> mpsc::Sender<FolderSnapshot> {
        let (tx, rx) = mpsc::channel();
        state.update = Some(Job {
            path: sub.to_path_buf(),
            cancel: Arc::new(AtomicBool::new(false)),
            rx,
        });
        tx
    }

    #[test]
    fn an_update_that_has_not_sent_keeps_running() {
        let (_t, mut state, sub) = updating_sub();
        let _tx = fake_update(&mut state, &sub);

        state.tick();

        assert!(state.update.is_some());
        assert!(screen(&state, 100, 30).contains("Updating…"));
    }

    #[test]
    fn an_update_that_dies_keeps_the_old_sizes() {
        let (_t, mut state, sub) = updating_sub();
        drop(fake_update(&mut state, &sub));
        state.next();
        state.toggle_current();
        let before = state.current_level().unwrap().snapshot.children.clone();
        let selected = state.current_level().unwrap().selected;
        assert!(screen(&state, 100, 30).contains("Updating…"));

        state.tick();

        assert!(state.update.is_none());
        assert!(state.current_level().unwrap().stale);
        assert!(!screen(&state, 100, 30).contains("Updating…"));
        assert_eq!(state.current_level().unwrap().snapshot.children, before);
        assert_eq!(state.current_level().unwrap().selected, selected);

        state.tick();

        assert!(state.update.is_none());
    }

    #[test]
    fn a_removal_after_a_dead_update_does_not_cache_the_old_sizes() {
        let (_t, mut state, sub) = updating_sub();
        drop(fake_update(&mut state, &sub));
        state.tick();
        fs::remove_file(sub.join("old.bin")).unwrap();

        state.handle_removal_finished(&RemovalReport {
            removed_count: 1,
            ..Default::default()
        });

        assert!(state.cache.read().unwrap().lookup_snapshot(&sub).is_none());
    }

    #[test]
    fn leaving_the_folder_stops_the_update() {
        let cancel = |state: &AnalyzeState| state.update.as_ref().unwrap().cancel.clone();

        let (_t, mut state, _) = updating_sub();
        let flag = cancel(&state);
        assert!(state.go_up());
        assert!(flag.load(Ordering::Relaxed));
        assert!(state.update.is_none());

        let (_t, mut state, _) = updating_sub();
        let flag = cancel(&state);
        state.enter_selected();
        assert!(flag.load(Ordering::Relaxed));
        assert!(state.update.is_none());

        let (_t, mut state, _) = updating_sub();
        let flag = cancel(&state);
        state.refresh_current();
        assert!(state.measuring_rx.is_some());
        assert!(flag.load(Ordering::Relaxed));
        assert!(state.update.is_none());

        let (_t, mut state, _) = updating_sub();
        let flag = cancel(&state);
        state.next();
        state.toggle_current();
        state.set_sort(SortOrder::Name);
        assert!(!flag.load(Ordering::Relaxed));
        assert!(state.update.is_some());
    }

    #[test]
    fn going_back_to_a_stale_folder_updates_it_again() {
        let (_t, mut state, sub) = updating_sub();
        state.enter_selected();
        assert!(state.measuring_rx.is_some());
        for _ in 0..1000 {
            if state.measuring_rx.is_none() {
                break;
            }
            state.tick();
            assert!(state.update.is_none());
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(state.measuring_rx.is_none());
        assert_eq!(state.levels.len(), 3);
        assert_eq!(
            state.current_level().unwrap().snapshot.path,
            sub.join("inner")
        );

        assert!(state.go_up());

        let level = state.current_level().unwrap();
        assert_eq!(level.snapshot.path, sub);
        assert!(level.stale);
        state.tick();
        assert_eq!(state.update.as_ref().unwrap().path, sub);
    }

    #[test]
    fn the_update_waits_for_a_measuring_or_a_removal_to_end() {
        let (_t, mut state, sub) = updating_sub();
        state.cancel_update();

        let (_tx, rx) = mpsc::channel();
        state.measuring_rx = Some(rx);
        state.tick();
        assert!(state.update.is_none());

        state.measuring_rx = None;
        state.overlay = AnalyzeOverlay::Deleting {
            progress: RemovalProgress {
                current_index: 0,
                total_count: 1,
                path: sub.join("old.bin"),
            },
            spinner_idx: 0,
        };
        state.tick();
        assert!(state.update.is_none());

        state.overlay = AnalyzeOverlay::Outcome(RemovalReport::default());
        state.tick();
        assert_eq!(state.update.as_ref().unwrap().path, sub);
    }

    #[test]
    fn the_update_waits_while_a_dialog_is_open() {
        let rows = [("gone.bin", 9 * MB, false), ("a.bin", MB, false)];
        let (_t, mut state, sub) = stale_sub(&[("a.bin", 10)], &rows);
        state.open_delete_confirmation(false);
        let only_gone = |state: &AnalyzeState| -> Vec<String> {
            state
                .items_to_delete()
                .into_iter()
                .map(|c| c.name)
                .collect()
        };
        assert_eq!(only_gone(&state), ["gone.bin"]);

        for _ in 0..1000 {
            if matches!(
                state.cache.read().unwrap().lookup_snapshot(&sub),
                Some(CachedSnapshot::Fresh(_))
            ) {
                break;
            }
            state.tick();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(matches!(
            state.cache.read().unwrap().lookup_snapshot(&sub),
            Some(CachedSnapshot::Fresh(_))
        ));
        for _ in 0..5 {
            state.tick();
            std::thread::sleep(Duration::from_millis(5));
        }

        assert_eq!(names(&state), ["gone.bin", "a.bin"]);
        assert!(state.current_level().unwrap().stale);
        assert_eq!(only_gone(&state), ["gone.bin"]);

        state.overlay = AnalyzeOverlay::None;
        finish_update(&mut state);

        assert_eq!(names(&state), ["a.bin"]);
    }

    #[test]
    fn a_removal_in_a_stale_folder_measures_it_again() {
        let rows = [("a.bin", 2 * MB, false), ("b.bin", MB, false)];
        let files = [("a.bin", 10), ("b.bin", 10)];
        let (_t, mut state, sub) = stale_sub(&files, &rows);
        state.cancel_update();
        fs::remove_file(sub.join("b.bin")).unwrap();

        state.handle_removal_finished(&RemovalReport {
            removed_count: 1,
            ..Default::default()
        });

        assert_eq!(names(&state), ["a.bin"]);
        assert!(state.current_level().unwrap().stale);
        assert!(state.cache.read().unwrap().lookup_snapshot(&sub).is_none());
        for _ in 0..1000 {
            state.tick();
            if state.update.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!state.current_level().unwrap().stale);
        assert_eq!(names(&state), ["a.bin"]);
    }

    #[test]
    fn a_removal_in_a_fresh_folder_keeps_it_cached() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("a.bin"), b"1").unwrap();
        fs::write(temp.path().join("b.bin"), b"1").unwrap();
        let mut state = AnalyzeState::new_sync(temp.path().to_path_buf(), IgnoreStore::default());
        fs::remove_file(temp.path().join("b.bin")).unwrap();

        state.handle_removal_finished(&RemovalReport {
            removed_count: 1,
            ..Default::default()
        });

        let cached = state.cache.read().unwrap().get_snapshot(temp.path());
        assert_eq!(cached.unwrap().children.len(), 1);
    }

    #[test]
    fn the_updating_mark_shows_only_while_stale() {
        let (_temp, mut state) = explore_fixture();
        state.levels[0].stale = true;

        let rendered = screen(&state, 100, 30);

        let marked = rendered.lines().find(|l| l.contains("Updating…")).unwrap();
        assert!(marked.contains("╭─"), "{rendered}");
        assert!(!rendered.contains("Measuring"), "{rendered}");

        state.levels[0].stale = false;
        assert!(!screen(&state, 100, 30).contains("Updating…"));
    }
}

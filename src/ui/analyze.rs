//! Full-featured Read-Only Explore Storage Browser.
//!
//! Features:
//! - Multi-level breadcrumb navigation stack with remembered scroll & focus offsets.
//! - 'O' key to reveal item in Finder (`/usr/bin/open -R`).
//! - Gauge proportion relative to current folder's total allocated size.
//! - Accurate folder `/` and package ` [package]` suffixes.
//! - Live measuring background progress dialog with cancellation.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use super::components::button::Button;
use super::components::chrome::AppChrome;
use super::components::gauge::bar;
use super::components::hints::{Hint, PrimaryAction};
use super::components::list::ListRow;
use super::components::modal::Modal;
use super::components::progress::ProgressView;
use super::components::toast::Toast;
use super::folder_preview::{FolderPreview, preview_lines};
use super::theme::Theme;
use crate::core::bytes::format_bytes;
pub use crate::core::cache::{ChildItem, FolderSnapshot, StorageCache};
use crate::core::cleaner::{RemovalProgress, RemovalReport};
use crate::core::path_utils::prettify_path;
use crate::core::safety::Safety;
use crate::core::sizer::DirSizer;
use crate::core::stores::IgnoreStore;

// Below this the panel would squeeze the list or itself.
const PREVIEW_MIN_WIDTH: u16 = 150;
const LIST_WIDTH_WITH_PREVIEW: u16 = 100;
const PREVIEW_GAP: u16 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalyzeConfirmButton {
    Cancel,
    Delete,
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

    pub fn sort_children(&self, children: &mut [ChildItem]) {
        match self {
            Self::Size => {
                children.sort_by(|a, b| {
                    b.size_bytes
                        .cmp(&a.size_bytes)
                        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                });
            }
            Self::Date => {
                children.sort_by(|a, b| {
                    b.created
                        .cmp(&a.created)
                        .then_with(|| b.size_bytes.cmp(&a.size_bytes))
                        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                });
            }
            Self::Name => {
                children.sort_by(|a, b| {
                    a.name
                        .to_lowercase()
                        .cmp(&b.name.to_lowercase())
                        .then_with(|| b.size_bytes.cmp(&a.size_bytes))
                });
            }
        }
    }
}

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

    pub fn open_delete_confirmation(&mut self) {
        let items = self.items_to_delete();
        if !items.is_empty() {
            self.overlay = AnalyzeOverlay::ConfirmDelete {
                items,
                button: AnalyzeConfirmButton::Cancel,
            };
        }
    }

    pub fn handle_removal_finished(&mut self, report: &RemovalReport) {
        if report.removed_count == 0 {
            return;
        }
        let updated_snapshot = if let Some(level) = self.current_level_mut() {
            level.snapshot.children.retain(|child| child.path.exists());
            level.snapshot.total_bytes = level.snapshot.children.iter().map(|c| c.size_bytes).sum();
            if level.selected >= level.snapshot.children.len()
                && !level.snapshot.children.is_empty()
            {
                level.selected = level.snapshot.children.len() - 1;
            }
            Some(level.snapshot.clone())
        } else {
            None
        };
        if let Some(snapshot) = updated_snapshot {
            let mut cache = self.cache.write().unwrap();
            cache.invalidate_prefix(&snapshot.path);
            cache.put_snapshot(snapshot.path.clone(), snapshot);
            self.preview = FolderPreview::default();
        }
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

            let size = if is_dir && !is_package {
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
            // Check if snapshot is cached for instant zero-latency transition
            if let Some(mut snapshot) = self.cache.read().unwrap().get_snapshot(&child.path) {
                self.sort_order.sort_children(&mut snapshot.children);
                self.levels.push(BrowserLevel {
                    snapshot,
                    selected: 0,
                    offset: 0,
                });
                return;
            }

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
        // If snapshot is already cached, push directly without spawning a thread!
        if let Some(mut snapshot) = self.cache.read().unwrap().get_snapshot(&path) {
            self.sort_order.sort_children(&mut snapshot.children);
            self.levels.push(BrowserLevel {
                snapshot,
                selected: 0,
                offset: 0,
            });
            self.measuring_rx = None;
            self.current_progress = None;
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

    pub fn cancel_measuring(&mut self) {
        self.cancel_flag.store(true, Ordering::Relaxed);
        self.measuring_rx = None;
        self.current_progress = None;
    }

    fn preview_target(&self) -> Option<PathBuf> {
        let busy = self.measuring_rx.is_some() || !matches!(self.overlay, AnalyzeOverlay::None);
        if busy || !self.preview.visible.get() {
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
                        });
                        self.measuring_rx = None;
                        self.current_progress = None;
                        break;
                    }
                }
            }
        }

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
        PrimaryAction::destructive(format!("Delete {selected_count} items"), "D")
    } else if can_open {
        PrimaryAction::new("Open", "↵")
    } else {
        PrimaryAction::disabled("Open", "↵")
    };

    let toast = state
        .status_notice
        .as_ref()
        .map(|msg| Toast::success(msg.clone()));

    let mut hints = vec![
        Hint::new("↑↓", "Move"),
        Hint::new("␣", "Select"),
        Hint::new("⇧S/N/D", state.sort_order.hint_label()),
        Hint::new("D", "Delete"),
    ];
    if can_open {
        hints.push(Hint::new("↵", "Open"));
    }
    hints.push(Hint::new("←", "Back"));
    hints.push(Hint::new("?", "More"));

    let chrome = AppChrome::new(crumbs)
        .full_width()
        .status(right_status)
        .hints(hints)
        .primary(primary_action)
        .toast(toast.as_ref());

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

    let list_area_height = content_rect.height as usize;
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
    let mut list_lines = Vec::new();

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

        let row = ListRow::new(title_spans)
            .leading(Span::styled(bullet, bullet_style))
            .meta(Span::styled(date_str, theme.footnote()))
            .detail(Span::styled(detail_str, size_style))
            .chevron(child.is_dir && !child.is_package)
            .reserve_chevron(true)
            .bar(fraction)
            .selected(is_selected);

        list_lines.push(row.render_line(list_rect.width as usize, theme));
    }

    if level.snapshot.children.is_empty() {
        list_lines.push(Line::from(""));
        list_lines.push(Line::from(Span::styled(
            "  (empty folder or no accessible files)",
            theme.callout(),
        )));
    }

    frame.render_widget(Paragraph::new(list_lines), list_rect);

    if show_preview && let Some(child) = level.snapshot.children.get(level.selected) {
        render_preview_panel(frame, content_rect, list_rect, child, state, theme);
    }

    // Measuring Modal
    if let Some(ref prog) = state.current_progress {
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
    let divider = Rect {
        x: list.right() + 1,
        width: 1,
        ..content
    };
    let panel_x = divider.right() + PREVIEW_GAP;
    let panel = Rect {
        x: panel_x,
        width: content.right().saturating_sub(panel_x),
        ..content
    };

    frame.render_widget(
        Block::default()
            .borders(Borders::LEFT)
            .border_style(theme.separator()),
        divider,
    );

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
    let delete_focused = focus_btn == AnalyzeConfirmButton::Delete;

    let (title, message_lines) = if items.len() == 1 {
        let item = &items[0];
        let pretty = prettify_path(&item.path);
        let title = format!("Delete {}?", item.name);
        let mut lines = vec![
            Line::from(vec![
                Span::styled("Permanently delete ", theme.body()),
                Span::styled(pretty, theme.headline()),
                Span::styled(
                    format!(" ({})?", format_bytes(item.size_bytes)),
                    theme.body(),
                ),
            ]),
            Line::from(Span::styled(
                "This action cannot be undone.",
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
                Span::styled("Permanently delete ", theme.body()),
                Span::styled(format!("{count} items"), theme.headline()),
                Span::styled(format!(" ({})?", format_bytes(total_bytes)), theme.body()),
            ]),
            Line::from(Span::styled(
                "This action cannot be undone.",
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

    Modal::new(&title)
        .message(message_lines)
        .buttons(vec![
            Button::new("Cancel (esc)").focused(cancel_focused),
            Button::destructive("Delete (↵)").focused(delete_focused),
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
    let title = if report.freed_bytes > 0 {
        "Deletion Complete"
    } else {
        "Nothing was deleted"
    };

    let mut message = vec![Line::from(vec![
        Span::styled(format_bytes(report.freed_bytes), theme.success()),
        Span::styled(
            format!(
                " freed · {} items permanently deleted.",
                report.removed_count
            ),
            theme.body(),
        ),
    ])];

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
            started_at: Instant::now(),
            overlay: AnalyzeOverlay::None,
            cache: Arc::new(RwLock::new(StorageCache::new())),
            sort_order: SortOrder::default(),
            preview: FolderPreview::default(),
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
        state.open_delete_confirmation();
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
        assert!(rendered.contains("Delete"));
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

        assert!(rendered.contains("CONTENTS"), "{rendered}");
        assert!(rendered.contains("1 item"), "{rendered}");
    }

    #[test]
    fn a_highlighted_file_shows_its_details() {
        let (_temp, mut state) = explore_fixture();
        state.next();

        let rendered = screen_with(&mut state, 205, "of this folder");

        assert!(!rendered.contains("CONTENTS"), "{rendered}");
    }

    #[test]
    fn the_list_stops_at_100_columns_beside_the_panel() {
        let (_temp, mut state) = explore_fixture();

        let rendered = screen_with(&mut state, 205, "nested.bin");

        let row = rendered.lines().nth(2).unwrap();
        let divider = row.chars().position(|c| c == '│').unwrap();
        assert!((103..=106).contains(&divider), "{divider}: {row}");
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
    fn a_dialog_pauses_the_measurement() {
        let (_temp, mut state) = explore_fixture();
        screen(&state, 205, 40);
        state.open_delete_confirmation();

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
}

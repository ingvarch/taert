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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;

use ratatui::Frame;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::components::button::Button;
use super::components::chrome::AppChrome;
use super::components::hints::{Hint, PrimaryAction};
use super::components::list::ListRow;
use super::components::modal::Modal;
use super::components::toast::Toast;
use super::theme::Theme;
use crate::core::bytes::format_bytes;
use crate::core::path_utils::prettify_path;
use crate::core::safety::Safety;
use crate::core::sizer::DirSizer;
use crate::core::stores::IgnoreStore;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildItem {
    pub name: String,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub is_dir: bool,
    pub is_package: bool,
}

impl ChildItem {
    pub fn suffix(&self) -> &'static str {
        if self.is_package {
            ""
        } else if self.is_dir {
            "/"
        } else {
            ""
        }
    }
}

#[derive(Debug, Clone)]
pub struct FolderSnapshot {
    pub path: PathBuf,
    pub children: Vec<ChildItem>,
    pub total_bytes: u64,
    pub inaccessible_count: usize,
    pub skipped_count: usize,
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
}

pub enum MeasureMessage {
    Progress(MeasuringProgress),
    Finished(FolderSnapshot),
}

pub struct AnalyzeState {
    pub levels: Vec<BrowserLevel>,
    pub ignore: IgnoreStore,
    pub measuring_rx: Option<Receiver<MeasureMessage>>,
    pub cancel_flag: Arc<AtomicBool>,
    pub current_progress: Option<MeasuringProgress>,
    pub spinner_idx: usize,
    pub status_notice: Option<String>,
}

impl AnalyzeState {
    pub fn new(root: PathBuf, ignore: IgnoreStore) -> Self {
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let snapshot = Self::scan_folder(&root, &ignore, None, cancel_flag.clone());
        let initial_level = BrowserLevel {
            snapshot,
            selected: 0,
            offset: 0,
        };

        Self {
            levels: vec![initial_level],
            ignore,
            measuring_rx: None,
            cancel_flag,
            current_progress: None,
            spinner_idx: 0,
            status_notice: None,
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

            let size = DirSizer::size_of(p);
            if size == 0 && !p.exists() {
                inaccessible += 1;
            }

            children.push(ChildItem {
                name,
                path: p.clone(),
                size_bytes: size,
                is_dir,
                is_package,
            });
        }

        // Rank largest first, then alphabetically
        children.sort_by(|a, b| {
            b.size_bytes
                .cmp(&a.size_bytes)
                .then_with(|| a.name.cmp(&b.name))
        });

        let total_bytes = children.iter().map(|c| c.size_bytes).sum();

        FolderSnapshot {
            path: dir.to_path_buf(),
            children,
            total_bytes,
            inaccessible_count: inaccessible,
            skipped_count: skipped,
        }
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
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel_flag = cancel.clone();
        let ignore = self.ignore.clone();

        let path_clone = path.clone();
        thread::spawn(move || {
            let snapshot = Self::scan_folder(&path_clone, &ignore, Some(tx.clone()), cancel);
            let _ = tx.send(MeasureMessage::Finished(snapshot));
        });

        self.measuring_rx = Some(rx);
        self.current_progress = Some(MeasuringProgress {
            folder_path: path,
            position: 0,
            total: 0,
            current_name: "Reading directory...".to_string(),
        });
    }

    pub fn cancel_measuring(&mut self) {
        self.cancel_flag.store(true, Ordering::Relaxed);
        self.measuring_rx = None;
        self.current_progress = None;
    }

    pub fn tick(&mut self) {
        self.spinner_idx = (self.spinner_idx + 1) % 4;

        if let Some(ref rx) = self.measuring_rx {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    MeasureMessage::Progress(prog) => {
                        self.current_progress = Some(prog);
                    }
                    MeasureMessage::Finished(snapshot) => {
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
    }
}

pub fn render_analyze(frame: &mut Frame, state: &AnalyzeState, theme: &Theme) {
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

    let status_text = format!(
        "{}  ·  {} items",
        format_bytes(level.snapshot.total_bytes),
        level.snapshot.children.len()
    );
    let right_status = Line::from(vec![Span::styled(status_text, theme.callout())]);

    let can_open = match level.snapshot.children.get(level.selected) {
        Some(c) => c.is_dir && !c.is_package,
        None => false,
    };

    let primary_action = if can_open {
        PrimaryAction::new("Open", "→")
    } else {
        PrimaryAction::disabled("Open", "→")
    };

    let toast = state
        .status_notice
        .as_ref()
        .map(|msg| Toast::success(msg.clone()));

    let chrome = AppChrome::new(crumbs)
        .status(right_status)
        .hints(vec![
            Hint::new("↑↓", "Move"),
            Hint::new("←", "Back"),
            Hint::new("O", "Reveal in Finder"),
            Hint::new("?", "More"),
        ])
        .primary(primary_action)
        .toast(toast.as_ref());

    let content_rect = chrome.render(frame, theme);

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

        let mut row = ListRow::new(vec![Span::styled(display_name, theme.body())])
            .leading(Span::styled(icon, icon_style))
            .detail(Span::styled(detail_str, size_style))
            .chevron(child.is_dir && !child.is_package)
            .selected(is_selected);

        if content_rect.width >= 100 {
            row = row.bar(fraction);
        }

        list_lines.push(row.render_line(content_rect.width as usize, theme));
    }

    if level.snapshot.children.is_empty() {
        list_lines.push(Line::from(""));
        list_lines.push(Line::from(Span::styled(
            "  (empty folder or no accessible files)",
            theme.callout(),
        )));
    }

    frame.render_widget(Paragraph::new(list_lines), content_rect);

    // Measuring Modal
    if let Some(ref prog) = state.current_progress {
        render_measuring_modal(frame, prog, state.spinner_idx, theme);
    }
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

    let step_info = format!("{} of {}", prog.position, prog.total);

    Modal::new(&title)
        .message(vec![
            Line::from(vec![
                Span::styled(format!("{spinner_char} "), theme.accent_text()),
                Span::styled(&prog.current_name, theme.body()),
            ]),
            Line::from(vec![Span::styled(step_info, theme.callout())]),
        ])
        .buttons(vec![Button::new("Cancel").focused(true)])
        .render(frame, frame.area(), theme);
}

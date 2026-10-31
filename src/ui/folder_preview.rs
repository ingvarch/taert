//! The contents of the folder highlighted in Explore, measured in the background.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::text::{Line, Span};

use super::analyze::{AnalyzeState, ChildItem, FolderSnapshot, SortOrder, StorageCache};
use super::components::gauge::bar;
use super::components::list::render_section_header;
use super::components::text::{fit_width, plural};
use super::theme::Theme;
use crate::core::bytes::format_bytes;
use crate::core::date_utils::format_system_time_opt;
use crate::core::path_utils::{middle_truncate, prettify_path};
use crate::core::stores::IgnoreStore;

// Keeps a selection that only passes through a folder from measuring it.
const SETTLE: Duration = Duration::from_millis(150);
const INDENT: &str = "  ";
const SIZE_W: usize = 9;
const MIN_BAR_W: usize = 8;
const MAX_BAR_W: usize = 30;

struct Job {
    path: PathBuf,
    cancel: Arc<AtomicBool>,
    rx: Receiver<FolderSnapshot>,
}

impl Job {
    fn start(path: &Path, ignore: &IgnoreStore, cache: &Arc<RwLock<StorageCache>>) -> Self {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));

        let target = path.to_path_buf();
        let ignore = ignore.clone();
        let cache = cache.clone();
        let flag = cancel.clone();
        thread::spawn(move || {
            let snapshot = AnalyzeState::scan_folder(&target, &ignore, None, flag.clone(), cache);
            if !flag.load(Ordering::Relaxed) {
                let _ = tx.send(snapshot);
            }
        });

        Self {
            path: path.to_path_buf(),
            cancel,
            rx,
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

#[derive(Default)]
pub struct FolderPreview {
    pub visible: Cell<bool>,
    snapshot: Option<FolderSnapshot>,
    pending: Option<(PathBuf, Instant)>,
    job: Option<Job>,
}

impl FolderPreview {
    pub fn snapshot_of(&self, path: &Path) -> Option<&FolderSnapshot> {
        self.snapshot.as_ref().filter(|s| s.path == path)
    }

    fn show(&mut self, mut snapshot: FolderSnapshot) {
        SortOrder::Size.sort_children(&mut snapshot.children);
        self.snapshot = Some(snapshot);
        self.pending = None;
        self.job = None;
    }

    /// Call on every tick. `target` is the folder to show, if any.
    pub fn follow(
        &mut self,
        target: Option<&Path>,
        now: Instant,
        ignore: &IgnoreStore,
        cache: &Arc<RwLock<StorageCache>>,
    ) {
        let Some(target) = target.filter(|path| self.snapshot_of(path).is_none()) else {
            self.pending = None;
            self.job = None;
            return;
        };

        if let Some(job) = &self.job {
            if job.path == target {
                if let Ok(snapshot) = job.rx.try_recv() {
                    self.show(snapshot);
                }
                return;
            }
            self.job = None;
        }

        let cached = cache.read().unwrap().get_snapshot(target);
        if let Some(snapshot) = cached {
            self.show(snapshot);
            return;
        }

        match &self.pending {
            Some((path, since)) if path == target => {
                if now.duration_since(*since) >= SETTLE {
                    self.job = Some(Job::start(target, ignore, cache));
                    self.pending = None;
                }
            }
            _ => self.pending = Some((target.to_path_buf(), now)),
        }
    }
}

fn item_icon<'t>(item: &ChildItem, theme: &'t Theme) -> &'t str {
    if item.is_package {
        theme.g.package
    } else if item.is_dir {
        theme.g.folder
    } else {
        theme.g.file
    }
}

/// The largest items of a folder, as many as fit in `rows`.
pub fn folder_rows(
    snapshot: &FolderSnapshot,
    width: usize,
    rows: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let children = &snapshot.children;
    let total = snapshot.total_bytes.max(1) as f64;
    let bar_w = (width / 4).clamp(MIN_BAR_W, MAX_BAR_W);
    let name_w = width.saturating_sub(INDENT.len() + 2 + 1 + bar_w + 1 + SIZE_W);
    let hidden = children.len().saturating_sub(rows);
    let shown = if hidden > 0 {
        rows.saturating_sub(1)
    } else {
        rows
    };

    let mut lines: Vec<Line<'static>> = children
        .iter()
        .take(shown)
        .map(|item| {
            let name = format!("{}{}", item.name, item.suffix());
            let mut spans = vec![
                Span::styled(
                    format!("{INDENT}{} ", item_icon(item, theme)),
                    theme.callout(),
                ),
                Span::styled(fit_width(&name, name_w), theme.body()),
                Span::raw(" "),
            ];
            let fraction = item.size_bytes as f64 / total;
            spans.extend(bar(fraction, bar_w as u16, theme.p.accent, theme));
            spans.push(Span::styled(
                format!(" {:>SIZE_W$}", format_bytes(item.size_bytes)),
                theme.size(item.size_bytes),
            ));
            Line::from(spans)
        })
        .collect();

    if hidden > 0 && rows > 0 {
        lines.push(Line::from(Span::styled(
            format!("{INDENT}and {} more", children.len() - shown),
            theme.footnote(),
        )));
    }
    lines
}

/// The panel beside the list: what the highlighted item is and, for a folder, what it holds.
pub fn preview_lines(
    item: &ChildItem,
    parent_bytes: u64,
    preview: &FolderPreview,
    width: usize,
    height: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let size = format_bytes(item.size_bytes);
    let title = format!("{}{}", item.name, item.suffix());
    let title_w = width.saturating_sub(INDENT.len() + 2 + size.len() + 1);
    let share = item.size_bytes as f64 / parent_bytes.max(1) as f64 * 100.0;
    let path = prettify_path(&item.path);

    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!("{INDENT}{} ", item_icon(item, theme)),
                theme.accent_text(),
            ),
            Span::styled(fit_width(&title, title_w), theme.headline()),
            Span::styled(format!(" {size}"), theme.size(item.size_bytes)),
        ]),
        Line::from(Span::styled(
            format!(
                "{INDENT}{share:.1}% of this folder · Created {}",
                format_system_time_opt(item.created)
            ),
            theme.callout(),
        )),
        Line::from(Span::styled(
            format!(
                "{INDENT}{}",
                middle_truncate(&path, width.saturating_sub(INDENT.len()))
            ),
            theme.footnote(),
        )),
    ];

    if !item.is_dir || item.is_package {
        return lines;
    }

    lines.push(Line::default());
    match preview.snapshot_of(&item.path) {
        Some(snapshot) if snapshot.children.is_empty() => {
            lines.push(render_section_header("Contents", None, width, theme));
            lines.push(Line::from(Span::styled(
                format!("{INDENT}Empty folder or no accessible files"),
                theme.callout(),
            )));
        }
        Some(snapshot) => {
            let count = plural(snapshot.children.len(), "item", "items");
            lines.push(render_section_header(
                "Contents",
                Some(&count),
                width,
                theme,
            ));
            let rows = height.saturating_sub(lines.len());
            lines.extend(folder_rows(snapshot, width, rows, theme));
        }
        None => {
            lines.push(render_section_header("Contents", None, width, theme));
            lines.push(Line::from(Span::styled(
                format!("{INDENT}Measuring…"),
                theme.footnote(),
            )));
        }
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::env::UiEnv;
    use std::fs;
    use std::thread::sleep;

    const LATER: Duration = Duration::from_secs(1);

    fn new_cache() -> Arc<RwLock<StorageCache>> {
        Arc::new(RwLock::new(StorageCache::new()))
    }

    fn item(path: &Path, name: &str, size_bytes: u64) -> ChildItem {
        ChildItem {
            name: name.to_string(),
            path: path.join(name),
            size_bytes,
            is_dir: true,
            is_package: false,
            selected: false,
            created: None,
        }
    }

    fn snapshot(path: &Path, children: Vec<ChildItem>) -> FolderSnapshot {
        FolderSnapshot {
            path: path.to_path_buf(),
            total_bytes: children.iter().map(|c| c.size_bytes).sum(),
            children,
            inaccessible_count: 0,
            skipped_count: 0,
        }
    }

    fn text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn a_cached_folder_shows_at_once_with_the_largest_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        let cache = new_cache();
        let children = vec![item(path, "small", 1), item(path, "large", 9)];
        cache
            .write()
            .unwrap()
            .put_snapshot(path.to_path_buf(), snapshot(path, children));
        let mut preview = FolderPreview::default();

        preview.follow(Some(path), Instant::now(), &IgnoreStore::default(), &cache);

        let shown = preview.snapshot_of(path).unwrap();
        assert_eq!(shown.children[0].name, "large");
    }

    #[test]
    fn a_selection_passing_through_starts_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut preview = FolderPreview::default();

        preview.follow(
            Some(dir.path()),
            Instant::now(),
            &IgnoreStore::default(),
            &new_cache(),
        );

        assert!(preview.job.is_none());
    }

    #[test]
    fn a_selection_that_moves_on_quickly_starts_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (ignore, cache) = (IgnoreStore::default(), new_cache());
        let mut preview = FolderPreview::default();
        let now = Instant::now();

        preview.follow(Some(dir.path()), now, &ignore, &cache);
        preview.follow(Some(dir.path()), now + SETTLE / 2, &ignore, &cache);

        assert!(preview.job.is_none());
    }

    #[test]
    fn a_resting_selection_is_measured_in_the_background() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("file.bin"), b"123").unwrap();
        let (ignore, cache) = (IgnoreStore::default(), new_cache());
        let mut preview = FolderPreview::default();
        let now = Instant::now();

        preview.follow(Some(dir.path()), now, &ignore, &cache);
        preview.follow(Some(dir.path()), now + LATER, &ignore, &cache);
        assert!(preview.job.is_some());

        for _ in 0..1000 {
            preview.follow(Some(dir.path()), now + LATER, &ignore, &cache);
            if preview.snapshot_of(dir.path()).is_some() {
                break;
            }
            sleep(Duration::from_millis(2));
        }

        let shown = preview.snapshot_of(dir.path()).unwrap();
        assert_eq!(shown.children[0].name, "file.bin");
    }

    fn measuring(path: &Path, cache: &Arc<RwLock<StorageCache>>) -> FolderPreview {
        let ignore = IgnoreStore::default();
        let mut preview = FolderPreview::default();
        let now = Instant::now();
        preview.follow(Some(path), now, &ignore, cache);
        preview.follow(Some(path), now + LATER, &ignore, cache);
        preview
    }

    #[test]
    fn moving_on_cancels_the_measurement() {
        let (first, second) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let cache = new_cache();
        let mut preview = measuring(first.path(), &cache);
        let cancel = preview.job.as_ref().unwrap().cancel.clone();

        preview.follow(
            Some(second.path()),
            Instant::now(),
            &IgnoreStore::default(),
            &cache,
        );

        assert!(cancel.load(Ordering::Relaxed));
    }

    #[test]
    fn leaving_the_screen_cancels_the_measurement() {
        let dir = tempfile::tempdir().unwrap();
        let preview = measuring(dir.path(), &new_cache());
        let cancel = preview.job.as_ref().unwrap().cancel.clone();

        drop(preview);

        assert!(cancel.load(Ordering::Relaxed));
    }

    #[test]
    fn rows_that_do_not_fit_are_counted_on_the_last_line() {
        let theme = Theme::with_env(UiEnv::default());
        let path = Path::new("/folder");
        let children: Vec<_> = ["a", "b", "c", "d"]
            .iter()
            .map(|name| item(path, name, 10))
            .collect();

        let lines = folder_rows(&snapshot(path, children), 60, 3, &theme);

        assert_eq!(lines.len(), 3);
        assert!(text(&lines)[2].contains("and 2 more"));
    }

    #[test]
    fn every_row_fills_the_panel() {
        let theme = Theme::with_env(UiEnv::default());
        let path = Path::new("/folder");
        let name = "a-very-long-folder-name-that-does-not-fit-the-panel-at-all";
        let children = vec![item(path, name, 10), item(path, "short", 1)];

        let lines = folder_rows(&snapshot(path, children), 50, 5, &theme);

        assert_eq!(lines[0].width(), 50);
        assert_eq!(lines[1].width(), 50);
    }

    #[test]
    fn a_file_shows_its_details_without_waiting_for_contents() {
        let theme = Theme::with_env(UiEnv::default());
        let mut file = item(Path::new("/folder"), "movie.mov", 25);
        file.is_dir = false;

        let lines = preview_lines(&file, 100, &FolderPreview::default(), 60, 20, &theme);
        let lines = text(&lines).join("\n");

        assert!(lines.contains("movie.mov"));
        assert!(lines.contains("25.0% of this folder"));
        assert!(!lines.contains("Measuring"));
    }

    #[test]
    fn a_folder_waits_for_its_contents() {
        let theme = Theme::with_env(UiEnv::default());
        let folder = item(Path::new("/folder"), "sub", 25);

        let lines = preview_lines(&folder, 100, &FolderPreview::default(), 60, 20, &theme);

        assert!(text(&lines).join("\n").contains("Measuring"));
    }
}

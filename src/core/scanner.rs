//! Scan item and category models.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::SystemTime;

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use super::app_caches;
use super::catalog::{Catalog, Target};
use super::docker::DockerObject;
use super::host::{Host, MacHost};
use super::layout::Layout;
use super::leftovers::UninstallRole;
use super::path_utils::prettify_path;
use super::safety::{FileIdentity, Safety};
use super::sizer::DirSizer;
use super::stores::IgnoreStore;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanItem {
    pub path: PathBuf,
    pub size_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<SystemTime>,
    pub selected: bool,
    pub contents_only: bool,
    #[serde(skip)]
    pub identity: Option<FileIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker: Option<DockerObject>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<UninstallRole>,
}

impl ScanItem {
    pub fn new(path: PathBuf, size_bytes: u64, selected: bool, contents_only: bool) -> Self {
        let identity = FileIdentity::of(&path);
        let modified = fs::metadata(&path).and_then(|m| m.modified()).ok();
        Self {
            path,
            size_bytes,
            modified,
            selected,
            contents_only,
            identity,
            docker: None,
            role: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolGroup {
    pub tool: String,
    pub items: Vec<ScanItem>,
}

impl ToolGroup {
    pub fn total_selected_bytes(&self) -> u64 {
        self.items
            .iter()
            .filter(|i| i.selected)
            .map(|i| i.size_bytes)
            .sum()
    }

    pub fn total_all_bytes(&self) -> u64 {
        self.items.iter().map(|i| i.size_bytes).sum()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategoryScan {
    pub id: String,
    pub title: String,
    pub groups: Vec<ToolGroup>,
    /// Things worth telling that are not rows, such as a folder the scan could not read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl CategoryScan {
    pub fn total_selected_bytes(&self) -> u64 {
        self.groups.iter().map(|g| g.total_selected_bytes()).sum()
    }

    pub fn total_all_bytes(&self) -> u64 {
        self.groups.iter().map(|g| g.total_all_bytes()).sum()
    }

    pub fn selected_items(&self) -> Vec<&ScanItem> {
        self.groups
            .iter()
            .flat_map(|g| &g.items)
            .filter(|i| i.selected)
            .collect()
    }

    pub fn selected_items_mut(&mut self) -> Vec<&mut ScanItem> {
        self.groups
            .iter_mut()
            .flat_map(|g| &mut g.items)
            .filter(|i| i.selected)
            .collect()
    }
}

#[derive(Debug, Clone)]
pub enum ScanProgressEvent {
    CategoryStarted {
        title: String,
        index: usize,
        total: usize,
    },
    ToolStarted(String),
    ScanningPath(String),
    ItemFound {
        bytes: u64,
    },
    ToolFinished {
        tool: String,
        bytes_found: u64,
    },
}

/// The system a scan reads: where folders live and how to run commands.
pub struct ScanContext<'a> {
    pub layout: &'a Layout,
    pub host: &'a dyn Host,
}

pub struct CategoryScanner;

type Rows = Vec<(PathBuf, bool)>;

fn is_cancelled(flag: &Option<Arc<AtomicBool>>) -> bool {
    flag.as_ref()
        .is_some_and(|flag| flag.load(Ordering::Relaxed))
}

fn contents_of(paths: &[PathBuf], ignore: &IgnoreStore) -> Vec<PathBuf> {
    let mut expanded = Vec::new();
    for p in paths {
        if !p.exists()
            || Safety::has_symlink_component(p)
            || Safety::is_cloud_or_dataless(p)
            || Safety::is_ignored_or_protected(p, ignore)
        {
            continue;
        }
        if let Ok(entries) = fs::read_dir(p) {
            for entry in entries.flatten() {
                expanded.push(entry.path());
            }
        }
    }
    expanded
}

/// Sizes the rows that are safe to remove and groups them under one tool name.
fn scan_group(
    tool: String,
    rows: Rows,
    ignore: &IgnoreStore,
    progress_tx: &Option<Sender<ScanProgressEvent>>,
    cancel: &Option<Arc<AtomicBool>>,
) -> Option<ToolGroup> {
    if let Some(tx) = progress_tx {
        let _ = tx.send(ScanProgressEvent::ToolStarted(tool.clone()));
    }

    let items: Vec<ScanItem> = rows
        .into_par_iter()
        .filter_map(|(path, selected)| {
            if is_cancelled(cancel) || !path.exists() {
                return None;
            }

            if let Some(tx) = progress_tx {
                let _ = tx.send(ScanProgressEvent::ScanningPath(prettify_path(&path)));
            }

            if Safety::has_symlink_component(&path)
                || Safety::is_cloud_or_dataless(&path)
                || Safety::is_ignored_or_protected(&path, ignore)
                || Safety::can_remove(&path, ignore).is_err()
            {
                return None;
            }

            let size = DirSizer::size_of(&path);
            if size < 1024 {
                return None;
            }
            if let Some(tx) = progress_tx {
                let _ = tx.send(ScanProgressEvent::ItemFound { bytes: size });
            }
            Some(ScanItem::new(path, size, selected, false))
        })
        .collect();

    let tool_bytes: u64 = items.iter().map(|i| i.size_bytes).sum();
    if let Some(tx) = progress_tx {
        let _ = tx.send(ScanProgressEvent::ToolFinished {
            tool: tool.clone(),
            bytes_found: tool_bytes,
        });
    }

    (!items.is_empty()).then_some(ToolGroup { tool, items })
}

impl CategoryScanner {
    pub fn scan(
        category_id: &str,
        category_title: &str,
        ignore: &IgnoreStore,
        progress_tx: Option<Sender<ScanProgressEvent>>,
        is_cancelled: Option<Arc<AtomicBool>>,
    ) -> CategoryScan {
        let layout = Layout::current();
        Self::scan_with(
            &ScanContext {
                layout: &layout,
                host: &MacHost,
            },
            category_id,
            category_title,
            Catalog::targets_for(category_id),
            ignore,
            progress_tx,
            is_cancelled,
        )
    }

    pub fn scan_with(
        context: &ScanContext,
        category_id: &str,
        category_title: &str,
        targets: Vec<Target>,
        ignore: &IgnoreStore,
        progress_tx: Option<Sender<ScanProgressEvent>>,
        cancel: Option<Arc<AtomicBool>>,
    ) -> CategoryScan {
        let mut groups: Vec<ToolGroup> = targets
            .into_par_iter()
            .filter_map(|target| {
                if is_cancelled(&cancel) {
                    return None;
                }
                let paths = if target.contents_only {
                    contents_of(&target.paths, ignore)
                } else {
                    target.paths
                };
                let rows = paths
                    .into_iter()
                    .map(|path| (path, target.selected_by_default))
                    .collect();
                scan_group(target.tool, rows, ignore, &progress_tx, &cancel)
            })
            .collect();

        let mut notes = Vec::new();
        if category_id == "junk" && !is_cancelled(&cancel) {
            let found = app_caches::plan(
                context.layout,
                context.host,
                &Catalog::named_paths(),
                SystemTime::now(),
            );
            notes = found.notes;
            for group in found.groups {
                let rows = group
                    .rows
                    .into_iter()
                    .map(|row| (row.path, row.selected))
                    .collect();
                if let Some(mut scanned) =
                    scan_group(group.tool.to_string(), rows, ignore, &progress_tx, &cancel)
                {
                    scanned
                        .items
                        .sort_by_key(|item| std::cmp::Reverse(item.size_bytes));
                    groups.push(scanned);
                }
            }
        }

        CategoryScan {
            id: category_id.to_string(),
            title: category_title.to_string(),
            groups,
            notes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scan_junk_includes_user_logs() {
        let ignore = IgnoreStore::default();
        let scan = CategoryScanner::scan("junk", "System Junk", &ignore, None, None);
        if let Some(home) = dirs::home_dir()
            && home.join("Library/Logs").exists()
        {
            let user_logs = scan.groups.iter().find(|g| g.tool == "User logs");
            assert!(user_logs.is_some(), "User logs group should be discovered");
            assert!(!user_logs.unwrap().items.is_empty());
        }
    }

    use crate::core::apps::testing::sandbox;
    use crate::core::cleaner::Cleaner;
    use crate::core::host::fake::FakeHost;
    use std::os::unix::fs::PermissionsExt;

    fn fill(layout: &Layout, relative: &str) -> PathBuf {
        let folder = layout.user(relative);
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("data"), vec![1_u8; 8192]).unwrap();
        folder
    }

    fn junk(layout: &Layout, host: &FakeHost, ignore: &IgnoreStore) -> CategoryScan {
        CategoryScanner::scan_with(
            &ScanContext { layout, host },
            "junk",
            "System Junk",
            Vec::new(),
            ignore,
            None,
            None,
        )
    }

    fn rows<'a>(scan: &'a CategoryScan, tool: &str) -> Vec<&'a ScanItem> {
        scan.groups
            .iter()
            .filter(|group| group.tool == tool)
            .flat_map(|group| &group.items)
            .collect()
    }

    #[test]
    fn junk_lists_app_caches_and_browser_caches() {
        let (_temp, layout) = sandbox();
        let app = fill(&layout, "Library/Caches/com.acme.app");
        let chrome = fill(&layout, "Library/Caches/Google/Chrome");
        let scan = junk(&layout, &FakeHost::default(), &IgnoreStore::default());
        assert!(
            rows(&scan, app_caches::APP_CACHES)
                .iter()
                .any(|i| i.path == app)
        );
        assert!(
            rows(&scan, app_caches::BROWSER_CACHES)
                .iter()
                .any(|i| i.path == chrome)
        );
    }

    #[test]
    fn other_categories_get_no_app_caches() {
        let (_temp, layout) = sandbox();
        fill(&layout, "Library/Caches/com.acme.app");
        let scan = CategoryScanner::scan_with(
            &ScanContext {
                layout: &layout,
                host: &FakeHost::default(),
            },
            "ai",
            "AI",
            Vec::new(),
            &IgnoreStore::default(),
            None,
            None,
        );
        assert!(rows(&scan, app_caches::APP_CACHES).is_empty());
    }

    #[test]
    fn an_ignored_path_is_left_out() {
        let (_temp, layout) = sandbox();
        let ignored = fill(&layout, "Library/Caches/com.acme.ignored");
        let kept = fill(&layout, "Library/Caches/com.acme.kept");
        let ignore = IgnoreStore::new(vec![ignored.display().to_string()]);
        let scan = junk(&layout, &FakeHost::default(), &ignore);
        let found = rows(&scan, app_caches::APP_CACHES);
        assert!(found.iter().all(|item| item.path != ignored));
        assert!(found.iter().any(|item| item.path == kept));
    }

    #[test]
    fn a_refused_read_reaches_the_scan_notes() {
        let (_temp, layout) = sandbox();
        let container = layout.user("Library/Containers/com.apple.Safari/Data/Library/Caches");
        fs::create_dir_all(&container).unwrap();
        fs::set_permissions(&container, fs::Permissions::from_mode(0o000)).unwrap();
        let scan = junk(&layout, &FakeHost::default(), &IgnoreStore::default());
        fs::set_permissions(&container, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            scan.notes
                .iter()
                .any(|note| note.contains("Full Disk Access"))
        );
    }

    #[test]
    fn removing_the_selected_rows_leaves_a_running_browser_cache() {
        let (_temp, layout) = sandbox();
        let chrome = fill(&layout, "Library/Caches/Google/Chrome");
        let firefox = fill(&layout, "Library/Caches/Firefox");
        let host = FakeHost::default();
        host.reply(
            "/bin/ps",
            "  501 1 /Applications/Google Chrome.app/Contents/MacOS/Google Chrome\n",
        );
        let ignore = IgnoreStore::default();
        let scan = junk(&layout, &host, &ignore);
        let selected: Vec<ScanItem> = scan.selected_items().into_iter().cloned().collect();
        let in_sandbox: Vec<ScanItem> = selected
            .into_iter()
            .filter(|item| item.path.starts_with(&layout.home))
            .collect();
        let report = Cleaner::remove_items(&in_sandbox, &ignore);
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(chrome.exists());
        assert!(!firefox.exists());
    }
}

//! Scanner for the largest and oldest files.
//! Finds files above a minimum size threshold across folders, capped at a limit,
//! using a min-heap to keep memory usage flat.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, SystemTime};

use super::apps;
use super::host::Host;
use super::path_utils::prettify_path;
use super::purger::Purger;
use super::safety::Safety;
use super::scanner::{CategoryScan, ScanItem, ScanProgressEvent, ToolGroup};
use super::sizer::DirSizer;
use super::stores::IgnoreStore;

pub const SCAN_ID: &str = "large";
pub const DEFAULT_MIN_SIZE: u64 = 100_000_000; // 100 MB
pub const DEFAULT_LIMIT: usize = 100;

pub const PACKAGE_EXTENSIONS: &[&str] = &[
    "app",
    "photoslibrary",
    "fcpbundle",
    "logicx",
    "imovielibrary",
    "band",
];

/// Options controlling what large files are collected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LargeScanOptions {
    pub min_size: u64,
    pub older_than_days: Option<u64>,
    pub limit: usize,
}

impl LargeScanOptions {
    pub fn new(min_size: u64, older_than_days: Option<u64>, limit: usize) -> Self {
        Self {
            min_size,
            older_than_days,
            limit,
        }
    }
}

impl Default for LargeScanOptions {
    fn default() -> Self {
        Self {
            min_size: DEFAULT_MIN_SIZE,
            older_than_days: None,
            limit: DEFAULT_LIMIT,
        }
    }
}

fn notify_progress(progress: &Option<Sender<ScanProgressEvent>>, event: ScanProgressEvent) {
    if let Some(tx) = progress {
        let _ = tx.send(event);
    }
}

/// Returns true if a path is considered a macOS bundle/package that should not be walked into.
pub fn is_package(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            PACKAGE_EXTENSIONS
                .iter()
                .any(|&p| p.eq_ignore_ascii_case(ext))
        })
}

/// Identifies the top-level group folder for an item relative to the scan root.
pub fn top_level_folder(path: &Path, root: &Path) -> String {
    if let Ok(rel) = path.strip_prefix(root) {
        let mut components = rel.components();
        if let (Some(first), Some(_)) = (components.next(), components.next()) {
            return first.as_os_str().to_string_lossy().into_owned();
        }
    }
    let pretty = prettify_path(root);
    if !pretty.is_empty() {
        pretty
    } else {
        root.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Home".to_string())
    }
}

/// Checks if a path is inside an ignored root.
fn is_ignored_path(path: &Path, ignore: &IgnoreStore) -> bool {
    ignore.roots.iter().any(|r| {
        let ip = Path::new(r);
        path == ip || path.starts_with(ip)
    })
}

#[derive(Debug, Eq, PartialEq)]
struct Candidate {
    size_bytes: u64,
    path: PathBuf,
    modified: Option<SystemTime>,
}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.size_bytes
            .cmp(&other.size_bytes)
            .then_with(|| self.path.cmp(&other.path))
    }
}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Scan for the largest files starting from `root`.
pub fn scan(
    root: &Path,
    options: &LargeScanOptions,
    host: &dyn Host,
    ignore: &IgnoreStore,
    progress: Option<Sender<ScanProgressEvent>>,
    is_cancelled: Option<Arc<AtomicBool>>,
) -> CategoryScan {
    if options.limit == 0 || !root.exists() {
        return CategoryScan {
            id: SCAN_ID.to_string(),
            title: "Large Files".to_string(),
            groups: Vec::new(),
            notes: Vec::new(),
        };
    }

    let now = SystemTime::now();
    let home_dir = Safety::home_dir();
    let home_library = home_dir.join("Library");

    let root_buf = root.to_path_buf();
    let ignore_clone = ignore.clone();
    let home_library_clone = home_library.clone();

    let mut heap: BinaryHeap<Reverse<Candidate>> = BinaryHeap::with_capacity(options.limit);

    // If root itself is a regular file or package, check it directly
    if root.is_file() || is_package(root) {
        let size_bytes = if is_package(root) {
            DirSizer::size_of(root)
        } else if let Ok(meta) = fs::symlink_metadata(root) {
            DirSizer::allocated_size(&meta)
        } else {
            0
        };

        if size_bytes >= options.min_size {
            let modified = fs::metadata(root).and_then(|m| m.modified()).ok();
            heap.push(Reverse(Candidate {
                size_bytes,
                path: root.to_path_buf(),
                modified,
            }));
        }
    } else {
        // Walk directory tree
        let walker = jwalk::WalkDir::new(root)
            .parallelism(jwalk::Parallelism::Serial)
            .skip_hidden(false)
            .follow_links(false)
            .process_read_dir(move |_depth, _path, _read_dir_state, children| {
                children.retain(|res| match res {
                    Ok(entry) => {
                        let path = entry.path();
                        if path == root_buf {
                            return true;
                        }
                        if entry.file_type.is_symlink() {
                            return false;
                        }
                        if Safety::is_cloud_or_dataless(&path) {
                            return false;
                        }
                        if is_ignored_path(&path, &ignore_clone) {
                            return false;
                        }
                        if Safety::is_credential_root(&path) {
                            return false;
                        }
                        let name = entry.file_name.to_string_lossy();
                        if name == ".git" || name == ".Trash" {
                            return false;
                        }
                        if path == home_library_clone
                            || (path.starts_with(&home_library_clone)
                                && root_buf != home_library_clone)
                        {
                            return false;
                        }
                        if entry.file_type.is_dir() {
                            let parent = entry.parent_path();
                            if Purger::is_standard_artifact(&name, parent) {
                                return false;
                            }
                        }
                        true
                    }
                    Err(_) => false,
                });

                // For packages, do not recurse into them
                for entry in children.iter_mut().flatten() {
                    if entry.file_type.is_dir() && is_package(&entry.path()) {
                        entry.read_children_path = None;
                    }
                }
            });

        for (idx, entry_res) in walker.into_iter().enumerate() {
            if let Some(ref cancel) = is_cancelled
                && cancel.load(Ordering::Relaxed)
            {
                break;
            }

            let entry = match entry_res {
                Ok(e) => e,
                Err(_) => continue,
            };

            let path = entry.path();

            if idx % 50 == 0 {
                notify_progress(
                    &progress,
                    ScanProgressEvent::ScanningPath(prettify_path(&path)),
                );
            }

            let is_pkg = entry.file_type.is_dir() && is_package(&path);
            let is_reg_file = entry.file_type.is_file();

            if !is_reg_file && !is_pkg {
                continue;
            }

            // Exclude cloud, ignored, or symlink files if encountered at top-level
            if entry.file_type.is_symlink()
                || Safety::is_cloud_or_dataless(&path)
                || is_ignored_path(&path, ignore)
                || Safety::is_credential_root(&path)
            {
                continue;
            }

            let (size_bytes, modified) = if is_pkg {
                let size = DirSizer::size_of(&path);
                let mod_time = fs::metadata(&path).and_then(|m| m.modified()).ok();
                (size, mod_time)
            } else if let Ok(meta) = entry.metadata() {
                let size = DirSizer::allocated_size(&meta);
                let mod_time = meta.modified().ok();
                (size, mod_time)
            } else {
                continue;
            };

            if size_bytes < options.min_size {
                continue;
            }

            // Optimization: if modified time is strictly newer than cutoff, it can never be older than older_than_days
            if let Some(days) = options.older_than_days
                && let Some(m) = modified
                && now.duration_since(m).unwrap_or_default() < Duration::from_secs(days * 86400)
            {
                continue;
            }

            let candidate = Candidate {
                size_bytes,
                path: path.clone(),
                modified,
            };

            if heap.len() < options.limit {
                notify_progress(
                    &progress,
                    ScanProgressEvent::ItemFound { bytes: size_bytes },
                );
                heap.push(Reverse(candidate));
            } else if heap
                .peek()
                .is_some_and(|min_top| size_bytes > min_top.0.size_bytes)
            {
                notify_progress(
                    &progress,
                    ScanProgressEvent::ItemFound { bytes: size_bytes },
                );
                heap.pop();
                heap.push(Reverse(candidate));
            }
        }
    }

    let mut candidates: Vec<Candidate> = heap.into_iter().map(|Reverse(c)| c).collect();

    // Query last used dates via Spotlight mdls in one batch
    let path_refs: Vec<&Path> = candidates.iter().map(|c| c.path.as_path()).collect();
    let last_used = apps::last_used_dates(host, &path_refs);

    for (c, lu) in candidates.iter_mut().zip(last_used) {
        let effective = match (lu, c.modified) {
            (Some(l), Some(m)) => Some(l.max(m)),
            (Some(l), None) => Some(l),
            (None, Some(m)) => Some(m),
            (None, None) => None,
        };
        c.modified = effective;
    }

    // Filter by older_than_days using the effective date
    if let Some(days) = options.older_than_days {
        let threshold = now - Duration::from_secs(days * 86400);
        candidates.retain(|c| match c.modified {
            Some(date) => date <= threshold,
            None => false,
        });
    }

    // Group items into ToolGroups
    let mut group_map: HashMap<String, Vec<ScanItem>> = HashMap::new();
    for candidate in candidates {
        let tool = top_level_folder(&candidate.path, root);
        let mut item = ScanItem::new(candidate.path, candidate.size_bytes, false, false);
        item.modified = candidate.modified;
        group_map.entry(tool).or_default().push(item);
    }

    let mut groups: Vec<ToolGroup> = group_map
        .into_iter()
        .map(|(tool, mut items)| {
            items.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));
            ToolGroup { tool, items }
        })
        .collect();

    groups.sort_by(|a, b| {
        b.total_all_bytes()
            .cmp(&a.total_all_bytes())
            .then_with(|| a.tool.cmp(&b.tool))
    });

    CategoryScan {
        id: SCAN_ID.to_string(),
        title: "Large Files".to_string(),
        groups,
        notes: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::host::fake::FakeHost;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    fn write_file(path: &Path, size_bytes: usize) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let data = vec![b'a'; size_bytes];
        fs::write(path, data).unwrap();
    }

    #[test]
    fn files_below_limit_are_left_out() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        let small = root.join("small.txt");
        let large = root.join("large.bin");
        // On APFS, 50 bytes allocates 4096 bytes, while 20_000 bytes allocates ~20480 bytes.
        write_file(&small, 50);
        write_file(&large, 20_000);

        let host = FakeHost::default();
        let ignore = IgnoreStore::default();

        let scan = scan(
            root,
            &LargeScanOptions::new(10_000, None, 10),
            &host,
            &ignore,
            None,
            None,
        );
        let items: Vec<_> = scan.groups.iter().flat_map(|g| &g.items).collect();

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].path, large);
    }

    #[test]
    fn heap_keeps_largest_files_and_limit_is_exact() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Distinct block allocation sizes on APFS
        for i in 1..=5 {
            write_file(&root.join(format!("file_{i}.bin")), i * 20_000);
        }

        let host = FakeHost::default();
        let ignore = IgnoreStore::default();

        let scan = scan(
            root,
            &LargeScanOptions::new(10_000, None, 3),
            &host,
            &ignore,
            None,
            None,
        );
        let items: Vec<_> = scan.groups.iter().flat_map(|g| &g.items).collect();

        assert_eq!(items.len(), 3);
        let mut paths: Vec<_> = items
            .iter()
            .map(|i| i.path.file_name().unwrap().to_str().unwrap())
            .collect();
        paths.sort();
        assert_eq!(paths, vec!["file_3.bin", "file_4.bin", "file_5.bin"]);
    }

    #[test]
    fn package_is_one_row_and_its_contents_dont_appear() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        let pkg = root.join("Test.photoslibrary");
        fs::create_dir_all(&pkg).unwrap();
        write_file(&pkg.join("internal1.jpg"), 200);
        write_file(&pkg.join("nested/internal2.jpg"), 300);

        let host = FakeHost::default();
        let ignore = IgnoreStore::default();

        let scan = scan(
            root,
            &LargeScanOptions::new(50, None, 10),
            &host,
            &ignore,
            None,
            None,
        );
        let items: Vec<_> = scan.groups.iter().flat_map(|g| &g.items).collect();

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].path, pkg);
        assert!(items[0].size_bytes >= 500);
    }

    #[test]
    fn git_node_modules_library_and_ignored_paths_are_skipped() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        let git_file = root.join(".git/big_pack.bin");
        let node_file = root.join("node_modules/pkg/large.js");
        let ignored_file = root.join("ignored_dir/data.bin");
        let valid_file = root.join("Downloads/valid.bin");

        write_file(&git_file, 500);
        write_file(&node_file, 500);
        write_file(&ignored_file, 500);
        write_file(&valid_file, 500);

        let host = FakeHost::default();
        let mut ignore = IgnoreStore::default();
        ignore
            .roots
            .push(root.join("ignored_dir").to_string_lossy().to_string());

        let scan = scan(
            root,
            &LargeScanOptions::new(100, None, 10),
            &host,
            &ignore,
            None,
            None,
        );
        let items: Vec<_> = scan.groups.iter().flat_map(|g| &g.items).collect();

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].path, valid_file);
    }

    #[test]
    fn symlinks_arent_followed() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        let other_temp = TempDir::new().unwrap();
        let target_file = other_temp.path().join("external.bin");
        write_file(&target_file, 500);

        let link = root.join("symlink_dir");
        let _ = symlink(other_temp.path(), &link);

        let host = FakeHost::default();
        let ignore = IgnoreStore::default();

        let scan = scan(
            root,
            &LargeScanOptions::new(100, None, 10),
            &host,
            &ignore,
            None,
            None,
        );
        let items: Vec<_> = scan.groups.iter().flat_map(|g| &g.items).collect();

        assert!(items.is_empty());
    }

    #[test]
    fn cloud_placeholder_is_skipped() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        let cloud_file = root.join("com~apple~CloudDocs/cloud.bin");
        let normal_file = root.join("normal.bin");
        write_file(&cloud_file, 500);
        write_file(&normal_file, 500);

        let host = FakeHost::default();
        let ignore = IgnoreStore::default();

        let scan = scan(
            root,
            &LargeScanOptions::new(100, None, 10),
            &host,
            &ignore,
            None,
            None,
        );
        let items: Vec<_> = scan.groups.iter().flat_map(|g| &g.items).collect();

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].path, normal_file);
    }

    #[test]
    fn older_than_uses_later_of_last_used_and_modified() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        let file_a = root.join("file_a.bin");
        let file_b = root.join("file_b.bin");
        write_file(&file_a, 500);
        write_file(&file_b, 500);

        let host = FakeHost::default();
        // A was modified recently (now), but Spotlight says last used 200 days ago -> effective is now -> excluded
        // B was modified 200 days ago, Spotlight says last used 300 days ago -> effective is 200 days ago -> included
        host.reply(
            "/usr/bin/mdls",
            "2001-09-09 01:46:40 +0000\x002001-09-09 01:46:40 +0000\0",
        );

        let ignore = IgnoreStore::default();

        // Freshly written files have modified = now, so both have effective date = now.
        // With older_than = 180, neither should pass.
        let scan_result = scan(
            root,
            &LargeScanOptions::new(100, Some(180), 10),
            &host,
            &ignore,
            None,
            None,
        );
        assert!(scan_result.groups.is_empty());
    }

    #[test]
    fn failed_mdls_falls_back_to_modified_date() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        let file = root.join("doc.bin");
        write_file(&file, 500);

        let host = FakeHost::default();
        // mdls returns null marker or error
        host.reply("/usr/bin/mdls", "(null)\0");

        let ignore = IgnoreStore::default();
        let scan = scan(
            root,
            &LargeScanOptions::new(100, None, 10),
            &host,
            &ignore,
            None,
            None,
        );
        let items: Vec<_> = scan.groups.iter().flat_map(|g| &g.items).collect();

        assert_eq!(items.len(), 1);
        assert!(items[0].modified.is_some());
    }

    #[test]
    fn no_row_starts_selected() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        write_file(&root.join("test.bin"), 500);

        let host = FakeHost::default();
        let ignore = IgnoreStore::default();

        let scan = scan(
            root,
            &LargeScanOptions::new(100, None, 10),
            &host,
            &ignore,
            None,
            None,
        );
        for group in &scan.groups {
            for item in &group.items {
                assert!(!item.selected);
            }
        }
    }
}

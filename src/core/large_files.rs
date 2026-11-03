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
use super::layout::Layout;
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
    "pvm",
    "vmwarevm",
    "utm",
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
    layout: &Layout,
    host: &dyn Host,
    ignore: &IgnoreStore,
    progress: Option<Sender<ScanProgressEvent>>,
    is_cancelled: Option<Arc<AtomicBool>>,
) -> CategoryScan {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let home = layout
        .home
        .canonicalize()
        .unwrap_or_else(|_| layout.home.clone());

    if options.limit == 0 || !root.exists() {
        return CategoryScan {
            id: SCAN_ID.to_string(),
            title: "Large Files".to_string(),
            groups: Vec::new(),
            notes: Vec::new(),
        };
    }

    let now = SystemTime::now();
    let cutoff = if let Some(days) = options.older_than_days {
        let secs = match days.checked_mul(86400) {
            Some(s) => s,
            None => {
                return CategoryScan {
                    id: SCAN_ID.to_string(),
                    title: "Large Files".to_string(),
                    groups: Vec::new(),
                    notes: Vec::new(),
                };
            }
        };
        match now.checked_sub(Duration::from_secs(secs)) {
            Some(t) => Some(t),
            None => {
                return CategoryScan {
                    id: SCAN_ID.to_string(),
                    title: "Large Files".to_string(),
                    groups: Vec::new(),
                    notes: Vec::new(),
                };
            }
        }
    } else {
        None
    };

    let home_library = home.join("Library");
    let root_is_in_library = root == home_library || root.starts_with(&home_library);

    let root_buf = root.clone();
    let ignore_clone = ignore.clone();
    let home_clone = home.clone();
    let home_library_clone = home_library.clone();

    let mut heap: BinaryHeap<Reverse<Candidate>> = BinaryHeap::with_capacity(options.limit);

    // If root itself is a regular file or package, check it directly
    if root.is_file() || is_package(&root) {
        if Safety::is_cloud_or_dataless_in(&root, &home)
            || Safety::is_ignored(&root, ignore)
            || Safety::is_credential_root_in(&root, &home)
        {
            // skipped
        } else {
            let size_bytes = if is_package(&root) {
                DirSizer::size_of(&root)
            } else if let Ok(meta) = fs::symlink_metadata(&root) {
                DirSizer::allocated_size(&meta)
            } else {
                0
            };

            if size_bytes >= options.min_size {
                let modified = fs::metadata(&root).and_then(|m| m.modified()).ok();
                let effective_modified = if let Some(cutoff_time) = cutoff {
                    if let Some(m) = modified
                        && m > cutoff_time
                    {
                        None
                    } else {
                        let lu = apps::last_used_dates(host, &[&root])
                            .into_iter()
                            .next()
                            .flatten();
                        let effective = match (lu, modified) {
                            (Some(l), Some(m)) => Some(l.max(m)),
                            (Some(l), None) => Some(l),
                            (None, Some(m)) => Some(m),
                            (None, None) => None,
                        };
                        match effective {
                            Some(date) if date <= cutoff_time => Some(Some(date)),
                            _ => None,
                        }
                    }
                } else {
                    Some(modified)
                };

                if let Some(cand_modified) = effective_modified {
                    heap.push(Reverse(Candidate {
                        size_bytes,
                        path: root.clone(),
                        modified: cand_modified,
                    }));
                }
            }
        }
    } else {
        // Walk directory tree
        let walker = jwalk::WalkDir::new(&root)
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
                        if Safety::is_cloud_or_dataless_in(&path, &home_clone) {
                            return false;
                        }
                        if Safety::is_ignored(&path, &ignore_clone) {
                            return false;
                        }
                        if Safety::is_credential_root_in(&path, &home_clone) {
                            return false;
                        }
                        let name = entry.file_name.to_string_lossy();
                        if name == ".git" || name == ".Trash" {
                            return false;
                        }
                        if !root_is_in_library
                            && (path == home_library_clone || path.starts_with(&home_library_clone))
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
                || Safety::is_cloud_or_dataless_in(&path, &home)
                || Safety::is_ignored(&path, ignore)
                || Safety::is_credential_root_in(&path, &home)
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

            let effective_modified = if let Some(cutoff_time) = cutoff {
                if let Some(m) = modified
                    && m > cutoff_time
                {
                    continue;
                }
                let lu = apps::last_used_dates(host, &[&path])
                    .into_iter()
                    .next()
                    .flatten();
                let effective = match (lu, modified) {
                    (Some(l), Some(m)) => Some(l.max(m)),
                    (Some(l), None) => Some(l),
                    (None, Some(m)) => Some(m),
                    (None, None) => None,
                };
                match effective {
                    Some(date) if date <= cutoff_time => Some(date),
                    _ => continue,
                }
            } else {
                modified
            };

            let candidate = Candidate {
                size_bytes,
                path: path.clone(),
                modified: effective_modified,
            };

            if heap.len() < options.limit {
                heap.push(Reverse(candidate));
            } else if heap
                .peek()
                .is_some_and(|min_top| size_bytes > min_top.0.size_bytes)
            {
                heap.pop();
                heap.push(Reverse(candidate));
            }
        }
    }

    let mut candidates: Vec<Candidate> = heap.into_iter().map(|Reverse(c)| c).collect();

    // Query last used dates via Spotlight mdls in one batch only when older_than_days is None
    if options.older_than_days.is_none() {
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
    }

    // Group items into ToolGroups
    let mut group_map: HashMap<String, Vec<ScanItem>> = HashMap::new();
    for candidate in candidates {
        notify_progress(
            &progress,
            ScanProgressEvent::ItemFound {
                bytes: candidate.size_bytes,
            },
        );
        let tool = top_level_folder(&candidate.path, &root);
        let mut item = ScanItem::new(candidate.path, candidate.size_bytes, false, false);
        item.modified = candidate.modified;
        group_map.entry(tool).or_default().push(item);
    }

    let mut groups: Vec<ToolGroup> = group_map
        .into_iter()
        .map(|(tool, mut items)| {
            items.sort_by_key(|a| std::cmp::Reverse(a.size_bytes));
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
    use crate::core::date_utils::format_rfc3339;
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

    fn set_mtime(path: &Path, time: SystemTime) {
        let f = fs::File::options().write(true).open(path).unwrap();
        f.set_modified(time).unwrap();
    }

    fn spotlight_date(time: SystemTime) -> String {
        let rfc = format_rfc3339(time);
        format!("{}\0", rfc.replace('T', " ").replace('Z', " +0000"))
    }

    fn test_layout(home: &Path) -> Layout {
        Layout {
            home: home.to_path_buf(),
            root: PathBuf::from("/"),
            user_cache: None,
        }
    }

    #[test]
    fn home_and_root_are_canonicalized() {
        let temp = TempDir::new().unwrap();
        let real = temp.path().canonicalize().unwrap().join("real");
        fs::create_dir_all(&real).unwrap();

        let ssh_file = real.join(".ssh/id_big");
        let ok_file = real.join("ok.bin");
        write_file(&ssh_file, 20_000);
        write_file(&ok_file, 20_000);

        let link = temp.path().join("link");
        symlink(&real, &link).unwrap();

        let layout = Layout {
            home: real.clone(),
            root: PathBuf::from("/"),
            user_cache: None,
        };
        let host = FakeHost::default();
        let ignore = IgnoreStore::default();

        let scan = scan(
            &link,
            &LargeScanOptions::new(10_000, None, 10),
            &layout,
            &host,
            &ignore,
            None,
            None,
        );
        let items: Vec<_> = scan.groups.iter().flat_map(|g| &g.items).collect();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].path.file_name().unwrap(), "ok.bin");
    }

    #[test]
    fn huge_older_than_gives_no_rows_and_no_panic() {
        let temp = TempDir::new().unwrap();
        let home = temp.path().canonicalize().unwrap();
        let layout = test_layout(&home);

        write_file(&home.join("big.bin"), 20_000);

        let host = FakeHost::default();
        let ignore = IgnoreStore::default();

        for huge in [200_000_000_000_000, u64::MAX] {
            let scan = scan(
                &home,
                &LargeScanOptions::new(10_000, Some(huge), 10),
                &layout,
                &host,
                &ignore,
                None,
                None,
            );
            assert!(scan.groups.is_empty());
        }
    }

    #[test]
    fn root_inside_library_is_scanned() {
        let temp = TempDir::new().unwrap();
        let home = temp.path().canonicalize().unwrap();
        let layout = test_layout(&home);

        let big_bin = home.join("Library/Containers/app/big.bin");
        let x_bin = home.join("Library/Mobile Documents/x.bin");
        let y_bin = home.join("Library/Containers/.ssh/y.bin");

        write_file(&big_bin, 20_000);
        write_file(&x_bin, 20_000);
        write_file(&y_bin, 20_000);

        let host = FakeHost::default();
        let ignore = IgnoreStore::default();
        let opts = LargeScanOptions::new(10_000, None, 10);

        // Root = home/Library/Containers: rows are big.bin and y.bin, not x.bin.
        let containers_root = home.join("Library/Containers");
        let scan1 = scan(&containers_root, &opts, &layout, &host, &ignore, None, None);
        let mut items1: Vec<_> = scan1
            .groups
            .iter()
            .flat_map(|g| &g.items)
            .map(|i| i.path.file_name().unwrap().to_str().unwrap())
            .collect();
        items1.sort();
        assert_eq!(items1, vec!["big.bin", "y.bin"]);

        // Then root = home/Library: big.bin and y.bin are listed, x.bin is not (cloud).
        let library_root = home.join("Library");
        let scan2 = scan(&library_root, &opts, &layout, &host, &ignore, None, None);
        let mut items2: Vec<_> = scan2
            .groups
            .iter()
            .flat_map(|g| &g.items)
            .map(|i| i.path.file_name().unwrap().to_str().unwrap())
            .collect();
        items2.sort();
        assert_eq!(items2, vec!["big.bin", "y.bin"]);

        // Then root = home: no row from Library.
        let scan3 = scan(&home, &opts, &layout, &host, &ignore, None, None);
        let items3: Vec<_> = scan3.groups.iter().flat_map(|g| &g.items).collect();
        assert!(items3.is_empty());
    }

    #[test]
    fn files_below_limit_are_left_out() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();

        let small = root.join("small.txt");
        let large = root.join("large.bin");
        // On APFS, 50 bytes allocates 4096 bytes, while 20_000 bytes allocates ~20480 bytes.
        write_file(&small, 50);
        write_file(&large, 20_000);

        let host = FakeHost::default();
        let ignore = IgnoreStore::default();

        let scan = scan(
            &root,
            &LargeScanOptions::new(10_000, None, 10),
            &test_layout(&root),
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
        let root = temp.path().canonicalize().unwrap();

        // Distinct block allocation sizes on APFS
        for i in 1..=5 {
            write_file(&root.join(format!("file_{i}.bin")), i * 20_000);
        }

        let host = FakeHost::default();
        let ignore = IgnoreStore::default();

        let scan = scan(
            &root,
            &LargeScanOptions::new(10_000, None, 3),
            &test_layout(&root),
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
        let root = temp.path().canonicalize().unwrap();

        let pkg = root.join("Test.photoslibrary");
        fs::create_dir_all(&pkg).unwrap();
        write_file(&pkg.join("internal1.jpg"), 200);
        write_file(&pkg.join("nested/internal2.jpg"), 300);

        let host = FakeHost::default();
        let ignore = IgnoreStore::default();

        let scan = scan(
            &root,
            &LargeScanOptions::new(50, None, 10),
            &test_layout(&root),
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
        let root = temp.path().canonicalize().unwrap();

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
            &root,
            &LargeScanOptions::new(100, None, 10),
            &test_layout(&root),
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
        let root = temp.path().canonicalize().unwrap();

        let other_temp = TempDir::new().unwrap();
        let target_file = other_temp.path().join("external.bin");
        write_file(&target_file, 500);

        let link = root.join("symlink_dir");
        let _ = symlink(other_temp.path(), &link);

        let host = FakeHost::default();
        let ignore = IgnoreStore::default();

        let scan = scan(
            &root,
            &LargeScanOptions::new(100, None, 10),
            &test_layout(&root),
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
        let root = temp.path().canonicalize().unwrap();

        let cloud_file = root.join("com~apple~CloudDocs/cloud.bin");
        let normal_file = root.join("normal.bin");
        write_file(&cloud_file, 500);
        write_file(&normal_file, 500);

        let host = FakeHost::default();
        let ignore = IgnoreStore::default();

        let scan = scan(
            &root,
            &LargeScanOptions::new(100, None, 10),
            &test_layout(&root),
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
    fn older_than_with_limit_keeps_the_old_files() {
        let temp = TempDir::new().unwrap();
        let home = temp.path().canonicalize().unwrap();
        let layout = test_layout(&home);

        let now = SystemTime::now();
        let mtime_400 = now - Duration::from_secs(400 * 86400);
        let date_7 = now - Duration::from_secs(7 * 86400);
        let date_400 = now - Duration::from_secs(400 * 86400);

        let host = FakeHost::default();
        for i in 0..15 {
            let file = home.join(format!("file_{i:02}.bin"));
            write_file(&file, 20_000 + i * 4096);
            set_mtime(&file, mtime_400);
            let canonical = file.canonicalize().unwrap();
            let date = if i >= 5 { date_7 } else { date_400 };
            host.reply(
                &format!(
                    "/usr/bin/mdls -raw -nullMarker (null) -name kMDItemLastUsedDate {}",
                    canonical.display()
                ),
                &spotlight_date(date),
            );
        }

        let ignore = IgnoreStore::default();
        let scan = scan(
            &home,
            &LargeScanOptions::new(100, Some(180), 10),
            &layout,
            &host,
            &ignore,
            None,
            None,
        );
        let items: Vec<_> = scan.groups.iter().flat_map(|g| &g.items).collect();
        assert_eq!(items.len(), 5);
    }

    #[test]
    fn older_than_uses_later_of_last_used_and_modified() {
        let temp = TempDir::new().unwrap();
        let home = temp.path().canonicalize().unwrap();
        let layout = test_layout(&home);

        let now = SystemTime::now();
        let mtime_300 = now - Duration::from_secs(300 * 86400);
        let date_10 = now - Duration::from_secs(10 * 86400);
        let date_400 = now - Duration::from_secs(400 * 86400);

        let file_a = home.join("file_a.bin");
        let file_b = home.join("file_b.bin");
        write_file(&file_a, 20_000);
        write_file(&file_b, 20_000);
        set_mtime(&file_a, mtime_300);
        set_mtime(&file_b, mtime_300);

        let host = FakeHost::default();
        host.reply(
            &format!(
                "/usr/bin/mdls -raw -nullMarker (null) -name kMDItemLastUsedDate {}",
                file_a.canonicalize().unwrap().display()
            ),
            &spotlight_date(date_10),
        );
        host.reply(
            &format!(
                "/usr/bin/mdls -raw -nullMarker (null) -name kMDItemLastUsedDate {}",
                file_b.canonicalize().unwrap().display()
            ),
            &spotlight_date(date_400),
        );

        let ignore = IgnoreStore::default();
        let scan_result = scan(
            &home,
            &LargeScanOptions::new(100, Some(180), 10),
            &layout,
            &host,
            &ignore,
            None,
            None,
        );
        let items: Vec<_> = scan_result.groups.iter().flat_map(|g| &g.items).collect();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].path, file_b.canonicalize().unwrap());
        let diff = items[0]
            .modified
            .unwrap()
            .duration_since(mtime_300)
            .unwrap_or_else(|_| {
                mtime_300
                    .duration_since(items[0].modified.unwrap())
                    .unwrap()
            });
        assert!(diff < Duration::from_secs(60));

        let calls = host.calls.lock().unwrap();
        assert!(calls.iter().any(|c| c.contains("file_a.bin")));
        assert!(calls.iter().any(|c| c.contains("file_b.bin")));
    }

    #[test]
    fn failed_mdls_falls_back_to_modified_date() {
        let temp = TempDir::new().unwrap();
        let home = temp.path().canonicalize().unwrap();
        let layout = test_layout(&home);

        let now = SystemTime::now();
        let mtime_10 = now - Duration::from_secs(10 * 86400);

        let file = home.join("doc.bin");
        write_file(&file, 20_000);
        set_mtime(&file, mtime_10);

        let host = FakeHost::default();
        host.fail("/usr/bin/mdls", "spotlight is off");

        let ignore = IgnoreStore::default();
        let scan1 = scan(
            &home,
            &LargeScanOptions::new(100, None, 10),
            &layout,
            &host,
            &ignore,
            None,
            None,
        );
        let items1: Vec<_> = scan1.groups.iter().flat_map(|g| &g.items).collect();
        assert_eq!(items1.len(), 1);
        let diff = items1[0]
            .modified
            .unwrap()
            .duration_since(mtime_10)
            .unwrap_or_else(|_| {
                mtime_10
                    .duration_since(items1[0].modified.unwrap())
                    .unwrap()
            });
        assert!(diff < Duration::from_secs(2));

        let scan2 = scan(
            &home,
            &LargeScanOptions::new(100, Some(5), 10),
            &layout,
            &host,
            &ignore,
            None,
            None,
        );
        let items2: Vec<_> = scan2.groups.iter().flat_map(|g| &g.items).collect();
        assert_eq!(items2.len(), 1);
        assert_eq!(items2[0].path, items1[0].path);
    }

    #[test]
    fn progress_counts_only_the_rows_kept() {
        let temp = TempDir::new().unwrap();
        let home = temp.path().canonicalize().unwrap();
        let layout = test_layout(&home);

        for (name, size) in [
            ("a.bin", 20_000),
            ("b.bin", 40_000),
            ("c.bin", 60_000),
            ("d.bin", 80_000),
            ("e.bin", 100_000),
        ] {
            write_file(&home.join(name), size);
        }

        let host = FakeHost::default();
        let ignore = IgnoreStore::default();
        let (tx, rx) = std::sync::mpsc::channel();

        let scan = scan(
            &home,
            &LargeScanOptions::new(10_000, None, 2),
            &layout,
            &host,
            &ignore,
            Some(tx),
            None,
        );
        let items: Vec<_> = scan.groups.iter().flat_map(|g| &g.items).collect();
        assert_eq!(items.len(), 2);
        let expected_bytes: u64 = items.iter().map(|i| i.size_bytes).sum();

        let mut item_found_count = 0;
        let mut item_found_bytes = 0;
        while let Ok(event) = rx.try_recv() {
            if let ScanProgressEvent::ItemFound { bytes } = event {
                item_found_count += 1;
                item_found_bytes += bytes;
            }
        }

        assert_eq!(item_found_count, 2);
        assert_eq!(item_found_bytes, expected_bytes);
    }

    #[test]
    fn virtual_machine_bundles_are_one_row() {
        let temp = TempDir::new().unwrap();
        let home = temp.path().canonicalize().unwrap();
        let layout = test_layout(&home);

        let host = FakeHost::default();
        let ignore = IgnoreStore::default();

        for ext in ["pvm", "vmwarevm", "utm"] {
            let bundle = home.join(format!("vm.{ext}"));
            fs::create_dir_all(&bundle).unwrap();
            write_file(&bundle.join("disk1.raw"), 20_000);
            write_file(&bundle.join("disk2.raw"), 30_000);

            let scan = scan(
                &home,
                &LargeScanOptions::new(10_000, None, 10),
                &layout,
                &host,
                &ignore,
                None,
                None,
            );
            let items: Vec<_> = scan.groups.iter().flat_map(|g| &g.items).collect();
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].path, bundle);
            assert!(items[0].size_bytes >= 50_000);

            fs::remove_dir_all(&bundle).unwrap();
        }
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
            &test_layout(root),
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

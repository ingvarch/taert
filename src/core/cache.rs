//! In-memory smart cache for storage analysis and exploration.
//!
//! Features:
//! - Directory recursive size caching with mtime validation and TTL.
//! - Subtree pre-aggregation: walking a folder computes and caches sizes for all its subdirectories.
//! - Folder snapshot caching for instant drill-down and zero-latency breadcrumb navigation.
//! - Fine-grained prefix invalidation on deletion or manual refresh.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// Represents an item in a directory listing snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildItem {
    pub name: String,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub is_dir: bool,
    pub is_package: bool,
    pub selected: bool,
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

/// A snapshot of a directory's contents.
#[derive(Debug, Clone)]
pub struct FolderSnapshot {
    pub path: PathBuf,
    pub children: Vec<ChildItem>,
    pub total_bytes: u64,
    pub inaccessible_count: usize,
    pub skipped_count: usize,
}

/// Cached directory size entry with metadata timestamp.
#[derive(Debug, Clone)]
pub struct CachedDirSize {
    pub bytes: u64,
    pub files_count: usize,
    pub mtime: Option<SystemTime>,
    pub cached_at: Instant,
}

/// Smart storage cache holding directory sizes and snapshots.
#[derive(Debug, Clone)]
pub struct StorageCache {
    sizes: HashMap<PathBuf, CachedDirSize>,
    snapshots: HashMap<PathBuf, (FolderSnapshot, Option<SystemTime>, Instant)>,
    ttl: Duration,
}

impl Default for StorageCache {
    fn default() -> Self {
        Self::new()
    }
}

impl StorageCache {
    /// Create a cache with a default TTL of 15 minutes.
    pub fn new() -> Self {
        Self {
            sizes: HashMap::new(),
            snapshots: HashMap::new(),
            ttl: Duration::from_secs(15 * 60),
        }
    }

    /// Create a cache with custom TTL.
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            sizes: HashMap::new(),
            snapshots: HashMap::new(),
            ttl,
        }
    }

    /// Retrieve the cached recursive size of a directory if still valid.
    pub fn get_size(&self, path: &Path) -> Option<u64> {
        let entry = self.sizes.get(path)?;
        if entry.cached_at.elapsed() > self.ttl {
            return None;
        }

        // Validate mtime if directory is still on disk
        if let Ok(meta) = fs::metadata(path) {
            let current_mtime = meta.modified().ok();
            if entry.mtime.is_some() && current_mtime.is_some() && entry.mtime != current_mtime {
                return None;
            }
        } else {
            return None;
        }

        Some(entry.bytes)
    }

    /// Retrieve a cached folder snapshot if still valid.
    pub fn get_snapshot(&self, path: &Path) -> Option<FolderSnapshot> {
        let (snapshot, mtime, cached_at) = self.snapshots.get(path)?;
        if cached_at.elapsed() > self.ttl {
            return None;
        }

        // Validate mtime
        if let Ok(meta) = fs::metadata(path) {
            let current_mtime = meta.modified().ok();
            if mtime.is_some() && current_mtime.is_some() && mtime != &current_mtime {
                return None;
            }
        } else {
            return None;
        }

        Some(snapshot.clone())
    }

    /// Cache a directory size entry.
    pub fn put_size(
        &mut self,
        path: PathBuf,
        bytes: u64,
        files_count: usize,
        mtime: Option<SystemTime>,
    ) {
        self.sizes.insert(
            path,
            CachedDirSize {
                bytes,
                files_count,
                mtime,
                cached_at: Instant::now(),
            },
        );
    }

    /// Cache a folder snapshot.
    pub fn put_snapshot(&mut self, path: PathBuf, snapshot: FolderSnapshot) {
        let mtime = fs::metadata(&path).ok().and_then(|m| m.modified().ok());
        self.snapshots
            .insert(path, (snapshot, mtime, Instant::now()));
    }

    /// Invalidate all entries starting with `prefix`.
    pub fn invalidate_prefix(&mut self, prefix: &Path) {
        self.sizes.retain(|p, _| !p.starts_with(prefix));
        self.snapshots.retain(|p, _| !p.starts_with(prefix));
    }

    /// Invalidate a single path and its ancestor snapshots (since totals change).
    pub fn invalidate_path(&mut self, path: &Path) {
        self.sizes.remove(path);
        self.snapshots.remove(path);
        // Also remove ancestor snapshots since their children list/totals are affected
        self.snapshots.retain(|p, _| !path.starts_with(p));
    }

    /// Clear all cached sizes and snapshots.
    pub fn clear(&mut self) {
        self.sizes.clear();
        self.snapshots.clear();
    }

    /// Number of cached directory sizes.
    pub fn size_count(&self) -> usize {
        self.sizes.len()
    }

    /// Number of cached folder snapshots.
    pub fn snapshot_count(&self) -> usize {
        self.snapshots.len()
    }

    /// Pre-aggregates recursive sizes for all directories in a walked subtree
    /// and populates the cache.
    pub fn populate_from_dir_tree(
        &mut self,
        root: &Path,
        mut dirs: HashSet<PathBuf>,
        direct_bytes: HashMap<PathBuf, u64>,
        direct_files: HashMap<PathBuf, usize>,
    ) {
        dirs.insert(root.to_path_buf());

        let mut sorted_dirs: Vec<PathBuf> = dirs.into_iter().collect();
        // Sort from deepest to shallowest (most path components to least)
        sorted_dirs.sort_by_key(|b| std::cmp::Reverse(b.components().count()));

        let mut recursive_bytes: HashMap<PathBuf, u64> = HashMap::with_capacity(sorted_dirs.len());
        let mut recursive_files: HashMap<PathBuf, usize> =
            HashMap::with_capacity(sorted_dirs.len());

        for d in &sorted_dirs {
            let b = *direct_bytes.get(d).unwrap_or(&0);
            let f = *direct_files.get(d).unwrap_or(&0);
            recursive_bytes.insert(d.clone(), b);
            recursive_files.insert(d.clone(), f);
        }

        for d in &sorted_dirs {
            if d == root {
                continue;
            }
            if let Some(parent) = d.parent()
                && (parent.starts_with(root) || parent == root)
            {
                let child_b = *recursive_bytes.get(d).unwrap_or(&0);
                let child_f = *recursive_files.get(d).unwrap_or(&0);
                if let Some(p_b) = recursive_bytes.get_mut(parent) {
                    *p_b = p_b.saturating_add(child_b);
                }
                if let Some(p_f) = recursive_files.get_mut(parent) {
                    *p_f = p_f.saturating_add(child_f);
                }
            }
        }

        let now = Instant::now();
        for (dir_path, bytes) in recursive_bytes {
            let files = *recursive_files.get(&dir_path).unwrap_or(&0);
            let mtime = fs::metadata(&dir_path).ok().and_then(|m| m.modified().ok());
            self.sizes.insert(
                dir_path,
                CachedDirSize {
                    bytes,
                    files_count: files,
                    mtime,
                    cached_at: now,
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_storage_cache_basic_put_get() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().to_path_buf();

        let mut cache = StorageCache::new();
        assert_eq!(cache.get_size(&dir), None);

        let mtime = fs::metadata(&dir).ok().and_then(|m| m.modified().ok());
        cache.put_size(dir.clone(), 1024, 5, mtime);
        assert_eq!(cache.get_size(&dir), Some(1024));
        assert_eq!(cache.size_count(), 1);

        cache.invalidate_path(&dir);
        assert_eq!(cache.get_size(&dir), None);
        assert_eq!(cache.size_count(), 0);
    }

    #[test]
    fn test_storage_cache_tree_aggregation() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let sub_a = root.join("sub_a");
        let sub_b = sub_a.join("sub_b");

        fs::create_dir_all(&sub_b).unwrap();

        let mut dirs = HashSet::new();
        dirs.insert(root.clone());
        dirs.insert(sub_a.clone());
        dirs.insert(sub_b.clone());

        let mut direct_bytes = HashMap::new();
        direct_bytes.insert(sub_b.clone(), 1000);
        direct_bytes.insert(sub_a.clone(), 500);
        direct_bytes.insert(root.clone(), 200);

        let mut direct_files = HashMap::new();
        direct_files.insert(sub_b.clone(), 1);
        direct_files.insert(sub_a.clone(), 1);
        direct_files.insert(root.clone(), 1);

        let mut cache = StorageCache::new();
        cache.populate_from_dir_tree(&root, dirs, direct_bytes, direct_files);

        // sub_b = 1000
        assert_eq!(cache.get_size(&sub_b), Some(1000));
        // sub_a = 500 + 1000 = 1500
        assert_eq!(cache.get_size(&sub_a), Some(1500));
        // root = 200 + 1500 = 1700
        assert_eq!(cache.get_size(&root), Some(1700));

        // Test prefix invalidation
        cache.invalidate_prefix(&sub_a);
        assert_eq!(cache.get_size(&sub_a), None);
        assert_eq!(cache.get_size(&sub_b), None);
        assert_eq!(cache.get_size(&root), Some(1700));
    }

    #[test]
    fn test_storage_cache_snapshot() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().to_path_buf();

        let snapshot = FolderSnapshot {
            path: dir.clone(),
            children: vec![ChildItem {
                name: "file.txt".to_string(),
                path: dir.join("file.txt"),
                size_bytes: 512,
                is_dir: false,
                is_package: false,
                selected: false,
            }],
            total_bytes: 512,
            inaccessible_count: 0,
            skipped_count: 0,
        };

        let mut cache = StorageCache::new();
        assert!(cache.get_snapshot(&dir).is_none());

        cache.put_snapshot(dir.clone(), snapshot);
        let retrieved = cache.get_snapshot(&dir);
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().total_bytes, 512);

        cache.clear();
        assert_eq!(cache.snapshot_count(), 0);
        assert!(cache.get_snapshot(&dir).is_none());
    }

    #[test]
    fn test_storage_cache_ttl_expiration() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().to_path_buf();

        let mut cache = StorageCache::with_ttl(Duration::from_millis(1));
        let mtime = fs::metadata(&dir).ok().and_then(|m| m.modified().ok());
        cache.put_size(dir.clone(), 2048, 1, mtime);

        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(cache.get_size(&dir), None);
    }
}

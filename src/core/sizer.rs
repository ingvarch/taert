//! Directory and file size calculator.
//! Uses macOS allocated block size (st_blocks * 512) for accurate APFS disk measurement.
//! Dedupes hardlinks and avoids traversing symlinks or cloud/dataless files.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use super::cache::StorageCache;
use super::safety::{FileIdentity, Safety};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeasurementStatus {
    Complete,
    Partial,
    Unavailable,
    CloudOrDataless,
    Symlink,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Measurement {
    pub bytes: u64,
    pub status: MeasurementStatus,
    pub skipped_entries: usize,
}

impl Measurement {
    pub fn zero(status: MeasurementStatus) -> Self {
        Self {
            bytes: 0,
            status,
            skipped_entries: 0,
        }
    }
}

pub struct DirSizer;

impl DirSizer {
    pub fn size_of(path: &Path) -> u64 {
        Self::measure(path).bytes
    }

    pub fn measure(path: &Path) -> Measurement {
        Self::measure_with_progress(path, None, |_, _, _| {})
    }

    pub fn measure_with_progress<F>(
        path: &Path,
        cancel: Option<&Arc<AtomicBool>>,
        on_progress: F,
    ) -> Measurement
    where
        F: FnMut(&Path, u64, usize),
    {
        Self::measure_with_progress_and_cache(path, cancel, None, on_progress)
    }

    pub fn measure_with_progress_and_cache<F>(
        path: &Path,
        cancel: Option<&Arc<AtomicBool>>,
        cache: Option<&Arc<RwLock<StorageCache>>>,
        mut on_progress: F,
    ) -> Measurement
    where
        F: FnMut(&Path, u64, usize),
    {
        if let Some(c) = cache
            && let Some(cached_bytes) = c.read().unwrap().get_size(path)
        {
            return Measurement {
                bytes: cached_bytes,
                status: MeasurementStatus::Complete,
                skipped_entries: 0,
            };
        }

        if Safety::has_symlink_component(path) {
            return Measurement {
                bytes: 0,
                status: MeasurementStatus::Symlink,
                skipped_entries: 1,
            };
        }

        if Safety::is_cloud_or_dataless(path) {
            return Measurement {
                bytes: 0,
                status: MeasurementStatus::CloudOrDataless,
                skipped_entries: 1,
            };
        }

        let meta = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(_) => {
                return Measurement {
                    bytes: 0,
                    status: MeasurementStatus::Unavailable,
                    skipped_entries: 1,
                };
            }
        };

        if !meta.is_dir() {
            let allocated = Self::allocated_size(&meta);
            return Measurement {
                bytes: allocated,
                status: MeasurementStatus::Complete,
                skipped_entries: 0,
            };
        }

        // Parallel or recursive walk for directory
        let mut total_bytes: u64 = 0;
        let mut skipped = 0;
        let mut seen_hardlinks: HashSet<FileIdentity> = HashSet::new();
        let mut dirs: HashSet<PathBuf> = HashSet::new();
        dirs.insert(path.to_path_buf());
        let mut direct_bytes: HashMap<PathBuf, u64> = HashMap::new();
        let mut direct_files: HashMap<PathBuf, usize> = HashMap::new();

        let walker = jwalk::WalkDir::new(path)
            .skip_hidden(false)
            .follow_links(false)
            .process_read_dir(|_, _, _, children| {
                // Filter out symlinks or provider owned dirs from recursion
                children.retain(|res| match res {
                    Ok(entry) => {
                        let path = entry.path();
                        if entry.file_type.is_symlink() {
                            return false;
                        }
                        if Safety::is_cloud_or_dataless(&path) {
                            return false;
                        }
                        true
                    }
                    Err(_) => false,
                });
            });

        let mut files_count = 0;
        let mut was_cancelled = false;
        for (idx, entry_res) in walker.into_iter().enumerate() {
            if let Some(c) = cancel
                && c.load(Ordering::Relaxed)
            {
                was_cancelled = true;
                break;
            }

            match entry_res {
                Ok(entry) => {
                    let entry_path = entry.path();
                    if entry.file_type.is_dir() {
                        dirs.insert(entry_path.clone());
                    } else if entry.file_type.is_file() {
                        files_count += 1;
                        if let Ok(m) = entry.metadata() {
                            let identity = FileIdentity {
                                dev: m.dev(),
                                ino: m.ino(),
                            };
                            if m.nlink() > 1 && !seen_hardlinks.insert(identity) {
                                // Already counted hardlink
                                continue;
                            }
                            let allocated = Self::allocated_size(&m);
                            total_bytes = total_bytes.saturating_add(allocated);

                            if let Some(parent) = entry_path.parent() {
                                *direct_bytes.entry(parent.to_path_buf()).or_default() += allocated;
                                *direct_files.entry(parent.to_path_buf()).or_default() += 1;
                            }
                        } else {
                            skipped += 1;
                        }
                    }
                    if idx % 30 == 0 {
                        on_progress(&entry_path, total_bytes, files_count);
                    }
                }
                Err(_) => {
                    skipped += 1;
                }
            }
        }

        if !was_cancelled && let Some(c) = cache {
            c.write()
                .unwrap()
                .populate_from_dir_tree(path, dirs, direct_bytes, direct_files);
        }

        Measurement {
            bytes: total_bytes,
            status: if skipped == 0 {
                MeasurementStatus::Complete
            } else {
                MeasurementStatus::Partial
            },
            skipped_entries: skipped,
        }
    }

    /// Calculate allocated byte size from file metadata.
    /// On Unix/macOS, st_blocks represents the number of 512-byte blocks allocated.
    #[inline]
    fn allocated_size(meta: &fs::Metadata) -> u64 {
        let blocks = meta.blocks();
        if blocks > 0 {
            blocks.saturating_mul(512)
        } else {
            meta.len()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn test_measure_single_file() {
        let temp = tempdir().unwrap();
        let file_path = temp.path().join("test.txt");
        let mut f = fs::File::create(&file_path).unwrap();
        f.write_all(b"hello world").unwrap();

        let measurement = DirSizer::measure(&file_path);
        assert!(measurement.bytes > 0);
        assert_eq!(measurement.status, MeasurementStatus::Complete);
        assert_eq!(measurement.skipped_entries, 0);
    }

    #[test]
    fn test_measure_directory() {
        let temp = tempdir().unwrap();
        let dir = temp.path().join("dir");
        fs::create_dir_all(&dir).unwrap();

        let f1 = dir.join("f1.txt");
        let f2 = dir.join("f2.txt");
        fs::write(&f1, b"sample 1").unwrap();
        fs::write(&f2, b"sample 2").unwrap();

        let measurement = DirSizer::measure(&dir);
        assert!(measurement.bytes > 0);
        assert_eq!(measurement.status, MeasurementStatus::Complete);
    }
}

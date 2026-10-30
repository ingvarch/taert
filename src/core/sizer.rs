//! Directory and file size calculator.
//! Uses macOS allocated block size (st_blocks * 512) for accurate APFS disk measurement.
//! Dedupes hardlinks and avoids traversing symlinks or cloud/dataless files.

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use serde::{Deserialize, Serialize};

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

        for entry_res in walker {
            match entry_res {
                Ok(entry) => {
                    if entry.file_type.is_file() {
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
                        } else {
                            skipped += 1;
                        }
                    }
                }
                Err(_) => {
                    skipped += 1;
                }
            }
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

//! Safe file deletion engine.
//! Re-checks safety conditions and file identity before removing any file or folder.
//! Supports progress callback for interactive spinners and status updates.

use std::fs;
use std::path::PathBuf;

use super::safety::{FileIdentity, Safety};
use super::scanner::ScanItem;
use super::stores::IgnoreStore;

#[derive(Debug, Clone)]
pub struct RemovalFailure {
    pub path: PathBuf,
    pub error: String,
}

#[derive(Debug, Default, Clone)]
pub struct RemovalReport {
    pub removed_count: usize,
    pub freed_bytes: u64,
    pub failures: Vec<RemovalFailure>,
}

#[derive(Debug, Clone)]
pub struct RemovalProgress {
    pub current_index: usize,
    pub total_count: usize,
    pub path: PathBuf,
}

pub struct Cleaner;

impl Cleaner {
    /// Permanently remove selected items with strict safety re-checks.
    pub fn remove_items(items: &[ScanItem], ignore: &IgnoreStore) -> RemovalReport {
        Self::remove_items_with_progress(items, ignore, None::<fn(RemovalProgress)>)
    }

    /// Permanently remove selected items and report progress via callback before each item.
    pub fn remove_items_with_progress<F>(
        items: &[ScanItem],
        ignore: &IgnoreStore,
        mut progress_cb: Option<F>,
    ) -> RemovalReport
    where
        F: FnMut(RemovalProgress),
    {
        let mut report = RemovalReport::default();
        let total = items.iter().filter(|i| i.selected).count();
        let mut current_pos = 0;

        for item in items {
            if !item.selected {
                continue;
            }

            current_pos += 1;
            if let Some(ref mut cb) = progress_cb {
                cb(RemovalProgress {
                    current_index: current_pos,
                    total_count: total,
                    path: item.path.clone(),
                });
            }

            // 1. Re-validate safety rule immediately before deletion
            if let Err(e) = Safety::can_remove(&item.path, ignore) {
                report.failures.push(RemovalFailure {
                    path: item.path.clone(),
                    error: format!("Safety check failed: {e}"),
                });
                continue;
            }

            // 2. Re-validate FileIdentity (check device and inode haven't changed / been replaced)
            if let Some(expected_id) = item.identity {
                if let Some(current_id) = FileIdentity::of(&item.path) {
                    if expected_id != current_id {
                        report.failures.push(RemovalFailure {
                            path: item.path.clone(),
                            error:
                                "File identity mismatch (file was modified or replaced since scan)"
                                    .into(),
                        });
                        continue;
                    }
                } else {
                    report.failures.push(RemovalFailure {
                        path: item.path.clone(),
                        error: "Failed to read file identity prior to deletion".into(),
                    });
                    continue;
                }
            }

            // 3. Perform removal
            let removal_result = if item.contents_only {
                Self::remove_contents(&item.path)
            } else if item.path.is_dir() {
                fs::remove_dir_all(&item.path)
            } else {
                fs::remove_file(&item.path)
            };

            match removal_result {
                Ok(_) => {
                    report.removed_count += 1;
                    report.freed_bytes = report.freed_bytes.saturating_add(item.size_bytes);
                }
                Err(err) => {
                    report.failures.push(RemovalFailure {
                        path: item.path.clone(),
                        error: err.to_string(),
                    });
                }
            }
        }

        report
    }

    /// Remove directory contents while preserving the root directory itself.
    fn remove_contents(dir_path: &PathBuf) -> std::io::Result<()> {
        if !dir_path.is_dir() {
            return Ok(());
        }
        for entry in fs::read_dir(dir_path)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                fs::remove_dir_all(&path)?;
            } else {
                fs::remove_file(&path)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_cleaner_removes_temp_item() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("cache");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("file.bin"), b"test").unwrap();

        let item = ScanItem::new(target.clone(), 100, true, false);
        let ignore = IgnoreStore::default();
        let mut progress_count = 0;
        let report = Cleaner::remove_items_with_progress(
            &[item],
            &ignore,
            Some(|p: RemovalProgress| {
                progress_count += 1;
                assert_eq!(p.current_index, 1);
                assert_eq!(p.total_count, 1);
            }),
        );

        assert_eq!(progress_count, 1);
        assert_eq!(report.removed_count, 1);
        assert_eq!(report.failures.len(), 0);
        assert!(!target.exists());
    }

    #[test]
    fn test_cleaner_contents_only() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("logs");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("app.log"), b"log").unwrap();

        let item = ScanItem::new(target.clone(), 50, true, true);
        let ignore = IgnoreStore::default();
        let report = Cleaner::remove_items(&[item], &ignore);

        assert_eq!(report.removed_count, 1);
        assert!(target.exists());
        assert!(!target.join("app.log").exists());
    }
}

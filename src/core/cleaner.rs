//! Safe file deletion engine.
//! Re-checks safety conditions and file identity before removing any file or folder.
//! Supports progress callback for interactive spinners and status updates.

use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

use super::docker::{DockerClient, DockerRemover};
use super::safety::{FileIdentity, Safety, SafetyError};
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
    /// Things worth telling that are not failures.
    pub notes: Vec<String>,
}

#[derive(Debug, Error)]
pub enum ItemRemovalError {
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

impl ItemRemovalError {
    pub fn is_permission_denied(&self) -> bool {
        match self {
            Self::Refused(_) => false,
            Self::Io(error) => matches!(error.raw_os_error(), Some(libc::EACCES | libc::EPERM)),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RemovalProgress {
    pub current_index: usize,
    pub total_count: usize,
    pub path: PathBuf,
}

fn connect_docker() -> Result<Box<dyn DockerRemover>, String> {
    match DockerClient::connect() {
        Ok(client) => Ok(Box::new(client)),
        Err(error) => Err(error.to_string()),
    }
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
        progress_cb: Option<F>,
    ) -> RemovalReport
    where
        F: FnMut(RemovalProgress),
    {
        Self::remove_with(items, ignore, progress_cb, connect_docker)
    }

    fn remove_with<F, C>(
        items: &[ScanItem],
        ignore: &IgnoreStore,
        mut progress_cb: Option<F>,
        connect: C,
    ) -> RemovalReport
    where
        F: FnMut(RemovalProgress),
        C: FnOnce() -> Result<Box<dyn DockerRemover>, String>,
    {
        let mut report = RemovalReport::default();
        let total = items.iter().filter(|i| i.selected).count();
        let mut current_pos = 0;

        let wants_docker = items.iter().any(|i| i.selected && i.docker.is_some());
        let docker = wants_docker.then(connect);
        let used_before = docker
            .as_ref()
            .and_then(|docker| docker.as_ref().ok()?.used_bytes());
        let mut docker_listed_bytes = 0_u64;

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

            // Docker objects are no files, so nothing below may see them.
            if let Some(object) = &item.docker {
                let outcome = match &docker {
                    Some(Ok(docker)) => docker.remove(object),
                    Some(Err(error)) => Err(error.clone()),
                    None => Err("Docker was not connected".to_string()),
                };
                match outcome {
                    Ok(()) => {
                        report.removed_count += 1;
                        docker_listed_bytes = docker_listed_bytes.saturating_add(item.size_bytes);
                    }
                    Err(error) => report.failures.push(RemovalFailure {
                        path: item.path.clone(),
                        error,
                    }),
                }
                continue;
            }

            let failures = Self::remove_row(item, ignore);
            if failures.is_empty() {
                report.removed_count += 1;
                report.freed_bytes = report.freed_bytes.saturating_add(item.size_bytes);
            }
            report.failures.extend(failures);
        }

        // Images share layers, so only the daemon knows what a removal really freed.
        let used_after = docker
            .as_ref()
            .and_then(|docker| docker.as_ref().ok()?.used_bytes());
        let docker_freed = match (used_before, used_after) {
            (Some(before), Some(after)) => before.saturating_sub(after),
            _ => docker_listed_bytes,
        };
        report.freed_bytes = report.freed_bytes.saturating_add(docker_freed);

        report
    }

    /// Checks every folder of a row before the first one goes.
    fn remove_row(item: &ScanItem, ignore: &IgnoreStore) -> Vec<RemovalFailure> {
        let check = |path: &Path| Safety::can_remove(path, ignore);
        let failure = |folder: &ScanItem, error: ItemRemovalError| RemovalFailure {
            path: folder.path.clone(),
            error: error.to_string(),
        };
        let refused: Vec<RemovalFailure> = item
            .folders()
            .filter_map(|folder| Some(failure(folder, Self::verify(folder, check).err()?)))
            .collect();
        if !refused.is_empty() {
            return refused;
        }
        item.folders()
            .filter_map(|folder| Some(failure(folder, Self::remove_verified(folder, check).err()?)))
            .collect()
    }

    /// Runs the safety rule, confirms the path is the one scanned, and removes it.
    pub fn remove_verified(
        item: &ScanItem,
        check: impl Fn(&Path) -> Result<(), SafetyError>,
    ) -> Result<(), ItemRemovalError> {
        Self::verify(item, check)?;
        if item.contents_only {
            Self::remove_contents(&item.path)?;
        } else if item.path.is_dir() {
            fs::remove_dir_all(&item.path)?;
        } else {
            fs::remove_file(&item.path)?;
        }
        Ok(())
    }

    /// The checks of `remove_verified`, for a removal that happens elsewhere.
    pub fn verify(
        item: &ScanItem,
        check: impl Fn(&Path) -> Result<(), SafetyError>,
    ) -> Result<(), ItemRemovalError> {
        check(&item.path)
            .map_err(|error| ItemRemovalError::Refused(format!("Safety check failed: {error}")))?;

        let Some(expected) = item.identity else {
            return Ok(());
        };
        match FileIdentity::of(&item.path) {
            Some(current) if current == expected => Ok(()),
            Some(_) => Err(ItemRemovalError::Refused(
                "File identity mismatch (file was modified or replaced since scan)".into(),
            )),
            None => Err(ItemRemovalError::Refused(
                "Failed to read file identity prior to deletion".into(),
            )),
        }
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
    use crate::core::docker::DockerObject;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use tempfile::tempdir;

    #[derive(Default)]
    struct Daemon {
        removed: RefCell<Vec<DockerObject>>,
        refused: RefCell<Vec<DockerObject>>,
        used: Cell<Option<u64>>,
    }

    struct FakeDocker(Rc<Daemon>);

    impl DockerRemover for FakeDocker {
        fn remove(&self, object: &DockerObject) -> Result<(), String> {
            if self.0.refused.borrow().contains(object) {
                return Err("volume is in use".to_string());
            }
            self.0.removed.borrow_mut().push(object.clone());
            let used = self.0.used.get().map(|used| used.saturating_sub(600));
            self.0.used.set(used);
            Ok(())
        }

        fn used_bytes(&self) -> Option<u64> {
            self.0.used.get()
        }
    }

    fn remove_with(items: &[ScanItem], daemon: &Rc<Daemon>) -> RemovalReport {
        let daemon = daemon.clone();
        Cleaner::remove_with(
            items,
            &IgnoreStore::default(),
            None::<fn(RemovalProgress)>,
            move || Ok(Box::new(FakeDocker(daemon)) as Box<dyn DockerRemover>),
        )
    }

    fn volume(name: &str) -> DockerObject {
        DockerObject::Volume {
            name: name.to_string(),
        }
    }

    fn docker_item(object: DockerObject, size_bytes: u64) -> ScanItem {
        ScanItem::docker(object, "label".to_string(), size_bytes, true)
    }

    #[test]
    fn a_docker_item_never_touches_a_file_with_the_same_name() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("pgdata");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("data.bin"), b"keep").unwrap();
        let label = target.display().to_string();
        let item = ScanItem::docker(volume("pgdata"), label, 10, true);
        let daemon = Rc::new(Daemon::default());

        let report = remove_with(&[item], &daemon);

        assert!(target.join("data.bin").exists());
        assert_eq!(*daemon.removed.borrow(), [volume("pgdata")]);
        assert_eq!(report.removed_count, 1);
    }

    #[test]
    fn a_docker_item_left_unselected_stays() {
        let mut item = docker_item(volume("pgdata"), 10);
        item.selected = false;
        let daemon = Rc::new(Daemon::default());

        let report = remove_with(&[item], &daemon);

        assert!(daemon.removed.borrow().is_empty());
        assert_eq!(report.removed_count, 0);
    }

    #[test]
    fn the_freed_size_comes_from_the_daemon_and_not_the_list() {
        let daemon = Rc::new(Daemon::default());
        daemon.used.set(Some(1_000));

        let report = remove_with(&[docker_item(volume("a"), 1)], &daemon);

        assert_eq!(report.freed_bytes, 600);
    }

    #[test]
    fn the_freed_size_falls_back_to_the_list_when_the_daemon_cannot_tell() {
        let daemon = Rc::new(Daemon::default());

        let report = remove_with(&[docker_item(volume("a"), 25)], &daemon);

        assert_eq!(report.freed_bytes, 25);
    }

    #[test]
    fn a_refusal_is_a_failure_and_the_rest_goes_on() {
        let daemon = Rc::new(Daemon::default());
        daemon.refused.borrow_mut().push(volume("busy"));
        let items = [
            docker_item(volume("busy"), 5),
            docker_item(volume("free"), 5),
        ];

        let report = remove_with(&items, &daemon);

        assert_eq!(report.removed_count, 1);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].error, "volume is in use");
        assert_eq!(report.freed_bytes, 5);
    }

    #[test]
    fn an_unreachable_daemon_fails_docker_items_and_files_still_go() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("cache");
        fs::create_dir_all(&target).unwrap();
        let items = [
            docker_item(volume("a"), 5),
            ScanItem::new(target.clone(), 7, true, false),
        ];

        let report = Cleaner::remove_with(
            &items,
            &IgnoreStore::default(),
            None::<fn(RemovalProgress)>,
            || Err("Docker is not running".to_string()),
        );

        assert_eq!(report.removed_count, 1);
        assert_eq!(report.failures[0].error, "Docker is not running");
        assert!(!target.exists());
    }

    #[test]
    fn files_alone_never_connect_to_docker() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("cache");
        fs::create_dir_all(&target).unwrap();
        let item = ScanItem::new(target, 7, true, false);

        let report = Cleaner::remove_with(
            &[item],
            &IgnoreStore::default(),
            None::<fn(RemovalProgress)>,
            || panic!("connected to Docker"),
        );

        assert_eq!(report.removed_count, 1);
    }

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

    fn refuse(_: &std::path::Path) -> Result<(), SafetyError> {
        Err(SafetyError::Ignored)
    }

    fn allow(_: &std::path::Path) -> Result<(), SafetyError> {
        Ok(())
    }

    #[test]
    fn a_refused_path_stays_and_says_why() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("data");
        fs::create_dir_all(&target).unwrap();
        let item = ScanItem::new(target.clone(), 1, true, false);

        let error = Cleaner::remove_verified(&item, refuse).unwrap_err();

        assert!(target.exists());
        assert!(error.to_string().contains("Safety check failed"));
        assert!(!error.is_permission_denied());
    }

    fn row_of_two_folders(root: &Path) -> (ScanItem, PathBuf, PathBuf) {
        let (first, second) = (root.join("first"), root.join("second"));
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        let mut row = ScanItem::new(first.clone(), 7, true, false);
        row.also = vec![ScanItem::new(second.clone(), 3, true, false)];
        (row, first, second)
    }

    #[test]
    fn a_row_of_two_folders_goes_whole_and_counts_once() {
        let temp = tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let (row, first, second) = row_of_two_folders(&root);

        let report = Cleaner::remove_items(&[row], &IgnoreStore::default());

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.removed_count, 1);
        assert!(!first.exists());
        assert!(!second.exists());
    }

    #[test]
    fn a_row_stays_whole_when_a_later_folder_was_replaced() {
        let temp = tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let (row, first, second) = row_of_two_folders(&root);
        fs::remove_dir(&second).unwrap();
        fs::write(&second, b"something else").unwrap();

        let report = Cleaner::remove_items(&[row], &IgnoreStore::default());

        assert_eq!(report.removed_count, 0);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].path, second);
        assert!(first.exists());
        assert!(second.exists());
    }

    #[test]
    fn a_path_replaced_since_the_scan_stays() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("data");
        fs::create_dir_all(&target).unwrap();
        let item = ScanItem::new(target.clone(), 1, true, false);
        fs::remove_dir(&target).unwrap();
        fs::create_dir_all(temp.path().join("spacer")).unwrap();
        fs::create_dir_all(&target).unwrap();

        let error = Cleaner::remove_verified(&item, allow).unwrap_err();

        assert!(target.exists());
        assert!(error.to_string().contains("identity"));
    }

    #[test]
    fn a_folder_the_user_cannot_write_is_a_permission_error() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempdir().unwrap();
        let parent = temp.path().join("locked");
        let target = parent.join("data");
        fs::create_dir_all(&target).unwrap();
        let item = ScanItem::new(target.clone(), 1, true, false);
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o555)).unwrap();

        let error = Cleaner::remove_verified(&item, allow).unwrap_err();

        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(error.is_permission_denied(), "{error}");
    }

    #[test]
    fn a_verified_path_is_removed() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("data.bin");
        fs::write(&target, b"x").unwrap();
        let item = ScanItem::new(target.clone(), 1, true, false);

        Cleaner::remove_verified(&item, allow).unwrap();

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

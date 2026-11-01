//! Safe file deletion engine.
//! Re-checks safety conditions and file identity before removing any file or folder.
//! Supports progress callback for interactive spinners and status updates.

use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

use super::docker::{DockerClient, DockerRemover};
use super::history::{ItemOutcome, OutcomeKind};
use super::host::Host;
use super::safety::{FileIdentity, Safety, SafetyError};
use super::scanner::ScanItem;
use super::stores::IgnoreStore;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RemovalMode {
    Permanent,
    #[default]
    Trash,
}

/// Determines whether an item goes to the Trash or is permanently removed.
pub fn item_removal_mode(item: &ScanItem, scan_id: &str, permanent: bool) -> RemovalMode {
    if permanent || item.contents_only || item.docker.is_some() {
        return RemovalMode::Permanent;
    }
    match scan_id {
        super::uninstaller::PLAN_ID => {
            if super::uninstaller::needs_root(item) {
                RemovalMode::Permanent
            } else {
                RemovalMode::Trash
            }
        }
        "analyze" => RemovalMode::Trash,
        _ => RemovalMode::Permanent,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovalFailure {
    pub path: PathBuf,
    pub error: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RemovalReport {
    pub removed_count: usize,
    pub freed_bytes: u64,
    pub trashed_bytes: u64,
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
    #[error("{0}")]
    Trash(String),
}

impl ItemRemovalError {
    pub fn is_permission_denied(&self) -> bool {
        match self {
            Self::Refused(_) => false,
            Self::Io(error) => matches!(error.raw_os_error(), Some(libc::EACCES | libc::EPERM)),
            Self::Trash(error) => {
                let lower = error.to_lowercase();
                lower.contains("permission denied") || lower.contains("operation not permitted")
            }
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

pub struct Cleaner<'a> {
    pub host: &'a dyn Host,
    pub mode: RemovalMode,
}

impl<'a> Cleaner<'a> {
    pub fn new(host: &'a dyn Host, mode: RemovalMode) -> Self {
        Self { host, mode }
    }

    pub fn permanent(host: &'a dyn Host) -> Self {
        Self {
            host,
            mode: RemovalMode::Permanent,
        }
    }

    pub fn trash(host: &'a dyn Host) -> Self {
        Self {
            host,
            mode: RemovalMode::Trash,
        }
    }

    /// Remove selected items with strict safety re-checks.
    pub fn remove_items(&self, items: &[ScanItem], ignore: &IgnoreStore) -> RemovalReport {
        self.remove_items_with_progress(items, ignore, None::<fn(RemovalProgress)>)
    }

    /// Remove selected items and report progress via callback before each item.
    pub fn remove_items_with_progress<F>(
        &self,
        items: &[ScanItem],
        ignore: &IgnoreStore,
        progress_cb: Option<F>,
    ) -> RemovalReport
    where
        F: FnMut(RemovalProgress),
    {
        self.remove_with_callbacks(
            items,
            ignore,
            progress_cb,
            None::<fn(&ItemOutcome)>,
            connect_docker,
        )
    }

    /// Remove selected items, reporting progress and item-level outcomes via callbacks.
    pub fn remove_items_with_callbacks<F, O>(
        &self,
        items: &[ScanItem],
        ignore: &IgnoreStore,
        progress_cb: Option<F>,
        outcome_cb: Option<O>,
    ) -> RemovalReport
    where
        F: FnMut(RemovalProgress),
        O: FnMut(&ItemOutcome),
    {
        self.remove_with_callbacks(items, ignore, progress_cb, outcome_cb, connect_docker)
    }

    #[allow(dead_code)]
    pub(crate) fn remove_with<F, C>(
        &self,
        items: &[ScanItem],
        ignore: &IgnoreStore,
        progress_cb: Option<F>,
        connect: C,
    ) -> RemovalReport
    where
        F: FnMut(RemovalProgress),
        C: FnOnce() -> Result<Box<dyn DockerRemover>, String>,
    {
        self.remove_with_callbacks(
            items,
            ignore,
            progress_cb,
            None::<fn(&ItemOutcome)>,
            connect,
        )
    }

    fn remove_with_callbacks<F, O, C>(
        &self,
        items: &[ScanItem],
        ignore: &IgnoreStore,
        mut progress_cb: Option<F>,
        mut outcome_cb: Option<O>,
        connect: C,
    ) -> RemovalReport
    where
        F: FnMut(RemovalProgress),
        O: FnMut(&ItemOutcome),
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
                        if let Some(ref mut cb) = outcome_cb {
                            cb(&ItemOutcome {
                                path: item.path.clone(),
                                bytes: item.size_bytes,
                                outcome: OutcomeKind::Removed,
                                error: None,
                            });
                        }
                    }
                    Err(error) => {
                        if let Some(ref mut cb) = outcome_cb {
                            cb(&ItemOutcome {
                                path: item.path.clone(),
                                bytes: 0,
                                outcome: OutcomeKind::Failed,
                                error: Some(error.clone()),
                            });
                        }
                        report.failures.push(RemovalFailure {
                            path: item.path.clone(),
                            error,
                        });
                    }
                }
                continue;
            }

            self.remove_row(item, ignore, &mut report, outcome_cb.as_mut());
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

    /// Checks every folder of a row before the first one goes, then counts what went.
    fn remove_row<O: FnMut(&ItemOutcome)>(
        &self,
        item: &ScanItem,
        ignore: &IgnoreStore,
        report: &mut RemovalReport,
        mut outcome_cb: Option<&mut O>,
    ) {
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
            for f in &refused {
                if let Some(ref mut cb) = outcome_cb {
                    cb(&ItemOutcome {
                        path: f.path.clone(),
                        bytes: 0,
                        outcome: OutcomeKind::Failed,
                        error: Some(f.error.clone()),
                    });
                }
            }
            report.failures.extend(refused);
            return;
        }
        let mut any_went = false;
        for folder in item.folders() {
            match self.remove_verified(folder, check) {
                Ok(()) => {
                    any_went = true;
                    let is_trash = self.mode == RemovalMode::Trash && !folder.contents_only;
                    let bytes = folder.own_bytes();
                    if is_trash {
                        report.trashed_bytes =
                            report.trashed_bytes.saturating_add(bytes);
                    } else {
                        report.freed_bytes = report.freed_bytes.saturating_add(bytes);
                    }
                    if let Some(ref mut cb) = outcome_cb {
                        cb(&ItemOutcome {
                            path: folder.path.clone(),
                            bytes,
                            outcome: if is_trash {
                                OutcomeKind::Trashed
                            } else {
                                OutcomeKind::Removed
                            },
                            error: None,
                        });
                    }
                }
                Err(error) => {
                    let fail = failure(folder, error);
                    if let Some(ref mut cb) = outcome_cb {
                        cb(&ItemOutcome {
                            path: fail.path.clone(),
                            bytes: 0,
                            outcome: OutcomeKind::Failed,
                            error: Some(fail.error.clone()),
                        });
                    }
                    report.failures.push(fail);
                }
            }
        }
        report.removed_count += usize::from(any_went);
    }

    /// Runs the safety rule, confirms the path is the one scanned, and removes it.
    pub fn remove_verified(
        &self,
        item: &ScanItem,
        check: impl Fn(&Path) -> Result<(), SafetyError>,
    ) -> Result<(), ItemRemovalError> {
        Self::verify(item, check)?;
        if item.contents_only || self.mode == RemovalMode::Permanent {
            if item.contents_only {
                Self::remove_contents(&item.path)?;
            } else if item.path.is_dir() {
                fs::remove_dir_all(&item.path)?;
            } else {
                fs::remove_file(&item.path)?;
            }
        } else {
            self.host
                .trash(&item.path)
                .map_err(ItemRemovalError::Trash)?;
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
    use crate::core::host::MacHost;
    use crate::core::host::fake::FakeHost;
    use std::cell::{Cell, RefCell};
    use std::os::unix::fs::PermissionsExt;
    use std::rc::Rc;
    use tempfile::tempdir;

    fn cleaner() -> Cleaner<'static> {
        Cleaner::new(&MacHost, RemovalMode::Permanent)
    }

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
        cleaner().remove_with(
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

        let host = FakeHost::default();
        let cleaner = Cleaner::new(&host, RemovalMode::Permanent);
        let report = cleaner.remove_with(
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

        let report = cleaner().remove_with(
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
        let report = cleaner().remove_items_with_progress(
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

        let error = cleaner().remove_verified(&item, refuse).unwrap_err();

        assert!(target.exists());
        assert!(error.to_string().contains("Safety check failed"));
        assert!(!error.is_permission_denied());
    }

    fn row_of_two_folders(root: &Path) -> (ScanItem, PathBuf, PathBuf) {
        let (first, second) = (root.join("first"), root.join("second"));
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        let mut row = ScanItem::new(first.clone(), 10, true, false);
        row.also = vec![ScanItem::new(second.clone(), 3, true, false)];
        (row, first, second)
    }

    /// `remove_dir_all` fails on `folder`, because nothing can leave this subfolder.
    fn lock_inside(folder: &Path) -> PathBuf {
        let locked = folder.join("locked");
        fs::create_dir_all(&locked).unwrap();
        fs::write(locked.join("data"), b"x").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
        locked
    }

    fn unlock(locked: &Path) {
        fs::set_permissions(locked, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn a_row_of_two_folders_goes_whole_and_counts_once() {
        let temp = tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let (row, first, second) = row_of_two_folders(&root);

        let report = cleaner().remove_items(&[row], &IgnoreStore::default());

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.removed_count, 1);
        assert_eq!(report.freed_bytes, 10);
        assert!(!first.exists());
        assert!(!second.exists());
    }

    #[test]
    fn a_row_counts_its_first_folder_when_the_second_fails_during_removal() {
        let temp = tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let (row, first, second) = row_of_two_folders(&root);
        let locked = lock_inside(&second);

        let report = cleaner().remove_items(&[row], &IgnoreStore::default());

        unlock(&locked);
        assert_eq!(report.removed_count, 1);
        assert_eq!(report.freed_bytes, 7);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].path, second);
        assert!(!first.exists());
        assert!(second.exists());
    }

    #[test]
    fn a_row_counts_its_second_folder_when_the_first_fails_during_removal() {
        let temp = tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let (row, first, second) = row_of_two_folders(&root);
        let locked = lock_inside(&first);

        let report = cleaner().remove_items(&[row], &IgnoreStore::default());

        unlock(&locked);
        assert_eq!(report.removed_count, 1);
        assert_eq!(report.freed_bytes, 3);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].path, first);
        assert!(first.exists());
        assert!(!second.exists());
    }

    #[test]
    fn a_row_counts_nothing_when_every_folder_fails_during_removal() {
        let temp = tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let (row, first, second) = row_of_two_folders(&root);
        let locks = [lock_inside(&first), lock_inside(&second)];

        let report = cleaner().remove_items(&[row], &IgnoreStore::default());

        locks.iter().for_each(|locked| unlock(locked));
        assert_eq!(report.removed_count, 0);
        assert_eq!(report.freed_bytes, 0);
        assert_eq!(report.failures.len(), 2);
    }

    #[test]
    fn a_row_stays_whole_when_a_later_folder_was_replaced() {
        let temp = tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let (row, first, second) = row_of_two_folders(&root);
        fs::remove_dir(&second).unwrap();
        fs::write(&second, b"something else").unwrap();

        let report = cleaner().remove_items(&[row], &IgnoreStore::default());

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

        let error = cleaner().remove_verified(&item, allow).unwrap_err();

        assert!(target.exists());
        assert!(error.to_string().contains("identity"));
    }

    #[test]
    fn a_folder_the_user_cannot_write_is_a_permission_error() {
        let temp = tempdir().unwrap();
        let parent = temp.path().join("locked");
        let target = parent.join("data");
        fs::create_dir_all(&target).unwrap();
        let item = ScanItem::new(target.clone(), 1, true, false);
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o555)).unwrap();

        let error = cleaner().remove_verified(&item, allow).unwrap_err();

        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(error.is_permission_denied(), "{error}");
    }

    #[test]
    fn a_verified_path_is_removed() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("data.bin");
        fs::write(&target, b"x").unwrap();
        let item = ScanItem::new(target.clone(), 1, true, false);

        cleaner().remove_verified(&item, allow).unwrap();

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
        let report = cleaner().remove_items(&[item], &ignore);

        assert_eq!(report.removed_count, 1);
        assert!(target.exists());
        assert!(!target.join("app.log").exists());
    }

    #[test]
    fn in_trash_mode_the_host_gets_the_call_and_fs_removes_nothing() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("trash_test");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("file.txt"), b"hello").unwrap();
        let item = ScanItem::new(target.clone(), 5, true, false);

        let host = FakeHost::default();
        let cleaner = Cleaner::new(&host, RemovalMode::Trash);
        let report = cleaner.remove_items(&[item], &IgnoreStore::default());

        assert_eq!(host.trashed(), vec![target.clone()]);
        assert!(target.exists());
        assert_eq!(report.removed_count, 1);
        assert_eq!(report.trashed_bytes, 5);
        assert_eq!(report.freed_bytes, 0);
    }

    #[test]
    fn a_refused_path_and_a_replaced_path_dont_reach_the_host() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("refused");
        fs::create_dir_all(&target).unwrap();
        let item = ScanItem::new(target.clone(), 5, true, false);

        let host = FakeHost::default();
        let cleaner = Cleaner::new(&host, RemovalMode::Trash);
        let error = cleaner.remove_verified(&item, refuse).unwrap_err();
        assert!(error.to_string().contains("Safety check failed"));
        assert!(host.trashed().is_empty());

        // Replaced path
        fs::remove_dir(&target).unwrap();
        fs::create_dir_all(temp.path().join("spacer")).unwrap();
        fs::create_dir_all(&target).unwrap();
        let error = cleaner.remove_verified(&item, allow).unwrap_err();
        assert!(error.to_string().contains("identity"));
        assert!(host.trashed().is_empty());
    }

    #[test]
    fn a_failed_trash_call_leaves_the_file_in_place_and_lists_a_failure() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("fail_trash");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("file.txt"), b"content").unwrap();
        let item = ScanItem::new(target.clone(), 7, true, false);

        let host = FakeHost::default();
        host.fail_trash("operation not permitted");
        let cleaner = Cleaner::new(&host, RemovalMode::Trash);
        let report = cleaner.remove_items(&[item], &IgnoreStore::default());

        assert!(target.exists());
        assert_eq!(report.removed_count, 0);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].path, target);
        assert!(report.failures[0].error.contains("operation not permitted"));
    }

    #[test]
    fn contents_only_items_are_removed_permanently() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("logs");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("app.log"), b"log data").unwrap();
        let item = ScanItem::new(target.clone(), 8, true, true);

        let host = FakeHost::default();
        let cleaner = Cleaner::new(&host, RemovalMode::Trash);
        let report = cleaner.remove_items(&[item], &IgnoreStore::default());

        assert!(host.trashed().is_empty());
        assert!(target.exists());
        assert!(!target.join("app.log").exists());
        assert_eq!(report.removed_count, 1);
        assert_eq!(report.freed_bytes, 8);
        assert_eq!(report.trashed_bytes, 0);
    }

    #[test]
    fn item_removal_mode_routes_correctly() {
        let item = ScanItem::new(
            PathBuf::from("/Users/alice/Library/Caches/foo"),
            100,
            true,
            false,
        );
        assert_eq!(
            item_removal_mode(&item, "clean", false),
            RemovalMode::Permanent
        );
        assert_eq!(
            item_removal_mode(&item, "purge", false),
            RemovalMode::Permanent
        );
        assert_eq!(
            item_removal_mode(&item, "analyze", false),
            RemovalMode::Trash
        );
        assert_eq!(
            item_removal_mode(&item, "analyze", true),
            RemovalMode::Permanent
        );

        let mut contents_only = item.clone();
        contents_only.contents_only = true;
        assert_eq!(
            item_removal_mode(&contents_only, "analyze", false),
            RemovalMode::Permanent
        );

        let root_item = ScanItem::new(
            PathBuf::from("/Library/PrivilegedHelperTools/foo"),
            100,
            true,
            false,
        );
        assert_eq!(
            item_removal_mode(&root_item, "uninstall", false),
            RemovalMode::Permanent
        );
    }

    #[test]
    fn outcome_callback_receives_each_item() {
        let temp = tempfile::tempdir().unwrap();
        let file_path = temp.path().join("file.txt");
        std::fs::write(&file_path, "hello").unwrap();

        let item = ScanItem::new(file_path.clone(), 5, true, false);
        let mut outcomes = Vec::new();

        let cleaner = cleaner();
        cleaner.remove_items_with_callbacks(
            &[item],
            &IgnoreStore::default(),
            None::<fn(RemovalProgress)>,
            Some(|outcome: &ItemOutcome| {
                outcomes.push(outcome.clone());
            }),
        );

        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].path, file_path);
        assert_eq!(outcomes[0].outcome, OutcomeKind::Removed);
    }
}

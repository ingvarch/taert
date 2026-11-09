//! Local copies of iCloud Drive files that iCloud can download again.
//!
//! taert never deletes a file here. It asks iCloud to remove the local copy of a file that
//! iCloud has uploaded, and iCloud downloads the file again when it is opened.

use std::ffi::OsStr;
use std::fs;
use std::io::ErrorKind;
use std::os::darwin::fs::MetadataExt as DarwinMetadataExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::SystemTime;

use super::cleaner::{RemovalFailure, RemovalReport};
use super::history::{ItemOutcome, OutcomeKind};
use super::host::Host;
use super::layout::Layout;
use super::path_utils::{STALE_AFTER, prettify_path_in};
use super::safety::{FileIdentity, SF_DATALESS, Safety, SafetyError};
use super::scanner::{CategoryScan, ScanItem, ScanProgressEvent, ToolGroup};
use super::stores::IgnoreStore;

pub const SCAN_ID: &str = "icloud";
pub const TITLE: &str = "iCloud Local Copies";
pub const DRIVE_GROUP: &str = "iCloud Drive";
pub const APPS_GROUP: &str = "Apps";
pub const STAY_NOTE: &str = "They stay in iCloud and download again when you open them.";
pub const MISSING_NOTE: &str = "No iCloud Drive folder was found on this Mac.";
pub const UNREADABLE_NOTE: &str = "macOS hides some iCloud folders from this terminal. \
     Give the terminal Full Disk Access in System Settings to include them.";
pub const CANCELLED_NOTE: &str = "Cancelled before the end.";

pub(crate) const MOBILE_DOCUMENTS: &str = "Library/Mobile Documents";
pub(crate) const DRIVE: &str = "com~apple~CloudDocs";

pub fn root(layout: &Layout) -> PathBuf {
    layout.user(MOBILE_DOCUMENTS)
}

fn is_hidden(name: &OsStr) -> bool {
    name.to_string_lossy().starts_with('.')
}

/// The rows of the list: what is in iCloud Drive, and the folder of each app. A symlink is
/// no row, and neither is a plain file next to the app folders.
pub(crate) fn rows(layout: &Layout) -> std::io::Result<Vec<(&'static str, PathBuf)>> {
    let root = root(layout);
    let mut rows = Vec::new();
    let mut drive_children = Vec::new();
    match fs::read_dir(root.join(DRIVE)) {
        Ok(entries) => drive_children.extend(entries.flatten()),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    for entry in drive_children {
        let kind = entry.file_type()?;
        if !is_hidden(&entry.file_name()) && !kind.is_symlink() {
            rows.push((DRIVE_GROUP, entry.path()));
        }
    }
    for entry in fs::read_dir(&root)?.flatten() {
        let name = entry.file_name();
        if name != DRIVE && !is_hidden(&name) && entry.file_type()?.is_dir() {
            rows.push((APPS_GROUP, entry.path()));
        }
    }
    rows.sort();
    Ok(rows)
}

/// `inside` is a path under `root`. Hidden entries and the iCloud trash are not rows.
pub(crate) fn is_hidden_place(inside: &Path) -> bool {
    let names: Vec<&OsStr> = inside.iter().collect();
    let hidden = |name: Option<&&OsStr>| name.is_some_and(|name| is_hidden(name));
    hidden(names.first())
        || (names.first().is_some_and(|first| *first == DRIVE) && hidden(names.get(1)))
        || names.contains(&OsStr::new(".Trash"))
}

pub(crate) fn is_row(layout: &Layout, path: &Path) -> bool {
    let root = root(layout);
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return false;
    };
    if is_hidden(name) {
        return false;
    }
    (parent == root && name != DRIVE) || parent == root.join(DRIVE)
}

struct FileVisit {
    path: PathBuf,
    identity: FileIdentity,
    bytes: u64,
    last_use: Option<SystemTime>,
    dataless: bool,
}

impl FileVisit {
    /// Whether the file takes room on this Mac.
    fn is_local(&self) -> bool {
        self.bytes > 0 && !self.dataless
    }
}

enum Visit {
    Folder { modified: Option<SystemTime> },
    File(FileVisit),
    Unreadable,
}

fn newest(left: Option<SystemTime>, right: Option<SystemTime>) -> Option<SystemTime> {
    left.into_iter().chain(right).max()
}

fn open_entry(
    path: &Path,
    meta: &fs::Metadata,
    ignore: &IgnoreStore,
    folders: &mut Vec<PathBuf>,
    visit: &mut impl FnMut(Visit),
) {
    let kind = meta.file_type();
    if kind.is_dir() {
        let trash = path.file_name().is_some_and(|name| name == ".Trash");
        if trash || Safety::is_ignored(path, ignore) {
            return;
        }
        visit(Visit::Folder {
            modified: meta.modified().ok(),
        });
        if meta.st_flags() & SF_DATALESS == 0 {
            folders.push(path.to_path_buf());
        }
    } else if kind.is_file() {
        visit(Visit::File(FileVisit {
            path: path.to_path_buf(),
            identity: FileIdentity::from_meta(meta),
            bytes: meta.blocks() * 512,
            last_use: newest(meta.modified().ok(), meta.accessed().ok()),
            dataless: meta.st_flags() & SF_DATALESS != 0,
        }));
    }
}

/// Looks at every file and folder of a row once, without following links. A folder that is
/// only in iCloud is not listed, because listing it would make iCloud fetch the listing.
fn walk(
    row: &Path,
    ignore: &IgnoreStore,
    cancel: Option<&AtomicBool>,
    mut visit: impl FnMut(Visit),
) {
    let mut folders = Vec::new();
    match fs::symlink_metadata(row) {
        Ok(meta) => open_entry(row, &meta, ignore, &mut folders, &mut visit),
        Err(_) => return visit(Visit::Unreadable),
    }
    while let Some(dir) = folders.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            visit(Visit::Unreadable);
            continue;
        };
        for entry in entries {
            if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
                return;
            }
            match entry.and_then(|entry| Ok((entry.path(), entry.metadata()?))) {
                Ok((path, meta)) => open_entry(&path, &meta, ignore, &mut folders, &mut visit),
                Err(_) => visit(Visit::Unreadable),
            }
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Measure {
    pub bytes: u64,
    pub files: usize,
    pub last_use: Option<SystemTime>,
    pub unreadable: bool,
}

/// What a run on this row would free, and when anything in it was last used.
pub(crate) fn measure(
    row: &Path,
    layout: &Layout,
    host: &dyn Host,
    ignore: &IgnoreStore,
    cancel: Option<&AtomicBool>,
) -> Measure {
    let mut measure = Measure::default();
    walk(row, ignore, cancel, |visit| match visit {
        Visit::Folder { modified } => measure.last_use = newest(measure.last_use, modified),
        Visit::Unreadable => measure.unreadable = true,
        Visit::File(file) => {
            measure.last_use = newest(measure.last_use, file.last_use);
            if file.is_local()
                && Safety::can_evict(&file.path, file.identity, ignore, layout, host).is_ok()
            {
                measure.bytes += file.bytes;
                measure.files += 1;
            }
        }
    });
    measure
}

/// A row starts selected when nothing in it was used for 30 days and all of it was read.
pub(crate) fn starts_selected(measure: &Measure, now: SystemTime) -> bool {
    !measure.unreadable
        && measure
            .last_use
            .is_none_or(|used| used <= now - STALE_AFTER)
}

pub fn scan(
    layout: &Layout,
    host: &dyn Host,
    ignore: &IgnoreStore,
    now: SystemTime,
    progress: Option<Sender<ScanProgressEvent>>,
    cancel: Option<Arc<AtomicBool>>,
) -> CategoryScan {
    let mut scan = CategoryScan {
        id: SCAN_ID.to_string(),
        title: TITLE.to_string(),
        groups: Vec::new(),
        notes: Vec::new(),
    };
    let rows = match rows(layout) {
        Ok(rows) => rows,
        Err(error) => {
            let note = if error.kind() == ErrorKind::NotFound {
                MISSING_NOTE
            } else {
                UNREADABLE_NOTE
            };
            scan.notes.push(note.to_string());
            return scan;
        }
    };

    let mut unreadable = false;
    let mut found = [(DRIVE_GROUP, Vec::new()), (APPS_GROUP, Vec::new())];
    for (group, path) in rows {
        if cancel
            .as_deref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
        {
            break;
        }
        if Safety::is_ignored(&path, ignore) {
            continue;
        }
        if let Some(tx) = &progress {
            let _ = tx.send(ScanProgressEvent::ScanningPath(prettify_path_in(
                &path,
                &layout.home,
            )));
        }
        let measured = measure(&path, layout, host, ignore, cancel.as_deref());
        unreadable |= measured.unreadable;
        if measured.files == 0 {
            continue;
        }
        let mut item = ScanItem::new(path, measured.bytes, starts_selected(&measured, now), false);
        item.modified = measured.last_use;
        if let Some(tx) = &progress {
            let _ = tx.send(ScanProgressEvent::ItemFound {
                bytes: measured.bytes,
            });
        }
        if let Some((_, items)) = found.iter_mut().find(|(name, _)| *name == group) {
            items.push(item);
        }
    }

    for (tool, mut items) in found {
        if items.is_empty() {
            continue;
        }
        items.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes).then(a.path.cmp(&b.path)));
        scan.groups.push(ToolGroup {
            tool: tool.to_string(),
            items,
        });
    }
    if unreadable {
        scan.notes.push(UNREADABLE_NOTE.to_string());
    }
    scan
}

fn left_note(count: usize) -> String {
    let (files, them, they) = if count == 1 {
        ("file", "it", "it")
    } else {
        ("files", "them", "they")
    };
    format!(
        "Left {count} {files} on this Mac: iCloud has not finished with {them}, \
         or {they} changed since the scan."
    )
}

/// Asks iCloud to remove the local copy of each file in the selected rows. Every row is
/// walked again and every file is checked again, because the scan may be hours old.
pub fn evict_selected(
    host: &dyn Host,
    layout: &Layout,
    ignore: &IgnoreStore,
    items: &[ScanItem],
    cancelled: &AtomicBool,
    mut on_progress: impl FnMut(usize, &Path),
    mut on_outcome: impl FnMut(&ItemOutcome),
) -> RemovalReport {
    let mut report = RemovalReport::default();
    let mut left = 0;
    let fail = |report: &mut RemovalReport,
                on_outcome: &mut dyn FnMut(&ItemOutcome),
                path: &Path,
                bytes: u64,
                error: String| {
        on_outcome(&ItemOutcome {
            path: path.to_path_buf(),
            bytes,
            outcome: OutcomeKind::Failed,
            error: Some(error.clone()),
        });
        report.failures.push(RemovalFailure {
            path: path.to_path_buf(),
            error,
        });
    };

    for (index, item) in items.iter().enumerate() {
        if cancelled.load(Ordering::Relaxed) {
            break;
        }
        on_progress(index, &item.path);
        if !is_row(layout, &item.path) || Safety::has_symlink_component(&item.path) {
            let error = "not an iCloud folder".to_string();
            fail(&mut report, &mut on_outcome, &item.path, 0, error);
            continue;
        }
        let same = item.identity.is_some() && FileIdentity::of(&item.path) == item.identity;
        if !same {
            let error = "changed since the scan".to_string();
            fail(&mut report, &mut on_outcome, &item.path, 0, error);
            continue;
        }

        let mut freed = 0;
        walk(&item.path, ignore, Some(cancelled), |visit| {
            let Visit::File(file) = visit else {
                return;
            };
            if !file.is_local() {
                return;
            }
            match Safety::can_evict(&file.path, file.identity, ignore, layout, host) {
                Ok(()) => match host.evict(&file.path) {
                    Ok(()) => {
                        report.removed_count += 1;
                        freed += file.bytes;
                    }
                    Err(error) => fail(&mut report, &mut on_outcome, &file.path, file.bytes, error),
                },
                Err(SafetyError::NotInICloud | SafetyError::Ignored) => {}
                Err(SafetyError::StaysLocal(_)) => left += 1,
                Err(error) => fail(
                    &mut report,
                    &mut on_outcome,
                    &file.path,
                    file.bytes,
                    error.to_string(),
                ),
            }
        });
        report.freed_bytes += freed;
        if freed > 0 {
            on_outcome(&ItemOutcome::changed(&item.path, freed));
        }
    }

    if left > 0 {
        report.notes.push(left_note(left));
    }
    if cancelled.load(Ordering::Relaxed) {
        report.notes.push(CANCELLED_NOTE.to_string());
    }
    report
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use crate::core::host::{ICloudState, fake::FakeHost};
    use std::fs::{self, File, FileTimes};
    use std::os::unix::fs::MetadataExt;
    use std::time::{Duration, SystemTime};

    fn days_ago(days: u64) -> SystemTime {
        SystemTime::now() - Duration::from_secs(days * 24 * 60 * 60)
    }

    /// Sets the access and modification times of a file or folder.
    pub fn age_at(path: &Path, accessed_days: u64, modified_days: u64) {
        let times = FileTimes::new()
            .set_accessed(days_ago(accessed_days))
            .set_modified(days_ago(modified_days));
        File::open(path).unwrap().set_times(times).unwrap();
    }

    pub fn age(path: &Path, days: u64) {
        age_at(path, days, days);
    }

    /// Ages everything under `row` and the row, folders last, because a file written into
    /// a folder changes its time.
    pub fn age_tree(row: &Path, days: u64) {
        let mut found = vec![row.to_path_buf()];
        let mut next = 0;
        while let Some(dir) = found.get(next).cloned() {
            next += 1;
            if dir.is_dir() {
                found.extend(fs::read_dir(&dir).unwrap().flatten().map(|e| e.path()));
            }
        }
        for path in found.iter().rev().filter(|p| !p.is_symlink()) {
            age(path, days);
        }
    }

    pub fn local_only(path: &Path, days: u64) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![7_u8; 8192]).unwrap();
        age(path, days);
    }

    /// A file iCloud has uploaded. Returns the bytes it takes on disk.
    pub fn kept(host: &FakeHost, path: &Path, days: u64) -> u64 {
        local_only(path, days);
        host.set_icloud(path, ICloudState::EVICTABLE);
        fs::symlink_metadata(path).unwrap().blocks() * 512
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{age, age_at, age_tree, kept, local_only};
    use super::*;
    use crate::core::apps::testing::sandbox;
    use crate::core::history::{HistoryLog, command_for_scan};
    use crate::core::host::{ICloudState, fake::FakeHost};
    use crate::core::scanner::ScanItem;
    use std::fs;
    use std::os::unix::fs::MetadataExt;
    use std::sync::atomic::AtomicBool;
    use std::time::SystemTime;

    const OLD: u64 = 60;
    const RECENT: u64 = 2;

    fn drive(layout: &Layout) -> PathBuf {
        root(layout).join(DRIVE)
    }

    fn app(layout: &Layout, name: &str) -> PathBuf {
        root(layout).join(format!("iCloud~com~example~{name}"))
    }

    fn scan_now(layout: &Layout, host: &FakeHost) -> crate::core::scanner::CategoryScan {
        scan(
            layout,
            host,
            &IgnoreStore::default(),
            SystemTime::now(),
            None,
            None,
        )
    }

    fn paths(scan: &crate::core::scanner::CategoryScan, tool: &str) -> Vec<PathBuf> {
        scan.groups
            .iter()
            .filter(|group| group.tool == tool)
            .flat_map(|group| group.items.iter().map(|item| item.path.clone()))
            .collect()
    }

    fn row_of<'a>(
        scan: &'a crate::core::scanner::CategoryScan,
        path: &Path,
    ) -> Option<&'a ScanItem> {
        scan.groups
            .iter()
            .flat_map(|group| &group.items)
            .find(|item| item.path == path)
    }

    fn evict(
        host: &FakeHost,
        layout: &Layout,
        items: &[ScanItem],
    ) -> (crate::core::cleaner::RemovalReport, Vec<ItemOutcome>) {
        let mut outcomes = Vec::new();
        let report = evict_selected(
            host,
            layout,
            &IgnoreStore::default(),
            items,
            &AtomicBool::new(false),
            |_, _| {},
            |outcome| outcomes.push(outcome.clone()),
        );
        (report, outcomes)
    }

    fn selected(scan: &crate::core::scanner::CategoryScan) -> Vec<ScanItem> {
        scan.selected_items().into_iter().cloned().collect()
    }

    #[test]
    fn rows_are_drive_entries_and_app_folders() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        kept(&host, &drive(&layout).join("Archive/a.pdf"), OLD);
        kept(&host, &drive(&layout).join("top.pdf"), OLD);
        kept(&host, &app(&layout, "Notes").join("Documents/n.txt"), OLD);

        let scan = scan_now(&layout, &host);

        assert_eq!(scan.id, SCAN_ID);
        assert_eq!(scan.title, TITLE);
        let tools: Vec<_> = scan.groups.iter().map(|g| g.tool.as_str()).collect();
        assert_eq!(tools, [DRIVE_GROUP, APPS_GROUP]);
        let mut drive_rows = paths(&scan, DRIVE_GROUP);
        drive_rows.sort();
        assert_eq!(
            drive_rows,
            [
                drive(&layout).join("Archive"),
                drive(&layout).join("top.pdf")
            ]
        );
        assert_eq!(paths(&scan, APPS_GROUP), [app(&layout, "Notes")]);
    }

    #[test]
    fn hidden_entries_and_the_icloud_trash_make_no_rows() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        kept(&host, &root(&layout).join(".Trash/x/a"), OLD);
        kept(&host, &drive(&layout).join(".Trash/y"), OLD);
        kept(&host, &drive(&layout).join(".DS_Store"), OLD);
        kept(&host, &root(&layout).join(".hidden/z"), OLD);

        let scan = scan_now(&layout, &host);

        assert!(scan.groups.is_empty());
        assert_eq!(rows(&layout).unwrap(), []);
    }

    #[test]
    fn a_row_counts_only_files_icloud_can_download_again() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        let row = drive(&layout).join("Archive");
        let size = kept(&host, &row.join("kept.pdf"), OLD);
        let e = ICloudState::EVICTABLE;
        let others = [
            (
                "not_uploaded",
                ICloudState {
                    uploaded: false,
                    ..e
                },
            ),
            (
                "uploading",
                ICloudState {
                    uploading: true,
                    ..e
                },
            ),
            (
                "conflicted",
                ICloudState {
                    conflicts: true,
                    ..e
                },
            ),
            (
                "not_downloaded",
                ICloudState {
                    downloaded: false,
                    ..e
                },
            ),
        ];
        for (name, state) in others {
            local_only(&row.join(name), OLD);
            host.set_icloud(&row.join(name), state);
        }
        age_tree(&row, OLD);

        let scan = scan_now(&layout, &host);

        assert_eq!(row_of(&scan, &row).unwrap().size_bytes, size);
    }

    #[test]
    fn rows_with_nothing_to_remove_are_left_out() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        local_only(&app(&layout, "Local").join("a.txt"), OLD);
        fs::create_dir_all(app(&layout, "Empty")).unwrap();
        let zero = app(&layout, "Zero").join("z.txt");
        fs::create_dir_all(zero.parent().unwrap()).unwrap();
        fs::write(&zero, "").unwrap();
        host.set_icloud(&zero, ICloudState::EVICTABLE);

        let scan = scan_now(&layout, &host);

        assert!(scan.groups.is_empty());
    }

    #[test]
    fn a_row_starts_selected_when_nothing_in_it_was_used_for_30_days() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        let names = ["Old", "Read", "Written", "Ineligible", "Folder"];
        let rows: Vec<_> = names.iter().map(|name| app(&layout, name)).collect();
        for row in &rows {
            kept(&host, &row.join("sub/a.txt"), OLD);
        }
        age_at(&rows[1].join("sub/a.txt"), RECENT, OLD);
        age_at(&rows[2].join("sub/a.txt"), OLD, RECENT);
        local_only(&rows[3].join("sub/local.txt"), RECENT);
        for row in &rows {
            age(&row.join("sub"), OLD);
            age(row, OLD);
        }
        age(&rows[4].join("sub"), RECENT);

        let scan = scan_now(&layout, &host);

        let flags: Vec<_> = rows
            .iter()
            .map(|row| row_of(&scan, row).unwrap().selected)
            .collect();
        assert_eq!(flags, [true, false, false, false, false]);
    }

    #[test]
    fn scanning_reads_no_file_and_changes_nothing() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        let file = app(&layout, "Notes").join("a.txt");
        kept(&host, &file, OLD);
        let before = fs::metadata(&file).unwrap();

        scan_now(&layout, &host);

        let after = fs::metadata(&file).unwrap();
        assert_eq!(
            (before.atime(), before.atime_nsec()),
            (after.atime(), after.atime_nsec())
        );
        assert!(host.evicted().is_empty());
        assert_eq!(fs::read(&file).unwrap(), vec![7_u8; 8192]);
    }

    #[test]
    fn the_scan_never_follows_symlinks() {
        let (temp, layout) = sandbox();
        let host = FakeHost::default();
        let outside = temp.path().canonicalize().unwrap().join("outside");
        let far = outside.join("far.txt");
        kept(&host, &far, OLD);
        fs::create_dir_all(drive(&layout)).unwrap();
        std::os::unix::fs::symlink(&outside, drive(&layout).join("Linked")).unwrap();
        let row = app(&layout, "Notes");
        let size = kept(&host, &row.join("a.txt"), OLD);
        let link = row.join("link.txt");
        std::os::unix::fs::symlink(&far, &link).unwrap();
        host.set_icloud(&link, ICloudState::EVICTABLE);

        let scan = scan_now(&layout, &host);

        assert_eq!(paths(&scan, DRIVE_GROUP), Vec::<PathBuf>::new());
        assert_eq!(row_of(&scan, &row).unwrap().size_bytes, size);
        assert_eq!(rows(&layout).unwrap(), [(APPS_GROUP, row)]);
    }

    #[test]
    fn ignored_rows_and_files_are_left_out() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        let hidden = app(&layout, "Hidden");
        kept(&host, &hidden.join("a.txt"), OLD);
        let row = app(&layout, "Notes");
        let size = kept(&host, &row.join("a.txt"), OLD);
        kept(&host, &row.join("skip/b.txt"), OLD);
        let ignore = IgnoreStore::new(vec![
            hidden.to_string_lossy().into_owned(),
            row.join("skip").to_string_lossy().into_owned(),
        ]);

        let scan = scan(&layout, &host, &ignore, SystemTime::now(), None, None);

        assert!(row_of(&scan, &hidden).is_none());
        assert_eq!(row_of(&scan, &row).unwrap().size_bytes, size);
    }

    #[test]
    fn without_mobile_documents_the_scan_says_so() {
        let (_temp, layout) = sandbox();

        let scan = scan_now(&layout, &FakeHost::default());

        assert!(scan.groups.is_empty());
        assert_eq!(scan.notes, [MISSING_NOTE]);
    }

    #[test]
    fn scan_notes_a_row_it_cannot_read() {
        use std::os::unix::fs::PermissionsExt;
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        let row = app(&layout, "Notes");
        kept(&host, &row.join("a.txt"), OLD);
        let locked = row.join("locked");
        fs::create_dir_all(&locked).unwrap();
        age_tree(&row, OLD);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let scan = scan_now(&layout, &host);

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!row_of(&scan, &row).unwrap().selected);
        assert!(scan.notes.contains(&UNREADABLE_NOTE.to_string()));
    }

    #[test]
    fn evict_selected_removes_local_copies_of_eligible_files_only() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        let row = app(&layout, "Notes");
        let files = [
            row.join("a.txt"),
            row.join("deep/b.txt"),
            row.join("deep/er/c.txt"),
        ];
        let size: u64 = files.iter().map(|f| kept(&host, f, OLD)).sum();
        local_only(&row.join("local.txt"), OLD);
        local_only(&row.join("busy.txt"), OLD);
        host.set_icloud(
            &row.join("busy.txt"),
            ICloudState {
                uploading: true,
                ..ICloudState::EVICTABLE
            },
        );
        age_tree(&row, OLD);
        let scan = scan_now(&layout, &host);
        assert!(row_of(&scan, &row).unwrap().selected);

        let (report, _) = evict(&host, &layout, &selected(&scan));

        let mut evicted = host.evicted();
        evicted.sort();
        let mut expected = files.to_vec();
        expected.sort();
        assert_eq!(evicted, expected);
        assert!(evicted.iter().all(|path| path.is_file()));
        let forbidden = [root(&layout), drive(&layout), app(&layout, "Notes")];
        assert!(evicted.iter().all(|path| !forbidden.contains(path)));
        assert_eq!(report.removed_count, 3);
        assert_eq!(report.freed_bytes, size);
        assert!(report.failures.is_empty());
    }

    #[test]
    fn a_run_rereads_icloud_and_leaves_what_changed_since_the_scan() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        let row = app(&layout, "Notes");
        let (a, b, c) = (row.join("a.txt"), row.join("b.txt"), row.join("c.txt"));
        for file in [&a, &b, &c] {
            kept(&host, file, OLD);
        }
        age_tree(&row, OLD);
        let scan = scan_now(&layout, &host);
        host.set_icloud(
            &a,
            ICloudState {
                uploading: true,
                ..ICloudState::EVICTABLE
            },
        );
        local_only(&row.join("new.txt"), 0);
        fs::remove_file(&c).unwrap();

        let (report, _) = evict(&host, &layout, &selected(&scan));

        assert_eq!(host.evicted(), [b]);
        assert!(report.failures.is_empty());
        assert_eq!(
            report.notes,
            [
                "Left 1 file on this Mac: iCloud has not finished with it, or it changed since the scan."
            ]
        );
    }

    #[test]
    fn files_icloud_never_kept_and_ignored_files_leave_no_note() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        let row = app(&layout, "Notes");
        kept(&host, &row.join("a.txt"), OLD);
        kept(&host, &row.join("skip/b.txt"), OLD);
        age_tree(&row, OLD);
        let scan = scan_now(&layout, &host);
        local_only(&row.join("local.txt"), OLD);
        let ignore = IgnoreStore::new(vec![row.join("skip").to_string_lossy().into_owned()]);

        let report = evict_selected(
            &host,
            &layout,
            &ignore,
            &selected(&scan),
            &AtomicBool::new(false),
            |_, _| {},
            |_| {},
        );

        assert_eq!(host.evicted(), [row.join("a.txt")]);
        assert!(report.failures.is_empty());
        assert!(report.notes.is_empty());
    }

    #[test]
    fn the_left_note_is_grammatical_for_one_and_many() {
        assert!(
            left_note(1).starts_with("Left 1 file on this Mac: iCloud has not finished with it,")
        );
        assert!(
            left_note(2)
                .starts_with("Left 2 files on this Mac: iCloud has not finished with them,")
        );
    }

    #[test]
    fn a_row_folder_replaced_since_the_scan_is_refused() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        let row = app(&layout, "Notes");
        kept(&host, &row.join("a.txt"), OLD);
        age_tree(&row, OLD);
        let scan = scan_now(&layout, &host);
        let items = selected(&scan);
        fs::rename(&row, app(&layout, "Gone")).unwrap();
        kept(&host, &row.join("a.txt"), OLD);
        assert_ne!(
            crate::core::safety::FileIdentity::of(&row),
            items[0].identity
        );

        let (report, _) = evict(&host, &layout, &items);

        assert!(host.evicted().is_empty());
        assert_eq!(report.failures.len(), 1);
        assert!(report.failures[0].error.contains("changed since the scan"));
    }

    #[test]
    fn a_row_that_is_not_an_icloud_row_is_refused() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        let places = [
            layout.home.join("Documents"),
            root(&layout),
            drive(&layout),
            root(&layout).join(".Trash"),
        ];
        let mut items = Vec::new();
        for place in &places {
            kept(&host, &place.join("a.txt"), OLD);
            items.push(ScanItem::new(place.clone(), 8192, true, false));
        }

        let (report, _) = evict(&host, &layout, &items);

        assert!(host.evicted().is_empty());
        assert_eq!(report.failures.len(), 4);
    }

    #[test]
    fn an_eviction_error_is_reported_and_the_run_goes_on() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        let row = app(&layout, "Notes");
        let (bad, good) = (row.join("bad.txt"), row.join("good.txt"));
        kept(&host, &bad, OLD);
        let good_bytes = kept(&host, &good, OLD);
        age_tree(&row, OLD);
        let scan = scan_now(&layout, &host);
        host.fail_evict(&bad, "busy");

        let (report, outcomes) = evict(&host, &layout, &selected(&scan));

        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.removed_count, 1);
        assert!(outcomes.contains(&ItemOutcome::changed(&row, good_bytes)));
        assert!(outcomes.iter().any(|o| o.path == bad
            && o.outcome == OutcomeKind::Failed
            && o.error.as_deref() == Some("busy")));
    }

    #[test]
    fn cancelling_stops_before_the_next_file() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        let row = app(&layout, "Notes");
        kept(&host, &row.join("a.txt"), OLD);
        kept(&host, &row.join("b.txt"), OLD);
        age_tree(&row, OLD);
        let scan = scan_now(&layout, &host);
        let cancelled = AtomicBool::new(false);

        let report = evict_selected(
            &host,
            &layout,
            &IgnoreStore::default(),
            &selected(&scan),
            &cancelled,
            |_, _| cancelled.store(true, Ordering::Relaxed),
            |_| {},
        );

        assert!(host.evicted().is_empty());
        assert!(report.notes.contains(&CANCELLED_NOTE.to_string()));
    }

    #[test]
    fn a_run_is_logged_under_icloud() {
        let (temp, layout) = sandbox();
        let host = FakeHost::default();
        let row = app(&layout, "Notes");
        let bytes = kept(&host, &row.join("a.txt"), OLD);
        age_tree(&row, OLD);
        let scan = scan_now(&layout, &host);
        let log = HistoryLog::new(temp.path().join("history.jsonl"));

        let session = log.start_run(&command_for_scan(SCAN_ID), "force", Some(1));
        evict_selected(
            &host,
            &layout,
            &IgnoreStore::default(),
            &selected(&scan),
            &AtomicBool::new(false),
            |_, _| {},
            |outcome| session.record_item(outcome),
        );
        session.finish();

        let runs = log.load_runs().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].command, "icloud");
        assert_eq!(runs[0].items.len(), 1);
        assert_eq!(runs[0].items[0].path, row.to_string_lossy());
        assert_eq!(runs[0].items[0].outcome, OutcomeKind::Changed);
        assert_eq!(runs[0].items[0].bytes, bytes);
        assert!(runs[0].summary().contains("changed 1"));
    }
}

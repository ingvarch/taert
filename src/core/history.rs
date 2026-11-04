//! History log of removals and changes.
//!
//! Records cleanup and uninstall events into `~/.config/taert/history.jsonl`
//! in append-only JSONL format with permissions `0600`.

use std::collections::{HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::bytes::format_bytes;
use super::catalog::Category;
use super::date_utils::{format_rfc3339, format_system_time, parse_rfc3339};
use super::safety::Safety;
use super::{compress, docker, homebrew, large_files, purger};

/// The history command a scan is logged under when its rows are removed.
pub fn command_for_scan(scan_id: &str) -> String {
    match scan_id {
        docker::SCAN_ID => "docker".to_string(),
        homebrew::CLEANUP_ID => "brew cleanup".to_string(),
        homebrew::SCAN_ID => "brew uninstall".to_string(),
        purger::SCAN_ID => "purge".to_string(),
        large_files::SCAN_ID => "large".to_string(),
        compress::SCAN_ID => "compress".to_string(),
        id => Category::from_id(id)
            .map_or("clean", Category::history_command)
            .to_string(),
    }
}

static RUN_COUNTER: AtomicUsize = AtomicUsize::new(0);
static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// The outcome of an item removal or modification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutcomeKind {
    Removed,
    Trashed,
    Failed,
    Changed,
}

impl std::fmt::Display for OutcomeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Removed => write!(f, "removed"),
            Self::Trashed => write!(f, "trashed"),
            Self::Failed => write!(f, "failed"),
            Self::Changed => write!(f, "changed"),
        }
    }
}

/// Outcome reported for an individual item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemOutcome {
    pub path: PathBuf,
    pub bytes: u64,
    pub outcome: OutcomeKind,
    pub error: Option<String>,
}

impl ItemOutcome {
    pub fn changed(path: impl Into<PathBuf>, bytes: u64) -> Self {
        Self {
            path: path.into(),
            bytes,
            outcome: OutcomeKind::Changed,
            error: None,
        }
    }
}

/// A line in the JSONL history file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "lowercase")]
pub enum HistoryEvent {
    Start {
        run: String,
        at: String,
        command: String,
        mode: String,
        version: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        total: Option<usize>,
    },
    Item {
        run: String,
        path: String,
        bytes: u64,
        outcome: OutcomeKind,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    End {
        run: String,
        at: String,
    },
}

/// An individual item in a reconstructed run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunItem {
    pub path: String,
    pub bytes: u64,
    pub outcome: OutcomeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A reconstructed run with all its items.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    pub run: String,
    pub at: String,
    pub command: String,
    pub mode: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
    pub completed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    #[serde(default)]
    pub items: Vec<RunItem>,
}

impl RunRecord {
    pub fn is_interrupted(&self) -> bool {
        !self.completed
    }

    /// Short human-readable summary of the run outcome.
    pub fn summary(&self) -> String {
        if self.is_interrupted() {
            let count = self.items.len();
            if let Some(total) = self.total {
                format!("interrupted after {count} of {total}")
            } else {
                format!("interrupted after {count}")
            }
        } else {
            let mut parts = Vec::new();

            let trashed: Vec<_> = self
                .items
                .iter()
                .filter(|i| i.outcome == OutcomeKind::Trashed)
                .collect();
            let removed: Vec<_> = self
                .items
                .iter()
                .filter(|i| i.outcome == OutcomeKind::Removed)
                .collect();
            let changed: Vec<_> = self
                .items
                .iter()
                .filter(|i| i.outcome == OutcomeKind::Changed)
                .collect();
            let failed: Vec<_> = self
                .items
                .iter()
                .filter(|i| i.outcome == OutcomeKind::Failed)
                .collect();

            if !trashed.is_empty() {
                let bytes: u64 = trashed.iter().map(|i| i.bytes).sum();
                parts.push(format!("trash {}, {}", trashed.len(), format_bytes(bytes)));
            }
            if !removed.is_empty() {
                let bytes: u64 = removed.iter().map(|i| i.bytes).sum();
                parts.push(format!(
                    "removed {}, {}",
                    removed.len(),
                    format_bytes(bytes)
                ));
            }
            if !changed.is_empty() {
                let bytes: u64 = changed.iter().map(|i| i.bytes).sum();
                parts.push(format!(
                    "changed {}, {}",
                    changed.len(),
                    format_bytes(bytes)
                ));
            }
            if !failed.is_empty() {
                parts.push(format!("{} failed", failed.len()));
            }

            if parts.is_empty() {
                "nothing removed".to_string()
            } else {
                parts.join(", ")
            }
        }
    }

    /// Formats the run as a fixed-column summary line for `taert history`.
    ///
    /// Columns align to exact widths:
    /// - Date (0..16) + 2 spaces
    /// - Command (18..35)
    /// - Mode (35..48)
    /// - Summary (48..)
    pub fn format_line(&self) -> String {
        let date_str = if let Some(time) = parse_rfc3339(&self.at) {
            format_system_time(time)
        } else {
            self.at.clone()
        };

        // Command starts at 18, mode starts at 35 (width 17).
        let mut line = format!("{date_str}  {}", self.command);
        if line.len() < 35 {
            line.push_str(&" ".repeat(35 - line.len()));
        } else {
            line.push(' ');
        }

        // Mode starts at 35, summary starts at 48 (width 13).
        line.push_str(&self.mode);
        if line.len() < 48 {
            line.push_str(&" ".repeat(48 - line.len()));
        } else {
            line.push_str("  ");
        }

        line.push_str(&self.summary());
        line
    }
}

/// Generates a human-readable run identifier: `YYYYMMDD-HHMMSS-XXXX`.
pub fn generate_run_id(time: SystemTime) -> String {
    let dur = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = dur.as_secs() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        libc::gmtime_r(&secs, &mut tm);
    }
    let pid = std::process::id();
    let count = RUN_COUNTER.fetch_add(1, Ordering::Relaxed) % 1000;
    let micros = dur.subsec_micros() % 1000;
    let suffix = ((pid as u64 * 31 + count as u64 * 7 + micros as u64) % 10000) as u16;
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}-{:04}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        suffix
    )
}

/// Moves the `NNNN` suffix of the id on from `start` until no id in `used` has it.
fn free_run_id(second: &str, start: u32, used: &HashSet<String>) -> String {
    (0..10_000)
        .map(|step| format!("{second}{:04}", (start + step) % 10_000))
        .find(|candidate| !used.contains(candidate))
        .unwrap_or_else(|| format!("{second}{start:04}"))
}

/// Run ids in the history file that start with `prefix`.
fn run_ids_in(path: &Path, prefix: &str) -> HashSet<String> {
    read_lines(path)
        .unwrap_or_default()
        .iter()
        .filter(|line| line.contains(prefix))
        .filter_map(|line| run_of(line))
        .filter(|run| run.starts_with(prefix))
        .collect()
}

/// The lines of the history file, with bytes that are not UTF-8 replaced.
fn read_lines(path: &Path) -> io::Result<Vec<String>> {
    let bytes = fs::read(path)?;
    Ok(String::from_utf8_lossy(&bytes)
        .lines()
        .map(String::from)
        .collect())
}

/// The `run` field of a history line.
fn run_of(line: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(line).ok()?;
    value.get("run")?.as_str().map(String::from)
}

/// Checks whether recording history has been disabled via `TAERT_NO_HISTORY=1`.
pub fn is_history_disabled() -> bool {
    std::env::var("TAERT_NO_HISTORY").as_deref() == Ok("1")
}

/// Log file manager for recording and reading runs.
pub struct HistoryLog {
    pub path: PathBuf,
    pub max_runs: usize,
}

impl HistoryLog {
    pub const DEFAULT_MAX_RUNS: usize = 1_000;

    /// The default log path: `~/.config/taert/history.jsonl`.
    pub fn default_path() -> PathBuf {
        Safety::home_dir()
            .join(".config")
            .join("taert")
            .join("history.jsonl")
    }

    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            max_runs: Self::DEFAULT_MAX_RUNS,
        }
    }

    pub fn with_max_runs(mut self, max_runs: usize) -> Self {
        self.max_runs = max_runs;
        self
    }

    /// Starts a new run session.
    pub fn start_run(&self, command: &str, mode: &str, total: Option<usize>) -> HistorySession {
        self.start_run_at(SystemTime::now(), command, mode, total)
    }

    fn start_run_at(
        &self,
        time: SystemTime,
        command: &str,
        mode: &str,
        total: Option<usize>,
    ) -> HistorySession {
        let disabled = is_history_disabled();
        let id = generate_run_id(time);
        let mut session = HistorySession {
            log_path: self.path.clone(),
            run_id: id.clone(),
            disabled,
            warned: AtomicBool::new(false),
            max_runs: self.max_runs,
        };

        if disabled {
            return session;
        }

        let result = self.write_start_line(id, command, mode, total);
        match result {
            Ok(run_id) => session.run_id = run_id,
            Err(e) => session.warn(&format!("could not write to history log: {e}")),
        }
        session
    }

    /// Picks a free run id and writes the start line under one lock.
    fn write_start_line(
        &self,
        id: String,
        command: &str,
        mode: &str,
        total: Option<usize>,
    ) -> io::Result<String> {
        let _lock = lock_history_folder(&self.path)?;
        let (second, suffix) = id.split_at(id.len() - 4);
        let used = run_ids_in(&self.path, second);
        let run = free_run_id(second, suffix.parse().unwrap_or_default(), &used);
        let event = HistoryEvent::Start {
            run: run.clone(),
            at: format_rfc3339(SystemTime::now()),
            command: command.to_string(),
            mode: mode.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            total,
        };
        let line = serde_json::to_string(&event).map_err(io::Error::other)?;
        append_line(&self.path, &line)?;
        Ok(run)
    }

    /// Reads all runs from the file in chronological order.
    /// Broken or corrupted lines are skipped. A read error is returned.
    pub fn load_runs(&self) -> io::Result<Vec<RunRecord>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }

        let mut runs: Vec<RunRecord> = Vec::new();
        let mut run_indexes: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();

        for line in read_lines(&self.path)? {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let event: HistoryEvent = match serde_json::from_str(trimmed) {
                Ok(ev) => ev,
                Err(_) => continue, // skip broken lines
            };

            match event {
                HistoryEvent::Start {
                    run,
                    at,
                    command,
                    mode,
                    version,
                    total,
                } => {
                    let record = RunRecord {
                        run: run.clone(),
                        at,
                        command,
                        mode,
                        version,
                        total,
                        completed: false,
                        ended_at: None,
                        items: Vec::new(),
                    };
                    if let Some(&idx) = run_indexes.get(&run) {
                        runs[idx] = record;
                    } else {
                        run_indexes.insert(run, runs.len());
                        runs.push(record);
                    }
                }
                HistoryEvent::Item {
                    run,
                    path,
                    bytes,
                    outcome,
                    error,
                } => {
                    let item = RunItem {
                        path,
                        bytes,
                        outcome,
                        error,
                    };
                    if let Some(&idx) = run_indexes.get(&run) {
                        runs[idx].items.push(item);
                    } else {
                        // Fallback if Start was missing or corrupted
                        let idx = runs.len();
                        run_indexes.insert(run.clone(), idx);
                        runs.push(RunRecord {
                            run,
                            at: String::new(),
                            command: String::new(),
                            mode: String::new(),
                            version: String::new(),
                            total: None,
                            completed: false,
                            ended_at: None,
                            items: vec![item],
                        });
                    }
                }
                HistoryEvent::End { run, at } => {
                    if let Some(&idx) = run_indexes.get(&run) {
                        runs[idx].completed = true;
                        runs[idx].ended_at = Some(at);
                    }
                }
            }
        }

        Ok(runs)
    }

    /// Rewrites the file keeping only the last `max_runs` runs.
    pub fn prune(&self) -> io::Result<()> {
        if !self.path.exists() {
            return Ok(());
        }

        let _lock = lock_history_folder(&self.path)?;
        if !self.path.exists() {
            return Ok(());
        }

        let mut lines_with_run: Vec<(Option<String>, String)> = Vec::new();
        let mut run_order: VecDeque<String> = VecDeque::new();
        let mut seen_runs: HashSet<String> = HashSet::new();

        for line in read_lines(&self.path)? {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let run_id = run_of(trimmed);

            if let Some(ref id) = run_id
                && !seen_runs.contains(id)
            {
                seen_runs.insert(id.clone());
                run_order.push_back(id.clone());
            }

            lines_with_run.push((run_id, line));
        }

        if run_order.len() <= self.max_runs {
            return Ok(());
        }

        let excess = run_order.len() - self.max_runs;
        let mut keep_set: HashSet<String> = HashSet::new();
        for (i, id) in run_order.into_iter().enumerate() {
            if i >= excess {
                keep_set.insert(id);
            }
        }

        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        let temp_path = parent.join(format!(
            ".history.{}.{}.tmp",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));

        let mut temp_file = create_with_mode_0600(&temp_path)?;
        for (run_id, line) in lines_with_run {
            let keep = match run_id {
                Some(id) => keep_set.contains(&id),
                None => false,
            };
            if keep {
                writeln!(temp_file, "{line}")?;
            }
        }
        temp_file.flush()?;
        drop(temp_file);

        if let Err(e) = fs::rename(&temp_path, &self.path) {
            let _ = fs::remove_file(&temp_path);
            return Err(e);
        }
        Ok(())
    }
}

/// An active run session recording events.
pub struct HistorySession {
    log_path: PathBuf,
    run_id: String,
    disabled: bool,
    warned: AtomicBool,
    max_runs: usize,
}

impl HistorySession {
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    fn warn(&self, message: &str) {
        if !self.warned.swap(true, Ordering::Relaxed) {
            eprintln!("Warning: {message}");
        }
    }

    fn append_line(&self, event: &HistoryEvent) {
        if self.disabled {
            return;
        }

        let line = match serde_json::to_string(event) {
            Ok(l) => l,
            Err(e) => {
                self.warn(&format!("could not write to history log: {e}"));
                return;
            }
        };

        if let Err(e) = append_line_locked(&self.log_path, &line) {
            self.warn(&format!("could not write to history log: {e}"));
        }
    }

    /// Records an outcome for an item.
    pub fn record_item(&self, outcome: &ItemOutcome) {
        if self.disabled {
            return;
        }

        let event = HistoryEvent::Item {
            run: self.run_id.clone(),
            path: outcome.path.to_string_lossy().into_owned(),
            bytes: outcome.bytes,
            outcome: outcome.outcome,
            error: outcome.error.clone(),
        };

        self.append_line(&event);
    }

    /// Completes the run by writing the `end` event and pruning if needed.
    pub fn finish(self) {
        if self.disabled {
            return;
        }

        let at = format_rfc3339(SystemTime::now());
        let event = HistoryEvent::End {
            run: self.run_id.clone(),
            at,
        };
        self.append_line(&event);

        let log = HistoryLog::new(self.log_path.clone()).with_max_runs(self.max_runs);
        if let Err(e) = log.prune() {
            self.warn(&format!("could not write to history log: {e}"));
        }
    }
}

struct FolderLock(File);

impl FolderLock {
    fn acquire(folder: &Path) -> io::Result<Self> {
        let file = File::open(folder)?;
        let fd = file.as_raw_fd();
        let ret = unsafe { libc::flock(fd, libc::LOCK_EX) };
        if ret != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(file))
    }
}

impl Drop for FolderLock {
    fn drop(&mut self) {
        let fd = self.0.as_raw_fd();
        unsafe {
            libc::flock(fd, libc::LOCK_UN);
        }
    }
}

fn lock_history_folder(path: &Path) -> io::Result<FolderLock> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    FolderLock::acquire(parent)
}

fn append_line_locked(path: &Path, line: &str) -> io::Result<()> {
    let _lock = lock_history_folder(path)?;
    append_line(path, line)
}

fn append_line(path: &Path, line: &str) -> io::Result<()> {
    let mut file = open_for_append(path)?;
    writeln!(file, "{line}")?;
    file.flush()?;
    Ok(())
}

fn open_for_append(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    use std::os::unix::fs::OpenOptionsExt;
    let mut options = OpenOptions::new();
    options.create(true).append(true).mode(0o600);
    let file = options.open(path)?;

    if let Ok(meta) = file.metadata() {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o777 != 0o600 {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        }
    }
    Ok(file)
}

fn create_with_mode_0600(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true).mode(0o600);
    let file = options.open(path)?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::tempdir;

    #[test]
    fn scans_map_to_their_history_commands() {
        let commands = [
            ("docker", "docker"),
            ("brew-cleanup", "brew cleanup"),
            ("brew", "brew uninstall"),
            ("purge", "purge"),
            ("large", "large"),
            ("compress", "compress"),
            ("dev", "clean dev"),
            ("junk", "clean junk"),
            ("ai", "clean ai"),
            ("trash", "clean trash"),
            ("orphans", "clean orphans"),
            ("clean", "clean"),
            ("slim", "clean"),
            ("anything else", "clean"),
        ];
        for (scan_id, command) in commands {
            assert_eq!(command_for_scan(scan_id), command, "{scan_id}");
        }
    }

    #[test]
    fn run_writes_start_items_and_end() {
        let temp = tempdir().unwrap();
        let log_file = temp.path().join("history.jsonl");
        let log = HistoryLog::new(log_file.clone());

        let session = log.start_run("clean dev", "force", Some(2));
        session.record_item(&ItemOutcome {
            path: PathBuf::from("/Users/me/cache1"),
            bytes: 1024,
            outcome: OutcomeKind::Removed,
            error: None,
        });
        session.record_item(&ItemOutcome {
            path: PathBuf::from("/Users/me/log1"),
            bytes: 0,
            outcome: OutcomeKind::Failed,
            error: Some("Permission denied".to_string()),
        });
        session.finish();

        let runs = log.load_runs().unwrap();
        assert_eq!(runs.len(), 1);
        let run = &runs[0];
        assert_eq!(run.command, "clean dev");
        assert_eq!(run.mode, "force");
        assert!(run.completed);
        assert_eq!(run.items.len(), 2);
        assert_eq!(run.items[0].outcome, OutcomeKind::Removed);
        assert_eq!(run.items[1].outcome, OutcomeKind::Failed);
        assert_eq!(run.items[1].error.as_deref(), Some("Permission denied"));
    }

    #[test]
    fn run_with_no_end_shows_as_interrupted() {
        let temp = tempdir().unwrap();
        let log_file = temp.path().join("history.jsonl");
        let log = HistoryLog::new(log_file.clone());

        let session = log.start_run("purge ~/Projects", "interactive", Some(9));
        session.record_item(&ItemOutcome {
            path: PathBuf::from("/p1"),
            bytes: 100,
            outcome: OutcomeKind::Removed,
            error: None,
        });
        session.record_item(&ItemOutcome {
            path: PathBuf::from("/p2"),
            bytes: 200,
            outcome: OutcomeKind::Removed,
            error: None,
        });
        session.record_item(&ItemOutcome {
            path: PathBuf::from("/p3"),
            bytes: 300,
            outcome: OutcomeKind::Removed,
            error: None,
        });
        // Intentionally drop session without calling session.finish()
        drop(session);

        let runs = log.load_runs().unwrap();
        assert_eq!(runs.len(), 1);
        assert!(runs[0].is_interrupted());
        assert_eq!(runs[0].summary(), "interrupted after 3 of 9");
    }

    #[test]
    fn broken_line_is_skipped() {
        let temp = tempdir().unwrap();
        let log_file = temp.path().join("history.jsonl");

        let content = "\
{\"event\":\"start\",\"run\":\"run1\",\"at\":\"2026-11-01T10:00:00Z\",\"command\":\"clean dev\",\"mode\":\"force\",\"version\":\"0.6.0\"}
{BROKEN JSON NOT VALID}
{\"event\":\"item\",\"run\":\"run1\",\"path\":\"/path1\",\"bytes\":1024,\"outcome\":\"removed\"}
another corrupted line!
{\"event\":\"end\",\"run\":\"run1\",\"at\":\"2026-11-01T10:00:05Z\"}
";
        fs::write(&log_file, content).unwrap();

        let log = HistoryLog::new(log_file);
        let runs = log.load_runs().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].run, "run1");
        assert_eq!(runs[0].items.len(), 1);
        assert!(runs[0].completed);
    }

    #[test]
    fn file_has_mode_0600() {
        let temp = tempdir().unwrap();
        let log_file = temp.path().join("history.jsonl");
        let log = HistoryLog::new(log_file.clone());

        let session = log.start_run("clean dev", "force", None);
        session.finish();

        use std::os::unix::fs::MetadataExt;
        let meta = fs::metadata(&log_file).unwrap();
        assert_eq!(meta.mode() & 0o777, 0o600);
    }

    #[test]
    fn run_ids_are_unique_within_one_second() {
        let temp = tempdir().unwrap();
        let log = HistoryLog::new(temp.path().join("history.jsonl"));
        let time = UNIX_EPOCH + Duration::from_secs(1_800_000_000);

        let ids: HashSet<String> = (0..1001)
            .map(|_| {
                log.start_run_at(time, "cmd", "force", None)
                    .run_id()
                    .to_string()
            })
            .collect();

        assert_eq!(ids.len(), 1001);
        assert_eq!(log.load_runs().unwrap().len(), 1001);
    }

    /// `line(id)` for every id of the second of `time` except the one ending
    /// in 4242, and that free id.
    fn every_id_but_one(time: SystemTime, line: fn(&str) -> String) -> (String, String) {
        let prefix = &generate_run_id(time)[..16];
        let lines = (0..10_000)
            .filter(|n| *n != 4242)
            .map(|n| line(&format!("{prefix}{n:04}")))
            .collect();
        (lines, format!("{prefix}4242"))
    }

    fn start_event(id: &str) -> HistoryEvent {
        HistoryEvent::Start {
            run: id.to_string(),
            at: "2026-11-01T10:00:00Z".to_string(),
            command: "cmd".to_string(),
            mode: "force".to_string(),
            version: "0.6.0".to_string(),
            total: None,
        }
    }

    fn start_line(id: &str) -> String {
        format!("{}\n", serde_json::to_string(&start_event(id)).unwrap())
    }

    fn start_line_with_spaces(id: &str) -> String {
        format!(
            "{{\"event\": \"start\", \"run\": \"{id}\", \"at\": \"2026-11-01T10:00:00Z\", \"command\": \"cmd\", \"mode\": \"force\", \"version\": \"0.6.0\"}}\n"
        )
    }

    #[test]
    fn a_run_id_already_in_the_file_is_not_given_again() {
        let temp = tempdir().unwrap();
        let log = HistoryLog::new(temp.path().join("history.jsonl"));
        let time = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let (lines, free) = every_id_but_one(time, start_line);
        fs::write(&log.path, lines).unwrap();

        let session = log.start_run_at(time, "cmd", "force", None);

        assert_eq!(session.run_id(), free);
    }

    #[test]
    fn a_line_that_is_not_utf8_does_not_hide_later_run_ids() {
        let temp = tempdir().unwrap();
        let log = HistoryLog::new(temp.path().join("history.jsonl"));
        let time = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let (lines, free) = every_id_but_one(time, start_line);
        let mut content = b"\xff\xfe\n".to_vec();
        content.extend_from_slice(lines.as_bytes());
        fs::write(&log.path, content).unwrap();

        let session = log.start_run_at(time, "cmd", "force", None);

        assert_eq!(session.run_id(), free);
    }

    #[test]
    fn a_run_id_written_with_spaces_is_not_given_again() {
        let temp = tempdir().unwrap();
        let log = HistoryLog::new(temp.path().join("history.jsonl"));
        let time = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let (lines, free) = every_id_but_one(time, start_line_with_spaces);
        fs::write(&log.path, lines).unwrap();

        let session = log.start_run_at(time, "cmd", "force", None);

        assert_eq!(session.run_id(), free);
    }

    #[test]
    fn oldest_runs_are_pruned_when_limit_passed() {
        let temp = tempdir().unwrap();
        let log_file = temp.path().join("history.jsonl");
        let log = HistoryLog::new(log_file.clone()).with_max_runs(2);

        // Run 1
        let s1 = log.start_run("cmd1", "force", None);
        let id1 = s1.run_id().to_string();
        s1.finish();

        // Run 2
        let s2 = log.start_run("cmd2", "force", None);
        let id2 = s2.run_id().to_string();
        s2.finish();

        let runs = log.load_runs().unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].run, id1);
        assert_eq!(runs[1].run, id2);

        // Run 3 exceeds limit of 2
        let s3 = log.start_run("cmd3", "force", None);
        let id3 = s3.run_id().to_string();
        s3.finish();

        let runs = log.load_runs().unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].run, id2);
        assert_eq!(runs[1].run, id3);

        use std::os::unix::fs::MetadataExt;
        let meta = fs::metadata(&log_file).unwrap();
        assert_eq!(meta.mode() & 0o777, 0o600);
    }

    #[test]
    fn summary_formatting_matches_spec() {
        let r1 = RunRecord {
            run: "1".into(),
            at: "2026-11-01T13:40:00Z".into(),
            command: "clean dev".into(),
            mode: "force".into(),
            version: "0.6.0".into(),
            total: Some(12),
            completed: true,
            ended_at: Some("2026-11-01T13:40:03Z".into()),
            items: (0..12)
                .map(|i| RunItem {
                    path: format!("/path/{i}"),
                    bytes: if i == 0 { 8_100_000_000 } else { 0 },
                    outcome: OutcomeKind::Removed,
                    error: None,
                })
                .collect(),
        };
        assert_eq!(r1.summary(), "removed 12, 8.1 GB");

        let r2 = RunRecord {
            run: "2".into(),
            at: "2026-10-28T09:12:00Z".into(),
            command: "uninstall Foo".into(),
            mode: "interactive".into(),
            version: "0.6.0".into(),
            total: Some(15),
            completed: true,
            ended_at: Some("2026-10-28T09:12:10Z".into()),
            items: {
                let mut v: Vec<_> = (0..14)
                    .map(|i| RunItem {
                        path: format!("/path/{i}"),
                        bytes: if i == 0 { 2_300_000_000 } else { 0 },
                        outcome: OutcomeKind::Trashed,
                        error: None,
                    })
                    .collect();
                v.push(RunItem {
                    path: "/failed/item".into(),
                    bytes: 0,
                    outcome: OutcomeKind::Failed,
                    error: Some("Operation not permitted".into()),
                });
                v
            },
        };
        assert_eq!(r2.summary(), "trash 14, 2.3 GB, 1 failed");
    }

    #[test]
    fn format_line_spacing_matches_spec() {
        let r1 = RunRecord {
            run: "1".into(),
            at: "2026-11-01T13:40:00Z".into(),
            command: "clean dev".into(),
            mode: "force".into(),
            version: "0.6.0".into(),
            total: Some(12),
            completed: true,
            ended_at: Some("2026-11-01T13:40:03Z".into()),
            items: (0..12)
                .map(|i| RunItem {
                    path: format!("/path/{i}"),
                    bytes: if i == 0 { 8_100_000_000 } else { 0 },
                    outcome: OutcomeKind::Removed,
                    error: None,
                })
                .collect(),
        };
        let line = r1.format_line();
        // Index of mode "force" should be 35
        assert_eq!(line.find("force"), Some(35));
        // Index of summary "removed" should be 48
        assert_eq!(line.find("removed 12, 8.1 GB"), Some(48));

        let r2 = RunRecord {
            run: "2".into(),
            at: "2026-10-28T09:12:00Z".into(),
            command: "uninstall Foo".into(),
            mode: "interactive".into(),
            version: "0.6.0".into(),
            total: Some(15),
            completed: true,
            ended_at: Some("2026-10-28T09:12:10Z".into()),
            items: {
                let mut v: Vec<_> = (0..14)
                    .map(|i| RunItem {
                        path: format!("/path/{i}"),
                        bytes: if i == 0 { 2_300_000_000 } else { 0 },
                        outcome: OutcomeKind::Trashed,
                        error: None,
                    })
                    .collect();
                v.push(RunItem {
                    path: "/failed/item".into(),
                    bytes: 0,
                    outcome: OutcomeKind::Failed,
                    error: Some("error".into()),
                });
                v
            },
        };
        let line2 = r2.format_line();
        assert_eq!(line2.find("interactive"), Some(35));
        assert_eq!(line2.find("trash 14, 2.3 GB, 1 failed"), Some(48));

        let r3 = RunRecord {
            run: "3".into(),
            at: "2026-10-27T22:03:00Z".into(),
            command: "purge ~/Projects".into(),
            mode: "interactive".into(),
            version: "0.6.0".into(),
            total: Some(9),
            completed: false,
            ended_at: None,
            items: (0..3)
                .map(|i| RunItem {
                    path: format!("/p/{i}"),
                    bytes: 10,
                    outcome: OutcomeKind::Removed,
                    error: None,
                })
                .collect(),
        };
        let line3 = r3.format_line();
        assert_eq!(line3.find("interactive"), Some(35));
        assert_eq!(line3.find("interrupted after 3 of 9"), Some(48));
    }

    #[test]
    fn a_run_still_open_keeps_its_lines_when_another_run_prunes_the_file() {
        let temp = tempdir().unwrap();
        let log_file = temp.path().join("history.jsonl");
        let log = HistoryLog::new(log_file.clone()).with_max_runs(3);

        // Write one finished old run
        let old = log.start_run("clean old", "force", None);
        old.finish();

        // Start run A and keep it open
        let session_a = log.start_run("clean A", "force", None);
        let id_a = session_a.run_id().to_string();

        // Start and finish runs B and C (3rd finish prunes: 4 runs > 3)
        let session_b = log.start_run("clean B", "force", None);
        session_b.finish();
        let session_c = log.start_run("clean C", "force", None);
        session_c.finish();

        // Then A records item and finishes
        session_a.record_item(&ItemOutcome::changed(PathBuf::from("/a"), 100));
        session_a.finish();

        let runs = log.load_runs().unwrap();
        let run_a = runs
            .iter()
            .find(|r| r.run == id_a)
            .expect("run A must be found");
        assert_eq!(run_a.items.len(), 1);
        assert!(run_a.completed);
    }

    /// Runs `call` on a thread so a read that never ends fails the test instead of hanging it.
    fn within_timeout<T: Send + 'static>(call: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(call());
        });
        rx.recv_timeout(Duration::from_secs(5))
            .expect("the call did not return in 5 seconds")
    }

    #[test]
    fn load_runs_of_a_folder_returns_an_error() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("history.jsonl");
        fs::create_dir(&path).unwrap();
        let log = HistoryLog::new(path);

        let result = within_timeout(move || log.load_runs());

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::IsADirectory);
    }

    #[test]
    fn prune_of_a_folder_returns_an_error_and_releases_the_lock() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("history.jsonl");
        fs::create_dir(&path).unwrap();

        let result = within_timeout({
            let log = HistoryLog::new(path.clone());
            move || log.prune()
        });
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::IsADirectory);
        assert!(path.is_dir());

        fs::remove_dir(&path).unwrap();
        let log = HistoryLog::new(path.clone());
        let session = within_timeout(move || log.start_run("clean junk", "force", None));
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains(session.run_id())
        );
    }

    fn two_runs_around_a_line_that_is_not_utf8() -> Vec<u8> {
        let mut content = start_line("run1").into_bytes();
        content.extend_from_slice(b"\xff\xfe\n");
        content.extend_from_slice(start_line("run2").as_bytes());
        content
    }

    #[test]
    fn a_line_that_is_not_utf8_is_skipped_and_later_runs_load() {
        let temp = tempdir().unwrap();
        let log = HistoryLog::new(temp.path().join("history.jsonl"));
        fs::write(&log.path, two_runs_around_a_line_that_is_not_utf8()).unwrap();

        let runs = within_timeout(move || log.load_runs()).unwrap();

        let ids: Vec<&str> = runs.iter().map(|r| r.run.as_str()).collect();
        assert_eq!(ids, ["run1", "run2"]);
    }

    #[test]
    fn a_line_that_is_not_utf8_is_skipped_and_later_runs_survive_a_prune() {
        let temp = tempdir().unwrap();
        let log = HistoryLog::new(temp.path().join("history.jsonl")).with_max_runs(1);
        fs::write(&log.path, two_runs_around_a_line_that_is_not_utf8()).unwrap();

        let log = within_timeout(move || {
            log.prune().unwrap();
            log
        });

        let runs = log.load_runs().unwrap();
        let ids: Vec<&str> = runs.iter().map(|r| r.run.as_str()).collect();
        assert_eq!(ids, ["run2"]);
    }

    #[test]
    fn appends_wait_for_the_lock() {
        use std::os::unix::io::AsRawFd;
        use std::sync::mpsc;

        let temp = tempdir().unwrap();
        let log_file = temp.path().join("history.jsonl");
        let log = HistoryLog::new(log_file.clone());
        let session = log.start_run("clean junk", "force", None);

        // Take libc::flock(LOCK_EX) on the temp folder in the test
        let folder = File::open(temp.path()).unwrap();
        let fd = folder.as_raw_fd();
        let ret = unsafe { libc::flock(fd, libc::LOCK_EX) };
        assert_eq!(ret, 0);

        let (tx, rx) = mpsc::channel();
        let outcome = ItemOutcome::changed(PathBuf::from("/test"), 50);

        std::thread::spawn(move || {
            session.record_item(&outcome);
            let _ = tx.send(());
        });

        std::thread::sleep(Duration::from_millis(200));
        assert!(rx.try_recv().is_err());

        // Release the lock
        unsafe {
            libc::flock(fd, libc::LOCK_UN);
        }

        assert!(rx.recv_timeout(Duration::from_secs(5)).is_ok());

        let runs = log.load_runs().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].items.len(), 1);
        assert_eq!(runs[0].items[0].path, "/test");
    }
}

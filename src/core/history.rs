//! History log of removals and changes.
//!
//! Records cleanup and uninstall events into `~/.config/taert/history.jsonl`
//! in append-only JSONL format with permissions `0600`.

use std::cell::RefCell;
use std::collections::{HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::bytes::format_bytes;
use super::date_utils::{format_rfc3339, format_system_time, parse_rfc3339};
use super::safety::Safety;

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

    pub fn default_log() -> Self {
        Self::new(Self::default_path())
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
        let disabled = is_history_disabled();
        let run_id = generate_run_id(SystemTime::now());
        let session = HistorySession {
            log_path: self.path.clone(),
            run_id: run_id.clone(),
            disabled,
            warned: RefCell::new(false),
            file: RefCell::new(None),
            max_runs: self.max_runs,
        };

        if disabled {
            return session;
        }

        let at = format_rfc3339(SystemTime::now());
        let event = HistoryEvent::Start {
            run: run_id,
            at,
            command: command.to_string(),
            mode: mode.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            total,
        };

        match open_for_append(&self.path) {
            Ok(mut file) => {
                if let Ok(line) = serde_json::to_string(&event) {
                    let _ = writeln!(file, "{line}");
                    let _ = file.flush();
                }
                *session.file.borrow_mut() = Some(file);
            }
            Err(e) => {
                session.warn(&format!("could not write to history log: {e}"));
            }
        }

        session
    }

    /// Reads all runs from the file in chronological order.
    /// Broken or corrupted lines are skipped and never cause failure.
    pub fn load_runs(&self) -> io::Result<Vec<RunRecord>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }

        let file = File::open(&self.path)?;
        let reader = BufReader::new(file);

        let mut runs: Vec<RunRecord> = Vec::new();
        let mut run_indexes: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();

        for line_res in reader.lines() {
            let line = match line_res {
                Ok(l) => l,
                Err(_) => continue,
            };
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

        let file = File::open(&self.path)?;
        let reader = BufReader::new(file);

        let mut lines_with_run: Vec<(Option<String>, String)> = Vec::new();
        let mut run_order: VecDeque<String> = VecDeque::new();
        let mut seen_runs: HashSet<String> = HashSet::new();

        for line_res in reader.lines() {
            let line = match line_res {
                Ok(l) => l,
                Err(_) => continue,
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let run_id = serde_json::from_str::<serde_json::Value>(trimmed)
                .ok()
                .and_then(|val| val.get("run").and_then(|r| r.as_str()).map(String::from));

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
    warned: RefCell<bool>,
    file: RefCell<Option<File>>,
    max_runs: usize,
}

impl HistorySession {
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    fn warn(&self, message: &str) {
        if !*self.warned.borrow() {
            *self.warned.borrow_mut() = true;
            eprintln!("Warning: {message}");
        }
    }

    /// Records an outcome for an item.
    pub fn record_item(&self, outcome: &ItemOutcome) {
        if self.disabled {
            return;
        }

        let mut file_guard = self.file.borrow_mut();
        let Some(file) = file_guard.as_mut() else {
            return;
        };

        let event = HistoryEvent::Item {
            run: self.run_id.clone(),
            path: outcome.path.to_string_lossy().into_owned(),
            bytes: outcome.bytes,
            outcome: outcome.outcome,
            error: outcome.error.clone(),
        };

        if let Ok(line) = serde_json::to_string(&event)
            && let Err(e) = writeln!(file, "{line}").and_then(|_| file.flush())
        {
            self.warn(&format!("could not write to history log: {e}"));
        }
    }

    /// Completes the run by writing the `end` event and pruning if needed.
    pub fn finish(self) {
        if self.disabled {
            return;
        }

        let file_opt = self.file.borrow_mut().take();
        if let Some(mut file) = file_opt {
            let at = format_rfc3339(SystemTime::now());
            let event = HistoryEvent::End {
                run: self.run_id.clone(),
                at,
            };

            if let Ok(line) = serde_json::to_string(&event) {
                let _ = writeln!(file, "{line}");
                let _ = file.flush();
            }
            drop(file);
        }

        let log = HistoryLog::new(self.log_path).with_max_runs(self.max_runs);
        let _ = log.prune();
    }
}

fn open_for_append(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    #[cfg(unix)]
    {
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

    #[cfg(not(unix))]
    {
        OpenOptions::new().create(true).append(true).open(path)
    }
}

fn create_with_mode_0600(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = OpenOptions::new();
        options.create(true).write(true).truncate(true).mode(0o600);
        let file = options.open(path)?;
        Ok(file)
    }

    #[cfg(not(unix))]
    {
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

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

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let meta = fs::metadata(&log_file).unwrap();
            assert_eq!(meta.mode() & 0o777, 0o600);
        }
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

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let meta = fs::metadata(&log_file).unwrap();
            assert_eq!(meta.mode() & 0o777, 0o600);
        }
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
}

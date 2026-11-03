//! Background cleanup scheduling via macOS launchd.
//!
//! Configures and manages `~/Library/LaunchAgents/io.github.ingvarch.taert.schedule.plist`
//! to run unattended cache cleanup on a recurring schedule.

use std::fs::{self, File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use super::bytes::format_bytes;
use super::catalog::Catalog;
use super::cleaner::{Cleaner, RemovalMode};
use super::date_utils::{format_rfc3339, format_system_time, parse_rfc3339};
use super::history::{HistoryLog, ItemOutcome, OutcomeKind, RunRecord};
use super::host::Host;
use super::layout::Layout;
use super::scanner::{CategoryScanner, ScanContext, ScanItem};
use super::stores::{Config, IgnoreStore, ScheduleConfig, ScheduleSkip};

pub const SCHEDULE_LABEL: &str = "io.github.ingvarch.taert.schedule";
pub const SCHEDULE_PLIST_NAME: &str = "io.github.ingvarch.taert.schedule.plist";
pub const VALID_CATEGORIES: &[&str] = &["dev", "junk", "ai", "trash"];
const LAUNCHCTL_TIMEOUT: Duration = Duration::from_secs(5);

/// Recurrence frequency for background cleanup.
#[derive(Debug, Copy, Clone, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScheduleFrequency {
    Day,
    Week,
    Month,
}

impl ScheduleFrequency {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
        }
    }
}

impl std::fmt::Display for ScheduleFrequency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SchedulePlist {
    label: String,
    #[serde(rename = "LowPriorityIO")]
    low_priority_io: bool,
    nice: i32,
    process_type: String,
    program_arguments: Vec<String>,
    start_calendar_interval: CalendarInterval,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct CalendarInterval {
    #[serde(skip_serializing_if = "Option::is_none")]
    day: Option<u32>,
    hour: u32,
    minute: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    weekday: Option<u32>,
}

impl CalendarInterval {
    fn for_frequency(freq: ScheduleFrequency) -> Self {
        match freq {
            ScheduleFrequency::Day => Self {
                day: None,
                hour: 9,
                minute: 0,
                weekday: None,
            },
            ScheduleFrequency::Week => Self {
                day: None,
                hour: 9,
                minute: 0,
                weekday: Some(1), // Monday
            },
            ScheduleFrequency::Month => Self {
                day: Some(1), // 1st of month
                hour: 9,
                minute: 0,
                weekday: None,
            },
        }
    }
}

/// Generates launchd plist XML content for the schedule job.
pub fn generate_plist(binary: &Path, freq: ScheduleFrequency) -> Result<String, String> {
    let plist = SchedulePlist {
        label: SCHEDULE_LABEL.to_string(),
        low_priority_io: true,
        nice: 10,
        process_type: "Background".to_string(),
        program_arguments: vec![
            binary.to_string_lossy().into_owned(),
            "schedule".to_string(),
            "run".to_string(),
        ],
        start_calendar_interval: CalendarInterval::for_frequency(freq),
    };

    let mut buf = Vec::new();
    plist::to_writer_xml(&mut buf, &plist).map_err(|e| e.to_string())?;
    String::from_utf8(buf).map_err(|e| e.to_string())
}

/// Returns the stable link in Homebrew's bin folder if `exe` is in Caskroom,
/// otherwise leaves the path unchanged.
pub fn resolve_binary_path(exe: &Path) -> PathBuf {
    let parts: Vec<&str> = exe.iter().filter_map(|p| p.to_str()).collect();
    if let Some(pos) = parts.iter().position(|&p| p == "Caskroom") {
        let prefix: PathBuf = parts[..pos].iter().collect();
        let bin_name = exe.file_name().unwrap_or_default();
        return prefix.join("bin").join(bin_name);
    }
    exe.to_path_buf()
}

/// Path to the LaunchAgent plist file for this user.
pub fn agent_plist_path(layout: &Layout) -> PathBuf {
    layout.user(&format!("Library/LaunchAgents/{SCHEDULE_PLIST_NAME}"))
}

/// Path to the schedule lock file preventing overlapping runs.
pub fn schedule_lock_path(layout: &Layout) -> PathBuf {
    layout.user(".config/taert/schedule.lock")
}

/// Checks whether the launchd agent is currently loaded via `launchctl print gui/<uid>/<label>`.
pub fn is_agent_loaded(host: &dyn Host, uid: u32) -> bool {
    let output = host.run(
        "/bin/launchctl",
        &["print", &format!("gui/{uid}/{SCHEDULE_LABEL}")],
        LAUNCHCTL_TIMEOUT,
    );
    output.ok
}

/// Trims and lower-cases names, drops empty ones and repeats (first place wins).
pub fn normalize_categories(categories: &[String]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for cat in categories {
        let name = cat.trim().to_lowercase();
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// Validates categories specified for scheduled cleanup.
pub fn validate_categories(categories: &[String]) -> Result<(), String> {
    if categories.is_empty() {
        return Err("At least one category must be specified".to_string());
    }
    for cat in categories {
        let normalized = cat.trim().to_lowercase();
        if normalized == "purge"
            || normalized == "docker"
            || normalized == "uninstall"
            || normalized == "clean orphans"
            || normalized == "orphans"
        {
            return Err(format!(
                "Category '{cat}' cannot be scheduled; its results require interactive review"
            ));
        }
        if !VALID_CATEGORIES.contains(&normalized.as_str()) {
            return Err(format!(
                "Invalid category '{cat}'. Allowed categories: {}",
                VALID_CATEGORIES.join(", ")
            ));
        }
    }
    Ok(())
}

/// Returns the descriptive title for a category.
pub fn category_title(id: &str) -> &'static str {
    match id {
        "dev" => "Developer Caches",
        "junk" => "System Junk",
        "ai" => "AI Tool Caches",
        "trash" => "Trash",
        _ => "Caches",
    }
}

/// File lock wrapper using `flock` to guarantee non-overlapping runs.
pub struct ScheduleLock {
    file: File,
}

#[derive(Debug)]
pub enum LockError {
    Held,
    Failed(String),
}

impl ScheduleLock {
    pub fn try_acquire(path: &Path) -> Result<Self, LockError> {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|e| LockError::Failed(format!("Failed to open lock file: {e}")))?;

        let fd = file.as_raw_fd();
        let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            return Err(if err.kind() == std::io::ErrorKind::WouldBlock {
                LockError::Held
            } else {
                LockError::Failed(format!("Failed to lock {}: {err}", path.display()))
            });
        }

        Ok(Self { file })
    }
}

impl Drop for ScheduleLock {
    fn drop(&mut self) {
        let fd = self.file.as_raw_fd();
        unsafe {
            libc::flock(fd, libc::LOCK_UN);
        }
    }
}

/// Enables or updates the scheduled cleanup job.
pub fn set_schedule(
    host: &dyn Host,
    layout: &Layout,
    config: &mut Config,
    every: ScheduleFrequency,
    categories: Vec<String>,
    min_reclaim: u64,
    current_exe: Option<&Path>,
) -> Result<(), String> {
    validate_categories(&categories)?;
    let categories = normalize_categories(&categories);

    let plist_path = agent_plist_path(layout);
    let uid = host.uid();
    // An existing plist that cannot be read could not be put back after a failed load.
    let old_plist = if plist_path.exists() {
        Some(fs::read(&plist_path).map_err(|e| format!("Failed to read LaunchAgent plist: {e}"))?)
    } else {
        None
    };

    // If agent already exists on disk, bootout first
    if old_plist.is_some() {
        let _ = host.run(
            "/bin/launchctl",
            &["bootout", &format!("gui/{uid}/{SCHEDULE_LABEL}")],
            LAUNCHCTL_TIMEOUT,
        );
    }

    // Resolve binary path
    let default_exe =
        std::env::current_exe().unwrap_or_else(|_| PathBuf::from("/opt/homebrew/bin/taert"));
    let exe = current_exe.unwrap_or(&default_exe);
    let bin_path = resolve_binary_path(exe);

    // Generate and write plist
    let xml = generate_plist(&bin_path, every)?;
    if let Some(parent) = plist_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create LaunchAgents folder: {e}"))?;
    }
    fs::write(&plist_path, xml).map_err(|e| format!("Failed to write LaunchAgent plist: {e}"))?;

    // Bootstrap agent in launchd
    let boot_out = host.run(
        "/bin/launchctl",
        &[
            "bootstrap",
            &format!("gui/{uid}"),
            &plist_path.to_string_lossy(),
        ],
        LAUNCHCTL_TIMEOUT,
    );
    if !boot_out.ok {
        match &old_plist {
            Some(bytes) => {
                let _ = fs::write(&plist_path, bytes);
            }
            None => {
                let _ = fs::remove_file(&plist_path);
            }
        }
        let detail = boot_out.stderr.trim();
        return Err(if detail.is_empty() {
            "launchctl bootstrap failed".to_string()
        } else {
            format!("launchctl bootstrap failed: {detail}")
        });
    }

    // Update config
    config.schedule = Some(ScheduleConfig {
        every,
        categories,
        min_reclaim,
        last_skip: None,
    });
    let _ = config.save_to(&layout.config_path());

    Ok(())
}

/// Disables and removes the scheduled cleanup job.
pub fn remove_schedule(
    host: &dyn Host,
    layout: &Layout,
    config: &mut Config,
) -> Result<(), String> {
    let plist_path = agent_plist_path(layout);
    let uid = host.uid();

    // 1. Unload agent with bootout
    let _ = host.run(
        "/bin/launchctl",
        &["bootout", &format!("gui/{uid}/{SCHEDULE_LABEL}")],
        LAUNCHCTL_TIMEOUT,
    );

    // 2. Remove plist file
    if plist_path.exists() {
        fs::remove_file(&plist_path)
            .map_err(|e| format!("Failed to remove LaunchAgent plist: {e}"))?;
    }

    // 3. Update config
    config.schedule = None;
    let _ = config.save_to(&layout.config_path());

    Ok(())
}

fn format_reclaim_threshold(bytes: u64) -> String {
    let s = format_bytes(bytes);
    s.replace(".0 ", " ")
}

fn is_permission_error(error: Option<&str>) -> bool {
    let Some(err) = error else { return false };
    err.contains("Operation not permitted")
        || err.contains("Permission denied")
        || err.contains("EPERM")
        || err.contains("os error 1")
}

/// Formats the output for `taert schedule show`.
pub fn show_schedule(
    host: &dyn Host,
    layout: &Layout,
    config: &Config,
    history: &HistoryLog,
) -> String {
    let mut out = String::new();

    let plist_path = agent_plist_path(layout);
    let uid = host.uid();

    // 1. Current settings
    let Some(sched) = &config.schedule else {
        if !plist_path.exists() {
            return "Scheduled cleanup is off.\n".to_string();
        }
        out.push_str("Scheduled cleanup is off (config disabled, but LaunchAgent plist exists).\n");
        return out;
    };

    let categories_str = sched.categories.join(", ");
    let threshold_str = format_reclaim_threshold(sched.min_reclaim);
    out.push_str(&format!(
        "Every {}: {}, when at least {} can be freed.\n",
        sched.every, categories_str, threshold_str
    ));

    // 2. Last run details from history
    let scheduled_runs: Vec<RunRecord> = history
        .load_runs()
        .unwrap_or_default()
        .into_iter()
        .rev()
        .filter(|r| r.mode == "schedule")
        .collect();

    let last_run = scheduled_runs.first();
    let newer_skip = sched.last_skip.as_ref().filter(|skip| {
        let Some(run) = last_run else { return true };
        match (parse_rfc3339(&run.at), parse_rfc3339(&skip.at)) {
            (Some(run_at), Some(skip_at)) => skip_at > run_at,
            (None, Some(_)) => true,
            _ => false,
        }
    });
    if let Some(skip) = newer_skip {
        let date_str = if let Some(t) = parse_rfc3339(&skip.at) {
            format_system_time(t)
        } else {
            skip.at.clone()
        };
        out.push_str(&format!(
            "Last run: skipped on {} ({} found, below {} threshold).\n",
            date_str,
            format_bytes(skip.bytes),
            threshold_str
        ));
    } else if let Some(run) = last_run {
        let date_str = if let Some(t) = parse_rfc3339(&run.at) {
            format_system_time(t)
        } else {
            run.at.clone()
        };
        let freed_bytes: u64 = run
            .items
            .iter()
            .filter(|i| i.outcome == OutcomeKind::Removed || i.outcome == OutcomeKind::Trashed)
            .map(|i| i.bytes)
            .sum();
        out.push_str(&format!(
            "Last run: {}, freed {}.\n",
            date_str,
            format_bytes(freed_bytes)
        ));
    } else {
        out.push_str("Last run: never.\n");
    }

    // 3. Inspect plist binary path
    let plist_exists = plist_path.exists();
    let plist_binary = if plist_exists {
        plist::Value::from_file(&plist_path)
            .ok()
            .and_then(|val| val.into_dictionary())
            .and_then(|dict| {
                dict.get("ProgramArguments")
                    .and_then(plist::Value::as_array)
                    .and_then(|args| args.first().and_then(plist::Value::as_string))
                    .map(PathBuf::from)
            })
    } else {
        None
    };

    if !plist_exists {
        out.push_str(&format!(
            "\nWarning: The launch agent file is missing. A Homebrew upgrade can remove it. Load it again with:\n  taert schedule set --every {} --categories {} --min-reclaim {}\n",
            sched.every,
            sched.categories.join(","),
            sched.min_reclaim
        ));
    } else {
        if let Some(ref bin) = plist_binary
            && !bin.exists()
        {
            out.push_str(&format!(
                "\nWarning: Binary '{}' configured in LaunchAgent does not exist.\n",
                bin.display()
            ));
        }

        // 4. Check if agent is loaded
        if !is_agent_loaded(host, uid) {
            out.push_str(
                "\nWarning: Launch agent is not loaded in launchd. It may have been disabled in System Settings > General > Login Items & Extensions.\n"
            );
        }
    }

    // 5. Check if Full Disk Access is needed
    let fda_failed = last_run.is_some_and(|r| {
        r.items
            .iter()
            .any(|i| i.outcome == OutcomeKind::Failed && is_permission_error(i.error.as_deref()))
    });

    if fda_failed {
        let bin_display = plist_binary
            .as_deref()
            .map(|p| p.to_string_lossy())
            .unwrap_or_else(|| "/opt/homebrew/bin/taert".into());
        out.push_str(&format!(
            "\nScheduled cleanup failed to remove some files due to missing permissions.\nTo grant taert Full Disk Access:\n  1. Open System Settings > Privacy & Security > Full Disk Access\n  2. Add and enable: {}\n",
            bin_display
        ));
    }

    out
}

/// Unattended background cleanup execution (`taert schedule run`).
/// Prints nothing to stdout/stderr and never opens the TUI.
pub fn run_scheduled(
    host: &dyn Host,
    layout: &Layout,
    config: &mut Config,
    lock_path: Option<&Path>,
) -> Result<(), String> {
    let Some(sched) = &config.schedule else {
        return Ok(());
    };

    // 1. Acquire non-overlapping lock
    let default_lock = schedule_lock_path(layout);
    let lock_target = lock_path.unwrap_or(&default_lock);
    let _lock = match ScheduleLock::try_acquire(lock_target) {
        Ok(l) => l,
        Err(LockError::Held) => return Ok(()),
        Err(LockError::Failed(message)) => return Err(message),
    };

    // 2. Scan configured categories
    let ignore = IgnoreStore::load_from(&layout.ignore_path()).unwrap_or_default();
    let context = ScanContext { layout, host };
    let mut all_items: Vec<ScanItem> = Vec::new();

    for cat in &normalize_categories(&sched.categories) {
        let scan = CategoryScanner::scan_with(
            &context,
            cat,
            category_title(cat),
            Catalog::targets_in(layout, cat),
            &ignore,
            None,
            None,
        );
        for item in scan.selected_items() {
            all_items.push(item.clone());
        }
    }

    let total_selected_bytes: u64 = all_items.iter().map(|i| i.size_bytes).sum();

    // 3. Stop if selected total is below min_reclaim, and record the skip
    if total_selected_bytes < sched.min_reclaim {
        let now = format_rfc3339(SystemTime::now());
        if let Some(s) = config.schedule.as_mut() {
            s.last_skip = Some(ScheduleSkip {
                at: now,
                bytes: total_selected_bytes,
            });
        }
        let _ = config.save_to(&layout.config_path());
        return Ok(());
    }

    // 4. Remove selected rows only, through Cleaner
    let history = HistoryLog::new(layout.history_path());

    let session = history.start_run("schedule run", "schedule", Some(all_items.len()));
    let cleaner = Cleaner::new(host, RemovalMode::Permanent);

    let _report = cleaner.remove_items_with_callbacks(
        &all_items,
        &ignore,
        None::<fn(crate::core::cleaner::RemovalProgress)>,
        Some(|outcome: &ItemOutcome| session.record_item(outcome)),
    );

    session.finish();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::host::fake::FakeHost;
    use tempfile::tempdir;

    #[test]
    fn caskroom_path_maps_to_the_stable_link_and_other_paths_stay_as_they_are() {
        let silicon = Path::new("/opt/homebrew/Caskroom/taert/0.5.0/taert");
        assert_eq!(
            resolve_binary_path(silicon),
            PathBuf::from("/opt/homebrew/bin/taert")
        );

        let intel = Path::new("/usr/local/Caskroom/taert/1.0.0/taert");
        assert_eq!(
            resolve_binary_path(intel),
            PathBuf::from("/usr/local/bin/taert")
        );

        let stable = Path::new("/usr/local/bin/taert");
        assert_eq!(
            resolve_binary_path(stable),
            PathBuf::from("/usr/local/bin/taert")
        );

        let cargo = Path::new("/Users/igor/.cargo/bin/taert");
        assert_eq!(
            resolve_binary_path(cargo),
            PathBuf::from("/Users/igor/.cargo/bin/taert")
        );
    }

    #[test]
    fn the_plist_matches_the_stored_example() {
        let bin = Path::new("/opt/homebrew/bin/taert");
        let examples = [
            (
                ScheduleFrequency::Day,
                include_str!("../../tests/fixtures/schedule_day.plist"),
            ),
            (
                ScheduleFrequency::Week,
                include_str!("../../tests/fixtures/schedule_week.plist"),
            ),
            (
                ScheduleFrequency::Month,
                include_str!("../../tests/fixtures/schedule_month.plist"),
            ),
        ];
        for (freq, example) in examples {
            assert_eq!(generate_plist(bin, freq).unwrap(), example, "{freq}");
        }
    }

    #[test]
    fn uninstall_and_orphans_cannot_be_scheduled() {
        for name in ["uninstall", "orphans", "clean orphans", "Orphans"] {
            let err = validate_categories(&[name.to_string()]).unwrap_err();
            assert!(err.contains("cannot be scheduled"), "{name}: {err}");
        }
    }

    #[test]
    fn purge_and_docker_are_refused_as_categories() {
        assert!(validate_categories(&["purge".to_string()]).is_err());
        assert!(validate_categories(&["docker".to_string()]).is_err());
        assert!(validate_categories(&["dev".to_string(), "purge".to_string()]).is_err());
        assert!(validate_categories(&["dev".to_string(), "junk".to_string()]).is_ok());
    }

    #[test]
    fn set_runs_bootout_before_bootstrap_when_an_agent_exists() {
        let temp = tempdir().unwrap();
        let layout = Layout {
            home: temp.path().to_path_buf(),
            root: temp.path().to_path_buf(),
            user_cache: None,
        };
        let host = FakeHost::default();
        host.reply("/bin/launchctl", "");
        let mut config = Config::default();

        // Create existing plist file
        let plist_file = agent_plist_path(&layout);
        fs::create_dir_all(plist_file.parent().unwrap()).unwrap();
        fs::write(&plist_file, b"old").unwrap();

        let bin = temp.path().join("taert");
        fs::write(&bin, b"bin").unwrap();

        set_schedule(
            &host,
            &layout,
            &mut config,
            ScheduleFrequency::Week,
            vec!["dev".to_string()],
            1_000_000_000,
            Some(&bin),
        )
        .unwrap();

        assert!(host.ran("/bin/launchctl bootout"));
        assert!(host.ran("/bin/launchctl bootstrap"));
        let bootout_pos = host.position("/bin/launchctl bootout").unwrap();
        let bootstrap_pos = host.position("/bin/launchctl bootstrap").unwrap();
        assert!(bootout_pos < bootstrap_pos);
    }

    #[test]
    fn schedule_uses_the_uid_of_the_host() {
        let temp = tempdir().unwrap();
        let layout = Layout {
            home: temp.path().to_path_buf(),
            root: temp.path().to_path_buf(),
            user_cache: None,
        };
        let host = FakeHost::default();
        host.reply("/bin/launchctl", "");
        host.set_uid(777);
        let bin = temp.path().join("taert");
        fs::write(&bin, b"bin").unwrap();
        let mut config = Config::default();

        set_schedule(
            &host,
            &layout,
            &mut config,
            ScheduleFrequency::Week,
            vec!["dev".to_string()],
            1_000_000,
            Some(&bin),
        )
        .unwrap();

        let calls = host.calls.lock().unwrap();
        let gui_calls: Vec<&String> = calls.iter().filter(|c| c.contains("gui/")).collect();
        assert!(!gui_calls.is_empty(), "No gui/ calls found in {calls:?}");
        for call in gui_calls {
            assert!(
                call.contains("gui/777"),
                "Call {call} does not name gui/777"
            );
        }
    }

    #[test]
    fn set_saves_the_settings_inside_the_layout() {
        let temp = tempdir().unwrap();
        let layout = Layout {
            home: temp.path().to_path_buf(),
            root: temp.path().to_path_buf(),
            user_cache: None,
        };
        let bin = temp.path().join("taert");
        fs::write(&bin, b"bin").unwrap();
        let mut config = Config::default();

        let host = FakeHost::default();
        host.reply("/bin/launchctl", "");
        set_schedule(
            &host,
            &layout,
            &mut config,
            ScheduleFrequency::Week,
            vec!["dev".to_string()],
            1_000_000,
            Some(&bin),
        )
        .unwrap();

        let saved = Config::load_from(&layout.config_path()).unwrap();
        assert_eq!(saved.schedule.unwrap().min_reclaim, 1_000_000);
    }

    #[test]
    fn off_saves_the_cleared_settings_inside_the_layout() {
        let temp = tempdir().unwrap();
        let layout = Layout {
            home: temp.path().to_path_buf(),
            root: temp.path().to_path_buf(),
            user_cache: None,
        };
        let mut config = Config::default();

        remove_schedule(&FakeHost::default(), &layout, &mut config).unwrap();

        assert!(layout.config_path().exists());
    }

    #[test]
    fn off_removes_the_plist_after_bootout() {
        let temp = tempdir().unwrap();
        let layout = Layout {
            home: temp.path().to_path_buf(),
            root: temp.path().to_path_buf(),
            user_cache: None,
        };
        let host = FakeHost::default();
        let mut config = Config::default();

        let plist_file = agent_plist_path(&layout);
        fs::create_dir_all(plist_file.parent().unwrap()).unwrap();
        fs::write(&plist_file, b"content").unwrap();
        assert!(plist_file.exists());

        remove_schedule(&host, &layout, &mut config).unwrap();

        assert!(host.ran("/bin/launchctl bootout"));
        assert!(!plist_file.exists());
        assert!(config.schedule.is_none());
    }

    #[test]
    fn show_reports_a_missing_binary_and_an_unloaded_agent() {
        let temp = tempdir().unwrap();
        let layout = Layout {
            home: temp.path().to_path_buf(),
            root: temp.path().to_path_buf(),
            user_cache: None,
        };
        let host = FakeHost::default();
        host.fail("/bin/launchctl print", "Could not find service");

        let missing_bin = temp.path().join("does_not_exist_binary");
        let plist_file = agent_plist_path(&layout);
        fs::create_dir_all(plist_file.parent().unwrap()).unwrap();
        let xml = generate_plist(&missing_bin, ScheduleFrequency::Week).unwrap();
        fs::write(&plist_file, xml).unwrap();

        let config = Config {
            schedule: Some(ScheduleConfig {
                every: ScheduleFrequency::Week,
                categories: vec!["dev".to_string(), "junk".to_string()],
                min_reclaim: 1_000_000_000,
                last_skip: None,
            }),
            ..Default::default()
        };
        let history = HistoryLog::new(temp.path().join("history.jsonl"));

        let output = show_schedule(&host, &layout, &config, &history);
        assert!(output.contains("Every week: dev, junk, when at least 1 GB can be freed."));
        assert!(output.contains("Binary '"));
        assert!(output.contains("does not exist"));
        assert!(output.contains("Launch agent is not loaded in launchd"));
    }

    fn sandbox() -> (tempfile::TempDir, Layout) {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let layout = Layout {
            home: base.join("home"),
            root: base.join("root"),
            user_cache: None,
        };
        fs::create_dir_all(&layout.home).unwrap();
        fs::create_dir_all(&layout.root).unwrap();
        (temp, layout)
    }

    #[test]
    fn total_below_the_limit_removes_nothing_and_records_a_skip() {
        let (_temp, layout) = sandbox();
        let homebrew = layout.user("Library/Caches/Homebrew");
        fs::create_dir_all(&homebrew).unwrap();
        fs::write(homebrew.join("cache.tar.gz"), vec![0u8; 2048]).unwrap();

        let host = FakeHost::idle();
        let lock_path = layout.user("schedule.lock");

        let mut config = Config {
            schedule: Some(ScheduleConfig {
                every: ScheduleFrequency::Week,
                categories: vec!["dev".to_string()],
                min_reclaim: 1_000_000, // 1 MB limit, we only have 2 KB
                last_skip: None,
            }),
            ..Default::default()
        };

        run_scheduled(&host, &layout, &mut config, Some(&lock_path)).unwrap();

        // Files were NOT removed
        assert!(homebrew.join("cache.tar.gz").exists());
        // Skip was recorded
        let skip = config
            .schedule
            .as_ref()
            .unwrap()
            .last_skip
            .as_ref()
            .unwrap();
        assert!(skip.bytes >= 2048);
        assert!(!layout.history_path().exists());
        let saved = Config::load_from(&layout.config_path()).unwrap();
        assert_eq!(saved.schedule.unwrap().last_skip.unwrap().bytes, skip.bytes);
    }

    #[test]
    fn only_selected_rows_are_removed() {
        let (_temp, layout) = sandbox();
        let homebrew = homebrew_cache(&layout);
        let maven = layout.user(".m2/repository/artifact.jar");
        fs::create_dir_all(maven.parent().unwrap()).unwrap();
        fs::write(&maven, vec![0u8; 8192]).unwrap();
        let mut config = scheduled(&["dev"], 1024);

        run_scheduled(
            &FakeHost::idle(),
            &layout,
            &mut config,
            Some(&layout.user("schedule.lock")),
        )
        .unwrap();

        assert!(!homebrew.exists());
        assert!(maven.exists());
        let runs = HistoryLog::new(layout.history_path()).load_runs().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].mode, "schedule");
        assert!(runs[0].completed);
    }

    #[test]
    fn run_keeps_rows_the_ignore_list_of_the_layout_protects() {
        let (_temp, layout) = sandbox();
        let homebrew = layout.user("Library/Caches/Homebrew");
        fs::create_dir_all(&homebrew).unwrap();
        fs::write(homebrew.join("cache.tar.gz"), vec![0u8; 8192]).unwrap();
        IgnoreStore::new(vec![homebrew.display().to_string()])
            .save_to(&layout.ignore_path())
            .unwrap();
        let mut config = Config {
            schedule: Some(ScheduleConfig {
                every: ScheduleFrequency::Week,
                categories: vec!["dev".to_string()],
                min_reclaim: 1024,
                last_skip: None,
            }),
            ..Default::default()
        };

        run_scheduled(
            &FakeHost::idle(),
            &layout,
            &mut config,
            Some(&layout.user("schedule.lock")),
        )
        .unwrap();

        assert!(homebrew.join("cache.tar.gz").exists());
    }

    fn homebrew_cache(layout: &Layout) -> PathBuf {
        let dir = layout.user("Library/Caches/Homebrew");
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("cache.tar.gz");
        fs::write(&file, vec![0u8; 8192]).unwrap();
        file
    }

    fn scheduled(categories: &[&str], min_reclaim: u64) -> Config {
        Config {
            schedule: Some(ScheduleConfig {
                every: ScheduleFrequency::Week,
                categories: categories.iter().map(|c| c.to_string()).collect(),
                min_reclaim,
                last_skip: None,
            }),
            ..Default::default()
        }
    }

    fn week_config() -> Config {
        scheduled(&["dev", "junk"], 1_000_000_000)
    }

    #[test]
    fn show_says_the_agent_file_is_missing() {
        let (_temp, layout) = sandbox();
        let history = HistoryLog::new(layout.history_path());

        let output = show_schedule(&FakeHost::default(), &layout, &week_config(), &history);

        assert!(
            output.contains("The launch agent file is missing"),
            "{output}"
        );
        assert!(output.contains(
            "taert schedule set --every week --categories dev,junk --min-reclaim 1000000000"
        ));
        assert!(!output.contains("System Settings"));
        assert!(!output.contains("not loaded"));
    }

    #[test]
    fn show_with_a_loaded_agent_has_no_warning() {
        let (temp, layout) = sandbox();
        let bin = temp.path().join("taert");
        fs::write(&bin, b"bin").unwrap();
        let plist_file = agent_plist_path(&layout);
        fs::create_dir_all(plist_file.parent().unwrap()).unwrap();
        fs::write(
            &plist_file,
            generate_plist(&bin, ScheduleFrequency::Week).unwrap(),
        )
        .unwrap();
        let host = FakeHost::default();
        let uid = unsafe { libc::getuid() };
        host.set_uid(uid);
        host.reply(
            &format!("/bin/launchctl print gui/{uid}/{SCHEDULE_LABEL}"),
            "state = waiting",
        );
        let history = HistoryLog::new(layout.history_path());

        let output = show_schedule(&host, &layout, &week_config(), &history);

        assert!(!output.contains("not loaded"), "{output}");
        assert!(!output.contains("missing"), "{output}");
        assert!(!output.contains("Warning"), "{output}");
    }

    #[test]
    fn set_stores_lower_case_names_once() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        host.reply("/bin/launchctl", "");
        let mut config = Config::default();

        set_schedule(
            &host,
            &layout,
            &mut config,
            ScheduleFrequency::Week,
            vec!["DEV".into(), " dev ".into(), "junk".into()],
            1024,
            Some(&layout.user("taert")),
        )
        .unwrap();

        let saved = Config::load_from(&layout.config_path()).unwrap();
        assert_eq!(saved.schedule.unwrap().categories, ["dev", "junk"]);
        assert_eq!(config.schedule.unwrap().categories, ["dev", "junk"]);
    }

    #[test]
    fn names_are_trimmed_lower_cased_and_listed_once() {
        let names: Vec<String> = [" Dev", "", "JUNK", "dev", "  "]
            .iter()
            .map(|s| s.to_string())
            .collect();

        assert_eq!(normalize_categories(&names), ["dev", "junk"]);
    }

    #[test]
    fn a_run_with_upper_case_names_removes_the_target() {
        let (_temp, layout) = sandbox();
        let homebrew = homebrew_cache(&layout);
        let mut config = scheduled(&["DEV"], 1024);

        run_scheduled(
            &FakeHost::idle(),
            &layout,
            &mut config,
            Some(&layout.user("schedule.lock")),
        )
        .unwrap();

        assert!(!homebrew.exists());
    }

    #[test]
    fn a_run_with_repeated_names_scans_once() {
        let (_temp, layout) = sandbox();
        let homebrew = homebrew_cache(&layout);
        let mut config = scheduled(&["dev", "dev"], 1024);

        run_scheduled(
            &FakeHost::idle(),
            &layout,
            &mut config,
            Some(&layout.user("schedule.lock")),
        )
        .unwrap();

        let runs = HistoryLog::new(layout.history_path()).load_runs().unwrap();
        let path = homebrew.parent().unwrap().display().to_string();
        let items: Vec<_> = runs[0]
            .items
            .iter()
            .filter(|i| i.path.contains(&path))
            .collect();
        assert_eq!(items.len(), 1, "{:?}", runs[0].items);
        assert!(
            runs[0]
                .items
                .iter()
                .all(|i| i.outcome != OutcomeKind::Failed)
        );
    }

    #[test]
    fn a_failed_bootstrap_fails_set_and_saves_nothing() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        host.fail(
            "/bin/launchctl bootstrap",
            "Bootstrap failed: 5: Input/output error\n",
        );
        let mut config = Config::default();
        let bin = layout.user("taert");

        let err = set_schedule(
            &host,
            &layout,
            &mut config,
            ScheduleFrequency::Week,
            vec!["dev".into()],
            1024,
            Some(&bin),
        )
        .unwrap_err();

        assert!(err.contains("Bootstrap failed: 5"), "{err}");
        assert!(err.starts_with("launchctl bootstrap failed: "));
        assert!(config.schedule.is_none());
        assert!(!layout.config_path().exists());
        assert!(!agent_plist_path(&layout).exists());
        assert_eq!(
            host.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.starts_with("/bin/launchctl bootstrap"))
                .count(),
            1
        );
    }

    #[test]
    fn a_failed_bootstrap_puts_the_old_plist_back() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        host.fail("/bin/launchctl bootstrap", "");
        let plist_file = agent_plist_path(&layout);
        fs::create_dir_all(plist_file.parent().unwrap()).unwrap();
        fs::write(&plist_file, b"old").unwrap();
        let mut config = Config::default();

        let err = set_schedule(
            &host,
            &layout,
            &mut config,
            ScheduleFrequency::Week,
            vec!["dev".into()],
            1024,
            Some(&layout.user("taert")),
        )
        .unwrap_err();

        assert_eq!(err, "launchctl bootstrap failed");
        assert_eq!(fs::read(&plist_file).unwrap(), b"old");
        assert_eq!(
            host.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.starts_with("/bin/launchctl bootstrap"))
                .count(),
            1
        );
        assert!(config.schedule.is_none());
        assert!(!layout.config_path().exists());
    }

    fn history_with_a_run_at(layout: &Layout, at: &str) {
        let start = format!(
            r#"{{"event":"start","run":"r1","at":"{at}","command":"schedule run","mode":"schedule","version":"0"}}"#
        );
        let end = format!(r#"{{"event":"end","run":"r1","at":"{at}"}}"#);
        fs::create_dir_all(layout.history_path().parent().unwrap()).unwrap();
        fs::write(layout.history_path(), format!("{start}\n{end}\n")).unwrap();
    }

    fn last_run_line(output: &str) -> &str {
        output
            .lines()
            .find(|l| l.starts_with("Last run:"))
            .unwrap_or_default()
    }

    fn show_with_skip(layout: &Layout, skip_at: &str) -> String {
        let mut config = week_config();
        config.schedule.as_mut().unwrap().last_skip = Some(ScheduleSkip {
            at: skip_at.to_string(),
            bytes: 2048,
        });
        let history = HistoryLog::new(layout.history_path());
        show_schedule(&FakeHost::default(), layout, &config, &history)
    }

    #[test]
    fn show_reports_the_newer_skip() {
        let (_temp, layout) = sandbox();
        history_with_a_run_at(&layout, "2026-01-01T00:00:00Z");

        let output = show_with_skip(&layout, "2026-02-01T00:00:00Z");

        let line = last_run_line(&output);
        assert!(line.starts_with("Last run: skipped on"), "{output}");
        assert!(!line.contains("freed"), "{output}");
    }

    #[test]
    fn show_reports_the_newer_run() {
        let (_temp, layout) = sandbox();
        history_with_a_run_at(&layout, "2026-02-01T00:00:00Z");

        let output = show_with_skip(&layout, "2026-01-01T00:00:00Z");

        let line = last_run_line(&output);
        assert!(line.contains("freed"), "{output}");
        assert!(!line.contains("skipped"), "{output}");
    }

    #[test]
    fn a_skip_with_a_broken_date_loses_to_the_run() {
        let (_temp, layout) = sandbox();
        history_with_a_run_at(&layout, "2026-01-01T00:00:00Z");

        let output = show_with_skip(&layout, "not a date");

        assert!(last_run_line(&output).contains("freed"), "{output}");
    }

    #[test]
    fn a_run_with_a_broken_date_loses_to_the_skip() {
        let (_temp, layout) = sandbox();
        history_with_a_run_at(&layout, "not a date");

        let output = show_with_skip(&layout, "2026-02-01T00:00:00Z");

        assert!(
            last_run_line(&output).starts_with("Last run: skipped on"),
            "{output}"
        );
    }

    #[test]
    fn an_unreadable_old_plist_stops_set_before_anything_changes() {
        use std::os::unix::fs::PermissionsExt;
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        host.reply("/bin/launchctl", "");
        let plist_file = agent_plist_path(&layout);
        fs::create_dir_all(plist_file.parent().unwrap()).unwrap();
        fs::write(&plist_file, b"old").unwrap();
        fs::set_permissions(&plist_file, fs::Permissions::from_mode(0o000)).unwrap();
        let mut config = Config::default();

        let err = set_schedule(
            &host,
            &layout,
            &mut config,
            ScheduleFrequency::Week,
            vec!["dev".into()],
            1024,
            Some(&layout.user("taert")),
        )
        .unwrap_err();

        fs::set_permissions(&plist_file, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(err.starts_with("Failed to read LaunchAgent plist"), "{err}");
        assert!(host.calls.lock().unwrap().is_empty());
        assert_eq!(fs::read(&plist_file).unwrap(), b"old");
        assert!(config.schedule.is_none());
    }

    #[test]
    fn an_unreadable_lock_file_fails_the_run() {
        let (temp, layout) = sandbox();
        let file = temp.path().join("file.txt");
        fs::write(&file, b"x").unwrap();
        let mut config = scheduled(&["dev"], 1024);

        let result = run_scheduled(
            &FakeHost::idle(),
            &layout,
            &mut config,
            Some(&file.join("schedule.lock")),
        );

        assert!(result.is_err());
    }

    #[test]
    fn a_held_lock_decides_the_outcome() {
        let (_temp, layout) = sandbox();
        let homebrew = homebrew_cache(&layout);
        let lock_path = layout.user("schedule.lock");
        let _held = ScheduleLock::try_acquire(&lock_path).unwrap();
        let mut config = scheduled(&["dev"], 1024);

        let result = run_scheduled(&FakeHost::idle(), &layout, &mut config, Some(&lock_path));

        assert!(result.is_ok());
        assert!(homebrew.exists());
        assert!(!layout.history_path().exists());
        assert!(config.schedule.unwrap().last_skip.is_none());
        assert!(!layout.config_path().exists());
    }
}

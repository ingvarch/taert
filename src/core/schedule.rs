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

impl ScheduleLock {
    pub fn try_acquire(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|e| format!("Failed to open lock file: {e}"))?;

        let fd = file.as_raw_fd();
        let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            return Err(format!("Schedule lock held by another process: {err}"));
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

    let plist_path = agent_plist_path(layout);
    let uid = host.uid();

    // If agent already exists on disk, bootout first
    if plist_path.exists() {
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
        eprintln!(
            "Warning: launchctl bootstrap returned: {}",
            boot_out.stderr.trim()
        );
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
    if let Some(run) = last_run {
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
    } else if let Some(skip) = &sched.last_skip {
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
    } else {
        out.push_str("Last run: never.\n");
    }

    // 3. Inspect plist binary path
    let plist_binary = if plist_path.exists() {
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
        Err(_) => return Ok(()), // Held lock exits with no work done
    };

    // 2. Scan configured categories
    let ignore = IgnoreStore::load_from(&layout.ignore_path()).unwrap_or_default();
    let context = ScanContext { layout, host };
    let mut all_items: Vec<ScanItem> = Vec::new();

    for cat in &sched.categories {
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
    fn generated_plist_matches_stored_example_for_each_frequency() {
        let bin = Path::new("/opt/homebrew/bin/taert");

        let day_plist = generate_plist(bin, ScheduleFrequency::Day).unwrap();
        assert!(
            day_plist
                .contains("<key>Label</key>\n\t<string>io.github.ingvarch.taert.schedule</string>")
        );
        assert!(day_plist.contains("<key>LowPriorityIO</key>\n\t<true/>"));
        assert!(day_plist.contains("<key>Nice</key>\n\t<integer>10</integer>"));
        assert!(day_plist.contains("<key>ProcessType</key>\n\t<string>Background</string>"));
        assert!(day_plist.contains("<string>/opt/homebrew/bin/taert</string>\n\t\t<string>schedule</string>\n\t\t<string>run</string>"));
        assert!(day_plist.contains("<key>StartCalendarInterval</key>\n\t<dict>\n\t\t<key>Hour</key>\n\t\t<integer>9</integer>\n\t\t<key>Minute</key>\n\t\t<integer>0</integer>\n\t</dict>"));

        let week_plist = generate_plist(bin, ScheduleFrequency::Week).unwrap();
        assert!(week_plist.contains("<key>StartCalendarInterval</key>\n\t<dict>\n\t\t<key>Hour</key>\n\t\t<integer>9</integer>\n\t\t<key>Minute</key>\n\t\t<integer>0</integer>\n\t\t<key>Weekday</key>\n\t\t<integer>1</integer>\n\t</dict>"));

        let month_plist = generate_plist(bin, ScheduleFrequency::Month).unwrap();
        assert!(month_plist.contains("<key>StartCalendarInterval</key>\n\t<dict>\n\t\t<key>Day</key>\n\t\t<integer>1</integer>\n\t\t<key>Hour</key>\n\t\t<integer>9</integer>\n\t\t<key>Minute</key>\n\t\t<integer>0</integer>\n\t</dict>"));
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

        set_schedule(
            &FakeHost::default(),
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
    fn held_lock_exits_with_no_work_done() {
        let temp = tempdir().unwrap();
        let lock_path = temp.path().join("schedule.lock");

        let _guard = ScheduleLock::try_acquire(&lock_path).unwrap();

        let host = FakeHost::default();
        let layout = Layout {
            home: temp.path().to_path_buf(),
            root: temp.path().to_path_buf(),
            user_cache: None,
        };
        let mut config = Config {
            schedule: Some(ScheduleConfig {
                every: ScheduleFrequency::Week,
                categories: vec!["dev".to_string()],
                min_reclaim: 100,
                last_skip: None,
            }),
            ..Default::default()
        };

        let result = run_scheduled(&host, &layout, &mut config, Some(&lock_path));
        assert!(result.is_ok());
        assert!(!layout.history_path().exists());
        assert!(!layout.config_path().exists());
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
        // launchctl print fails by default on FakeHost without reply
        host.fail("/bin/launchctl", "Could not find service");

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
    fn run_removes_only_selected_rows_and_writes_history_with_mode_schedule() {
        let (_temp, layout) = sandbox();
        let homebrew = layout.user("Library/Caches/Homebrew");
        fs::create_dir_all(&homebrew).unwrap();
        fs::write(homebrew.join("cache.tar.gz"), vec![0u8; 8192]).unwrap();

        let host = FakeHost::idle();
        let lock_path = layout.user("schedule.lock");

        let mut config = Config {
            schedule: Some(ScheduleConfig {
                every: ScheduleFrequency::Week,
                categories: vec!["dev".to_string()],
                min_reclaim: 1024, // 1 KB limit, we have 8 KB
                last_skip: None,
            }),
            ..Default::default()
        };

        run_scheduled(&host, &layout, &mut config, Some(&lock_path)).unwrap();

        // Files WERE removed
        assert!(!homebrew.join("cache.tar.gz").exists());
        // History was written with mode "schedule"
        let history = HistoryLog::new(layout.history_path());
        let runs = history.load_runs().unwrap();
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
}

//! Background cleanup scheduling via macOS launchd.
//!
//! Configures and manages `~/Library/LaunchAgents/io.github.ingvarch.taert.schedule.plist`
//! to run unattended cache cleanup on a recurring schedule.

use std::fs::{self, File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use super::admin;
use super::bytes::format_bytes;
use super::catalog::{Catalog, Category};
use super::cleaner::{Cleaner, RemovalMode};
use super::date_utils::{format_rfc3339, format_system_time, parse_rfc3339};
use super::history::{HistoryLog, ItemOutcome, OutcomeKind, RunRecord};
use super::host::Host;
use super::layout::Layout;
use super::scanner::{CategoryScanner, ScanContext, ScanItem};
use super::stores::{Config, IgnoreStore, ScheduleConfig, ScheduleSkip};

/// The program a launch agent runs when taert cannot tell its own path.
const DEFAULT_PROGRAM: &str = "/opt/homebrew/bin/taert";
pub const SCHEDULE_LABEL: &str = "io.github.ingvarch.taert.schedule";
pub const SCHEDULE_PLIST_NAME: &str = "io.github.ingvarch.taert.schedule.plist";
/// The newest run the schedule started.
pub fn last_scheduled_run(runs: &[RunRecord]) -> Option<&RunRecord> {
    runs.iter().rev().find(|run| run.mode == "schedule")
}

pub const VALID_CATEGORIES: &[&str] = &[
    Category::Dev.id(),
    Category::Junk.id(),
    Category::Ai.id(),
    Category::Trash.id(),
];
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
    start_calendar_interval: Vec<CalendarInterval>,
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

/// A time of day, `00:00` to `23:59`.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct TimeOfDay {
    hour: u32,
    minute: u32,
}

impl TimeOfDay {
    fn parse(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        let digits = [0, 1, 3, 4];
        if bytes.len() != 5
            || bytes[2] != b':'
            || !digits.iter().all(|&i| bytes[i].is_ascii_digit())
        {
            return None;
        }
        let number = |from: usize| text[from..from + 2].parse::<u32>().ok();
        let (hour, minute) = (number(0)?, number(3)?);
        (hour <= 23 && minute <= 59).then_some(Self { hour, minute })
    }
}

impl std::fmt::Display for TimeOfDay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:02}:{:02}", self.hour, self.minute)
    }
}

/// Config name, display name, and launchd number of each weekday, Monday first.
const WEEKDAYS: [(&str, &str, i32); 7] = [
    ("mon", "Mon", 1),
    ("tue", "Tue", 2),
    ("wed", "Wed", 3),
    ("thu", "Thu", 4),
    ("fri", "Fri", 5),
    ("sat", "Sat", 6),
    ("sun", "Sun", 0),
];
const REPEAT_HOURS: [u32; 6] = [2, 3, 4, 6, 8, 12];
const MAX_MONTH_DAY: u32 = 28;
/// A month rule matches within 31 days; two months leave room to spare.
const SEARCH_DAYS: i32 = 62;

/// When the schedule runs. Built from the config, so every value is checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleRule {
    every: ScheduleFrequency,
    at: TimeOfDay,
    days: Vec<usize>,
    day: u32,
    every_hours: Option<u32>,
}

impl ScheduleRule {
    pub fn from_config(config: &ScheduleConfig) -> Result<Self, String> {
        let at = TimeOfDay::parse(&config.at)
            .ok_or_else(|| format!("Time {} is not HH:MM between 00:00 and 23:59.", config.at))?;
        let names = WEEKDAYS.map(|(name, _, _)| name).join(", ");
        if config.days.is_empty() {
            return Err(format!("Weekdays are empty. Use {names}."));
        }
        let mut days = Vec::new();
        for name in &config.days {
            let index = WEEKDAYS
                .iter()
                .position(|(known, _, _)| known == name)
                .ok_or_else(|| format!("Weekday {name} is not one of {names}."))?;
            days.push(index);
        }
        days.sort_unstable();
        days.dedup();
        if !(1..=MAX_MONTH_DAY).contains(&config.day) {
            return Err(format!(
                "Day {} is not between 1 and {MAX_MONTH_DAY}.",
                config.day
            ));
        }
        if let Some(hours) = config.every_hours
            && !REPEAT_HOURS.contains(&hours)
        {
            let list = REPEAT_HOURS.map(|h| h.to_string()).join(", ");
            return Err(format!("Repeat every {hours} hours is not one of {list}."));
        }
        Ok(Self {
            every: config.every,
            at,
            days,
            day: config.day,
            every_hours: config.every_hours,
        })
    }

    /// The rule of the saved schedule with another frequency. Without a saved schedule, the defaults.
    pub fn for_frequency(
        saved: Option<&ScheduleConfig>,
        every: ScheduleFrequency,
    ) -> Result<Self, String> {
        let mut config = saved.cloned().unwrap_or_default();
        config.every = every;
        Self::from_config(&config)
    }

    pub fn times_of_day(&self) -> Vec<TimeOfDay> {
        let step = self.every_hours.unwrap_or(24);
        let mut times: Vec<TimeOfDay> = (0..24 / step)
            .map(|k| TimeOfDay {
                hour: (self.at.hour + k * step) % 24,
                minute: self.at.minute,
            })
            .collect();
        times.sort();
        times
    }

    pub fn describe(&self) -> String {
        let head = match self.every {
            ScheduleFrequency::Day => "Every day".to_string(),
            ScheduleFrequency::Week => {
                let names: Vec<&str> = self.days.iter().map(|&i| WEEKDAYS[i].1).collect();
                format!("Every week on {}", names.join(", "))
            }
            ScheduleFrequency::Month => format!("Every month on day {}", self.day),
        };
        match self.every_hours {
            None => format!("{head} at {}", self.at),
            Some(hours) => format!(
                "{head}, every {hours} hours from {}",
                self.times_of_day()[0]
            ),
        }
    }

    /// The first time after `now` that launchd starts the job, in the local time zone.
    pub fn next_run(&self, now: SystemTime) -> SystemTime {
        let secs = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as libc::time_t;
        let mut today: libc::tm = unsafe { std::mem::zeroed() };
        unsafe {
            libc::localtime_r(&secs, &mut today);
        }
        let times = self.times_of_day();
        for offset in 0..=SEARCH_DAYS {
            let mut date = today;
            date.tm_mday += offset;
            (date.tm_hour, date.tm_min, date.tm_sec, date.tm_isdst) = (12, 0, 0, -1);
            unsafe {
                libc::mktime(&mut date);
            }
            if !self.matches_date(&date) {
                continue;
            }
            let first = times
                .iter()
                .filter_map(|time| {
                    let mut moment = date;
                    (moment.tm_hour, moment.tm_min, moment.tm_isdst) =
                        (time.hour as i32, time.minute as i32, -1);
                    let found = unsafe { libc::mktime(&mut moment) };
                    (found > secs).then_some(found)
                })
                .min();
            if let Some(found) = first {
                return UNIX_EPOCH + Duration::from_secs(found as u64);
            }
        }
        now
    }

    fn matches_date(&self, date: &libc::tm) -> bool {
        match self.every {
            ScheduleFrequency::Day => true,
            ScheduleFrequency::Week => self.days.iter().any(|&i| WEEKDAYS[i].2 == date.tm_wday),
            ScheduleFrequency::Month => date.tm_mday as u32 == self.day,
        }
    }

    fn calendar_entries(&self) -> Vec<CalendarInterval> {
        let times = self.times_of_day();
        let entry = |time: &TimeOfDay, day: Option<u32>, weekday: Option<u32>| CalendarInterval {
            day,
            hour: time.hour,
            minute: time.minute,
            weekday,
        };
        match self.every {
            ScheduleFrequency::Day => times.iter().map(|t| entry(t, None, None)).collect(),
            ScheduleFrequency::Week => self
                .days
                .iter()
                .flat_map(|&i| {
                    let weekday = WEEKDAYS[i].2 as u32;
                    times.iter().map(move |t| entry(t, None, Some(weekday)))
                })
                .collect(),
            ScheduleFrequency::Month => times
                .iter()
                .map(|t| entry(t, Some(self.day), None))
                .collect(),
        }
    }

    fn day_names(&self) -> Vec<String> {
        self.days
            .iter()
            .map(|&i| WEEKDAYS[i].0.to_string())
            .collect()
    }
}

/// Generates launchd plist XML content for the schedule job.
pub fn generate_plist(binary: &Path, rule: &ScheduleRule) -> Result<String, String> {
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
        start_calendar_interval: rule.calendar_entries(),
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
        if matches!(
            normalized.as_str(),
            "purge" | "docker" | "uninstall" | "clean orphans"
        ) || Category::from_id(&normalized).is_some_and(|category| !category.schedulable())
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
    rule: &ScheduleRule,
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
    let default_exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from(DEFAULT_PROGRAM));
    let exe = current_exe.unwrap_or(&default_exe);
    let bin_path = resolve_binary_path(exe);

    // Generate and write plist
    let xml = generate_plist(&bin_path, rule)?;
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
        every: rule.every,
        categories,
        min_reclaim,
        at: rule.at.to_string(),
        days: rule.day_names(),
        day: rule.day,
        every_hours: rule.every_hours,
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

pub fn format_reclaim_threshold(bytes: u64) -> String {
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

/// What the schedule is set to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleSettings {
    Off {
        plist_exists: bool,
    },
    On {
        every: ScheduleFrequency,
        categories: Vec<String>,
        min_reclaim: u64,
    },
}

/// The result of the last scheduled run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LastRun {
    Never,
    Ran {
        at: String,
        freed: u64,
    },
    Skipped {
        at: String,
        found: u64,
        threshold: u64,
    },
}

/// Everything `taert schedule show` and Settings tell about the schedule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleStatus {
    pub settings: ScheduleSettings,
    pub last_run: LastRun,
    pub plist_missing: bool,
    pub missing_program: Option<PathBuf>,
    /// `None` when the plist is missing, so launchd was not asked.
    pub agent_loaded: Option<bool>,
    /// The program the last run needed Full Disk Access for.
    pub needs_full_disk_access: Option<PathBuf>,
}

/// The result of the last scheduled run: the newest scheduled `record` of the history, or a
/// skip recorded after it.
pub fn last_run(sched: &ScheduleConfig, record: Option<&RunRecord>) -> LastRun {
    let newer_skip = sched.last_skip.as_ref().filter(|skip| {
        let Some(run) = record else {
            return true;
        };
        match (parse_rfc3339(&run.at), parse_rfc3339(&skip.at)) {
            (Some(run_at), Some(skip_at)) => skip_at > run_at,
            (None, Some(_)) => true,
            _ => false,
        }
    });
    if let Some(skip) = newer_skip {
        LastRun::Skipped {
            at: skip.at.clone(),
            found: skip.bytes,
            threshold: sched.min_reclaim,
        }
    } else if let Some(run) = record {
        let freed = run
            .items
            .iter()
            .filter(|i| i.outcome == OutcomeKind::Removed || i.outcome == OutcomeKind::Trashed)
            .map(|i| i.bytes)
            .sum();
        LastRun::Ran {
            at: run.at.clone(),
            freed,
        }
    } else {
        LastRun::Never
    }
}

/// Gathers the facts about the schedule. Asks launchd once, and only when the plist exists.
pub fn schedule_status(
    host: &dyn Host,
    layout: &Layout,
    config: &Config,
    history: &HistoryLog,
) -> ScheduleStatus {
    let plist_path = agent_plist_path(layout);
    let plist_exists = plist_path.exists();
    let Some(sched) = &config.schedule else {
        return ScheduleStatus {
            settings: ScheduleSettings::Off { plist_exists },
            last_run: LastRun::Never,
            plist_missing: false,
            missing_program: None,
            agent_loaded: None,
            needs_full_disk_access: None,
        };
    };

    let runs = history.load_runs().unwrap_or_default();
    let last_run_record = last_scheduled_run(&runs);
    let last_run = last_run(sched, last_run_record);

    let program = plist_exists
        .then(|| plist::Value::from_file(&plist_path).ok())
        .flatten()
        .and_then(|val| val.into_dictionary())
        .and_then(|dict| {
            dict.get("ProgramArguments")
                .and_then(plist::Value::as_array)
                .and_then(|args| args.first().and_then(plist::Value::as_string))
                .map(PathBuf::from)
        });
    let missing_program = program.clone().filter(|bin| !bin.exists());
    let agent_loaded = plist_exists.then(|| is_agent_loaded(host, host.uid()));

    let permission_failed = last_run_record.is_some_and(|r| {
        r.items
            .iter()
            .any(|i| i.outcome == OutcomeKind::Failed && is_permission_error(i.error.as_deref()))
    });
    let needs_full_disk_access =
        permission_failed.then(|| program.unwrap_or_else(|| PathBuf::from(DEFAULT_PROGRAM)));

    ScheduleStatus {
        settings: ScheduleSettings::On {
            every: sched.every,
            categories: sched.categories.clone(),
            min_reclaim: sched.min_reclaim,
        },
        last_run,
        plist_missing: !plist_exists,
        missing_program,
        agent_loaded,
        needs_full_disk_access,
    }
}

/// Formats a time of the history or the config, or the text itself when it is not a date.
pub fn format_run_time(at: &str) -> String {
    parse_rfc3339(at).map_or_else(|| at.to_string(), format_system_time)
}

/// Writes the status as the text of `taert schedule show`.
pub fn format_schedule_status(status: &ScheduleStatus) -> String {
    let ScheduleSettings::On {
        every,
        categories,
        min_reclaim,
    } = &status.settings
    else {
        return match status.settings {
            ScheduleSettings::Off { plist_exists: true } => {
                "Scheduled cleanup is off (config disabled, but LaunchAgent plist exists).\n"
            }
            _ => "Scheduled cleanup is off.\n",
        }
        .to_string();
    };

    let threshold_str = format_reclaim_threshold(*min_reclaim);
    let mut out = format!(
        "Every {every}: {}, when at least {threshold_str} can be freed.\n",
        categories.join(", ")
    );

    match &status.last_run {
        LastRun::Skipped {
            at,
            found,
            threshold,
        } => out.push_str(&format!(
            "Last run: skipped on {} ({} found, below {} threshold).\n",
            format_run_time(at),
            format_bytes(*found),
            format_reclaim_threshold(*threshold)
        )),
        LastRun::Ran { at, freed } => out.push_str(&format!(
            "Last run: {}, freed {}.\n",
            format_run_time(at),
            format_bytes(*freed)
        )),
        LastRun::Never => out.push_str("Last run: never.\n"),
    }

    if status.plist_missing {
        out.push_str(&format!(
            "\nWarning: The launch agent file is missing. A Homebrew upgrade can remove it. Load it again with:\n  taert schedule set --every {every} --categories {} --min-reclaim {min_reclaim}\n",
            categories.join(",")
        ));
    } else {
        if let Some(bin) = &status.missing_program {
            out.push_str(&format!(
                "\nWarning: Binary '{}' configured in LaunchAgent does not exist.\n",
                bin.display()
            ));
        }
        if status.agent_loaded == Some(false) {
            out.push_str(
                "\nWarning: Launch agent is not loaded in launchd. It may have been disabled in System Settings > General > Login Items & Extensions.\n"
            );
        }
    }

    if let Some(program) = &status.needs_full_disk_access {
        out.push_str(&format!(
            "\nScheduled cleanup failed to remove some files due to missing permissions.\nTo grant taert Full Disk Access:\n  1. Open System Settings > Privacy & Security > Full Disk Access\n  2. Add and enable: {}\n",
            program.display()
        ));
    }

    out
}

/// Formats the output for `taert schedule show`.
pub fn show_schedule(
    host: &dyn Host,
    layout: &Layout,
    config: &Config,
    history: &HistoryLog,
) -> String {
    format_schedule_status(&schedule_status(host, layout, config, history))
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
    let context = ScanContext {
        layout,
        host,
        admin: admin::session(),
    };
    let mut all_items: Vec<ScanItem> = Vec::new();

    for cat in &normalize_categories(&sched.categories) {
        let Some(category) = Category::from_id(cat) else {
            continue;
        };
        let scan = CategoryScanner::scan_with(
            &context,
            cat,
            category.title(),
            Catalog::targets_in(layout, category),
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
    fn uninstall_and_orphans_cannot_be_scheduled() {
        for name in ["uninstall", "orphans", "clean orphans", "Orphans"] {
            let err = validate_categories(&[name.to_string()]).unwrap_err();
            assert!(err.contains("cannot be scheduled"), "{name}: {err}");
        }
    }

    #[test]
    fn every_schedule_category_parses() {
        assert_eq!(VALID_CATEGORIES, ["dev", "junk", "ai", "trash"]);
        for id in VALID_CATEGORIES {
            assert!(
                Category::from_id(id).is_some_and(Category::schedulable),
                "{id}"
            );
        }
        for category in Category::ALL {
            assert_eq!(
                VALID_CATEGORIES.contains(&category.id()),
                category.schedulable(),
                "{}",
                category.id()
            );
        }
        assert!(!Category::Orphans.schedulable());
    }

    #[test]
    fn an_unknown_category_lists_the_allowed_ones() {
        assert_eq!(
            validate_categories(&["foo".to_string()]),
            Err("Invalid category 'foo'. Allowed categories: dev, junk, ai, trash".to_string())
        );
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
            &default_rule(ScheduleFrequency::Week),
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
            &default_rule(ScheduleFrequency::Week),
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
            &default_rule(ScheduleFrequency::Week),
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
        let xml = generate_plist(&missing_bin, &default_rule(ScheduleFrequency::Week)).unwrap();
        fs::write(&plist_file, xml).unwrap();

        let config = Config {
            schedule: Some(ScheduleConfig {
                every: ScheduleFrequency::Week,
                categories: vec!["dev".to_string(), "junk".to_string()],
                min_reclaim: 1_000_000_000,
                last_skip: None,
                ..Default::default()
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
                ..Default::default()
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
                ..Default::default()
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
                ..Default::default()
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
            generate_plist(&bin, &default_rule(ScheduleFrequency::Week)).unwrap(),
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
            &default_rule(ScheduleFrequency::Week),
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
            &default_rule(ScheduleFrequency::Week),
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
            &default_rule(ScheduleFrequency::Week),
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
            &default_rule(ScheduleFrequency::Week),
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

    fn history_with_items(layout: &Layout, at: &str, items: &[(&str, u64, &str, Option<&str>)]) {
        let mut lines = vec![format!(
            r#"{{"event":"start","run":"r1","at":"{at}","command":"schedule run","mode":"schedule","version":"0"}}"#
        )];
        for (path, bytes, outcome, error) in items {
            let error = error.map_or("null".to_string(), |e| format!("\"{e}\""));
            lines.push(format!(
                r#"{{"event":"item","run":"r1","path":"{path}","bytes":{bytes},"outcome":"{outcome}","error":{error}}}"#
            ));
        }
        lines.push(format!(r#"{{"event":"end","run":"r1","at":"{at}"}}"#));
        fs::create_dir_all(layout.history_path().parent().unwrap()).unwrap();
        fs::write(layout.history_path(), lines.join("\n") + "\n").unwrap();
    }

    fn loaded_plist(layout: &Layout, bin: &Path) -> FakeHost {
        let plist_file = agent_plist_path(layout);
        fs::create_dir_all(plist_file.parent().unwrap()).unwrap();
        fs::write(
            &plist_file,
            generate_plist(bin, &default_rule(ScheduleFrequency::Week)).unwrap(),
        )
        .unwrap();
        let host = FakeHost::default();
        let uid = unsafe { libc::getuid() };
        host.set_uid(uid);
        host.reply(
            &format!("/bin/launchctl print gui/{uid}/{SCHEDULE_LABEL}"),
            "state = waiting",
        );
        host
    }

    fn status_of(host: &FakeHost, layout: &Layout, config: &Config) -> ScheduleStatus {
        let history = HistoryLog::new(layout.history_path());
        schedule_status(host, layout, config, &history)
    }

    #[test]
    fn status_before_any_run_is_never() {
        let (temp, layout) = sandbox();
        let bin = temp.path().join("taert");
        fs::write(&bin, b"bin").unwrap();
        let host = loaded_plist(&layout, &bin);

        let status = status_of(&host, &layout, &week_config());

        assert_eq!(
            status.settings,
            ScheduleSettings::On {
                every: ScheduleFrequency::Week,
                categories: vec!["dev".into(), "junk".into()],
                min_reclaim: 1_000_000_000,
            }
        );
        assert_eq!(status.last_run, LastRun::Never);
        assert!(!status.plist_missing);
        assert_eq!(status.missing_program, None);
        assert_eq!(status.agent_loaded, Some(true));
        assert_eq!(status.needs_full_disk_access, None);
    }

    #[test]
    fn status_adds_up_what_the_last_run_freed() {
        let (temp, layout) = sandbox();
        let bin = temp.path().join("taert");
        fs::write(&bin, b"bin").unwrap();
        let host = loaded_plist(&layout, &bin);
        history_with_items(
            &layout,
            "2026-01-01T00:00:00Z",
            &[
                ("/a", 1000, "removed", None),
                ("/b", 500, "trashed", None),
                ("/c", 70, "failed", Some("busy")),
            ],
        );

        let status = status_of(&host, &layout, &week_config());

        assert_eq!(
            status.last_run,
            LastRun::Ran {
                at: "2026-01-01T00:00:00Z".into(),
                freed: 1500
            }
        );
    }

    #[test]
    fn status_reports_a_skip_newer_than_the_last_run() {
        let (temp, layout) = sandbox();
        let bin = temp.path().join("taert");
        fs::write(&bin, b"bin").unwrap();
        let host = loaded_plist(&layout, &bin);
        history_with_a_run_at(&layout, "2026-01-01T00:00:00Z");
        let mut config = week_config();
        config.schedule.as_mut().unwrap().last_skip = Some(ScheduleSkip {
            at: "2026-02-01T00:00:00Z".into(),
            bytes: 2048,
        });

        let status = status_of(&host, &layout, &config);

        assert_eq!(
            status.last_run,
            LastRun::Skipped {
                at: "2026-02-01T00:00:00Z".into(),
                found: 2048,
                threshold: 1_000_000_000
            }
        );
    }

    #[test]
    fn status_keeps_a_run_newer_than_the_skip() {
        let (temp, layout) = sandbox();
        let bin = temp.path().join("taert");
        fs::write(&bin, b"bin").unwrap();
        let host = loaded_plist(&layout, &bin);
        history_with_a_run_at(&layout, "2026-03-01T00:00:00Z");
        let mut config = week_config();
        config.schedule.as_mut().unwrap().last_skip = Some(ScheduleSkip {
            at: "2026-02-01T00:00:00Z".into(),
            bytes: 2048,
        });

        let status = status_of(&host, &layout, &config);

        assert!(matches!(status.last_run, LastRun::Ran { .. }));
    }

    #[test]
    fn status_with_no_plist_does_not_ask_launchd() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();

        let status = status_of(&host, &layout, &week_config());

        assert!(status.plist_missing);
        assert_eq!(status.agent_loaded, None);
        assert_eq!(status.missing_program, None);
        assert!(!host.ran("/bin/launchctl"));
    }

    #[test]
    fn status_says_launchd_has_not_loaded_the_agent() {
        let (temp, layout) = sandbox();
        let bin = temp.path().join("taert");
        fs::write(&bin, b"bin").unwrap();
        loaded_plist(&layout, &bin);
        let host = FakeHost::default();
        host.fail("/bin/launchctl print", "Could not find service");

        let status = status_of(&host, &layout, &week_config());

        assert!(!status.plist_missing);
        assert_eq!(status.agent_loaded, Some(false));
    }

    #[test]
    fn status_names_a_program_that_is_missing() {
        let (temp, layout) = sandbox();
        let bin = temp.path().join("gone");
        let host = loaded_plist(&layout, &bin);

        let status = status_of(&host, &layout, &week_config());

        assert_eq!(status.missing_program, Some(bin));
    }

    #[test]
    fn status_names_the_program_that_needs_full_disk_access() {
        let (temp, layout) = sandbox();
        let bin = temp.path().join("taert");
        fs::write(&bin, b"bin").unwrap();
        let host = loaded_plist(&layout, &bin);
        history_with_items(
            &layout,
            "2026-01-01T00:00:00Z",
            &[(
                "/a",
                10,
                "failed",
                Some("Operation not permitted (os error 1)"),
            )],
        );

        let status = status_of(&host, &layout, &week_config());

        assert_eq!(status.needs_full_disk_access, Some(bin));
    }

    #[test]
    fn status_of_an_off_schedule_says_whether_a_plist_is_left() {
        let (temp, layout) = sandbox();
        let host = FakeHost::default();
        let off = status_of(&host, &layout, &Config::default());
        assert_eq!(
            off.settings,
            ScheduleSettings::Off {
                plist_exists: false
            }
        );

        let bin = temp.path().join("taert");
        loaded_plist(&layout, &bin);
        let left = status_of(&host, &layout, &Config::default());
        assert_eq!(left.settings, ScheduleSettings::Off { plist_exists: true });
    }

    #[test]
    fn the_text_of_an_off_schedule_is_unchanged() {
        let (temp, layout) = sandbox();
        let host = FakeHost::default();
        let status = status_of(&host, &layout, &Config::default());
        assert_eq!(
            format_schedule_status(&status),
            "Scheduled cleanup is off.\n"
        );

        loaded_plist(&layout, &temp.path().join("taert"));
        let status = status_of(&host, &layout, &Config::default());
        assert_eq!(
            format_schedule_status(&status),
            "Scheduled cleanup is off (config disabled, but LaunchAgent plist exists).\n"
        );
    }

    #[test]
    fn the_text_of_a_run_that_freed_bytes_is_unchanged() {
        let (temp, layout) = sandbox();
        let bin = temp.path().join("taert");
        fs::write(&bin, b"bin").unwrap();
        let host = loaded_plist(&layout, &bin);
        history_with_items(
            &layout,
            "2026-01-01T00:00:00Z",
            &[("/a", 1_500_000, "removed", None)],
        );

        let text = format_schedule_status(&status_of(&host, &layout, &week_config()));

        assert!(
            text.starts_with("Every week: dev, junk, when at least 1 GB can be freed.\nLast run: "),
            "{text}"
        );
        assert!(text.ends_with(", freed 1.5 MB.\n"), "{text}");
    }

    #[test]
    fn the_text_of_a_run_that_never_happened_is_unchanged() {
        let (temp, layout) = sandbox();
        let bin = temp.path().join("taert");
        fs::write(&bin, b"bin").unwrap();
        let host = loaded_plist(&layout, &bin);

        let text = format_schedule_status(&status_of(&host, &layout, &week_config()));

        assert_eq!(
            text,
            "Every week: dev, junk, when at least 1 GB can be freed.\nLast run: never.\n"
        );
    }

    #[test]
    fn the_text_of_a_skip_is_unchanged() {
        let (temp, layout) = sandbox();
        let bin = temp.path().join("taert");
        fs::write(&bin, b"bin").unwrap();
        let host = loaded_plist(&layout, &bin);
        let mut config = week_config();
        config.schedule.as_mut().unwrap().last_skip = Some(ScheduleSkip {
            at: "2026-02-01T00:00:00Z".into(),
            bytes: 2048,
        });

        let text = format_schedule_status(&status_of(&host, &layout, &config));

        let line = format!(
            "Last run: skipped on {} (2.0 KB found, below 1 GB threshold).\n",
            format_run_time("2026-02-01T00:00:00Z")
        );
        assert_eq!(
            text,
            format!("Every week: dev, junk, when at least 1 GB can be freed.\n{line}")
        );
    }

    #[test]
    fn the_text_of_a_permission_failure_names_the_program_of_the_plist() {
        let (temp, layout) = sandbox();
        let bin = temp.path().join("taert");
        fs::write(&bin, b"bin").unwrap();
        let host = loaded_plist(&layout, &bin);
        history_with_items(
            &layout,
            "2026-01-01T00:00:00Z",
            &[(
                "/a",
                10,
                "failed",
                Some("Operation not permitted (os error 1)"),
            )],
        );

        let text = format_schedule_status(&status_of(&host, &layout, &week_config()));

        let block = format!(
            "\nScheduled cleanup failed to remove some files due to missing permissions.\nTo grant taert Full Disk Access:\n  1. Open System Settings > Privacy & Security > Full Disk Access\n  2. Add and enable: {}\n",
            bin.display()
        );
        assert!(text.ends_with(&block), "{text}");
    }

    fn config_with(
        every: ScheduleFrequency,
        at: &str,
        days: &[&str],
        day: u32,
        every_hours: Option<u32>,
    ) -> ScheduleConfig {
        ScheduleConfig {
            every,
            at: at.to_string(),
            days: days.iter().map(|d| d.to_string()).collect(),
            day,
            every_hours,
            ..ScheduleConfig::default()
        }
    }

    fn rule_of(
        every: ScheduleFrequency,
        at: &str,
        days: &[&str],
        day: u32,
        every_hours: Option<u32>,
    ) -> ScheduleRule {
        ScheduleRule::from_config(&config_with(every, at, days, day, every_hours)).unwrap()
    }

    fn default_rule(every: ScheduleFrequency) -> ScheduleRule {
        ScheduleRule::for_frequency(None, every).unwrap()
    }

    fn local(year: i32, month: i32, day: i32, hour: i32, minute: i32) -> SystemTime {
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        tm.tm_year = year - 1900;
        tm.tm_mon = month - 1;
        tm.tm_mday = day;
        tm.tm_hour = hour;
        tm.tm_min = minute;
        tm.tm_isdst = -1;
        let secs = unsafe { libc::mktime(&mut tm) };
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs as u64)
    }

    #[test]
    fn a_config_without_the_new_fields_is_todays_schedule() {
        let config: ScheduleConfig = toml::from_str("every = \"week\"").unwrap();
        assert_eq!(config.at, "09:00");
        assert_eq!(config.days, vec!["mon".to_string()]);
        assert_eq!(config.day, 1);
        assert_eq!(config.every_hours, None);

        let bin = Path::new("/opt/homebrew/bin/taert");
        for (freq, example) in [
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
        ] {
            let config: ScheduleConfig = toml::from_str(&format!("every = \"{freq}\"")).unwrap();
            let rule = ScheduleRule::from_config(&config).unwrap();
            assert_eq!(generate_plist(bin, &rule).unwrap(), example, "{freq}");
        }
    }

    #[test]
    fn the_unused_fields_of_the_config_are_saved_and_a_missing_repeat_is_left_out() {
        let config = config_with(ScheduleFrequency::Day, "21:30", &["mon", "thu"], 15, None);
        let text = toml::to_string_pretty(&config).unwrap();
        assert!(text.contains("at = \"21:30\""), "{text}");
        let reread: ScheduleConfig = toml::from_str(&text).unwrap();
        assert_eq!(reread.days, ["mon", "thu"]);
        assert!(text.contains("day = 15"), "{text}");
        assert!(!text.contains("every_hours"), "{text}");
    }

    #[test]
    fn a_bad_value_is_refused_with_its_field_and_value() {
        let day = ScheduleFrequency::Day;
        let cases = [
            (
                config_with(day, "25:00", &["mon"], 1, None),
                "Time 25:00 is not HH:MM between 00:00 and 23:59.",
            ),
            (
                config_with(day, "9:5", &["mon"], 1, None),
                "Time 9:5 is not HH:MM between 00:00 and 23:59.",
            ),
            (
                config_with(day, "09:60", &["mon"], 1, None),
                "Time 09:60 is not HH:MM between 00:00 and 23:59.",
            ),
            (
                config_with(day, "09:00", &[], 1, None),
                "Weekdays are empty. Use mon, tue, wed, thu, fri, sat, sun.",
            ),
            (
                config_with(day, "09:00", &["mon", "fri2"], 1, None),
                "Weekday fri2 is not one of mon, tue, wed, thu, fri, sat, sun.",
            ),
            (
                config_with(day, "09:00", &["mon"], 0, None),
                "Day 0 is not between 1 and 28.",
            ),
            (
                config_with(day, "09:00", &["mon"], 29, None),
                "Day 29 is not between 1 and 28.",
            ),
            (
                config_with(day, "09:00", &["mon"], 31, None),
                "Day 31 is not between 1 and 28.",
            ),
            (
                config_with(day, "09:00", &["mon"], 1, Some(5)),
                "Repeat every 5 hours is not one of 2, 3, 4, 6, 8, 12.",
            ),
        ];
        for (config, message) in cases {
            assert_eq!(ScheduleRule::from_config(&config).unwrap_err(), message);
        }
    }

    #[test]
    fn times_of_day_run_around_the_clock_from_the_first_time() {
        let day = ScheduleFrequency::Day;
        let times = |at, hours| {
            rule_of(day, at, &["mon"], 1, hours)
                .times_of_day()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        };
        assert_eq!(times("09:00", None), ["09:00"]);
        assert_eq!(
            times("03:30", Some(6)),
            ["03:30", "09:30", "15:30", "21:30"]
        );
        assert_eq!(times("23:00", Some(12)), ["11:00", "23:00"]);
        assert_eq!(times("00:15", Some(2)).len(), 12);
    }

    #[test]
    fn describe_names_the_frequency_the_days_and_the_repeat() {
        let d =
            |every, at, days: &[&str], day, hours| rule_of(every, at, days, day, hours).describe();
        use ScheduleFrequency::{Day, Month, Week};
        assert_eq!(d(Day, "09:00", &["mon"], 1, None), "Every day at 09:00");
        assert_eq!(
            d(Week, "21:30", &["thu", "mon"], 1, None),
            "Every week on Mon, Thu at 21:30"
        );
        assert_eq!(
            d(Month, "09:00", &["mon"], 15, None),
            "Every month on day 15 at 09:00"
        );
        assert_eq!(
            d(Day, "03:30", &["mon"], 1, Some(6)),
            "Every day, every 6 hours from 03:30"
        );
        assert_eq!(
            d(Week, "03:30", &["mon", "thu"], 1, Some(6)),
            "Every week on Mon, Thu, every 6 hours from 03:30"
        );
        assert_eq!(
            d(Month, "03:30", &["mon"], 15, Some(12)),
            "Every month on day 15, every 12 hours from 03:30"
        );
        assert_eq!(
            d(Day, "21:30", &["mon"], 1, Some(6)),
            "Every day, every 6 hours from 03:30"
        );
        assert_eq!(
            d(Day, "23:00", &["mon"], 1, Some(12)),
            "Every day, every 12 hours from 11:00"
        );
    }

    #[test]
    fn each_plist_matches_its_stored_example() {
        let bin = Path::new("/opt/homebrew/bin/taert");
        use ScheduleFrequency::{Day, Month, Week};
        let examples = [
            (
                default_rule(Day),
                include_str!("../../tests/fixtures/schedule_day.plist"),
            ),
            (
                default_rule(Week),
                include_str!("../../tests/fixtures/schedule_week.plist"),
            ),
            (
                default_rule(Month),
                include_str!("../../tests/fixtures/schedule_month.plist"),
            ),
            (
                rule_of(Week, "21:30", &["mon", "thu"], 1, None),
                include_str!("../../tests/fixtures/schedule_week_two_days.plist"),
            ),
            (
                rule_of(Month, "09:00", &["mon"], 15, None),
                include_str!("../../tests/fixtures/schedule_month_day15.plist"),
            ),
            (
                rule_of(Day, "03:30", &["mon"], 1, Some(6)),
                include_str!("../../tests/fixtures/schedule_day_every_6_hours.plist"),
            ),
        ];
        for (rule, example) in examples {
            assert_eq!(
                generate_plist(bin, &rule).unwrap(),
                example,
                "{}",
                rule.describe()
            );
        }
    }

    #[test]
    fn sunday_is_weekday_zero_and_comes_last() {
        let rule = rule_of(ScheduleFrequency::Week, "09:00", &["sun", "sat"], 1, None);
        let xml = generate_plist(Path::new("/bin/taert"), &rule).unwrap();
        let weekdays: Vec<&str> = xml
            .split("<key>Weekday</key>")
            .skip(1)
            .map(|part| part.split("</integer>").next().unwrap().trim())
            .collect();
        assert_eq!(weekdays, ["<integer>6", "<integer>0"]);
    }

    #[test]
    fn a_weekday_named_twice_runs_once() {
        let rule = rule_of(ScheduleFrequency::Week, "09:00", &["mon", "mon"], 1, None);
        let xml = generate_plist(Path::new("/bin/taert"), &rule).unwrap();
        assert_eq!(xml.matches("<key>Weekday</key>").count(), 1);
        assert_eq!(
            rule,
            rule_of(ScheduleFrequency::Week, "09:00", &["mon"], 1, None)
        );
    }

    #[test]
    fn a_saved_rule_keeps_the_fields_the_frequency_does_not_use() {
        let saved = config_with(
            ScheduleFrequency::Week,
            "21:30",
            &["mon", "thu"],
            15,
            Some(6),
        );
        let rule = ScheduleRule::for_frequency(Some(&saved), ScheduleFrequency::Month).unwrap();
        assert_eq!(
            rule.describe(),
            "Every month on day 15, every 6 hours from 03:30"
        );
        let broken = config_with(ScheduleFrequency::Week, "25:00", &["mon"], 1, None);
        assert_eq!(
            ScheduleRule::for_frequency(Some(&broken), ScheduleFrequency::Day).unwrap_err(),
            "Time 25:00 is not HH:MM between 00:00 and 23:59."
        );
    }

    #[test]
    fn set_saves_the_four_values_of_the_rule() {
        let (temp, layout) = sandbox();
        let host = FakeHost::default();
        host.reply("/bin/launchctl", "");
        let mut config = Config::default();
        let rule = rule_of(
            ScheduleFrequency::Week,
            "21:30",
            &["thu", "mon"],
            15,
            Some(3),
        );
        let bin = temp.path().join("taert");
        set_schedule(
            &host,
            &layout,
            &mut config,
            &rule,
            vec!["dev".to_string()],
            1,
            Some(&bin),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(agent_plist_path(&layout)).unwrap(),
            generate_plist(&bin, &rule).unwrap()
        );
        let on_disk = Config::load_from(&layout.config_path())
            .unwrap()
            .schedule
            .unwrap();
        assert_eq!(on_disk.at, "21:30");
        assert_eq!(on_disk.days, ["mon", "thu"]);
        assert_eq!(on_disk.day, 15);
        assert_eq!(on_disk.every_hours, Some(3));
        let saved = config.schedule.unwrap();
        assert_eq!(saved.every, ScheduleFrequency::Week);
        assert_eq!(saved.at, "21:30");
        assert_eq!(saved.days, ["mon", "thu"]);
        assert_eq!(saved.day, 15);
        assert_eq!(saved.every_hours, Some(3));
        assert_eq!(ScheduleRule::from_config(&saved).unwrap(), rule);
    }

    #[test]
    fn next_run_of_a_day_rule_crosses_the_day_the_month_and_the_year() {
        let rule = rule_of(ScheduleFrequency::Day, "09:00", &["mon"], 1, None);
        assert_eq!(
            rule.next_run(local(2026, 11, 6, 8, 0)),
            local(2026, 11, 6, 9, 0)
        );
        assert_eq!(
            rule.next_run(local(2026, 11, 6, 9, 0)),
            local(2026, 11, 7, 9, 0)
        );
        assert_eq!(
            rule.next_run(local(2026, 11, 30, 10, 0)),
            local(2026, 12, 1, 9, 0)
        );
        assert_eq!(
            rule.next_run(local(2026, 12, 31, 23, 59)),
            local(2027, 1, 1, 9, 0)
        );
    }

    #[test]
    fn next_run_of_a_week_rule_crosses_the_end_of_the_week() {
        // 2026-11-05 is a Thursday.
        let rule = rule_of(ScheduleFrequency::Week, "21:30", &["mon", "thu"], 1, None);
        assert_eq!(
            rule.next_run(local(2026, 11, 5, 20, 0)),
            local(2026, 11, 5, 21, 30)
        );
        assert_eq!(
            rule.next_run(local(2026, 11, 5, 22, 0)),
            local(2026, 11, 9, 21, 30)
        );
        assert_eq!(
            rule.next_run(local(2026, 11, 8, 23, 0)),
            local(2026, 11, 9, 21, 30)
        );
        assert_eq!(
            rule.next_run(local(2026, 11, 9, 22, 0)),
            local(2026, 11, 12, 21, 30)
        );
        let sunday = rule_of(ScheduleFrequency::Week, "09:00", &["sun"], 1, None);
        assert_eq!(
            sunday.next_run(local(2026, 11, 7, 12, 0)),
            local(2026, 11, 8, 9, 0)
        );
        assert_eq!(
            sunday.next_run(local(2026, 11, 8, 9, 0)),
            local(2026, 11, 15, 9, 0)
        );
    }

    #[test]
    fn next_run_of_a_month_rule_crosses_the_month_and_the_year() {
        let rule = rule_of(ScheduleFrequency::Month, "09:00", &["mon"], 15, None);
        assert_eq!(
            rule.next_run(local(2026, 11, 15, 8, 0)),
            local(2026, 11, 15, 9, 0)
        );
        assert_eq!(
            rule.next_run(local(2026, 11, 15, 10, 0)),
            local(2026, 12, 15, 9, 0)
        );
        assert_eq!(
            rule.next_run(local(2026, 12, 20, 10, 0)),
            local(2027, 1, 15, 9, 0)
        );
        let last = rule_of(ScheduleFrequency::Month, "09:00", &["mon"], 28, None);
        assert_eq!(
            last.next_run(local(2027, 2, 28, 10, 0)),
            local(2027, 3, 28, 9, 0)
        );
    }

    #[test]
    fn next_run_of_a_repeating_rule_goes_to_the_next_time_of_the_day() {
        let rule = rule_of(ScheduleFrequency::Day, "03:30", &["mon"], 1, Some(6));
        assert_eq!(
            rule.next_run(local(2026, 11, 6, 10, 0)),
            local(2026, 11, 6, 15, 30)
        );
        assert_eq!(
            rule.next_run(local(2026, 11, 6, 22, 0)),
            local(2026, 11, 7, 3, 30)
        );
        let week = rule_of(ScheduleFrequency::Week, "21:30", &["fri"], 1, Some(12));
        assert_eq!(
            week.next_run(local(2026, 11, 6, 10, 0)),
            local(2026, 11, 6, 21, 30)
        );
        assert_eq!(
            week.next_run(local(2026, 11, 6, 22, 0)),
            local(2026, 11, 13, 9, 30)
        );
    }
}

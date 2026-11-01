//! System diagnosis engine ("taert doctor").
//! Explains what is slowing the Mac down, heating it up, or draining battery.
//! Factual, non-alarmist diagnostics with evidence and single actionable suggestions.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::host::Host;
use super::layout::Layout;
use super::profiler::{DetailedMemoryStatus, MemoryPressureLevel, SystemProfiler};
use super::startup::{self, StartupItem, StartupState};
use super::stores::IgnoreStore;
use super::sysinfo::{DiskUsage, SysInfo};
use super::telemetry::disk_severity;
use crate::core::bytes::format_bytes;

// Threshold constants
pub const CPU_THRESHOLD_PERCENT: f64 = 80.0;
pub const ENERGY_LOW_CPU_THRESHOLD: f64 = 20.0;
pub const ENERGY_HIGH_WAKEUPS_THRESHOLD: u64 = 500;
pub const ENERGY_HIGH_POWER_THRESHOLD: f64 = 20.0;
pub const SLEEP_BLOCKER_DURATION_THRESHOLD: Duration = Duration::from_secs(600); // 10 minutes
pub const WAKE_COUNT_THRESHOLD: usize = 10;
pub const NETWORK_RATE_THRESHOLD_BYTES_PER_SEC: u64 = 1_000_000; // 1 MB/s
pub const DEFAULT_DOCTOR_SECONDS: u64 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingSeverity {
    Critical,
    Warning,
    Notice,
    Info,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FindingAction {
    AskAppToQuit {
        name: String,
        bundle_id: Option<String>,
        pid: Option<u32>,
    },
    KillProcess {
        pid: u32,
        name: String,
    },
    DisableStartupItem {
        item: StartupItem,
    },
    ExploreStorage,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub id: String,
    pub severity: FindingSeverity,
    pub title: String,
    pub evidence: String,
    pub action: String,
    #[serde(skip)]
    pub action_target: Option<FindingAction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResult {
    pub id: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl CheckResult {
    pub fn ok(id: &str) -> Self {
        Self {
            id: id.to_string(),
            status: "ok".to_string(),
            reason: None,
        }
    }

    pub fn skipped(id: &str, reason: impl Into<String>) -> Self {
        Self {
            id: id.to_string(),
            status: "skipped".to_string(),
            reason: Some(reason.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DoctorReport {
    pub seconds: u64,
    pub findings: Vec<Finding>,
    pub checks: Vec<CheckResult>,
}

#[derive(Debug, Clone, Default)]
pub struct TopProcess {
    pub pid: u32,
    pub command: String,
    pub cpu: f64,
    pub power: f64,
    pub idlew: u64,
}

#[derive(Debug, Clone, Default)]
pub struct TopSample {
    pub processes: Vec<TopProcess>,
    pub swapins: u64,
    pub swapouts: u64,
}

#[derive(Debug, Clone, Default)]
pub struct TopDiagnostics {
    pub samples: Vec<TopSample>,
}

#[derive(Debug, Clone)]
pub struct SleepBlocker {
    pub pid: u32,
    pub command: String,
    pub duration_secs: u64,
    pub duration_display: String,
    pub assertion_type: String,
    pub name: String,
}

#[derive(Debug, Clone, Default)]
pub struct WakeDiagnostics {
    pub total_wakes_24h: usize,
    pub reasons_count: HashMap<String, usize>,
    pub wake_requests_by_process: HashMap<String, usize>,
}

#[derive(Debug, Clone, Default)]
pub struct ThermalDiagnostics {
    pub warning_recorded: bool,
    pub details: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct NetworkProcessDelta {
    pub pid: u32,
    pub command: String,
    pub delta_in: u64,
    pub delta_out: u64,
    pub rate_bytes_per_sec: u64,
}

#[derive(Debug, Clone)]
pub struct DoctorSnapshot {
    pub seconds: u64,
    pub top: Result<TopDiagnostics, String>,
    pub assertions: Result<Vec<SleepBlocker>, String>,
    pub wake: Result<WakeDiagnostics, String>,
    pub therm: Result<ThermalDiagnostics, String>,
    pub network: Result<Vec<NetworkProcessDelta>, String>,
    pub startup_items: Result<Vec<StartupItem>, String>,
    pub memory_pressure: Option<MemoryPressureLevel>,
    pub detailed_memory: Option<DetailedMemoryStatus>,
    pub disk_usage: Option<DiskUsage>,
    pub largest_memory_processes: Vec<(u32, String, u64)>, // (pid, name, bytes)
}

impl Default for DoctorSnapshot {
    fn default() -> Self {
        Self {
            seconds: DEFAULT_DOCTOR_SECONDS,
            top: Ok(TopDiagnostics::default()),
            assertions: Ok(Vec::new()),
            wake: Ok(WakeDiagnostics::default()),
            therm: Ok(ThermalDiagnostics::default()),
            network: Ok(Vec::new()),
            startup_items: Ok(Vec::new()),
            memory_pressure: None,
            detailed_memory: None,
            disk_usage: None,
            largest_memory_processes: Vec::new(),
        }
    }
}

/// Explains known macOS background services instead of blaming them.
pub fn explain_system_process(name: &str) -> Option<(&'static str, &'static str, &'static str)> {
    match name {
        "mds" | "mds_stores" | "mdworker" | "mdworker_shared" => Some((
            "Spotlight is indexing files",
            "Spotlight search index is updating in the background.",
            "Indexing will finish automatically.",
        )),
        "backupd" => Some((
            "Time Machine is backing up files",
            "Time Machine backup is running in the background.",
            "Backup will finish automatically.",
        )),
        "photoanalysisd" | "mediaanalysisd" => Some((
            "Photos is analyzing your library",
            "macOS is scanning photos and videos for faces, scenes, and search index.",
            "Analysis pauses automatically when you use the Mac heavily.",
        )),
        "softwareupdated" => Some((
            "macOS is updating software",
            "Software Update is downloading or preparing updates in the background.",
            "Updates will finish automatically.",
        )),
        "kernel_task" => Some((
            "macOS is cooling the CPU",
            "kernel_task throttles CPU cycles to prevent overheating when temperatures rise.",
            "Ensure vents are unobstructed and let the Mac cool down.",
        )),
        "WindowServer" => Some((
            "WindowServer is processing screen updates",
            "The macOS display compositor is processing graphics and window animations.",
            "Close graphics-heavy windows or external displays to reduce load.",
        )),
        _ => None,
    }
}

/// Parses output of `top -l <n> -s 2 -n 30 -o power -stats pid,command,cpu,power,idlew`
pub fn parse_top_output(stdout: &str) -> Result<TopDiagnostics, String> {
    if stdout.trim().is_empty() {
        return Err("empty top output".to_string());
    }

    let mut samples = Vec::new();
    let mut current_sample = TopSample::default();
    let mut in_table = false;
    let mut seen_first_process = false;

    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Processes:") {
            if seen_first_process {
                samples.push(current_sample);
                current_sample = TopSample::default();
                in_table = false;
            }
            seen_first_process = true;
            continue;
        }

        if trimmed.starts_with("VM:") {
            // e.g. "VM: 419T vsize, 57G framework vsize, 0(0) swapins, 0(0) swapouts."
            if let Some(swapins_pos) = trimmed.find("swapins") {
                let prefix = &trimmed[..swapins_pos];
                if let Some(last_space) = prefix.rfind(' ') {
                    let num_str = prefix[last_space..].trim();
                    if let Some(paren) = num_str.find('(') {
                        current_sample.swapins = num_str[..paren].parse().unwrap_or(0);
                    } else {
                        current_sample.swapins = num_str.parse().unwrap_or(0);
                    }
                }
            }
            if let Some(swapouts_pos) = trimmed.find("swapouts") {
                let prefix = &trimmed[..swapouts_pos];
                if let Some(last_space) = prefix.rfind(' ') {
                    let num_str = prefix[last_space..].trim();
                    if let Some(paren) = num_str.find('(') {
                        current_sample.swapouts = num_str[..paren].parse().unwrap_or(0);
                    } else {
                        current_sample.swapouts = num_str.parse().unwrap_or(0);
                    }
                }
            }
            continue;
        }

        if trimmed.starts_with("PID") && trimmed.contains("COMMAND") && trimmed.contains("%CPU") {
            in_table = true;
            continue;
        }

        if in_table {
            if trimmed.is_empty() {
                continue;
            }
            let tokens: Vec<&str> = trimmed.split_whitespace().collect();
            if tokens.len() >= 5
                && let Ok(pid) = tokens[0].parse::<u32>()
            {
                let len = tokens.len();
                let idlew = tokens[len - 1].parse::<u64>().unwrap_or(0);
                let power = tokens[len - 2].parse::<f64>().unwrap_or(0.0);
                let cpu_str = tokens[len - 3].trim_end_matches('%');
                let cpu = cpu_str.parse::<f64>().unwrap_or(0.0);
                let command = tokens[1..len - 3].join(" ");

                current_sample.processes.push(TopProcess {
                    pid,
                    command,
                    cpu,
                    power,
                    idlew,
                });
            }
        }
    }

    if seen_first_process {
        samples.push(current_sample);
    }

    if samples.is_empty() {
        return Err("no samples found in top output".to_string());
    }

    Ok(TopDiagnostics { samples })
}

/// Parses output of `pmset -g assertions`.
pub fn parse_pmset_assertions(stdout: &str) -> Vec<SleepBlocker> {
    let mut blockers = Vec::new();
    let mut in_owning_process = false;

    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Listed by owning process:") {
            in_owning_process = true;
            continue;
        }
        if trimmed.starts_with("Kernel Assertions:") {
            in_owning_process = false;
            continue;
        }

        if !in_owning_process || !trimmed.starts_with("pid ") {
            continue;
        }

        // Expected format:
        // pid 70796(caffeinate): [0x0001da9500059496] 00:00:01 PreventUserIdleDisplaySleep named: "caffeinate command-line tool"
        let Some(open_paren) = trimmed.find('(') else {
            continue;
        };
        let Some(close_paren) = trimmed.find("): [") else {
            continue;
        };
        let pid_str = &trimmed[4..open_paren].trim();
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };
        let command = trimmed[open_paren + 1..close_paren].trim().to_string();

        if command == "powerd" {
            // Ignore powerd's own assertions
            continue;
        }

        let after_bracket_pos = close_paren + 4;
        let Some(closing_bracket) = trimmed[after_bracket_pos..].find(']') else {
            continue;
        };
        let after_closing_bracket = &trimmed[after_bracket_pos + closing_bracket + 1..].trim();

        // Next token is duration: HH:MM:SS
        let tokens: Vec<&str> = after_closing_bracket.split_whitespace().collect();
        if tokens.len() < 2 {
            continue;
        }
        let duration_str = tokens[0];
        let assertion_type = tokens[1];

        // Filter assertion types
        if assertion_type == "UserIsActive" {
            continue;
        }
        if !matches!(
            assertion_type,
            "PreventUserIdleSystemSleep" | "PreventUserIdleDisplaySleep" | "PreventSystemSleep"
        ) {
            continue;
        }

        let mut duration_secs = 0u64;
        let time_parts: Vec<&str> = duration_str.split(':').collect();
        if time_parts.len() == 3 {
            let h = time_parts[0].parse::<u64>().unwrap_or(0);
            let m = time_parts[1].parse::<u64>().unwrap_or(0);
            let s = time_parts[2].parse::<u64>().unwrap_or(0);
            duration_secs = h * 3600 + m * 60 + s;
        }

        let name = if let Some(named_idx) = trimmed.find("named: \"") {
            let rest = &trimmed[named_idx + 8..];
            if let Some(quote_end) = rest.find('"') {
                rest[..quote_end].to_string()
            } else {
                rest.to_string()
            }
        } else {
            String::new()
        };

        // Ignore display sleep prevention from powerd
        if name.contains("Prevent sleep while display is on") {
            continue;
        }

        // Only blockers held for over 10 minutes (600 seconds)
        if duration_secs > SLEEP_BLOCKER_DURATION_THRESHOLD.as_secs() {
            blockers.push(SleepBlocker {
                pid,
                command,
                duration_secs,
                duration_display: duration_str.to_string(),
                assertion_type: assertion_type.to_string(),
                name,
            });
        }
    }

    blockers
}

/// Parses timestamps like "2026-10-26 09:11:16" to epoch seconds.
fn parse_log_timestamp(ts_str: &str) -> Option<u64> {
    if ts_str.len() < 19 {
        return None;
    }
    let year = ts_str[0..4].parse::<i32>().ok()?;
    let month = ts_str[5..7].parse::<i32>().ok()?;
    let day = ts_str[8..10].parse::<i32>().ok()?;
    let hour = ts_str[11..13].parse::<i32>().ok()?;
    let min = ts_str[14..16].parse::<i32>().ok()?;
    let sec = ts_str[17..19].parse::<i32>().ok()?;

    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = year - 1900;
    tm.tm_mon = month - 1;
    tm.tm_mday = day;
    tm.tm_hour = hour;
    tm.tm_min = min;
    tm.tm_sec = sec;
    tm.tm_isdst = -1;

    let time = unsafe { libc::timegm(&mut tm) };
    if time >= 0 { Some(time as u64) } else { None }
}

/// Parses output of `pmset -g log` filtered to the last 24 hours.
pub fn parse_pmset_log(stdout: &str, reference_time_secs: u64) -> WakeDiagnostics {
    let mut diagnostics = WakeDiagnostics::default();
    let cutoff = reference_time_secs.saturating_sub(86400);

    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.len() < 19 {
            continue;
        }
        let Some(timestamp) = parse_log_timestamp(&trimmed[0..19]) else {
            continue;
        };
        if timestamp < cutoff {
            continue;
        }

        // Check for Wake / DarkWake
        if (trimmed.contains(" Wake ") || trimmed.contains("DarkWake"))
            && trimmed.contains(": due to ")
        {
            diagnostics.total_wakes_24h += 1;
            let after_due_to = trimmed.split(": due to ").nth(1).unwrap_or("");
            let reason_part = after_due_to
                .split('/')
                .next()
                .unwrap_or("")
                .split(" Using")
                .next()
                .unwrap_or("")
                .trim();

            // Extract primary reason identifier
            let normalized_reason = if reason_part.contains("wifibt") {
                "wifibt".to_string()
            } else if reason_part.contains("Notification") {
                "Notification".to_string()
            } else if reason_part.contains("lid") {
                "lid open".to_string()
            } else if reason_part.contains("rtc") {
                "RTC alarm".to_string()
            } else if reason_part.contains("USB") {
                "USB device".to_string()
            } else if reason_part.contains("UserActivity") {
                "User activity".to_string()
            } else {
                reason_part
                    .split_whitespace()
                    .next()
                    .unwrap_or("unknown")
                    .to_string()
            };

            *diagnostics
                .reasons_count
                .entry(normalized_reason)
                .or_insert(0) += 1;
        }

        // Check for Wake Requests
        if trimmed.contains("Wake Requests") {
            let mut search_from = 0;
            while let Some(proc_idx) = trimmed[search_from..].find("process=") {
                let abs_idx = search_from + proc_idx + 8;
                let end = trimmed[abs_idx..]
                    .find(|c: char| c.is_whitespace() || c == ']')
                    .unwrap_or(trimmed.len() - abs_idx);
                let proc_name = &trimmed[abs_idx..abs_idx + end];
                if !proc_name.is_empty() {
                    *diagnostics
                        .wake_requests_by_process
                        .entry(proc_name.to_string())
                        .or_insert(0) += 1;
                }
                search_from = abs_idx + end;
            }
        }
    }

    diagnostics
}

/// Parses output of `pmset -g therm`.
pub fn parse_pmset_therm(stdout: &str) -> ThermalDiagnostics {
    let mut diagnostics = ThermalDiagnostics::default();

    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("Note: No ") && trimmed.ends_with("has been recorded") {
            continue;
        }

        // Recorded thermal status or limit
        if trimmed.contains("CPU_Speed_Limit") {
            if let Some(val_str) = trimmed.split('=').nth(1)
                && let Ok(limit) = val_str.trim().parse::<u32>()
                && limit < 100
            {
                diagnostics.warning_recorded = true;
                diagnostics
                    .details
                    .push(format!("CPU speed limited to {limit}%"));
            }
        } else if trimmed.contains("Thermal_Warning_Level") {
            if let Some(val_str) = trimmed.split('=').nth(1) {
                let level = val_str.trim();
                if level != "0" && !level.eq_ignore_ascii_case("normal") {
                    diagnostics.warning_recorded = true;
                    diagnostics
                        .details
                        .push(format!("Thermal warning level: {level}"));
                }
            }
        } else if trimmed.contains("Performance_Warning_Level") {
            if let Some(val_str) = trimmed.split('=').nth(1) {
                let level = val_str.trim();
                if level != "0" && !level.eq_ignore_ascii_case("normal") {
                    diagnostics.warning_recorded = true;
                    diagnostics
                        .details
                        .push(format!("Performance warning level: {level}"));
                }
            }
        } else if !trimmed.starts_with("Note:") {
            diagnostics.warning_recorded = true;
            diagnostics.details.push(trimmed.to_string());
        }
    }

    diagnostics
}

/// Parses output of `nettop -P -L 2 -s 2 -x -J bytes_in,bytes_out`.
pub fn parse_nettop_output(
    stdout: &str,
    sample_seconds: u64,
) -> Result<Vec<NetworkProcessDelta>, String> {
    if stdout.trim().is_empty() {
        return Err("empty nettop output".to_string());
    }

    let interval = sample_seconds.max(1);
    let mut samples: Vec<HashMap<String, (u32, String, u64, u64)>> = Vec::new();
    let mut current_sample = HashMap::new();
    let mut seen_header = false;

    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with(",bytes_in,bytes_out") {
            if seen_header {
                samples.push(current_sample);
                current_sample = HashMap::new();
            }
            seen_header = true;
            continue;
        }

        if !seen_header || trimmed.is_empty() {
            continue;
        }

        // Row format: <command>.<pid>,<bytes_in>,<bytes_out>,
        let parts: Vec<&str> = trimmed.split(',').collect();
        if parts.len() >= 3 {
            let proc_part = parts[0];
            let bytes_in = parts[1].parse::<u64>().unwrap_or(0);
            let bytes_out = parts[2].parse::<u64>().unwrap_or(0);

            let (command, pid) = if let Some(last_dot) = proc_part.rfind('.') {
                let cmd = proc_part[..last_dot].to_string();
                let p = proc_part[last_dot + 1..].parse::<u32>().unwrap_or(0);
                (cmd, p)
            } else {
                (proc_part.to_string(), 0)
            };

            current_sample.insert(proc_part.to_string(), (pid, command, bytes_in, bytes_out));
        }
    }

    if seen_header {
        samples.push(current_sample);
    }

    if samples.len() < 2 {
        return Err("insufficient nettop samples (need 2)".to_string());
    }

    let sample1 = &samples[0];
    let sample2 = &samples[1];
    let mut deltas = Vec::new();

    for (key, &(pid, ref command, bytes_in_2, bytes_out_2)) in sample2 {
        if let Some(&(_, _, bytes_in_1, bytes_out_1)) = sample1.get(key) {
            // Confirm totals are cumulative
            if bytes_in_2 >= bytes_in_1 && bytes_out_2 >= bytes_out_1 {
                let delta_in = bytes_in_2 - bytes_in_1;
                let delta_out = bytes_out_2 - bytes_out_1;
                let total_delta = delta_in + delta_out;
                let rate = total_delta / interval;

                if rate >= NETWORK_RATE_THRESHOLD_BYTES_PER_SEC {
                    deltas.push(NetworkProcessDelta {
                        pid,
                        command: command.clone(),
                        delta_in,
                        delta_out,
                        rate_bytes_per_sec: rate,
                    });
                }
            }
        }
    }

    deltas.sort_by_key(|d| std::cmp::Reverse(d.rate_bytes_per_sec));
    Ok(deltas)
}

/// Collects raw system diagnostics with explicit timeouts.
pub fn collect(
    host: &dyn Host,
    seconds: u64,
    layout: &Layout,
    ignore: &IgnoreStore,
) -> DoctorSnapshot {
    let watch_seconds = seconds.max(2);
    // Samples for top: (seconds / 2) + 1, skipping sample 1
    let n_samples = (watch_seconds / 2) + 1;
    let n_samples_str = n_samples.to_string();

    let top_timeout = Duration::from_secs(watch_seconds + 5);
    let nettop_timeout = Duration::from_secs(6);
    let pmset_timeout = Duration::from_secs(3);

    // Run probes concurrently within a thread scope
    let (top_res, assertions_res, wake_res, therm_res, nettop_res, startup_res) =
        std::thread::scope(|s| {
            let top_handle = s.spawn(|| {
                let out = host.run(
                    "/usr/bin/top",
                    &[
                        "-l",
                        &n_samples_str,
                        "-s",
                        "2",
                        "-n",
                        "30",
                        "-o",
                        "power",
                        "-stats",
                        "pid,command,cpu,power,idlew",
                    ],
                    top_timeout,
                );
                if out.ok {
                    parse_top_output(&out.stdout)
                } else {
                    Err(if out.stderr.is_empty() {
                        "top command timed out".to_string()
                    } else {
                        out.stderr.trim().to_string()
                    })
                }
            });

            let assertions_handle = s.spawn(|| {
                let out = host.run("/usr/bin/pmset", &["-g", "assertions"], pmset_timeout);
                if out.ok {
                    Ok(parse_pmset_assertions(&out.stdout))
                } else {
                    Err(if out.stderr.is_empty() {
                        "pmset -g assertions timed out".to_string()
                    } else {
                        out.stderr.trim().to_string()
                    })
                }
            });

            let wake_handle = s.spawn(|| {
                let out = host.run("/usr/bin/pmset", &["-g", "log"], pmset_timeout);
                if out.ok {
                    let now_secs = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    Ok(parse_pmset_log(&out.stdout, now_secs))
                } else {
                    Err(if out.stderr.is_empty() {
                        "pmset -g log timed out".to_string()
                    } else {
                        out.stderr.trim().to_string()
                    })
                }
            });

            let therm_handle = s.spawn(|| {
                let out = host.run("/usr/bin/pmset", &["-g", "therm"], pmset_timeout);
                if out.ok {
                    Ok(parse_pmset_therm(&out.stdout))
                } else {
                    Err(if out.stderr.is_empty() {
                        "pmset -g therm timed out".to_string()
                    } else {
                        out.stderr.trim().to_string()
                    })
                }
            });

            let nettop_handle = s.spawn(|| {
                let out = host.run(
                    "/usr/bin/nettop",
                    &["-P", "-L", "2", "-s", "2", "-x", "-J", "bytes_in,bytes_out"],
                    nettop_timeout,
                );
                if out.ok {
                    parse_nettop_output(&out.stdout, 2)
                } else {
                    Err(if out.stderr.is_empty() {
                        "nettop timed out".to_string()
                    } else {
                        out.stderr.trim().to_string()
                    })
                }
            });

            let startup_handle = s.spawn(|| {
                let scan = startup::scan_startup(host, layout, ignore, false);
                Ok(scan.items)
            });

            (
                top_handle.join().unwrap(),
                assertions_handle.join().unwrap(),
                wake_handle.join().unwrap(),
                therm_handle.join().unwrap(),
                nettop_handle.join().unwrap(),
                startup_handle.join().unwrap(),
            )
        });

    let memory_pressure = SystemProfiler::memory_pressure();
    let detailed_memory = Some(SystemProfiler::detailed_memory());
    let disk_usage = SysInfo::disk_usage(std::path::Path::new("/"));

    // Gather largest memory processes
    let ps_out = host.run(
        "/bin/ps",
        &["-Aceo", "pid=,rss=,comm=", "-r"],
        Duration::from_secs(2),
    );
    let mut largest_memory_processes = Vec::new();
    if ps_out.ok {
        for line in ps_out.stdout.lines().take(15) {
            let tokens: Vec<&str> = line.split_whitespace().collect();
            if tokens.len() >= 3
                && let (Ok(pid), Ok(rss_kb)) = (tokens[0].parse::<u32>(), tokens[1].parse::<u64>())
            {
                let name = tokens[2..].join(" ");
                largest_memory_processes.push((pid, name, rss_kb.saturating_mul(1024)));
            }
        }
    }

    DoctorSnapshot {
        seconds: watch_seconds,
        top: top_res,
        assertions: assertions_res,
        wake: wake_res,
        therm: therm_res,
        network: nettop_res,
        startup_items: startup_res,
        memory_pressure,
        detailed_memory,
        disk_usage,
        largest_memory_processes,
    }
}

/// Pure diagnosis function: maps snapshot to findings and check statuses.
pub fn diagnose(snapshot: &DoctorSnapshot) -> DoctorReport {
    let mut findings = Vec::new();
    let mut checks = Vec::new();

    // 1. Sustained CPU & Energy & System work checks
    match &snapshot.top {
        Ok(top_diag) => {
            checks.push(CheckResult::ok("sustained_cpu"));
            checks.push(CheckResult::ok("energy"));

            // Skip sample 1 as its rates are unreliable
            if top_diag.samples.len() >= 2 {
                let evaluated_samples = &top_diag.samples[1..];
                let num_samples = evaluated_samples.len();

                // Track processes across evaluated samples
                let mut pid_samples: HashMap<u32, Vec<&TopProcess>> = HashMap::new();
                for sample in evaluated_samples {
                    for proc in &sample.processes {
                        pid_samples.entry(proc.pid).or_default().push(proc);
                    }
                }

                let mut reported_pids = Vec::new();

                for (&pid, procs) in &pid_samples {
                    // Must be present in EVERY evaluated sample
                    if procs.len() == num_samples {
                        // Check if sustained above 80% CPU in EVERY sample
                        let all_above_cpu_threshold =
                            procs.iter().all(|p| p.cpu > CPU_THRESHOLD_PERCENT);
                        if all_above_cpu_threshold {
                            reported_pids.push(pid);
                            let avg_cpu: f64 =
                                procs.iter().map(|p| p.cpu).sum::<f64>() / num_samples as f64;
                            let cmd = &procs[0].command;

                            if let Some((expl_title, expl_ev, expl_action)) =
                                explain_system_process(cmd)
                            {
                                findings.push(Finding {
                                    id: "system_work".to_string(),
                                    severity: FindingSeverity::Notice,
                                    title: format!(
                                        "{cmd} is using {avg_cpu:.0}% CPU ({expl_title})"
                                    ),
                                    evidence: expl_ev.to_string(),
                                    action: expl_action.to_string(),
                                    action_target: None,
                                });
                            } else {
                                findings.push(Finding {
                                    id: "sustained_cpu".to_string(),
                                    severity: FindingSeverity::Warning,
                                    title: format!("{cmd} is using {avg_cpu:.0}% CPU"),
                                    evidence: format!("pid {pid}, for the whole check"),
                                    action: "Quit it if you don't need it.".to_string(),
                                    action_target: Some(FindingAction::KillProcess {
                                        pid,
                                        name: cmd.clone(),
                                    }),
                                });
                            }
                            continue;
                        }

                        // Check for high energy with low CPU
                        let all_low_cpu = procs.iter().all(|p| p.cpu < ENERGY_LOW_CPU_THRESHOLD);
                        let avg_power: f64 =
                            procs.iter().map(|p| p.power).sum::<f64>() / num_samples as f64;
                        let avg_idlew: u64 =
                            procs.iter().map(|p| p.idlew).sum::<u64>() / num_samples as u64;

                        if all_low_cpu
                            && (avg_power > ENERGY_HIGH_POWER_THRESHOLD
                                || avg_idlew > ENERGY_HIGH_WAKEUPS_THRESHOLD)
                        {
                            let cmd = &procs[0].command;
                            reported_pids.push(pid);
                            findings.push(Finding {
                                id: "energy".to_string(),
                                severity: FindingSeverity::Notice,
                                title: format!("{cmd} is using significant energy in the background"),
                                evidence: format!(
                                    "pid {pid}, power score {avg_power:.1}, {avg_idlew} wakeups/s while CPU is low"
                                ),
                                action: "Quit it to save battery.".to_string(),
                                action_target: Some(FindingAction::KillProcess {
                                    pid,
                                    name: cmd.clone(),
                                }),
                            });
                        }
                    }
                }
            }
        }
        Err(err) => {
            checks.push(CheckResult::skipped("sustained_cpu", err));
            checks.push(CheckResult::skipped("energy", err));
        }
    }

    // 2. Memory check (pressure and swap growth)
    let mut swap_growing = false;
    if let Ok(top_diag) = &snapshot.top
        && top_diag.samples.len() >= 2
    {
        let first = &top_diag.samples[1];
        let last = &top_diag.samples[top_diag.samples.len() - 1];
        if last.swapouts > first.swapouts {
            swap_growing = true;
        }
    }

    let is_pressure_warning = matches!(
        snapshot.memory_pressure,
        Some(MemoryPressureLevel::Warning) | Some(MemoryPressureLevel::Critical)
    );

    checks.push(CheckResult::ok("memory"));
    if is_pressure_warning || swap_growing {
        let (title, severity) = if snapshot.memory_pressure == Some(MemoryPressureLevel::Critical) {
            ("Memory pressure is critical", FindingSeverity::Critical)
        } else if swap_growing {
            ("Swap usage is actively growing", FindingSeverity::Warning)
        } else {
            ("Memory pressure is high", FindingSeverity::Warning)
        };

        let evidence = if !snapshot.largest_memory_processes.is_empty() {
            let top_names: Vec<String> = snapshot
                .largest_memory_processes
                .iter()
                .take(3)
                .map(|(_, name, bytes)| format!("{name} ({})", format_bytes(*bytes)))
                .collect();
            format!("Largest apps: {}", top_names.join(", "))
        } else {
            "System is low on free physical memory".to_string()
        };

        findings.push(Finding {
            id: "memory".to_string(),
            severity,
            title: title.to_string(),
            evidence,
            action: "Quit unused apps to free memory.".to_string(),
            action_target: None,
        });
    }

    // 3. Disk space check
    checks.push(CheckResult::ok("disk_space"));
    if let Some(disk) = &snapshot.disk_usage
        && disk.total_bytes > 0
    {
        let free_fraction = disk.available_bytes as f64 / disk.total_bytes as f64;
        let severity_level = disk_severity(free_fraction);
        if severity_level != super::telemetry::Severity::Good {
            let is_critical = free_fraction < 0.05;
            let title = if is_critical {
                "Disk is almost full"
            } else {
                "Disk space is low"
            };
            findings.push(Finding {
                id: "disk_space".to_string(),
                severity: if is_critical {
                    FindingSeverity::Critical
                } else {
                    FindingSeverity::Warning
                },
                title: title.to_string(),
                evidence: format!(
                    "Only {} ({:.1}%) free space available on {}",
                    format_bytes(disk.available_bytes),
                    free_fraction * 100.0,
                    disk.mount_point
                ),
                action: "Free space with Explore Storage (taert analyze).".to_string(),
                action_target: Some(FindingAction::ExploreStorage),
            });
        }
    }

    // 4. Sleep blockers check
    match &snapshot.assertions {
        Ok(blockers) => {
            checks.push(CheckResult::ok("sleep_blockers"));
            for blocker in blockers {
                findings.push(Finding {
                    id: "sleep_blocker".to_string(),
                    severity: FindingSeverity::Warning,
                    title: "Something is keeping the Mac awake".to_string(),
                    evidence: format!(
                        "{} has held {} for {}",
                        blocker.command, blocker.assertion_type, blocker.duration_display
                    ),
                    action: "Quit it to let the Mac sleep.".to_string(),
                    action_target: Some(FindingAction::KillProcess {
                        pid: blocker.pid,
                        name: blocker.command.clone(),
                    }),
                });
            }
        }
        Err(err) => {
            checks.push(CheckResult::skipped("sleep_blockers", err));
        }
    }

    // 5. Wake reasons check
    match &snapshot.wake {
        Ok(wake_diag) => {
            checks.push(CheckResult::ok("wake_reasons"));
            if wake_diag.total_wakes_24h >= WAKE_COUNT_THRESHOLD {
                // Find top reason
                let top_reason = wake_diag
                    .reasons_count
                    .iter()
                    .max_by_key(|(_, cnt)| *cnt)
                    .map(|(r, cnt)| (r.as_str(), *cnt));

                let evidence = if let Some((reason, count)) = top_reason {
                    format!("{count} times for {reason}")
                } else {
                    format!("Frequent wakeups ({})", wake_diag.total_wakes_24h)
                };

                let action = if top_reason
                    .is_some_and(|(r, _)| r == "wifibt" || r.contains("network"))
                {
                    "Network features can wake the Mac while it sleeps.".to_string()
                } else {
                    "Background activities or connected devices can wake the Mac while it sleeps."
                        .to_string()
                };

                findings.push(Finding {
                    id: "wake_reasons".to_string(),
                    severity: FindingSeverity::Notice,
                    title: format!(
                        "The Mac woke {} times in the last 24 hours",
                        wake_diag.total_wakes_24h
                    ),
                    evidence,
                    action,
                    action_target: None,
                });
            }
        }
        Err(err) => {
            checks.push(CheckResult::skipped("wake_reasons", err));
        }
    }

    // 6. Heat check
    match &snapshot.therm {
        Ok(therm_diag) => {
            checks.push(CheckResult::ok("heat"));
            if therm_diag.warning_recorded {
                let evidence = if !therm_diag.details.is_empty() {
                    therm_diag.details.join(", ")
                } else {
                    "Thermal throttling level recorded".to_string()
                };
                findings.push(Finding {
                    id: "heat".to_string(),
                    severity: FindingSeverity::Warning,
                    title: "Thermal or performance warning recorded".to_string(),
                    evidence,
                    action: "Ensure airflow is clear and let the Mac cool down.".to_string(),
                    action_target: None,
                });
            }
        }
        Err(err) => {
            checks.push(CheckResult::skipped("heat", err));
        }
    }

    // 7. Network check
    match &snapshot.network {
        Ok(deltas) => {
            checks.push(CheckResult::ok("network"));
            for delta in deltas {
                findings.push(Finding {
                    id: "network".to_string(),
                    severity: FindingSeverity::Notice,
                    title: format!(
                        "{} is moving {}/s over the network",
                        delta.command,
                        format_bytes(delta.rate_bytes_per_sec)
                    ),
                    evidence: format!(
                        "pid {}, {} in, {} out",
                        delta.pid,
                        format_bytes(delta.delta_in),
                        format_bytes(delta.delta_out)
                    ),
                    action: "Check what it is downloading or uploading.".to_string(),
                    action_target: Some(FindingAction::KillProcess {
                        pid: delta.pid,
                        name: delta.command.clone(),
                    }),
                });
            }
        }
        Err(err) => {
            checks.push(CheckResult::skipped("network", err));
        }
    }

    // 8. Startup items check
    match &snapshot.startup_items {
        Ok(items) => {
            checks.push(CheckResult::ok("startup"));
            for item in items {
                if item.state == StartupState::Broken {
                    let path_str = item
                        .program
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "missing file".to_string());
                    findings.push(Finding {
                        id: "startup".to_string(),
                        severity: FindingSeverity::Warning,
                        title: format!("Startup item '{}' points to a missing program", item.name),
                        evidence: format!("Missing executable at {path_str}"),
                        action: format!("Switch it off with: taert startup disable {}", item.label),
                        action_target: Some(FindingAction::DisableStartupItem {
                            item: item.clone(),
                        }),
                    });
                }
            }
        }
        Err(err) => {
            checks.push(CheckResult::skipped("startup", err));
        }
    }

    DoctorReport {
        seconds: snapshot.seconds,
        findings,
        checks,
    }
}

/// Formats and prints the plain text doctor report.
pub fn print_plain_report(report: &DoctorReport) {
    println!("Checked for {} seconds.", report.seconds);
    println!();

    if !report.findings.is_empty() {
        for finding in &report.findings {
            println!("{}", finding.title);
            println!("  {}", finding.evidence);
            println!("  {}", finding.action);
            println!();
        }
        println!("Nothing else stands out. CPU, memory, disk, heat, and network look normal.");
    } else {
        println!("Nothing stands out. CPU, memory, disk, heat, and network look normal.");
    }

    // Print notes for any skipped checks
    let skipped_checks: Vec<_> = report
        .checks
        .iter()
        .filter(|c| c.status == "skipped")
        .collect();
    if !skipped_checks.is_empty() {
        println!();
        for check in skipped_checks {
            let reason = check.reason.as_deref().unwrap_or("unknown error");
            println!("Note: Check '{}' was skipped ({}).", check.id, reason);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_TOP_QUIET: &str = r#"Processes: 800 total, 2 running, 798 sleeping, 3000 threads 
2026/11/01 10:00:00
Load Avg: 1.20, 1.10, 1.05 
CPU usage: 2.0% user, 2.0% sys, 96.0% idle 
SharedLibs: 1200M resident, 200M data, 200M linkedit.
MemRegions: 200000 total, 10G resident, 1000M private, 4000M shared.
PhysMem: 20G used (4000M wired, 200M compressor), 12G unused.
VM: 400T vsize, 50G framework vsize, 0(0) swapins, 0(0) swapouts.
Networks: packets: 1000/100K in, 1000/100K out.
Disks: 1000/100K read, 1000/100K written.

PID    COMMAND          %CPU POWER IDLEW
100    Finder           1.5  1.5   10    
101    Dock             0.5  0.5   5     
Processes: 800 total, 2 running, 798 sleeping, 3000 threads 
2026/11/01 10:00:02
Load Avg: 1.20, 1.10, 1.05 
CPU usage: 2.0% user, 2.0% sys, 96.0% idle 
SharedLibs: 1200M resident, 200M data, 200M linkedit.
MemRegions: 200000 total, 10G resident, 1000M private, 4000M shared.
PhysMem: 20G used (4000M wired, 200M compressor), 12G unused.
VM: 400T vsize, 50G framework vsize, 0(0) swapins, 0(0) swapouts.
Networks: packets: 1000/100K in, 1000/100K out.
Disks: 1000/100K read, 1000/100K written.

PID    COMMAND          %CPU POWER IDLEW
100    Finder           1.2  1.2   12    
101    Dock             0.4  0.4   4     
"#;

    const FIXTURE_TOP_SUSTAINED: &str = r#"Processes: 800 total, 2 running, 798 sleeping, 3000 threads 
2026/11/01 10:00:00
Load Avg: 3.50, 2.10, 1.05 
CPU usage: 90.0% user, 5.0% sys, 5.0% idle 
SharedLibs: 1200M resident, 200M data, 200M linkedit.
MemRegions: 200000 total, 10G resident, 1000M private, 4000M shared.
PhysMem: 20G used (4000M wired, 200M compressor), 12G unused.
VM: 400T vsize, 50G framework vsize, 0(0) swapins, 0(0) swapouts.
Networks: packets: 1000/100K in, 1000/100K out.
Disks: 1000/100K read, 1000/100K written.

PID    COMMAND          %CPU  POWER IDLEW
4123   node             190.0 190.0 10    
5555   one_time_burst   95.0  95.0  10    
Processes: 800 total, 2 running, 798 sleeping, 3000 threads 
2026/11/01 10:00:02
Load Avg: 3.50, 2.10, 1.05 
CPU usage: 90.0% user, 5.0% sys, 5.0% idle 
SharedLibs: 1200M resident, 200M data, 200M linkedit.
MemRegions: 200000 total, 10G resident, 1000M private, 4000M shared.
PhysMem: 20G used (4000M wired, 200M compressor), 12G unused.
VM: 400T vsize, 50G framework vsize, 0(0) swapins, 0(0) swapouts.
Networks: packets: 1000/100K in, 1000/100K out.
Disks: 1000/100K read, 1000/100K written.

PID    COMMAND          %CPU  POWER IDLEW
4123   node             185.0 185.0 12    
5555   one_time_burst   30.0  30.0  5     
Processes: 800 total, 2 running, 798 sleeping, 3000 threads 
2026/11/01 10:00:04
Load Avg: 3.50, 2.10, 1.05 
CPU usage: 90.0% user, 5.0% sys, 5.0% idle 
SharedLibs: 1200M resident, 200M data, 200M linkedit.
MemRegions: 200000 total, 10G resident, 1000M private, 4000M shared.
PhysMem: 20G used (4000M wired, 200M compressor), 12G unused.
VM: 400T vsize, 50G framework vsize, 0(0) swapins, 0(0) swapouts.
Networks: packets: 1000/100K in, 1000/100K out.
Disks: 1000/100K read, 1000/100K written.

PID    COMMAND          %CPU  POWER IDLEW
4123   node             195.0 195.0 15    
"#;

    const FIXTURE_ASSERTIONS: &str = r#"2026-11-01 20:00:00 +0000 
Assertion status system-wide:
   BackgroundTask                 0
   UserIsActive                   1
   PreventUserIdleDisplaySleep    0
   PreventUserIdleSystemSleep     1
Listed by owning process:
   pid 360(powerd): [0x000171d8000187e8] 07:23:38 PreventUserIdleSystemSleep named: "Powerd - Prevent sleep while display is on"  
   pid 360(powerd): [0x0000003a00098009] 33:40:39 UserIsActive named: "com.apple.powerd.ws.initialize"  
   pid 70001(caffeinate): [0x0001a12300059496] 02:14:09 PreventUserIdleSystemSleep named: "caffeinate command-line tool"  
   pid 70002(short_tool): [0x0001a12300059497] 00:05:00 PreventUserIdleSystemSleep named: "short task"  
   pid 417(WindowServer): [0x0000003d00098049] 00:02:21 UserIsActive named: "trackpad tickle"  
Kernel Assertions: 0x104=USB
"#;

    const FIXTURE_PMSET_LOG: &str = r#"2026-11-01 10:00:00 +0000 Sleep               	Entering Sleep state due to 'Clamshell Sleep':TCPKeepAlive=active Using Batt (Charge:80%) 100 secs  
2026-11-01 10:05:00 +0000 DarkWake            	DarkWake from Deep Idle [CDNP] : due to smc.sysState.Wake(0x70070000) wifibt SMC.OutboxNotEmpty bluetooth-pcie/ Using BATT (Charge:80%) 20 secs   
2026-11-01 10:10:00 +0000 DarkWake            	DarkWake from Deep Idle [CDNP] : due to smc.sysState.Wake(0x70070000) wifibt SMC.OutboxNotEmpty bluetooth-pcie/ Using BATT (Charge:80%) 20 secs   
2026-11-01 10:15:00 +0000 DarkWake            	DarkWake from Deep Idle [CDNP] : due to smc.sysState.Wake(0x70070000) wifibt SMC.OutboxNotEmpty bluetooth-pcie/ Using BATT (Charge:80%) 20 secs   
2026-11-01 10:20:00 +0000 DarkWake            	DarkWake from Deep Idle [CDNP] : due to smc.sysState.Wake(0x70070000) wifibt SMC.OutboxNotEmpty bluetooth-pcie/ Using BATT (Charge:80%) 20 secs   
2026-11-01 10:25:00 +0000 DarkWake            	DarkWake from Deep Idle [CDNP] : due to smc.sysState.Wake(0x70070000) wifibt SMC.OutboxNotEmpty bluetooth-pcie/ Using BATT (Charge:80%) 20 secs   
2026-11-01 10:30:00 +0000 DarkWake            	DarkWake from Deep Idle [CDNP] : due to smc.sysState.Wake(0x70070000) wifibt SMC.OutboxNotEmpty bluetooth-pcie/ Using BATT (Charge:80%) 20 secs   
2026-11-01 10:35:00 +0000 DarkWake            	DarkWake from Deep Idle [CDNP] : due to smc.sysState.Wake(0x70070000) wifibt SMC.OutboxNotEmpty bluetooth-pcie/ Using BATT (Charge:80%) 20 secs   
2026-11-01 10:40:00 +0000 DarkWake            	DarkWake from Deep Idle [CDNP] : due to smc.sysState.Wake(0x70070000) wifibt SMC.OutboxNotEmpty bluetooth-pcie/ Using BATT (Charge:80%) 20 secs   
2026-11-01 10:45:00 +0000 DarkWake            	DarkWake from Deep Idle [CDNP] : due to smc.sysState.Wake(0x70070000) wifibt SMC.OutboxNotEmpty bluetooth-pcie/ Using BATT (Charge:80%) 20 secs   
2026-11-01 10:50:00 +0000 DarkWake            	DarkWake from Deep Idle [CDNP] : due to smc.sysState.Wake(0x70070000) wifibt SMC.OutboxNotEmpty bluetooth-pcie/ Using BATT (Charge:80%) 20 secs   
2026-11-01 10:55:00 +0000 DarkWake            	DarkWake from Deep Idle [CDNP] : due to smc.sysState.Wake(0x70070000) wifibt SMC.OutboxNotEmpty bluetooth-pcie/ Using BATT (Charge:80%) 20 secs   
2026-11-01 11:00:00 +0000 DarkWake            	DarkWake from Deep Idle [CDNP] : due to smc.sysState.Wake(0x70070000) wifibt SMC.OutboxNotEmpty bluetooth-pcie/ Using BATT (Charge:80%) 20 secs   
2026-11-01 11:05:00 +0000 Wake                	Wake from Deep Idle [CDNVA] : due to Notification Using AC (Charge:80%) 2 secs    
2026-11-01 11:10:00 +0000 Wake                	Wake from Deep Idle [CDNVA] : due to Notification Using AC (Charge:80%) 2 secs    
2026-10-20 10:00:00 +0000 DarkWake            	DarkWake from Deep Idle [CDNP] : due to smc.sysState.Wake(0x70070000) old_wake/ Using BATT (Charge:80%) 20 secs   
"#;

    const FIXTURE_THERM_QUIET: &str = r#"Note: No thermal warning level has been recorded
Note: No performance warning level has been recorded
Note: No CPU power status has been recorded
"#;

    const FIXTURE_THERM_WARNED: &str = r#"CPU_Speed_Limit = 70
Thermal_Warning_Level = Moderate
Note: No CPU power status has been recorded
"#;

    const FIXTURE_NETTOP: &str = r#",bytes_in,bytes_out,
launchd.1,1000,1000,
downloader.4000,1000000,200000,
syslogd.381,0,1000,
,bytes_in,bytes_out,
launchd.1,1050,1050,
downloader.4000,4000000,200000,
syslogd.381,0,1000,
"#;

    #[test]
    fn test_parse_top_quiet() {
        let diag = parse_top_output(FIXTURE_TOP_QUIET).unwrap();
        assert_eq!(diag.samples.len(), 2);
        assert_eq!(diag.samples[0].processes.len(), 2);
    }

    #[test]
    fn test_parse_top_sustained_cpu() {
        let diag = parse_top_output(FIXTURE_TOP_SUSTAINED).unwrap();
        assert_eq!(diag.samples.len(), 3);

        let snapshot = DoctorSnapshot {
            seconds: 10,
            top: Ok(diag),
            ..Default::default()
        };

        let report = diagnose(&snapshot);
        // node is > 80% in samples 2 and 3 -> finding!
        // one_time_burst was only > 80% in sample 2, dropped to 30% in sample 3 -> NOT a finding!
        let sustained_findings: Vec<_> = report
            .findings
            .iter()
            .filter(|f| f.id == "sustained_cpu")
            .collect();
        assert_eq!(sustained_findings.len(), 1);
        assert!(sustained_findings[0].title.contains("node"));
        assert!(sustained_findings[0].evidence.contains("4123"));
    }

    #[test]
    fn test_one_sample_burst_is_not_a_finding() {
        let diag = parse_top_output(FIXTURE_TOP_SUSTAINED).unwrap();
        let snapshot = DoctorSnapshot {
            top: Ok(diag),
            ..Default::default()
        };

        let report = diagnose(&snapshot);
        assert!(
            report
                .findings
                .iter()
                .all(|f| !f.title.contains("one_time_burst")),
            "one-time burst should never be a finding"
        );
    }

    #[test]
    fn test_powerd_display_is_not_a_blocker() {
        let blockers = parse_pmset_assertions(FIXTURE_ASSERTIONS);
        assert!(
            blockers.iter().all(|b| b.command != "powerd"),
            "powerd must be ignored"
        );
    }

    #[test]
    fn test_short_blocker_is_not_a_finding_long_is() {
        let blockers = parse_pmset_assertions(FIXTURE_ASSERTIONS);
        assert_eq!(blockers.len(), 1);
        assert_eq!(blockers[0].command, "caffeinate");
        assert_eq!(blockers[0].duration_display, "02:14:09");
    }

    #[test]
    fn test_wake_reasons_grouped_and_24h_limited() {
        // Reference time: 2026-11-01 12:00:00 UTC
        let ref_time = parse_log_timestamp("2026-11-01 12:00:00").unwrap();
        let diag = parse_pmset_log(FIXTURE_PMSET_LOG, ref_time);

        // 12 wifibt + 2 Notification = 14 wakes within 24h
        // The one on 2026-10-20 is > 24h old and must be excluded!
        assert_eq!(diag.total_wakes_24h, 14);
        assert_eq!(*diag.reasons_count.get("wifibt").unwrap_or(&0), 12);
        assert_eq!(*diag.reasons_count.get("Notification").unwrap_or(&0), 2);
        assert_eq!(diag.reasons_count.get("old_wake"), None);

        let snapshot = DoctorSnapshot {
            wake: Ok(diag),
            ..Default::default()
        };
        let report = diagnose(&snapshot);
        let wake_finding = report.findings.iter().find(|f| f.id == "wake_reasons");
        assert!(wake_finding.is_some());
        let wf = wake_finding.unwrap();
        assert_eq!(wf.title, "The Mac woke 14 times in the last 24 hours");
        assert_eq!(wf.evidence, "12 times for wifibt");
        assert_eq!(
            wf.action,
            "Network features can wake the Mac while it sleeps."
        );
    }

    #[test]
    fn test_therm_parser() {
        let quiet = parse_pmset_therm(FIXTURE_THERM_QUIET);
        assert!(!quiet.warning_recorded);

        let warned = parse_pmset_therm(FIXTURE_THERM_WARNED);
        assert!(warned.warning_recorded);
        assert_eq!(warned.details.len(), 2);
    }

    #[test]
    fn test_nettop_parser() {
        // 4,000,000 - 1,000,000 = 3,000,000 bytes over 2s = 1.5 MB/s > 1 MB/s
        let deltas = parse_nettop_output(FIXTURE_NETTOP, 2).unwrap();
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].command, "downloader");
        assert_eq!(deltas[0].rate_bytes_per_sec, 1_500_000);
    }

    #[test]
    fn test_failed_command_gives_skipped_check() {
        let snapshot = DoctorSnapshot {
            network: Err("nettop timed out".to_string()),
            ..Default::default()
        };

        let report = diagnose(&snapshot);
        let net_check = report.checks.iter().find(|c| c.id == "network");
        assert!(net_check.is_some());
        let check = net_check.unwrap();
        assert_eq!(check.status, "skipped");
        assert_eq!(check.reason.as_deref(), Some("nettop timed out"));
    }

    #[test]
    fn test_quiet_mac_gives_no_findings() {
        let snapshot = DoctorSnapshot {
            seconds: 10,
            top: parse_top_output(FIXTURE_TOP_QUIET),
            assertions: Ok(Vec::new()),
            wake: Ok(WakeDiagnostics::default()),
            therm: Ok(ThermalDiagnostics::default()),
            network: Ok(Vec::new()),
            startup_items: Ok(Vec::new()),
            memory_pressure: Some(MemoryPressureLevel::Normal),
            disk_usage: Some(DiskUsage {
                mount_point: "/".to_string(),
                total_bytes: 1_000_000_000_000,
                free_bytes: 500_000_000_000,
                available_bytes: 500_000_000_000,
            }),
            ..Default::default()
        };

        let report = diagnose(&snapshot);
        assert!(report.findings.is_empty());
        assert!(report.checks.iter().all(|c| c.status == "ok"));
    }

    #[test]
    fn test_json_shape_and_skipped_check() {
        let snapshot = DoctorSnapshot {
            seconds: 10,
            wake: Ok(WakeDiagnostics::default()),
            network: Err("nettop timed out".to_string()),
            ..Default::default()
        };

        let report = diagnose(&snapshot);
        let json = serde_json::to_string_pretty(&report).unwrap();
        assert!(json.contains("\"seconds\": 10"));
        assert!(json.contains("\"checks\":"));
        assert!(json.contains("\"id\": \"network\""));
        assert!(json.contains("\"status\": \"skipped\""));
        assert!(json.contains("\"reason\": \"nettop timed out\""));
    }

    #[test]
    fn test_system_process_explanation() {
        let (title, ev, action) = explain_system_process("mds_stores").unwrap();
        assert!(title.contains("Spotlight"));
        assert!(ev.contains("search index"));
        assert!(action.contains("finish automatically"));

        let (kt_title, _, _) = explain_system_process("kernel_task").unwrap();
        assert!(kt_title.contains("cooling"));
    }

    #[test]
    fn test_disk_space_thresholds() {
        let mut snapshot = DoctorSnapshot {
            disk_usage: Some(DiskUsage {
                mount_point: "/".to_string(),
                total_bytes: 100_000_000_000,
                free_bytes: 8_000_000_000,
                available_bytes: 8_000_000_000,
            }),
            ..Default::default()
        };
        let report = diagnose(&snapshot);
        let disk_f = report.findings.iter().find(|f| f.id == "disk_space");
        assert!(disk_f.is_some());
        assert_eq!(disk_f.unwrap().severity, FindingSeverity::Warning);

        // 15% free -> safe
        snapshot.disk_usage = Some(DiskUsage {
            mount_point: "/".to_string(),
            total_bytes: 100_000_000_000,
            free_bytes: 15_000_000_000,
            available_bytes: 15_000_000_000,
        });
        let report2 = diagnose(&snapshot);
        assert!(report2.findings.iter().all(|f| f.id != "disk_space"));
    }

    #[test]
    fn test_cpu_threshold_boundary() {
        // Sample with 80.1% CPU (above threshold)
        let sample_above = TopSample {
            processes: vec![TopProcess {
                pid: 1000,
                command: "heavy_task".to_string(),
                cpu: 80.1,
                power: 80.1,
                idlew: 10,
            }],
            swapins: 0,
            swapouts: 0,
        };
        // Sample with 79.9% CPU (below threshold)
        let sample_below = TopSample {
            processes: vec![TopProcess {
                pid: 1000,
                command: "heavy_task".to_string(),
                cpu: 79.9,
                power: 79.9,
                idlew: 10,
            }],
            swapins: 0,
            swapouts: 0,
        };

        // Two samples: sample 1 (skipped), sample 2 (evaluated)
        let snapshot_above = DoctorSnapshot {
            top: Ok(TopDiagnostics {
                samples: vec![sample_above.clone(), sample_above],
            }),
            ..Default::default()
        };
        let report_above = diagnose(&snapshot_above);
        assert_eq!(
            report_above
                .findings
                .iter()
                .filter(|f| f.id == "sustained_cpu")
                .count(),
            1
        );

        let snapshot_below = DoctorSnapshot {
            top: Ok(TopDiagnostics {
                samples: vec![sample_below.clone(), sample_below],
            }),
            ..Default::default()
        };
        let report_below = diagnose(&snapshot_below);
        assert_eq!(
            report_below
                .findings
                .iter()
                .filter(|f| f.id == "sustained_cpu")
                .count(),
            0
        );
    }

    #[test]
    fn test_sleep_blocker_boundary() {
        // 601 seconds (above 600s threshold) -> finding
        let out_above = r#"Listed by owning process:
   pid 1234(my_app): [0x1] 00:10:01 PreventUserIdleSystemSleep named: "work"
"#;
        let blockers_above = parse_pmset_assertions(out_above);
        assert_eq!(blockers_above.len(), 1);

        // 599 seconds (below 600s threshold) -> not a finding
        let out_below = r#"Listed by owning process:
   pid 1234(my_app): [0x1] 00:09:59 PreventUserIdleSystemSleep named: "work"
"#;
        let blockers_below = parse_pmset_assertions(out_below);
        assert_eq!(blockers_below.len(), 0);
    }

    #[test]
    fn test_wake_count_boundary() {
        let mut reasons_10 = HashMap::new();
        reasons_10.insert("wifibt".to_string(), 10);
        let diag_10 = WakeDiagnostics {
            total_wakes_24h: 10,
            reasons_count: reasons_10,
            ..Default::default()
        };

        let snapshot_10 = DoctorSnapshot {
            wake: Ok(diag_10),
            ..Default::default()
        };
        let report_10 = diagnose(&snapshot_10);
        assert!(report_10.findings.iter().any(|f| f.id == "wake_reasons"));

        let mut reasons_9 = HashMap::new();
        reasons_9.insert("wifibt".to_string(), 9);
        let diag_9 = WakeDiagnostics {
            total_wakes_24h: 9,
            reasons_count: reasons_9,
            ..Default::default()
        };

        let snapshot_9 = DoctorSnapshot {
            wake: Ok(diag_9),
            ..Default::default()
        };
        let report_9 = diagnose(&snapshot_9);
        assert!(report_9.findings.iter().all(|f| f.id != "wake_reasons"));
    }

    #[test]
    fn test_network_throughput_boundary() {
        // 2,000,000 bytes over 2s = 1,000,000 B/s (at threshold)
        let out_at_threshold = r#",bytes_in,bytes_out,
test.100,0,0,
,bytes_in,bytes_out,
test.100,2000000,0,
"#;
        let deltas_at = parse_nettop_output(out_at_threshold, 2).unwrap();
        assert_eq!(deltas_at.len(), 1);

        // 1,999,998 bytes over 2s = 999,999 B/s (below threshold)
        let out_below = r#",bytes_in,bytes_out,
test.100,0,0,
,bytes_in,bytes_out,
test.100,1999998,0,
"#;
        let deltas_below = parse_nettop_output(out_below, 2).unwrap();
        assert_eq!(deltas_below.len(), 0);
    }

    #[test]
    fn test_energy_thresholds() {
        // Low CPU (10%), high power (25.0 > 20.0) -> energy finding
        let sample_energy = TopSample {
            processes: vec![TopProcess {
                pid: 2000,
                command: "background_sync".to_string(),
                cpu: 10.0,
                power: 25.0,
                idlew: 100,
            }],
            swapins: 0,
            swapouts: 0,
        };

        let snapshot = DoctorSnapshot {
            top: Ok(TopDiagnostics {
                samples: vec![sample_energy.clone(), sample_energy],
            }),
            ..Default::default()
        };
        let report = diagnose(&snapshot);
        let energy_findings: Vec<_> = report
            .findings
            .iter()
            .filter(|f| f.id == "energy")
            .collect();
        assert_eq!(energy_findings.len(), 1);
        assert!(energy_findings[0].title.contains("background_sync"));
    }
}

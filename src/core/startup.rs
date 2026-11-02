//! Review and switch off login and background items.
//!
//! Inspects launchd agents, daemons, and classic login items, associates them
//! with installed apps, reports live CPU/memory usage, and safely enables or disables them.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::apps::{self, InstalledApp};
use super::cleaner::{Cleaner, RemovalMode};
use super::host::{Host, MacHost, Output, shell_quote};
use super::layout::Layout;
use super::safety::Safety;
use super::scanner::ScanItem;
use super::stores::IgnoreStore;
use super::telemetry::parse_processes;

const TIMEOUT: Duration = Duration::from_secs(5);

/// Script to remove a login item by name via System Events.
pub const LOGIN_ITEM_SCRIPT: &[&str] = &[
    "on run argv",
    "set target to item 1 of argv",
    "tell application \"System Events\"",
    "repeat with i from (count of login items) to 1 by -1",
    "if name of login item i is target then delete login item i",
    "end repeat",
    "end tell",
    "end run",
];

/// Type of startup/background service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupKind {
    LoginItem,
    UserAgent,
    SystemAgent,
    Daemon,
    /// A Background Task Management record that taert only lists.
    Background,
}

impl StartupKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::LoginItem => "login item",
            Self::UserAgent => "user agent",
            Self::SystemAgent => "system agent",
            Self::Daemon => "daemon",
            Self::Background => "background item",
        }
    }
}

/// Operational state of a startup service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupState {
    Running,
    Enabled,
    Disabled,
    Broken,
}

impl StartupState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
            Self::Broken => "broken",
        }
    }
}

/// A discovered startup item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StartupItem {
    pub label: String,
    pub name: String,
    pub app_name: Option<String>,
    pub app_bundle: Option<PathBuf>,
    pub kind: StartupKind,
    pub state: StartupState,
    pub program: Option<PathBuf>,
    pub plist_path: Option<PathBuf>,
    pub pid: Option<u32>,
    pub cpu_percent: Option<f32>,
    pub memory_bytes: Option<u64>,
    pub is_apple: bool,
    pub is_security: bool,
    pub is_ignored: bool,
}

/// Stored entry for disabled classic login items.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DisabledLoginItem {
    pub name: String,
    pub path: PathBuf,
}

/// Result of scanning startup items.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StartupScan {
    pub items: Vec<StartupItem>,
    pub notes: Vec<String>,
}

/// Reads the program path from a launchd plist dictionary.
pub fn launch_program_from_dict(dict: &plist::Dictionary) -> Option<String> {
    let program = dict.get("Program").and_then(plist::Value::as_string);
    let bundle_program = dict.get("BundleProgram").and_then(plist::Value::as_string);
    let first_argument = || {
        dict.get("ProgramArguments")?
            .as_array()?
            .first()?
            .as_string()
    };
    program
        .or(bundle_program)
        .or_else(first_argument)
        .map(str::to_string)
}

/// Reads the program path from a launchd plist file.
pub fn launch_program(plist: &Path) -> Option<String> {
    let dict = plist::Value::from_file(plist).ok()?.into_dictionary()?;
    launch_program_from_dict(&dict)
}

/// Parses the output of `launchctl print-disabled <domain>`.
///
/// Returns a map of service label to disabled status (`true` = disabled, `false` = enabled).
pub fn parse_print_disabled(output: &str) -> HashMap<String, bool> {
    let mut map = HashMap::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some((left, right)) = trimmed.split_once("=>") {
            let label = left.trim().trim_matches('"');
            let status = right.trim();
            if !label.is_empty() {
                map.insert(label.to_string(), status == "disabled");
            }
        }
    }
    map
}

/// Service entry in `launchctl print` services block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchdServiceEntry {
    pub pid: Option<u32>,
    pub last_exit_code: Option<i32>,
    pub label: String,
}

/// Parses services listed inside `services = { ... }` in `launchctl print <domain>`.
pub fn parse_launchctl_services(output: &str) -> Vec<LaunchdServiceEntry> {
    let mut services = Vec::new();
    let mut in_services = false;

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("services = {") {
            in_services = true;
            continue;
        }
        if in_services {
            if trimmed == "}" {
                in_services = false;
                continue;
            }
            let mut parts = trimmed.split_whitespace();
            if let (Some(pid_str), Some(exit_str), Some(label)) =
                (parts.next(), parts.next(), parts.next())
            {
                let pid = pid_str.parse::<u32>().ok().filter(|&p| p > 0);
                let last_exit_code = exit_str.parse::<i32>().ok();
                services.push(LaunchdServiceEntry {
                    pid,
                    last_exit_code,
                    label: label.to_string(),
                });
            }
        }
    }
    services
}

/// Detailed service information parsed from `launchctl print <domain>/<label>`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LaunchdServiceDetail {
    pub path: Option<PathBuf>,
    pub program: Option<PathBuf>,
    pub parent_bundle_id: Option<String>,
    pub state: Option<String>,
    pub pid: Option<u32>,
}

/// Parses detailed attributes from `launchctl print <domain>/<label>`.
pub fn parse_service_detail(output: &str) -> LaunchdServiceDetail {
    let mut detail = LaunchdServiceDetail::default();
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("path = ") {
            let p = rest.trim();
            if !p.starts_with('(') {
                detail.path = Some(PathBuf::from(p));
            }
        } else if let Some(rest) = trimmed.strip_prefix("program = ") {
            detail.program = Some(PathBuf::from(rest.trim()));
        } else if let Some(rest) = trimmed.strip_prefix("parent bundle identifier = ") {
            detail.parent_bundle_id = Some(rest.trim().to_string());
        } else if let Some(rest) = trimmed.strip_prefix("state = ") {
            detail.state = Some(rest.trim().to_string());
        } else if let Some(rest) = trimmed.strip_prefix("pid = ") {
            detail.pid = rest.trim().parse::<u32>().ok();
        }
    }
    detail
}

/// Parses classic login items from tab-separated `osascript` output.
pub fn parse_login_items(output: &str) -> Vec<(String, PathBuf)> {
    let mut items = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some((name, path)) = trimmed.split_once('\t')
            && !name.trim().is_empty()
            && !path.trim().is_empty()
        {
            items.push((name.trim().to_string(), PathBuf::from(path.trim())));
        }
    }
    items
}

/// Matches a service against installed apps.
pub fn match_app(
    program: Option<&Path>,
    associated_ids: &[String],
    label: &str,
    apps: &[InstalledApp],
) -> (Option<String>, Option<PathBuf>) {
    // 1. Program path sits inside an app bundle
    if let Some(prog) = program {
        for app in apps {
            if prog.starts_with(&app.path) {
                return (Some(app.name.clone()), Some(app.path.clone()));
            }
        }
    }

    // 2. AssociatedBundleIdentifiers
    for assoc in associated_ids {
        for app in apps {
            if app.bundle_id.as_deref() == Some(assoc.as_str()) {
                return (Some(app.name.clone()), Some(app.path.clone()));
            }
        }
    }

    // 3. Bundle ID prefix match with label
    for app in apps {
        if let Some(ref bid) = app.bundle_id
            && (label == bid
                || label.starts_with(&format!("{bid}."))
                || bid.starts_with(&format!("{label}.")))
        {
            return (Some(app.name.clone()), Some(app.path.clone()));
        }
    }

    (None, None)
}

/// Loads disabled login items persisted in `~/.config/taert/disabled_login_items.json`.
pub fn load_disabled_login_items(layout: &Layout) -> Vec<DisabledLoginItem> {
    let path = layout.user(".config/taert/disabled_login_items.json");
    if let Ok(text) = fs::read_to_string(path) {
        serde_json::from_str(&text).unwrap_or_default()
    } else {
        Vec::new()
    }
}

/// Persists disabled login items to `~/.config/taert/disabled_login_items.json`.
pub fn save_disabled_login_items(
    layout: &Layout,
    items: &[DisabledLoginItem],
) -> Result<(), std::io::Error> {
    let path = layout.user(".config/taert/disabled_login_items.json");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(items)?;
    fs::write(path, json)?;
    Ok(())
}

fn host_uid() -> u32 {
    #[cfg(unix)]
    {
        unsafe { libc::getuid() }
    }
    #[cfg(not(unix))]
    {
        501
    }
}

const BACKGROUND_READ_ONLY: &str = "Background task entries are read-only";
const LAUNCHCTL: &str = "/bin/launchctl";
const OSASCRIPT: &str = "/usr/bin/osascript";
const LIST_LOGIN_ITEMS_SCRIPT: &str = "tell application \"System Events\"\nset res to \"\"\nrepeat with item_ref in every login item\nset res to res & (name of item_ref) & tab & (path of item_ref) & linefeed\nend repeat\nreturn res\nend tell";
const ADD_LOGIN_ITEM_SCRIPT: &[&str] = &[
    "on run argv",
    "tell application \"System Events\" to make login item at end with properties {path:(item 1 of argv), name:(item 2 of argv), hidden:false}",
    "end run",
];

// The dump is read through a pipe, so root writes to no file. The password dialog is
// part of this time, so it leaves room to type.
const BTM_TIMEOUT: Duration = Duration::from_secs(120);
const BTM_SCRIPT: &str = "/usr/bin/sfltool dumpbtm";
const BTM_PROMPT: &str = "Read Background Task Management records";

/// An `osascript` argument list: each script line after `-e`, then the arguments after `--`.
fn osascript_args<'a>(script: &[&'a str], args: &[&'a str]) -> Vec<&'a str> {
    let mut all: Vec<&str> = script.iter().flat_map(|line| ["-e", *line]).collect();
    all.push("--");
    all.extend_from_slice(args);
    all
}

/// One record from `sfltool dumpbtm`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtmEntry {
    pub identifier: String,
    pub name: Option<String>,
    pub developer: Option<String>,
    pub enabled: bool,
    pub url: Option<String>,
    pub executable: Option<PathBuf>,
}

/// What reading the Background Task Management records gave.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Btm {
    #[default]
    Skipped,
    Entries(Vec<BtmEntry>),
    Unavailable(String),
}

/// Parses `sfltool dumpbtm`. Lines it does not know are ignored.
pub fn parse_btm(output: &str) -> Vec<BtmEntry> {
    let mut entries = Vec::new();
    let mut current: Option<HashMap<&str, &str>> = None;
    let flush = |fields: Option<HashMap<&str, &str>>, entries: &mut Vec<BtmEntry>| {
        if let Some(entry) = fields.and_then(|f| btm_entry(&f)) {
            entries.push(entry);
        }
    };
    for line in output.lines() {
        let line = line.trim();
        let is_item_header = line
            .strip_prefix('#')
            .and_then(|rest| rest.strip_suffix(':'))
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
        if is_item_header {
            flush(current.replace(HashMap::new()), &mut entries);
        } else if line.starts_with("Records for UID") {
            flush(current.take(), &mut entries);
        } else if let (Some(fields), Some((key, value))) = (current.as_mut(), line.split_once(':'))
        {
            fields.entry(key.trim()).or_insert_with(|| value.trim());
        }
    }
    flush(current, &mut entries);
    entries
}

fn btm_entry(fields: &HashMap<&str, &str>) -> Option<BtmEntry> {
    let raw = fields
        .get("Identifier")
        .copied()
        .filter(|v| !v.is_empty())?;
    let identifier = match raw.split_once('.') {
        Some((n, rest)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => rest,
        _ => raw,
    };
    let text = |key: &str| {
        fields
            .get(key)
            .filter(|v| !v.is_empty())
            .map(|v| v.to_string())
    };
    let enabled = fields
        .get("Disposition")
        .and_then(|d| d.split_once('[')?.1.split_once(']'))
        .is_some_and(|(flags, _)| flags.split(',').any(|f| f.trim() == "enabled"));
    Some(BtmEntry {
        identifier: identifier.to_string(),
        name: text("Name"),
        developer: text("Developer Name"),
        enabled,
        url: text("URL"),
        executable: text("Executable Path").map(PathBuf::from),
    })
}

/// Reads the Background Task Management records as root. One password prompt.
pub fn read_btm(host: &dyn Host) -> Btm {
    match host.run_admin_output(BTM_SCRIPT, BTM_PROMPT, BTM_TIMEOUT) {
        Err(error) => Btm::Unavailable(format!(
            "Background task records unavailable: {}",
            if error.is_empty() {
                "sfltool failed"
            } else {
                &error
            }
        )),
        Ok(output) => {
            let entries = parse_btm(&output);
            if entries.is_empty() {
                Btm::Unavailable(
                    "Background task records unavailable: sfltool printed nothing taert can read."
                        .to_string(),
                )
            } else {
                Btm::Entries(entries)
            }
        }
    }
}

/// The path in a `file://` URL, with `%20` style escapes undone.
fn file_url_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let escaped = (bytes[i] == b'%')
            .then(|| rest.get(i + 1..i + 3))
            .flatten()
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escaped {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    Some(PathBuf::from(String::from_utf8_lossy(&out).into_owned()))
}

fn startup_state(broken: bool, disabled: bool, running: bool) -> StartupState {
    if broken {
        StartupState::Broken
    } else if disabled {
        StartupState::Disabled
    } else if running {
        StartupState::Running
    } else {
        StartupState::Enabled
    }
}

fn is_security_item(
    apps: &[InstalledApp],
    path: Option<&Path>,
    app_name: Option<&str>,
    label: &str,
) -> bool {
    let bundle_id = path
        .and_then(|p| apps.iter().find(|a| p.starts_with(&a.path)))
        .and_then(|a| a.bundle_id.as_deref());
    apps::is_security_vendor(bundle_id, app_name) || apps::is_security_vendor(Some(label), None)
}

fn metrics(pid: Option<u32>, pid_map: &HashMap<u32, (f32, u64)>) -> (Option<f32>, Option<u64>) {
    match pid.and_then(|p| pid_map.get(&p)) {
        Some(m) => (Some(m.0), Some(m.1)),
        None => (None, None),
    }
}

/// Scans all startup and background items. With `include_all` it asks for the administrator
/// password to read the Background Task Management records.
pub fn scan_startup(
    host: &dyn Host,
    layout: &Layout,
    ignore: &IgnoreStore,
    include_all: bool,
) -> StartupScan {
    let btm = if include_all {
        read_btm(host)
    } else {
        Btm::Skipped
    };
    scan_startup_with(host, layout, ignore, include_all, &btm)
}

/// Like `scan_startup`, with Background Task Management records read earlier.
pub fn scan_startup_with(
    host: &dyn Host,
    layout: &Layout,
    ignore: &IgnoreStore,
    include_all: bool,
    btm: &Btm,
) -> StartupScan {
    let mut scan = StartupScan::default();
    let uid = host_uid();
    let apps = apps::find_apps(layout);

    // Read running processes from `ps` to correlate pid, cpu, and memory
    let ps_out = host.run(
        "/bin/ps",
        &["-Aceo", "pid=,pcpu=,rss=,comm=", "-r"],
        TIMEOUT,
    );
    let processes = if ps_out.ok {
        parse_processes(&ps_out.stdout)
    } else {
        Vec::new()
    };
    let mut pid_map = HashMap::new();
    for p in &processes {
        pid_map.insert(p.pid, (p.cpu_percent, p.memory_bytes));
    }

    // Read disabled states from launchctl
    let disabled_gui_out = host.run(
        LAUNCHCTL,
        &["print-disabled", &format!("gui/{uid}")],
        TIMEOUT,
    );
    let mut disabled_map = if disabled_gui_out.ok {
        parse_print_disabled(&disabled_gui_out.stdout)
    } else {
        HashMap::new()
    };

    let disabled_sys_out = host.run(LAUNCHCTL, &["print-disabled", "system"], TIMEOUT);
    if disabled_sys_out.ok {
        disabled_map.extend(parse_print_disabled(&disabled_sys_out.stdout));
    }

    // Live services: the user's domain first, then the system domain (needs no root)
    let mut live_map: HashMap<String, (LaunchdServiceEntry, StartupKind)> = HashMap::new();
    for (domain, kind) in [
        (format!("gui/{uid}"), StartupKind::UserAgent),
        ("system".to_string(), StartupKind::Daemon),
    ] {
        let printed = host.run(LAUNCHCTL, &["print", &domain], TIMEOUT);
        if !printed.ok {
            continue;
        }
        for s in parse_launchctl_services(&printed.stdout) {
            if !s.label.starts_with("application.") {
                live_map.entry(s.label.clone()).or_insert((s, kind));
            }
        }
    }

    // 1. Plists from standard directories
    let plist_folders = [
        (layout.user("Library/LaunchAgents"), StartupKind::UserAgent),
        (
            layout.system("Library/LaunchAgents"),
            StartupKind::SystemAgent,
        ),
        (layout.system("Library/LaunchDaemons"), StartupKind::Daemon),
    ];

    let mut seen_labels: HashSet<String> = HashSet::new();
    let mut seen_plists: HashSet<PathBuf> = HashSet::new();

    for (folder, kind) in &plist_folders {
        if !folder.exists() {
            continue;
        }
        let Ok(entries) = fs::read_dir(folder) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "plist") {
                continue;
            }

            let Ok(val) = plist::Value::from_file(&path) else {
                continue;
            };
            let Some(dict) = val.into_dictionary() else {
                continue;
            };

            let label = dict
                .get("Label")
                .and_then(plist::Value::as_string)
                .map(str::to_string)
                .unwrap_or_else(|| {
                    path.file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string()
                });

            if label.starts_with("application.") {
                continue;
            }

            if !seen_labels.insert(label.clone()) {
                continue;
            }
            seen_plists.insert(path.clone());

            let is_apple = label.starts_with("com.apple.");
            if is_apple && !include_all {
                continue;
            }

            let prog_str = launch_program_from_dict(&dict);
            let program = prog_str.as_ref().map(PathBuf::from);

            let associated_ids: Vec<String> = dict
                .get("AssociatedBundleIdentifiers")
                .and_then(plist::Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(plist::Value::as_string)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();

            let (app_name, app_bundle) =
                match_app(program.as_deref(), &associated_ids, &label, &apps);

            let pid = live_map.remove(&label).and_then(|(l, _)| l.pid);
            let (cpu_percent, memory_bytes) = metrics(pid, &pid_map);

            let is_disabled = disabled_map.get(&label).copied().unwrap_or(false);
            let is_broken = program
                .as_ref()
                .is_some_and(|p| !p.exists() && p.is_absolute());
            let state = startup_state(is_broken, is_disabled, pid.is_some());

            let name = app_name.clone().unwrap_or_else(|| label_leaf_name(&label));

            let is_security =
                is_security_item(&apps, app_bundle.as_deref(), app_name.as_deref(), &label);

            let is_ignored = Safety::is_ignored_or_protected(&path, ignore)
                || program
                    .as_ref()
                    .is_some_and(|p| Safety::is_ignored_or_protected(p, ignore));

            scan.items.push(StartupItem {
                label: label.clone(),
                name,
                app_name,
                app_bundle,
                kind: *kind,
                state,
                program,
                plist_path: Some(path),
                pid,
                cpu_percent,
                memory_bytes,
                is_apple,
                is_security,
                is_ignored,
            });
        }
    }

    // 2. Background items registered without a plist on disk (from `live_map`)
    for (label, (service, kind)) in live_map {
        if label.starts_with("application.") {
            continue;
        }
        if !seen_labels.insert(label.clone()) {
            continue;
        }
        let is_apple = label.starts_with("com.apple.");
        if is_apple && !include_all {
            continue;
        }

        // Query detailed info from launchctl print <domain>/<label>
        let domain = domain_for(kind);
        let detail_out = host.run(LAUNCHCTL, &["print", &format!("{domain}/{label}")], TIMEOUT);
        let detail = if detail_out.ok {
            parse_service_detail(&detail_out.stdout)
        } else {
            LaunchdServiceDetail::default()
        };

        let pid = service.pid.or(detail.pid);
        let (cpu_percent, memory_bytes) = metrics(pid, &pid_map);

        let mut associated = Vec::new();
        if let Some(ref p_bid) = detail.parent_bundle_id {
            associated.push(p_bid.clone());
        }

        let (app_name, app_bundle) =
            match_app(detail.program.as_deref(), &associated, &label, &apps);

        let is_disabled = disabled_map.get(&label).copied().unwrap_or(false);
        let is_broken = detail
            .program
            .as_ref()
            .is_some_and(|p| !p.exists() && p.is_absolute());
        let running = pid.is_some() || detail.state.as_deref() == Some("running");
        let state = startup_state(is_broken, is_disabled, running);

        let name = app_name.clone().unwrap_or_else(|| label_leaf_name(&label));

        let is_security =
            is_security_item(&apps, app_bundle.as_deref(), app_name.as_deref(), &label);

        let is_ignored = detail
            .program
            .as_ref()
            .is_some_and(|p| Safety::is_ignored_or_protected(p, ignore));

        scan.items.push(StartupItem {
            label,
            name,
            app_name,
            app_bundle,
            kind,
            state,
            program: detail.program,
            plist_path: detail.path,
            pid,
            cpu_percent,
            memory_bytes,
            is_apple,
            is_security,
            is_ignored,
        });
    }

    // 3. Classic Login Items through osascript
    let osa_out = host.run(OSASCRIPT, &["-e", LIST_LOGIN_ITEMS_SCRIPT], TIMEOUT);
    if osa_out.ok {
        let login_items = parse_login_items(&osa_out.stdout);
        for (name, path) in login_items {
            let label = format!("loginitem.{name}");
            let is_broken = !path.exists();
            let is_running = processes.iter().any(|p| {
                p.name.eq_ignore_ascii_case(&name)
                    || path
                        .file_name()
                        .is_some_and(|f| p.name.eq_ignore_ascii_case(&f.to_string_lossy()))
            });
            let state = startup_state(is_broken, false, is_running);

            let app_name = apps
                .iter()
                .find(|a| a.path == path || path.starts_with(&a.path))
                .map(|a| a.name.clone())
                .or_else(|| Some(name.clone()));

            scan.items.push(StartupItem {
                is_security: is_security_item(&apps, Some(&path), app_name.as_deref(), &label),
                label,
                name: name.clone(),
                app_name,
                app_bundle: Some(path.clone()),
                kind: StartupKind::LoginItem,
                state,
                program: Some(path.clone()),
                plist_path: None,
                pid: None,
                cpu_percent: None,
                memory_bytes: None,
                is_apple: false,
                is_ignored: Safety::is_ignored_or_protected(&path, ignore),
            });
        }
    } else {
        scan.notes.push(
            "System Events login items unavailable: macOS Automation permission refused or not granted."
                .to_string(),
        );
    }

    // 4. Disabled classic login items from ~/.config/taert/disabled_login_items.json
    for disabled in load_disabled_login_items(layout) {
        if !scan
            .items
            .iter()
            .any(|it| it.kind == StartupKind::LoginItem && it.name == disabled.name)
        {
            let state = startup_state(!disabled.path.exists(), true, false);
            let label = format!("loginitem.{}", disabled.name);
            scan.items.push(StartupItem {
                is_security: is_security_item(
                    &apps,
                    Some(&disabled.path),
                    Some(&disabled.name),
                    &label,
                ),
                label,
                name: disabled.name.clone(),
                app_name: Some(disabled.name),
                app_bundle: Some(disabled.path.clone()),
                kind: StartupKind::LoginItem,
                state,
                program: Some(disabled.path.clone()),
                plist_path: None,
                pid: None,
                cpu_percent: None,
                memory_bytes: None,
                is_apple: false,
                is_ignored: Safety::is_ignored_or_protected(&disabled.path, ignore),
            });
        }
    }

    // 5. Background Task Management records that the sections above did not show
    match btm {
        Btm::Skipped => {}
        Btm::Unavailable(note) => scan.notes.push(note.clone()),
        Btm::Entries(entries) => {
            for entry in entries {
                let url_path = entry.url.as_deref().and_then(file_url_path);
                let listed = !seen_labels.insert(entry.identifier.clone())
                    || url_path.as_ref().is_some_and(|p| seen_plists.contains(p));
                if listed {
                    continue;
                }
                let name = entry
                    .name
                    .clone()
                    .unwrap_or_else(|| label_leaf_name(&entry.identifier));
                let is_login_item_row = scan
                    .items
                    .iter()
                    .any(|it| it.kind == StartupKind::LoginItem && it.name == name);
                if is_login_item_row {
                    continue;
                }
                let program = entry.executable.clone().or(url_path);
                let (app_name, app_bundle) =
                    match_app(program.as_deref(), &[], &entry.identifier, &apps);
                let is_apple = entry.identifier.starts_with("com.apple.")
                    || entry.developer.as_deref() == Some("Apple Inc.");
                scan.items.push(StartupItem {
                    is_security: is_security_item(
                        &apps,
                        app_bundle.as_deref(),
                        app_name.as_deref(),
                        &entry.identifier,
                    ),
                    label: entry.identifier.clone(),
                    name,
                    app_name,
                    app_bundle,
                    kind: StartupKind::Background,
                    state: startup_state(false, !entry.enabled, false),
                    is_ignored: program
                        .as_ref()
                        .is_some_and(|p| Safety::is_ignored_or_protected(p, ignore)),
                    program,
                    plist_path: None,
                    pid: None,
                    cpu_percent: None,
                    memory_bytes: None,
                    is_apple,
                });
            }
        }
    }

    scan.items.sort_by(|a, b| {
        a.kind
            .as_str()
            .cmp(b.kind.as_str())
            .then_with(|| a.name.cmp(&b.name))
    });

    scan
}

fn label_leaf_name(label: &str) -> String {
    label
        .split('.')
        .next_back()
        .filter(|s| !s.is_empty())
        .unwrap_or(label)
        .to_string()
}

/// Finds the item a command line names: a label, or a login item's name. A name that fits
/// several items is an error that lists them.
pub fn resolve_item(items: &[StartupItem], query: &str) -> Result<StartupItem, String> {
    let by_label: Vec<&StartupItem> = items.iter().filter(|it| it.label == query).collect();
    let found = if by_label.is_empty() {
        items
            .iter()
            .filter(|it| it.kind == StartupKind::LoginItem && it.name == query)
            .collect()
    } else {
        by_label
    };
    match found.as_slice() {
        [] => Err(format!("Startup item '{query}' not found")),
        [item] => Ok((*item).clone()),
        several => {
            let labels: Vec<&str> = several.iter().map(|it| it.label.as_str()).collect();
            Err(format!(
                "'{query}' fits several items: {}. Use a label.",
                labels.join(", ")
            ))
        }
    }
}

impl StartupItem {
    /// Switched off, yet the process is still there.
    pub fn is_disabled_but_running(&self) -> bool {
        self.state == StartupState::Disabled && self.pid.is_some()
    }

    /// The reason taert will not change this item, if there is one.
    pub fn read_only_reason(&self) -> Option<&'static str> {
        if self.is_apple || self.label.starts_with("com.apple.") {
            Some("Apple system services cannot be modified")
        } else if self.is_security {
            Some("Security agent services are read-only")
        } else if self.is_ignored {
            Some("Protected startup items are read-only")
        } else if self.kind == StartupKind::Background {
            Some(BACKGROUND_READ_ONLY)
        } else {
            None
        }
    }
}

fn domain_for(kind: StartupKind) -> String {
    match kind {
        StartupKind::Daemon => "system".to_string(),
        _ => format!("gui/{}", host_uid()),
    }
}

/// What launchd says about one service right now.
struct LaunchdState {
    disabled: bool,
    loaded: bool,
    pid: Option<u32>,
}

fn read_launchd_state(
    host: &dyn Host,
    kind: StartupKind,
    label: &str,
) -> Result<LaunchdState, String> {
    let domain = domain_for(kind);
    let again = |what: &str, out: &Output| {
        format!(
            "Could not read the launchd {what} again: {}",
            out.stderr.trim()
        )
    };
    let disabled = host.run(LAUNCHCTL, &["print-disabled", &domain], TIMEOUT);
    if !disabled.ok {
        return Err(again("disabled list", &disabled));
    }
    let printed = host.run(LAUNCHCTL, &["print", &domain], TIMEOUT);
    if !printed.ok {
        return Err(again("service list", &printed));
    }
    let service = parse_launchctl_services(&printed.stdout)
        .into_iter()
        .find(|s| s.label == label);
    Ok(LaunchdState {
        disabled: parse_print_disabled(&disabled.stdout).get(label) == Some(&true),
        loaded: service.is_some(),
        pid: service.and_then(|s| s.pid),
    })
}

fn login_item_listed(host: &dyn Host, name: &str) -> Result<bool, String> {
    let out = host.run(OSASCRIPT, &["-e", LIST_LOGIN_ITEMS_SCRIPT], TIMEOUT);
    if !out.ok {
        return Err(format!(
            "Could not read the login items again: {}",
            out.stderr.trim()
        ));
    }
    Ok(parse_login_items(&out.stdout)
        .iter()
        .any(|(listed, _)| listed == name))
}

/// Disables a startup item and reports the state launchd or System Events shows afterwards.
pub fn disable_item(
    host: &dyn Host,
    layout: &Layout,
    _ignore: &IgnoreStore,
    item: &StartupItem,
    dry_run: bool,
) -> Result<String, String> {
    if let Some(reason) = item.read_only_reason() {
        return Err(reason.to_string());
    }

    if dry_run {
        return Ok(format!(
            "Would disable {} ({})",
            item.label,
            item.kind.as_str()
        ));
    }

    let label = &item.label;
    match item.kind {
        StartupKind::LoginItem => {
            let Some(ref path) = item.program else {
                return Err("The login item has no saved path, so it was left in place".to_string());
            };
            // Save the path first: a failed save must leave the item where it is.
            let before = load_disabled_login_items(layout);
            let mut disabled = before.clone();
            if !disabled.iter().any(|d| d.name == item.name) {
                disabled.push(DisabledLoginItem {
                    name: item.name.clone(),
                    path: path.clone(),
                });
                save_disabled_login_items(layout, &disabled).map_err(|e| {
                    format!("Could not save the login item path, so it was left in place: {e}")
                })?;
            }
            let args = osascript_args(LOGIN_ITEM_SCRIPT, &[&item.name]);
            let out = host.run(OSASCRIPT, &args, TIMEOUT);
            if !out.ok {
                let _ = save_disabled_login_items(layout, &before);
                return Err(format!("Failed to remove login item: {}", out.stderr));
            }
            if login_item_listed(host, &item.name)? {
                return Err(format!("Login item {} is still listed", item.name));
            }
            Ok(format!("Disabled login item {}", item.name))
        }
        StartupKind::Daemon => {
            let quoted = shell_quote(label);
            let script = format!(
                "{LAUNCHCTL} disable system/{quoted} && {{ {LAUNCHCTL} bootout system/{quoted} || true; }}"
            );
            host.run_admin(&script, &format!("Disable system daemon {label}"))
                .map_err(|e| format!("Failed to disable daemon: {e}"))?;
            confirm_disabled(host, item, "")?;
            Ok(format!("Disabled system daemon {label}"))
        }
        StartupKind::UserAgent | StartupKind::SystemAgent => {
            let target = format!("{}/{label}", domain_for(item.kind));
            // 1. disable first so launchd can't restart it
            let dis_out = host.run(LAUNCHCTL, &["disable", &target], TIMEOUT);
            if !dis_out.ok {
                return Err(format!("launchctl disable failed: {}", dis_out.stderr));
            }
            // 2. bootout to stop it now
            let boot_out = host.run(LAUNCHCTL, &["bootout", &target], TIMEOUT);
            confirm_disabled(host, item, boot_out.stderr.trim())?;
            Ok(format!("Disabled service {label}"))
        }
        StartupKind::Background => Err(BACKGROUND_READ_ONLY.to_string()),
    }
}

fn confirm_disabled(
    host: &dyn Host,
    item: &StartupItem,
    bootout_error: &str,
) -> Result<(), String> {
    let state = read_launchd_state(host, item.kind, &item.label)?;
    if !state.disabled {
        return Err(format!(
            "launchd still lists {} as enabled after disable",
            item.label
        ));
    }
    if state.pid.is_some() {
        return Err(format!(
            "Disabled {}, but it is still running. {bootout_error}",
            item.label
        )
        .trim_end()
        .to_string());
    }
    Ok(())
}

/// Stops the running process of a launchd service and checks that it is gone.
pub fn stop_item(host: &dyn Host, item: &StartupItem) -> Result<String, String> {
    if let Some(reason) = item.read_only_reason() {
        return Err(reason.to_string());
    }
    let label = &item.label;
    match item.kind {
        StartupKind::Daemon => {
            let script = format!("{LAUNCHCTL} bootout system/{}", shell_quote(label));
            host.run_admin(&script, &format!("Stop system daemon {label}"))
                .map_err(|e| format!("Failed to stop daemon: {e}"))?;
        }
        StartupKind::UserAgent | StartupKind::SystemAgent => {
            let target = format!("{}/{label}", domain_for(item.kind));
            let out = host.run(LAUNCHCTL, &["bootout", &target], TIMEOUT);
            if !out.ok && read_launchd_state(host, item.kind, label)?.pid.is_some() {
                return Err(format!("launchctl bootout failed: {}", out.stderr.trim()));
            }
        }
        StartupKind::LoginItem | StartupKind::Background => {
            return Err("Only launchd services can be stopped".to_string());
        }
    }
    if read_launchd_state(host, item.kind, label)?.pid.is_some() {
        return Err(format!("{label} is still running"));
    }
    Ok(format!("Stopped {label}"))
}

/// Enables a startup item and reports the state launchd or System Events shows afterwards.
pub fn enable_item(
    host: &dyn Host,
    layout: &Layout,
    _ignore: &IgnoreStore,
    item: &StartupItem,
    dry_run: bool,
) -> Result<String, String> {
    if let Some(reason) = item.read_only_reason() {
        return Err(reason.to_string());
    }

    if dry_run {
        return Ok(format!(
            "Would enable {} ({})",
            item.label,
            item.kind.as_str()
        ));
    }

    let label = &item.label;
    match item.kind {
        StartupKind::LoginItem => {
            let Some(ref path) = item.program else {
                return Err("The login item has no saved path to restore".to_string());
            };
            let path = path.to_string_lossy();
            let args = osascript_args(ADD_LOGIN_ITEM_SCRIPT, &[&path, &item.name]);
            let out = host.run(OSASCRIPT, &args, TIMEOUT);
            if !out.ok {
                return Err(format!("Failed to enable login item: {}", out.stderr));
            }
            if !login_item_listed(host, &item.name)? {
                return Err(format!("Login item {} is not listed", item.name));
            }
            let mut disabled = load_disabled_login_items(layout);
            disabled.retain(|d| d.name != item.name);
            save_disabled_login_items(layout, &disabled).map_err(|e| {
                format!("Enabled, but could not update the saved login item list: {e}")
            })?;
            Ok(format!("Enabled login item {}", item.name))
        }
        StartupKind::Daemon => {
            let mut script = format!("{LAUNCHCTL} enable system/{}", shell_quote(label));
            if let Some(ref plist) = item.plist_path
                && plist.exists()
            {
                script.push_str(&format!(
                    " && {{ {LAUNCHCTL} bootstrap system {} || true; }}",
                    shell_quote(&plist.to_string_lossy())
                ));
            }
            host.run_admin(&script, &format!("Enable system daemon {label}"))
                .map_err(|e| format!("Failed to enable daemon: {e}"))?;
            if read_launchd_state(host, item.kind, label)?.disabled {
                return Err(format!("launchd still lists {label} as disabled"));
            }
            Ok(format!("Enabled system daemon {label}"))
        }
        StartupKind::UserAgent | StartupKind::SystemAgent => {
            let domain = domain_for(item.kind);
            let target = format!("{domain}/{label}");
            let en_out = host.run(LAUNCHCTL, &["enable", &target], TIMEOUT);
            if !en_out.ok {
                return Err(format!("launchctl enable failed: {}", en_out.stderr));
            }
            let mut boot_error = None;
            if let Some(ref plist) = item.plist_path
                && plist.exists()
            {
                let out = host.run(
                    LAUNCHCTL,
                    &["bootstrap", &domain, &plist.to_string_lossy()],
                    TIMEOUT,
                );
                if !out.ok {
                    boot_error = Some(out.stderr);
                }
            }
            let state = read_launchd_state(host, item.kind, label)?;
            if state.disabled {
                return Err(format!("launchd still lists {label} as disabled"));
            }
            if let Some(error) = boot_error
                && !state.loaded
            {
                return Err(format!("launchctl bootstrap failed: {}", error.trim()));
            }
            if state.loaded {
                Ok(format!("Enabled service {label}"))
            } else {
                Ok(format!(
                    "Enabled service {label} (will start at next login)"
                ))
            }
        }
        StartupKind::Background => Err(BACKGROUND_READ_ONLY.to_string()),
    }
}

/// Removes a broken plist file from `~/Library/LaunchAgents`.
pub fn remove_broken_plist(item: &StartupItem, ignore: &IgnoreStore) -> Result<(), String> {
    if item.state != StartupState::Broken {
        return Err("Only broken items can be removed".to_string());
    }
    let Some(ref plist) = item.plist_path else {
        return Err("Item has no plist file on disk".to_string());
    };
    if item.kind != StartupKind::UserAgent {
        return Err("Only user launch agents can be removed".to_string());
    }
    let scan_item = ScanItem::new(plist.clone(), 0, false, false);
    Cleaner::new(&MacHost, RemovalMode::Permanent)
        .remove_verified(&scan_item, |p| Safety::can_remove(p, ignore))
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::host::fake::FakeHost;
    use tempfile::tempdir;

    #[test]
    fn parses_print_disabled_fixture() {
        let output = r#"
            disabled services = {
                "com.docker.helper" => enabled
                "com.apple.ManagedClientAgent.enrollagent" => disabled
            }
        "#;
        let map = parse_print_disabled(output);
        assert_eq!(map.get("com.docker.helper"), Some(&false));
        assert_eq!(
            map.get("com.apple.ManagedClientAgent.enrollagent"),
            Some(&true)
        );
        assert_eq!(map.get("nonexistent"), None);
    }

    #[test]
    fn parses_launchctl_services_fixture() {
        let output = r#"
            gui/501 = {
                services = {
                         855      - 	application.com.knollsoft.Hookshot
                           0      0 	ch.protonmail.drive.launcher
                         724      - 	com.apple.syncdefaultsd
                }
            }
        "#;
        let services = parse_launchctl_services(output);
        assert_eq!(services.len(), 3);
        assert_eq!(services[0].pid, Some(855));
        assert_eq!(services[0].label, "application.com.knollsoft.Hookshot");
        assert_eq!(services[1].pid, None);
        assert_eq!(services[1].last_exit_code, Some(0));
        assert_eq!(services[1].label, "ch.protonmail.drive.launcher");
    }

    #[test]
    fn parses_service_detail_fixture() {
        let output = r#"
            gui/501/com.docker.helper = {
                path = /Library/PrivilegedHelperTools/com.docker.helper
                program = /Library/PrivilegedHelperTools/com.docker.helper
                parent bundle identifier = com.docker.docker
                state = running
                pid = 1234
            }
        "#;
        let detail = parse_service_detail(output);
        assert_eq!(
            detail.path,
            Some(PathBuf::from(
                "/Library/PrivilegedHelperTools/com.docker.helper"
            ))
        );
        assert_eq!(
            detail.program,
            Some(PathBuf::from(
                "/Library/PrivilegedHelperTools/com.docker.helper"
            ))
        );
        assert_eq!(
            detail.parent_bundle_id,
            Some("com.docker.docker".to_string())
        );
        assert_eq!(detail.state, Some("running".to_string()));
        assert_eq!(detail.pid, Some(1234));
    }

    #[test]
    fn rows_merge_the_plist_and_the_live_service_for_one_label() {
        let temp = tempdir().unwrap();
        let home = temp.path().join("home");
        let root = temp.path().join("root");
        let agents = home.join("Library/LaunchAgents");
        fs::create_dir_all(&agents).unwrap();

        let plist_path = agents.join("com.example.agent.plist");
        let dict = plist::Value::Dictionary(
            [
                (
                    "Label".to_string(),
                    plist::Value::String("com.example.agent".to_string()),
                ),
                (
                    "Program".to_string(),
                    plist::Value::String("/bin/sleep".to_string()),
                ),
            ]
            .into_iter()
            .collect(),
        );
        dict.to_file_xml(&plist_path).unwrap();

        let layout = Layout {
            home,
            root,
            user_cache: None,
        };

        let host = FakeHost::default();
        host.reply(
            "/bin/launchctl print gui/",
            "gui/501 = {\nservices = {\n 4242 - com.example.agent\n}\n}\n",
        );
        host.reply(
            "/bin/launchctl print-disabled gui/",
            "disabled services = {\n \"com.example.agent\" => enabled\n}\n",
        );
        host.reply("/bin/ps", " 4242 12.5 40960 sleep\n");

        let ignore = IgnoreStore::default();
        let scan = scan_startup(&host, &layout, &ignore, false);

        let item = scan
            .items
            .iter()
            .find(|it| it.label == "com.example.agent")
            .expect("should find agent");
        assert_eq!(item.state, StartupState::Running);
        assert_eq!(item.pid, Some(4242));
        assert_eq!(item.cpu_percent, Some(12.5));
        assert_eq!(item.memory_bytes, Some(40960 * 1024));
        assert_eq!(item.plist_path, Some(plist_path));
    }

    #[test]
    fn disable_runs_before_bootout() {
        let host = FakeHost::idle();
        reply_disabled(&host, "gui/", "com.test.agent");
        host.reply("/bin/launchctl", "");
        let temp = tempdir().unwrap();
        let layout = Layout {
            home: temp.path().to_path_buf(),
            root: temp.path().to_path_buf(),
            user_cache: None,
        };
        let ignore = IgnoreStore::default();

        let item = StartupItem {
            label: "com.test.agent".to_string(),
            name: "Test Agent".to_string(),
            app_name: None,
            app_bundle: None,
            kind: StartupKind::UserAgent,
            state: StartupState::Running,
            program: Some(PathBuf::from("/bin/sleep")),
            plist_path: None,
            pid: Some(100),
            cpu_percent: None,
            memory_bytes: None,
            is_apple: false,
            is_security: false,
            is_ignored: false,
        };

        let res = disable_item(&host, &layout, &ignore, &item, false);
        assert!(res.is_ok());

        let calls = host.calls.lock().unwrap();
        let disable_idx = calls
            .iter()
            .position(|c| c.starts_with("/bin/launchctl disable "))
            .expect("called disable");
        let bootout_idx = calls
            .iter()
            .position(|c| c.starts_with("/bin/launchctl bootout "))
            .expect("called bootout");
        assert!(disable_idx < bootout_idx, "disable must run before bootout");
    }

    #[test]
    fn daemon_goes_through_run_admin_user_agent_does_not() {
        let host = FakeHost::idle();
        reply_disabled(&host, "system", "com.test.daemon");
        host.reply("/bin/launchctl print system", "");
        let temp = tempdir().unwrap();
        let layout = Layout {
            home: temp.path().to_path_buf(),
            root: temp.path().to_path_buf(),
            user_cache: None,
        };
        let ignore = IgnoreStore::default();

        let daemon = StartupItem {
            label: "com.test.daemon".to_string(),
            name: "Test Daemon".to_string(),
            app_name: None,
            app_bundle: None,
            kind: StartupKind::Daemon,
            state: StartupState::Running,
            program: Some(PathBuf::from("/bin/sleep")),
            plist_path: None,
            pid: Some(101),
            cpu_percent: None,
            memory_bytes: None,
            is_apple: false,
            is_security: false,
            is_ignored: false,
        };

        let res = disable_item(&host, &layout, &ignore, &daemon, false);
        assert!(res.is_ok());
        assert!(!host.admin_scripts.lock().unwrap().is_empty());

        let user_agent = StartupItem {
            label: "com.test.agent".to_string(),
            name: "Test Agent".to_string(),
            app_name: None,
            app_bundle: None,
            kind: StartupKind::UserAgent,
            state: StartupState::Running,
            program: Some(PathBuf::from("/bin/sleep")),
            plist_path: None,
            pid: Some(102),
            cpu_percent: None,
            memory_bytes: None,
            is_apple: false,
            is_security: false,
            is_ignored: false,
        };

        let host2 = FakeHost::idle();
        reply_disabled(&host2, "gui/", "com.test.agent");
        host2.reply("/bin/launchctl", "");
        let res2 = disable_item(&host2, &layout, &ignore, &user_agent, false);
        assert!(res2.is_ok());
        assert!(host2.admin_scripts.lock().unwrap().is_empty());
    }

    #[test]
    fn com_apple_label_is_refused() {
        let host = FakeHost::idle();
        let temp = tempdir().unwrap();
        let layout = Layout {
            home: temp.path().to_path_buf(),
            root: temp.path().to_path_buf(),
            user_cache: None,
        };
        let ignore = IgnoreStore::default();

        let apple_item = StartupItem {
            label: "com.apple.syslogd".to_string(),
            name: "syslogd".to_string(),
            app_name: None,
            app_bundle: None,
            kind: StartupKind::Daemon,
            state: StartupState::Running,
            program: Some(PathBuf::from("/usr/libexec/syslogd")),
            plist_path: None,
            pid: Some(103),
            cpu_percent: None,
            memory_bytes: None,
            is_apple: false,
            is_security: false,
            is_ignored: false,
        };

        let res = disable_item(&host, &layout, &ignore, &apple_item, false);
        assert!(res.is_err());
        let res_en = enable_item(&host, &layout, &ignore, &apple_item, false);
        assert!(res_en.is_err());
        assert!(host.admin_scripts.lock().unwrap().is_empty());
    }

    #[test]
    fn plist_stays_on_disk_after_disable() {
        let host = FakeHost::idle();
        reply_disabled(&host, "gui/", "com.test.agent");
        host.reply("/bin/launchctl", "");
        let temp = tempdir().unwrap();
        let plist_path = temp.path().join("com.test.agent.plist");
        fs::write(&plist_path, b"<plist></plist>").unwrap();

        let layout = Layout {
            home: temp.path().to_path_buf(),
            root: temp.path().to_path_buf(),
            user_cache: None,
        };
        let ignore = IgnoreStore::default();

        let item = StartupItem {
            label: "com.test.agent".to_string(),
            name: "Test Agent".to_string(),
            app_name: None,
            app_bundle: None,
            kind: StartupKind::UserAgent,
            state: StartupState::Running,
            program: Some(PathBuf::from("/bin/sleep")),
            plist_path: Some(plist_path.clone()),
            pid: Some(104),
            cpu_percent: None,
            memory_bytes: None,
            is_apple: false,
            is_security: false,
            is_ignored: false,
        };

        let res = disable_item(&host, &layout, &ignore, &item, false);
        assert!(res.is_ok());
        assert!(plist_path.exists(), "plist must not be deleted on disable");
    }

    #[test]
    fn sfltool_never_runs_without_all() {
        let host = FakeHost::idle();
        let temp = tempdir().unwrap();
        let layout = Layout {
            home: temp.path().to_path_buf(),
            root: temp.path().to_path_buf(),
            user_cache: None,
        };
        let ignore = IgnoreStore::default();

        let _ = scan_startup(&host, &layout, &ignore, false);

        let calls = host.calls.lock().unwrap();
        assert!(
            !calls.iter().any(|c| c.contains("sfltool")),
            "sfltool must not run without --all"
        );
        let admin_calls = host.admin_scripts.lock().unwrap();
        assert!(
            !admin_calls.iter().any(|c| c.contains("sfltool")),
            "sfltool admin must not run without --all"
        );
    }

    #[test]
    fn row_with_missing_program_is_broken() {
        let temp = tempdir().unwrap();
        let agents = temp.path().join("Library/LaunchAgents");
        fs::create_dir_all(&agents).unwrap();

        let plist_path = agents.join("com.missing.agent.plist");
        let dict = plist::Value::Dictionary(
            [
                (
                    "Label".to_string(),
                    plist::Value::String("com.missing.agent".to_string()),
                ),
                (
                    "Program".to_string(),
                    plist::Value::String("/nonexistent/binary".to_string()),
                ),
            ]
            .into_iter()
            .collect(),
        );
        dict.to_file_xml(&plist_path).unwrap();

        let layout = Layout {
            home: temp.path().to_path_buf(),
            root: temp.path().to_path_buf(),
            user_cache: None,
        };
        let host = FakeHost::idle();
        let ignore = IgnoreStore::default();

        let scan = scan_startup(&host, &layout, &ignore, false);
        let item = scan
            .items
            .iter()
            .find(|it| it.label == "com.missing.agent")
            .expect("found item");
        assert_eq!(item.state, StartupState::Broken);
    }

    #[test]
    fn refused_automation_prompt_leaves_other_rows_in_place_with_a_note() {
        let temp = tempdir().unwrap();
        let agents = temp.path().join("Library/LaunchAgents");
        fs::create_dir_all(&agents).unwrap();

        let plist_path = agents.join("com.test.agent.plist");
        let dict = plist::Value::Dictionary(
            [
                (
                    "Label".to_string(),
                    plist::Value::String("com.test.agent".to_string()),
                ),
                (
                    "Program".to_string(),
                    plist::Value::String("/bin/sleep".to_string()),
                ),
            ]
            .into_iter()
            .collect(),
        );
        dict.to_file_xml(&plist_path).unwrap();

        let layout = Layout {
            home: temp.path().to_path_buf(),
            root: temp.path().to_path_buf(),
            user_cache: None,
        };

        let host = FakeHost::idle();
        host.fail(
            "/usr/bin/osascript",
            "execution error: Not authorized to send Apple events to System Events (-1743)",
        );

        let ignore = IgnoreStore::default();
        let scan = scan_startup(&host, &layout, &ignore, false);

        assert!(
            scan.items.iter().any(|it| it.label == "com.test.agent"),
            "other rows stay"
        );
        assert!(
            scan.notes
                .iter()
                .any(|n| n.contains("Automation permission")),
            "note added"
        );
    }

    #[test]
    fn enable_runs_enable_command() {
        let host = FakeHost::idle();
        host.reply("/bin/launchctl", "");
        let temp = tempdir().unwrap();
        let layout = Layout {
            home: temp.path().to_path_buf(),
            root: temp.path().to_path_buf(),
            user_cache: None,
        };
        let ignore = IgnoreStore::default();

        let item = StartupItem {
            label: "com.test.agent".to_string(),
            name: "Test Agent".to_string(),
            app_name: None,
            app_bundle: None,
            kind: StartupKind::UserAgent,
            state: StartupState::Disabled,
            program: Some(PathBuf::from("/bin/sleep")),
            plist_path: None,
            pid: None,
            cpu_percent: None,
            memory_bytes: None,
            is_apple: false,
            is_security: false,
            is_ignored: false,
        };

        let res = enable_item(&host, &layout, &ignore, &item, false);
        assert!(res.is_ok());

        let calls = host.calls.lock().unwrap();
        assert!(calls.iter().any(|c| c.contains("enable")));
    }

    #[test]
    fn remove_broken_plist_only_allows_broken_user_agents() {
        let temp = tempdir().unwrap();
        let plist_path = temp.path().join("broken.plist");
        fs::write(&plist_path, b"<plist></plist>").unwrap();
        let ignore = IgnoreStore::default();

        let mut item = StartupItem {
            label: "com.test.broken".to_string(),
            name: "Broken Agent".to_string(),
            app_name: None,
            app_bundle: None,
            kind: StartupKind::UserAgent,
            state: StartupState::Running, // not broken
            program: None,
            plist_path: Some(plist_path.clone()),
            pid: None,
            cpu_percent: None,
            memory_bytes: None,
            is_apple: false,
            is_security: false,
            is_ignored: false,
        };

        // Running item cannot be removed
        assert!(remove_broken_plist(&item, &ignore).is_err());

        // Daemon cannot be removed even if broken
        item.state = StartupState::Broken;
        item.kind = StartupKind::Daemon;
        assert!(remove_broken_plist(&item, &ignore).is_err());
    }

    fn reply_disabled(host: &FakeHost, domain: &str, label: &str) {
        host.reply(
            &format!("/bin/launchctl print-disabled {domain}"),
            &format!("disabled services = {{\n \"{label}\" => disabled\n}}\n"),
        );
    }

    fn layout_in(temp: &tempfile::TempDir) -> Layout {
        Layout {
            home: temp.path().join("home"),
            root: temp.path().join("root"),
            user_cache: None,
        }
    }

    fn test_item(label: &str, kind: StartupKind, state: StartupState) -> StartupItem {
        StartupItem {
            label: label.to_string(),
            name: label.to_string(),
            app_name: None,
            app_bundle: None,
            kind,
            state,
            program: Some(PathBuf::from("/bin/sleep")),
            plist_path: None,
            pid: None,
            cpu_percent: None,
            memory_bytes: None,
            is_apple: false,
            is_security: false,
            is_ignored: false,
        }
    }

    fn write_plist(folder: &Path, label: &str, program: &str) {
        fs::create_dir_all(folder).unwrap();
        let mut dict = plist::Dictionary::new();
        dict.insert("Label".into(), label.into());
        dict.insert("Program".into(), program.into());
        plist::Value::Dictionary(dict)
            .to_file_xml(folder.join(format!("{label}.plist")))
            .unwrap();
    }

    #[test]
    fn the_background_dump_never_goes_through_a_path_the_user_controls() {
        let temp = tempdir().unwrap();
        let layout = layout_in(&temp);
        let host = FakeHost::idle();

        scan_startup(&host, &layout, &IgnoreStore::default(), true);

        let scripts = host.admin_scripts.lock().unwrap();
        let script = scripts.iter().find(|s| s.contains("sfltool")).unwrap();
        assert!(!script.contains('>'), "no redirect: {script}");
        assert!(!script.contains(&*layout.home.to_string_lossy()));
    }

    #[test]
    fn a_daemon_that_stays_enabled_after_disable_is_a_failure() {
        let temp = tempdir().unwrap();
        let host = FakeHost::idle();
        host.reply(
            "/bin/launchctl print-disabled system",
            "disabled services = {\n \"com.test.daemon\" => enabled\n}\n",
        );
        let item = test_item(
            "com.test.daemon",
            StartupKind::Daemon,
            StartupState::Running,
        );

        let res = disable_item(
            &host,
            &layout_in(&temp),
            &IgnoreStore::default(),
            &item,
            false,
        );

        assert!(res.is_err(), "{res:?}");
    }

    #[test]
    fn daemons_come_from_print_system_without_all() {
        let temp = tempdir().unwrap();
        let layout = layout_in(&temp);
        write_plist(
            &layout.system("Library/LaunchDaemons"),
            "com.test.plistd",
            "/bin/sleep",
        );
        let host = FakeHost::idle();
        host.reply(
            "/bin/launchctl print system",
            "system = {\n services = {\n 77 - com.test.plistd\n 78 - com.test.bundled\n }\n}\n",
        );
        host.reply("/bin/ps", " 77 3.0 1024 plistd\n 78 1.0 2048 bundled\n");

        let scan = scan_startup(&host, &layout, &IgnoreStore::default(), false);

        let by_label = |l: &str| scan.items.iter().find(|it| it.label == l).unwrap();
        assert_eq!(by_label("com.test.plistd").state, StartupState::Running);
        assert_eq!(by_label("com.test.plistd").pid, Some(77));
        let bundled = by_label("com.test.bundled");
        assert_eq!(bundled.kind, StartupKind::Daemon);
        assert_eq!(bundled.pid, Some(78));
    }

    #[test]
    fn a_failed_save_keeps_the_login_item() {
        let temp = tempdir().unwrap();
        let layout = layout_in(&temp);
        fs::create_dir_all(&layout.home).unwrap();
        fs::write(
            layout.home.join(".config"),
            "a file where a folder should be",
        )
        .unwrap();
        let host = FakeHost::idle();
        let mut item = test_item(
            "loginitem.Demo",
            StartupKind::LoginItem,
            StartupState::Enabled,
        );
        item.name = "Demo".to_string();
        item.program = Some(PathBuf::from("/Applications/Demo.app"));

        let res = disable_item(&host, &layout, &IgnoreStore::default(), &item, false);

        assert!(res.is_err());
        assert!(!host.ran("/usr/bin/osascript"), "the item must stay");
    }

    #[test]
    fn a_security_login_item_is_read_only() {
        let temp = tempdir().unwrap();
        let layout = layout_in(&temp);
        let host = FakeHost::idle();
        host.reply(
            "/usr/bin/osascript",
            "CrowdStrike Falcon\t/Applications/Falcon.app\n",
        );

        let scan = scan_startup(&host, &layout, &IgnoreStore::default(), false);

        let item = scan.items.iter().find(|it| it.name == "CrowdStrike Falcon");
        let item = item.expect("login item listed");
        assert!(item.is_security);
        let res = disable_item(&host, &layout, &IgnoreStore::default(), item, false);
        assert!(res.is_err());
    }

    #[test]
    fn a_path_with_a_backslash_reaches_applescript_as_an_argument() {
        let temp = tempdir().unwrap();
        let host = FakeHost::idle();
        host.reply("/usr/bin/osascript", "");
        let path = "/Applications/Back\\slash \"Q\".app";
        let mut item = test_item(
            "loginitem.Demo",
            StartupKind::LoginItem,
            StartupState::Disabled,
        );
        item.name = "Demo".to_string();
        item.program = Some(PathBuf::from(path));

        let _ = enable_item(
            &host,
            &layout_in(&temp),
            &IgnoreStore::default(),
            &item,
            false,
        );

        let calls = host.calls.lock().unwrap();
        let make = calls
            .iter()
            .find(|c| c.contains("make login item"))
            .expect("script ran");
        assert!(make.contains(&format!("-- {path}")), "{make}");
        let before_args = make.split(" -- ").next().unwrap();
        assert!(
            !before_args.contains("Back"),
            "path must not be in the script"
        );
    }

    #[test]
    fn a_disabled_item_that_still_runs_keeps_both_facts() {
        let temp = tempdir().unwrap();
        let layout = layout_in(&temp);
        write_plist(
            &layout.user("Library/LaunchAgents"),
            "com.test.stuck",
            "/bin/sleep",
        );
        let host = FakeHost::idle();
        host.reply(
            "/bin/launchctl print-disabled gui/",
            "disabled services = {\n \"com.test.stuck\" => disabled\n}\n",
        );
        host.reply(
            "/bin/launchctl print gui/",
            "gui/501 = {\n services = {\n 55 - com.test.stuck\n }\n}\n",
        );

        let scan = scan_startup(&host, &layout, &IgnoreStore::default(), false);

        let item = scan.items.iter().find(|it| it.label == "com.test.stuck");
        let item = item.unwrap();
        assert_eq!(item.state, StartupState::Disabled);
        assert_eq!(item.pid, Some(55));
    }

    // The sample is rebuilt from the field layout in the DumpBTM README
    // (https://github.com/objective-see/DumpBTM) and in "In the background: Identification"
    // (https://eclecticlight.co/2026/02/20/in-the-background-identification/).
    // No real dump could be taken, so the names, ids and UUIDs are made up.
    const BTM_FIXTURE: &str = include_str!("../../tests/fixtures/sfltool_dumpbtm.txt");

    #[test]
    fn parses_the_background_task_dump_and_skips_lines_it_does_not_know() {
        let entries = parse_btm(BTM_FIXTURE);

        assert_eq!(entries.len(), 2);
        let sync = &entries[0];
        assert_eq!(sync.identifier, "com.example.sync.daemon");
        assert_eq!(sync.name.as_deref(), Some("Example Sync"));
        assert_eq!(sync.developer.as_deref(), Some("Example Software Ltd"));
        assert!(sync.enabled);
        assert_eq!(
            sync.url.as_deref(),
            Some("file:///Library/LaunchDaemons/com.example.sync.daemon.plist")
        );
        assert_eq!(
            sync.executable,
            Some(PathBuf::from(
                "/Applications/Example Sync.app/Contents/MacOS/syncd"
            ))
        );
        assert!(!entries[1].enabled);
        assert_eq!(entries[1].identifier, "com.example.helper");
    }

    #[test]
    fn dump_entries_become_read_only_rows_without_repeating_known_ones() {
        let temp = tempdir().unwrap();
        let layout = layout_in(&temp);
        write_plist(
            &layout.system("Library/LaunchDaemons"),
            "com.example.sync.daemon",
            "/bin/sleep",
        );
        let host = FakeHost::idle();
        *host.admin_stdout.lock().unwrap() = BTM_FIXTURE.to_string();
        let ignore = IgnoreStore::default();

        let scan = scan_startup(&host, &layout, &ignore, true);

        let rows: Vec<_> = scan
            .items
            .iter()
            .filter(|it| it.label.starts_with("com.example."))
            .collect();
        assert_eq!(rows.len(), 2, "{rows:?}");
        let helper = rows.iter().find(|it| it.label == "com.example.helper");
        let helper = helper.unwrap();
        assert_eq!(helper.kind, StartupKind::Background);
        assert_eq!(helper.state, StartupState::Disabled);
        assert!(disable_item(&host, &layout, &ignore, helper, false).is_err());
        assert!(enable_item(&host, &layout, &ignore, helper, false).is_err());
    }

    #[test]
    fn output_the_parser_cannot_read_gives_a_note_and_no_rows() {
        let temp = tempdir().unwrap();
        let host = FakeHost::idle();
        *host.admin_stdout.lock().unwrap() = "something from a future macOS\n".to_string();

        let scan = scan_startup(&host, &layout_in(&temp), &IgnoreStore::default(), true);

        assert!(
            scan.items
                .iter()
                .all(|it| it.kind != StartupKind::Background)
        );
        assert!(
            scan.notes.iter().any(|n| n.contains("sfltool")),
            "{:?}",
            scan.notes
        );
    }

    #[test]
    fn a_refused_password_gives_a_note() {
        let temp = tempdir().unwrap();
        let host = FakeHost::idle();
        *host.admin_result.lock().unwrap() =
            Some("The administrator password was not given".into());

        let scan = scan_startup(&host, &layout_in(&temp), &IgnoreStore::default(), true);

        assert!(
            scan.notes
                .iter()
                .any(|n| n.contains("password was not given"))
        );
    }

    #[test]
    fn a_name_that_fits_two_items_is_an_error_that_lists_them() {
        let mut a = test_item(
            "loginitem.Twin",
            StartupKind::LoginItem,
            StartupState::Enabled,
        );
        a.name = "Twin".to_string();
        let b = a.clone();
        let daemon = test_item("Twin", StartupKind::Daemon, StartupState::Running);

        let err = resolve_item(&[a.clone(), b], "Twin")
            .map(|_| ())
            .unwrap_err();
        assert!(err.contains("fits several items") && err.contains("loginitem.Twin"));

        let chosen = resolve_item(&[a, daemon.clone()], "Twin").unwrap();
        assert_eq!(chosen, daemon, "an exact label wins over a login item name");
        assert!(resolve_item(&[], "Twin").is_err());
    }

    #[test]
    fn an_app_name_shared_by_a_login_item_and_a_daemon_does_not_pick_the_daemon() {
        let mut login = test_item(
            "loginitem.Acme",
            StartupKind::LoginItem,
            StartupState::Enabled,
        );
        login.name = "Acme".to_string();
        login.app_name = Some("Acme".to_string());
        let mut daemon = test_item(
            "com.acme.daemon",
            StartupKind::Daemon,
            StartupState::Running,
        );
        daemon.name = "Acme".to_string();
        daemon.app_name = Some("Acme".to_string());

        let chosen = resolve_item(&[daemon, login.clone()], "Acme").unwrap();

        assert_eq!(chosen, login);
    }

    #[test]
    fn stop_boots_out_a_disabled_agent_and_checks_that_it_is_gone() {
        let temp = tempdir().unwrap();
        let _ = temp;
        let host = FakeHost::idle();
        host.reply("/bin/launchctl", "");
        let mut item = test_item(
            "com.test.stuck",
            StartupKind::UserAgent,
            StartupState::Disabled,
        );
        item.pid = Some(55);

        let res = stop_item(&host, &item);

        assert!(res.is_ok(), "{res:?}");
        assert!(host.ran("/bin/launchctl bootout gui/"));
    }

    #[test]
    fn stop_fails_when_the_process_is_still_there() {
        let host = FakeHost::idle();
        host.reply(
            "/bin/launchctl print gui/",
            "gui/501 = {\n services = {\n 55 - com.test.stuck\n }\n}\n",
        );
        host.reply("/bin/launchctl", "");
        let mut item = test_item(
            "com.test.stuck",
            StartupKind::UserAgent,
            StartupState::Disabled,
        );
        item.pid = Some(55);

        assert!(stop_item(&host, &item).is_err());
    }

    #[test]
    fn stopping_a_daemon_goes_through_admin_rights() {
        let host = FakeHost::idle();
        host.reply("/bin/launchctl", "");
        let mut item = test_item(
            "com.test.daemon",
            StartupKind::Daemon,
            StartupState::Disabled,
        );
        item.pid = Some(9);

        assert!(stop_item(&host, &item).is_ok());
        let scripts = host.admin_scripts.lock().unwrap();
        assert!(scripts[0].contains("bootout system/'com.test.daemon'"));
    }

    #[test]
    fn an_agent_that_is_still_listed_as_disabled_after_enable_is_a_failure() {
        let temp = tempdir().unwrap();
        let host = FakeHost::idle();
        reply_disabled(&host, "gui/", "com.test.agent");
        host.reply("/bin/launchctl", "");
        let item = test_item(
            "com.test.agent",
            StartupKind::UserAgent,
            StartupState::Disabled,
        );

        let res = enable_item(
            &host,
            &layout_in(&temp),
            &IgnoreStore::default(),
            &item,
            false,
        );

        assert!(res.is_err());
    }

    #[test]
    fn a_login_item_that_is_still_listed_after_removal_is_a_failure() {
        let temp = tempdir().unwrap();
        let layout = layout_in(&temp);
        let host = FakeHost::idle();
        host.reply("/usr/bin/osascript", "Demo\t/Applications/Demo.app\n");
        let mut item = test_item(
            "loginitem.Demo",
            StartupKind::LoginItem,
            StartupState::Enabled,
        );
        item.name = "Demo".to_string();
        item.program = Some(PathBuf::from("/Applications/Demo.app"));

        let res = disable_item(&host, &layout, &IgnoreStore::default(), &item, false);

        assert!(res.is_err());
    }

    #[test]
    fn a_login_item_is_saved_before_it_is_removed() {
        let temp = tempdir().unwrap();
        let layout = layout_in(&temp);
        let host = FakeHost::idle();
        host.reply("/usr/bin/osascript", "");
        let mut item = test_item(
            "loginitem.Demo",
            StartupKind::LoginItem,
            StartupState::Enabled,
        );
        item.name = "Demo".to_string();
        item.program = Some(PathBuf::from("/Applications/Demo.app"));
        let saved = layout.user(".config/taert/disabled_login_items.json");
        let seen = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = seen.clone();
        host.when("/usr/bin/osascript -e on run", move || {
            flag.store(saved.exists(), std::sync::atomic::Ordering::SeqCst);
        });

        let res = disable_item(&host, &layout, &IgnoreStore::default(), &item, false);

        assert!(res.is_ok(), "{res:?}");
        assert!(seen.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(load_disabled_login_items(&layout).len(), 1);
    }
}

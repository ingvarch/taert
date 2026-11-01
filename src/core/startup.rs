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
use super::cleaner::Cleaner;
use super::host::{Host, shell_quote};
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
}

impl StartupKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::LoginItem => "login item",
            Self::UserAgent => "user agent",
            Self::SystemAgent => "system agent",
            Self::Daemon => "daemon",
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

/// Scans all startup and background items.
pub fn scan_startup(
    host: &dyn Host,
    layout: &Layout,
    ignore: &IgnoreStore,
    include_all: bool,
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
        "/bin/launchctl",
        &["print-disabled", &format!("gui/{uid}")],
        TIMEOUT,
    );
    let mut disabled_map = if disabled_gui_out.ok {
        parse_print_disabled(&disabled_gui_out.stdout)
    } else {
        HashMap::new()
    };

    let disabled_sys_out = host.run("/bin/launchctl", &["print-disabled", "system"], TIMEOUT);
    if disabled_sys_out.ok {
        disabled_map.extend(parse_print_disabled(&disabled_sys_out.stdout));
    }

    // Read live services from launchctl print gui/<uid>
    let print_gui = host.run("/bin/launchctl", &["print", &format!("gui/{uid}")], TIMEOUT);
    let mut live_map: HashMap<String, LaunchdServiceEntry> = HashMap::new();
    if print_gui.ok {
        for s in parse_launchctl_services(&print_gui.stdout) {
            if !s.label.starts_with("application.") {
                live_map.insert(s.label.clone(), s);
            }
        }
    }

    // Read live services from launchctl print system (for daemons)
    if include_all {
        let print_sys = host.run("/bin/launchctl", &["print", "system"], TIMEOUT);
        if print_sys.ok {
            for s in parse_launchctl_services(&print_sys.stdout) {
                if !s.label.starts_with("application.") {
                    live_map.entry(s.label.clone()).or_insert(s);
                }
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

            let live = live_map.remove(&label);
            let pid = live.as_ref().and_then(|l| l.pid);
            let (cpu_percent, memory_bytes) = match pid {
                Some(p) => {
                    let metrics = pid_map.get(&p);
                    (metrics.map(|m| m.0), metrics.map(|m| m.1))
                }
                None => (None, None),
            };

            let is_disabled = disabled_map.get(&label).copied().unwrap_or(false);
            let is_broken = program
                .as_ref()
                .is_some_and(|p| !p.exists() && p.is_absolute());

            let state = if is_broken {
                StartupState::Broken
            } else if pid.is_some() {
                StartupState::Running
            } else if is_disabled {
                StartupState::Disabled
            } else {
                StartupState::Enabled
            };

            let name = app_name.clone().unwrap_or_else(|| label_leaf_name(&label));

            let is_security = apps::is_security_vendor(
                app_bundle
                    .as_ref()
                    .and_then(|_| apps.iter().find(|a| Some(&a.path) == app_bundle.as_ref()))
                    .and_then(|a| a.bundle_id.as_deref()),
                app_name.as_deref(),
            ) || apps::is_security_vendor(Some(&label), None);

            let is_ignored = Safety::is_ignored_or_protected(&path, ignore)
                || program
                    .as_ref()
                    .is_some_and(|p| Safety::is_ignored_or_protected(p, ignore));

            let item = StartupItem {
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
            };

            scan.items.push(item);
        }
    }

    // 2. Background items registered without a plist on disk (from `live_map`)
    for (label, service) in live_map {
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

        // Query detailed info from launchctl print gui/<uid>/<label>
        let detail_out = host.run(
            "/bin/launchctl",
            &["print", &format!("gui/{uid}/{label}")],
            TIMEOUT,
        );
        let detail = if detail_out.ok {
            parse_service_detail(&detail_out.stdout)
        } else {
            LaunchdServiceDetail::default()
        };

        let pid = service.pid.or(detail.pid);
        let (cpu_percent, memory_bytes) = match pid {
            Some(p) => {
                let metrics = pid_map.get(&p);
                (metrics.map(|m| m.0), metrics.map(|m| m.1))
            }
            None => (None, None),
        };

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

        let state = if is_broken {
            StartupState::Broken
        } else if pid.is_some() || detail.state.as_deref() == Some("running") {
            StartupState::Running
        } else if is_disabled {
            StartupState::Disabled
        } else {
            StartupState::Enabled
        };

        let name = app_name.clone().unwrap_or_else(|| label_leaf_name(&label));

        let is_security = apps::is_security_vendor(
            app_bundle
                .as_ref()
                .and_then(|_| apps.iter().find(|a| Some(&a.path) == app_bundle.as_ref()))
                .and_then(|a| a.bundle_id.as_deref()),
            app_name.as_deref(),
        ) || apps::is_security_vendor(Some(&label), None);

        let is_ignored = detail
            .program
            .as_ref()
            .is_some_and(|p| Safety::is_ignored_or_protected(p, ignore));

        scan.items.push(StartupItem {
            label,
            name,
            app_name,
            app_bundle,
            kind: StartupKind::UserAgent,
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
    let script = "tell application \"System Events\"\nset res to \"\"\nrepeat with item_ref in every login item\nset res to res & (name of item_ref) & tab & (path of item_ref) & linefeed\nend repeat\nreturn res\nend tell";
    let osa_out = host.run("/usr/bin/osascript", &["-e", script], TIMEOUT);
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
            let state = if is_broken {
                StartupState::Broken
            } else if is_running {
                StartupState::Running
            } else {
                StartupState::Enabled
            };

            let app_name = apps
                .iter()
                .find(|a| a.path == path || path.starts_with(&a.path))
                .map(|a| a.name.clone())
                .or_else(|| Some(name.clone()));

            scan.items.push(StartupItem {
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
                is_security: false,
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
            let is_broken = !disabled.path.exists();
            let state = if is_broken {
                StartupState::Broken
            } else {
                StartupState::Disabled
            };
            let label = format!("loginitem.{}", disabled.name);
            scan.items.push(StartupItem {
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
                is_security: false,
                is_ignored: Safety::is_ignored_or_protected(&disabled.path, ignore),
            });
        }
    }

    // 5. If --all is requested, query sfltool dumpbtm via Host::run_admin
    if include_all {
        let temp_file = layout.user(".config/taert/btm_dump.tmp");
        let script = format!(
            "/usr/bin/sfltool dumpbtm > {} 2>&1",
            shell_quote(&temp_file.to_string_lossy())
        );
        let admin_res = host.run_admin(&script, "Read Background Task Management records");
        if admin_res.is_ok() && temp_file.exists() {
            let _ = fs::remove_file(&temp_file);
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

/// Disables a startup item.
pub fn disable_item(
    host: &dyn Host,
    layout: &Layout,
    _ignore: &IgnoreStore,
    item: &StartupItem,
    dry_run: bool,
) -> Result<String, String> {
    if item.is_apple {
        return Err("Apple system services cannot be modified".to_string());
    }
    if item.is_security {
        return Err("Security agent services are read-only".to_string());
    }
    if item.is_ignored {
        return Err("Protected startup items are read-only".to_string());
    }

    if dry_run {
        return Ok(format!(
            "Would disable {} ({})",
            item.label,
            item.kind.as_str()
        ));
    }

    match item.kind {
        StartupKind::LoginItem => {
            let script_args = [
                "-e",
                LOGIN_ITEM_SCRIPT[0],
                "-e",
                LOGIN_ITEM_SCRIPT[1],
                "-e",
                LOGIN_ITEM_SCRIPT[2],
                "-e",
                LOGIN_ITEM_SCRIPT[3],
                "-e",
                LOGIN_ITEM_SCRIPT[4],
                "-e",
                LOGIN_ITEM_SCRIPT[5],
                "-e",
                LOGIN_ITEM_SCRIPT[6],
                "-e",
                LOGIN_ITEM_SCRIPT[7],
                "--",
                &item.name,
            ];
            let out = host.run("/usr/bin/osascript", &script_args, TIMEOUT);
            if !out.ok {
                return Err(format!("Failed to remove login item: {}", out.stderr));
            }
            let mut disabled = load_disabled_login_items(layout);
            if !disabled.iter().any(|d| d.name == item.name)
                && let Some(ref path) = item.program
            {
                disabled.push(DisabledLoginItem {
                    name: item.name.clone(),
                    path: path.clone(),
                });
                let _ = save_disabled_login_items(layout, &disabled);
            }
            Ok(format!("Disabled login item {}", item.name))
        }
        StartupKind::Daemon => {
            let label = &item.label;
            let script = format!(
                "/bin/launchctl disable system/{} && /bin/launchctl bootout system/{} || true",
                shell_quote(label),
                shell_quote(label)
            );
            host.run_admin(&script, &format!("Disable system daemon {label}"))
                .map_err(|e| format!("Failed to disable daemon: {e}"))?;
            Ok(format!("Disabled system daemon {label}"))
        }
        StartupKind::UserAgent | StartupKind::SystemAgent => {
            let uid = host_uid();
            let target = format!("gui/{uid}/{}", item.label);
            // 1. disable first so launchd can't restart it
            let dis_out = host.run("/bin/launchctl", &["disable", &target], TIMEOUT);
            if !dis_out.ok {
                return Err(format!("launchctl disable failed: {}", dis_out.stderr));
            }
            // 2. bootout to stop it now
            let _ = host.run("/bin/launchctl", &["bootout", &target], TIMEOUT);
            Ok(format!("Disabled service {}", item.label))
        }
    }
}

/// Enables a startup item.
pub fn enable_item(
    host: &dyn Host,
    layout: &Layout,
    _ignore: &IgnoreStore,
    item: &StartupItem,
    dry_run: bool,
) -> Result<String, String> {
    if item.is_apple {
        return Err("Apple system services cannot be modified".to_string());
    }
    if item.is_security {
        return Err("Security agent services are read-only".to_string());
    }
    if item.is_ignored {
        return Err("Protected startup items are read-only".to_string());
    }

    if dry_run {
        return Ok(format!(
            "Would enable {} ({})",
            item.label,
            item.kind.as_str()
        ));
    }

    match item.kind {
        StartupKind::LoginItem => {
            let path_str = item
                .program
                .as_ref()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            let script = format!(
                "tell application \"System Events\" to make login item at end with properties {{path: \"{}\", name: \"{}\", hidden: false}}",
                path_str.replace('"', "\\\""),
                item.name.replace('"', "\\\"")
            );
            let out = host.run("/usr/bin/osascript", &["-e", &script], TIMEOUT);
            if !out.ok {
                return Err(format!("Failed to enable login item: {}", out.stderr));
            }
            let mut disabled = load_disabled_login_items(layout);
            disabled.retain(|d| d.name != item.name);
            let _ = save_disabled_login_items(layout, &disabled);
            Ok(format!("Enabled login item {}", item.name))
        }
        StartupKind::Daemon => {
            let label = &item.label;
            let mut script = format!("/bin/launchctl enable system/{}", shell_quote(label));
            if let Some(ref plist) = item.plist_path
                && plist.exists()
            {
                script.push_str(&format!(
                    " && /bin/launchctl bootstrap system {}",
                    shell_quote(&plist.to_string_lossy())
                ));
            }
            host.run_admin(&script, &format!("Enable system daemon {label}"))
                .map_err(|e| format!("Failed to enable daemon: {e}"))?;
            Ok(format!("Enabled system daemon {label}"))
        }
        StartupKind::UserAgent | StartupKind::SystemAgent => {
            let uid = host_uid();
            let target = format!("gui/{uid}/{}", item.label);
            let en_out = host.run("/bin/launchctl", &["enable", &target], TIMEOUT);
            if !en_out.ok {
                return Err(format!("launchctl enable failed: {}", en_out.stderr));
            }
            if let Some(ref plist) = item.plist_path
                && plist.exists()
            {
                let _ = host.run(
                    "/bin/launchctl",
                    &["bootstrap", &format!("gui/{uid}"), &plist.to_string_lossy()],
                    TIMEOUT,
                );
                return Ok(format!("Enabled service {}", item.label));
            }
            Ok(format!(
                "Enabled service {} (will start at next login)",
                item.label
            ))
        }
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
    Cleaner::remove_verified(&scan_item, |p| Safety::can_remove(p, ignore))
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
            .position(|c| c.contains("disable"))
            .expect("called disable");
        let bootout_idx = calls
            .iter()
            .position(|c| c.contains("bootout"))
            .expect("called bootout");
        assert!(disable_idx < bootout_idx, "disable must run before bootout");
    }

    #[test]
    fn daemon_goes_through_run_admin_user_agent_does_not() {
        let host = FakeHost::idle();
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
            is_apple: true,
            is_security: false,
            is_ignored: false,
        };

        let res = disable_item(&host, &layout, &ignore, &apple_item, false);
        assert!(res.is_err());
        let res_en = enable_item(&host, &layout, &ignore, &apple_item, false);
        assert!(res_en.is_err());
    }

    #[test]
    fn plist_stays_on_disk_after_disable() {
        let host = FakeHost::idle();
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
}

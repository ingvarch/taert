//! Plans an uninstall as a reviewable list and carries it out.

use std::collections::HashSet;
use std::ffi::CString;
use std::fs;
use std::io::Cursor;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::Duration;

use plist::Value;
use serde::Serialize;

use super::apps::{
    InstalledApp, bundle_id_of, embedded_bundle_ids, is_common_name, official_uninstaller,
};
use super::cleaner::{Cleaner, RemovalFailure, RemovalProgress, RemovalReport};
use super::host::{Host, shell_quote};
use super::layout::Layout;
use super::leftovers::{Evidence, Leftover, UninstallRole, find_leftovers};
use super::path_utils::prettify_path;
use super::safety::{Safety, SafetyError};
use super::scanner::{CategoryScan, ScanItem, ScanProgressEvent, ToolGroup};
use super::sizer::DirSizer;
use super::stores::IgnoreStore;

pub const PLAN_ID: &str = "uninstall";

const MIN_SELECTED_NAME: usize = 4;
const EXIT_POLLS: usize = 10;
const EXIT_POLL: Duration = Duration::from_millis(200);
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const BREW_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const PS: &str = "/bin/ps";
const OSASCRIPT: &str = "/usr/bin/osascript";
const LAUNCHCTL: &str = "/bin/launchctl";
const DEFAULTS: &str = "/usr/bin/defaults";
const LSREGISTER: &str = "/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister";
const BREW_PATHS: &[&str] = &["opt/homebrew/bin/brew", "usr/local/bin/brew"];
const DOCK_DOMAIN: &str = "com.apple.dock";
const DOCK_LISTS: &[&str] = &["persistent-apps", "persistent-others", "recent-apps"];
const CONTAINER_METADATA: &str = ".com.apple.containermanagerd.metadata.plist";

const ADMIN_PROMPT: &str = "taert needs your password to remove apps and their system files.";
const APP_MANAGEMENT_HINT: &str = "macOS refused. Allow your terminal in System Settings > Privacy & Security > App Management, or move the app to the Trash in Finder";
const FULL_DISK_HINT: &str = "macOS refused. Give your terminal Full Disk Access in System Settings > Privacy & Security, then run this again";
const NOT_SAFE_AS_ROOT: &str = "A folder on the way could be replaced, so taert will not use administrator rights here. Remove it in Finder";
const NOT_REMOVED: &str = "It was still there after the removal";

/// Removes a login item by name. The name arrives as an argument, so no quoting
/// of it can break the script.
const LOGIN_ITEM_SCRIPT: &[&str] = &[
    "on run argv",
    "set target to item 1 of argv",
    "tell application \"System Events\"",
    "repeat with i from (count of login items) to 1 by -1",
    "if name of login item i is target then delete login item i",
    "end repeat",
    "end tell",
    "end run",
];

/// Configuration that can point at an app, read only to tell the user.
const CONFIG_FILES: &[&str] = &[
    ".bash_profile",
    ".bashrc",
    ".claude.json",
    ".claude/settings.json",
    ".codex/config.toml",
    ".codex/hooks.json",
    ".config/fish/config.fish",
    ".cursor/hooks.json",
    ".cursor/mcp.json",
    ".gemini/settings.json",
    ".profile",
    ".zprofile",
    ".zshenv",
    ".zshrc",
];

const RECEIPTS: &str = "private/var/db/receipts";
const SYSTEM_REPORTS: &str = "Library/Logs/DiagnosticReports";
const CASKROOMS: &[&str] = &["opt/homebrew/Caskroom", "usr/local/Caskroom"];

/// Data that holds far more than settings, compared by the start of the path
/// relative to the home folder. Removing it takes a decision of its own.
const PROFILE_PREFIXES: &[&str] = &[
    "library/application support/bravesoftware",
    "library/application support/com.operasoftware.opera",
    "library/application support/google/chrome",
    "library/application support/microsoft edge",
];

const DATA_LIBRARIES: &[&str] = &[
    "library/application support/arc",
    "library/application support/atlauncher",
    "library/application support/cemu",
    "library/application support/chromium",
    "library/application support/citra",
    "library/application support/crossover",
    "library/application support/dolphin",
    "library/application support/firefox",
    "library/application support/minecraft",
    "library/application support/multimc",
    "library/application support/openemu",
    "library/application support/pcsx2",
    "library/application support/ppsspp",
    "library/application support/prismlauncher",
    "library/application support/retroarch",
    "library/application support/rpcs3",
    "library/application support/ryujinx",
    "library/application support/vivaldi",
    "library/application support/zen",
    "library/containers/com.isaacmarovitz.whisky",
];

#[derive(Debug, Clone, Serialize)]
pub struct AppPlan {
    pub app: InstalledApp,
    pub cask: Option<String>,
    pub login_helpers: Vec<String>,
    /// Another copy of the app stays, so only this bundle goes.
    pub shared: bool,
}

/// `apps[i]` is the app of `scan.groups[i]`.
#[derive(Debug, Clone, Serialize)]
pub struct UninstallPlan {
    pub scan: CategoryScan,
    pub apps: Vec<AppPlan>,
    pub notes: Vec<String>,
}

/// What `build_plan` works from.
pub struct PlanRequest<'a> {
    pub selected: &'a [InstalledApp],
    pub installed: &'a [InstalledApp],
    pub ignore: &'a IgnoreStore,
    pub host: &'a dyn Host,
    pub layout: &'a Layout,
}

fn is_copy(app: &InstalledApp, other: &InstalledApp) -> bool {
    let same_id = match (&app.bundle_id, &other.bundle_id) {
        (Some(one), Some(two)) => one.eq_ignore_ascii_case(two),
        _ => false,
    };
    same_id || app.name.to_lowercase() == other.name.to_lowercase()
}

fn vendor_prefix(id: &str) -> String {
    let parts: Vec<&str> = id.split('.').take(2).collect();
    parts.join(".").to_ascii_lowercase()
}

fn group_is_shared(
    path: &Path,
    app: &InstalledApp,
    staying: &[InstalledApp],
    layout: &Layout,
) -> bool {
    let in_group_containers =
        path.parent() == Some(layout.user("Library/Group Containers").as_path());
    let vendor = app.bundle_id.as_deref().map(vendor_prefix);
    in_group_containers
        && staying
            .iter()
            .any(|other| other.bundle_id.as_deref().map(vendor_prefix) == vendor)
}

fn is_held_back(path: &Path, layout: &Layout) -> bool {
    let Ok(relative) = path.strip_prefix(&layout.home) else {
        return false;
    };
    let relative = relative.to_string_lossy().to_lowercase();
    PROFILE_PREFIXES
        .iter()
        .any(|prefix| relative.starts_with(prefix))
        || DATA_LIBRARIES
            .iter()
            .any(|root| relative == *root || relative.starts_with(&format!("{root}/")))
}

fn selected_by_default(
    found: &Leftover,
    app: &InstalledApp,
    staying: &[InstalledApp],
    layout: &Layout,
) -> bool {
    if is_held_back(&found.path, layout) || group_is_shared(&found.path, app, staying, layout) {
        return false;
    }
    // Reports here are root's: macOS clears them, and they are not worth a password.
    if found.path.starts_with(layout.system(SYSTEM_REPORTS)) {
        return false;
    }
    match found.evidence {
        Evidence::BundleId | Evidence::Manifest | Evidence::Receipt => true,
        Evidence::RelatedName => false,
        // A name can be a coincidence, so it needs to be distinctive and in the
        // place an app would use. Dot folders and system folders stay a decision.
        Evidence::Name => {
            let in_app_place = found.path.starts_with(layout.user("Library"))
                || found.path.starts_with(layout.user(".cache"));
            in_app_place
                && app.name.chars().count() >= MIN_SELECTED_NAME
                && !is_common_name(&app.name)
        }
    }
}

fn is_cask_token(token: &str) -> bool {
    token
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.@+".contains(c))
}

fn folders_in(folder: &Path) -> Vec<PathBuf> {
    let Ok(read) = fs::read_dir(folder) else {
        return Vec::new();
    };
    read.flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect()
}

fn cask_holds(cask: &Path, bundle: &std::ffi::OsStr) -> bool {
    folders_in(cask).iter().any(|version| {
        let listed = |folder: &Path| fs::symlink_metadata(folder.join(bundle)).is_ok();
        listed(version) || folders_in(version).iter().any(|folder| listed(folder))
    })
}

/// The cask that installed an app. More than one candidate means none, because
/// Homebrew must not uninstall a cask taert only guessed.
fn find_cask(app: &InstalledApp, layout: &Layout) -> Option<String> {
    let bundle = app.path.file_name()?;
    let mut tokens = HashSet::new();
    for room in CASKROOMS {
        let room = layout.system(room);
        for cask in folders_in(&room) {
            if cask_holds(&cask, bundle) {
                tokens.insert(cask.file_name()?.to_string_lossy().into_owned());
            }
        }
    }
    let mut tokens = tokens.into_iter().filter(|token| is_cask_token(token));
    match (tokens.next(), tokens.next()) {
        (Some(token), None) => Some(token),
        _ => None,
    }
}

fn leftover_rows(
    request: &PlanRequest,
    app: &InstalledApp,
    staying: &[InstalledApp],
    listed: &mut HashSet<PathBuf>,
    progress: Option<&Sender<ScanProgressEvent>>,
) -> Vec<ScanItem> {
    let layout = request.layout;
    let findings = find_leftovers(app, staying, request.host, layout);
    let mut rows = Vec::new();

    for found in &findings.leftovers {
        let removable = Safety::can_uninstall(&found.path, request.ignore, layout).is_ok();
        if !removable || !listed.insert(found.path.clone()) {
            continue;
        }
        if let Some(tx) = progress {
            let _ = tx.send(ScanProgressEvent::ScanningPath(prettify_path(&found.path)));
        }
        let selected = selected_by_default(found, app, staying, layout);
        let size = DirSizer::size_of(&found.path);
        let mut row = ScanItem::new(found.path.clone(), size, selected, false);
        row.role = Some(UninstallRole::Leftover {
            evidence: found.evidence,
        });
        rows.push(row);
    }

    for package in findings.receipts {
        let path = layout.system(RECEIPTS).join(format!("{package}.plist"));
        if listed.insert(path.clone()) {
            let size = DirSizer::size_of(&path);
            let mut row = ScanItem::new(path, size, true, false);
            row.role = Some(UninstallRole::Receipt { package });
            rows.push(row);
        }
    }

    rows.sort_by_key(|row| !row.selected);
    rows
}

/// Finds what each app leaves and turns it into a list to review.
pub fn build_plan(
    request: &PlanRequest,
    progress: Option<Sender<ScanProgressEvent>>,
    cancel: Option<Arc<AtomicBool>>,
) -> UninstallPlan {
    let cancelled = || {
        cancel
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
    };
    let staying: Vec<InstalledApp> = request
        .installed
        .iter()
        .filter(|app| {
            !request
                .selected
                .iter()
                .any(|chosen| chosen.path == app.path)
        })
        .cloned()
        .collect();

    let mut plan = UninstallPlan {
        scan: CategoryScan {
            id: PLAN_ID.to_string(),
            title: "Review".to_string(),
            groups: Vec::new(),
        },
        apps: Vec::new(),
        notes: Vec::new(),
    };
    let mut listed = HashSet::new();

    for app in request.selected {
        if cancelled() {
            break;
        }
        if let Some(vendor) = official_uninstaller(app) {
            plan.notes.push(format!(
                "{} needs the uninstaller from {vendor}, so taert leaves it alone.",
                app.name
            ));
            continue;
        }
        if let Err(reason) = Safety::can_uninstall(&app.path, request.ignore, request.layout) {
            plan.notes.push(format!("{} stays: {reason}.", app.name));
            continue;
        }
        if let Some(tx) = &progress {
            let _ = tx.send(ScanProgressEvent::ToolStarted(app.name.clone()));
            let bytes = app.size_bytes;
            let _ = tx.send(ScanProgressEvent::ItemFound { bytes });
        }

        let mut bundle = ScanItem::new(app.path.clone(), app.size_bytes, true, false);
        bundle.role = Some(UninstallRole::Bundle);
        let mut items = vec![bundle];

        let copy = staying.iter().find(|other| is_copy(app, other));
        if let Some(copy) = copy {
            plan.notes.push(format!(
                "Another copy of {} stays at {}, so its settings and data stay too.",
                app.name,
                prettify_path(&copy.path)
            ));
        } else {
            items.extend(leftover_rows(
                request,
                app,
                &staying,
                &mut listed,
                progress.as_ref(),
            ));
        }

        let cask = find_cask(app, request.layout);
        let tool = match &cask {
            Some(token) => format!("{} (Homebrew cask {token})", app.name),
            None => app.name.clone(),
        };
        let (_, login_helpers) = embedded_bundle_ids(&app.path, app.bundle_id.as_deref());
        plan.scan.groups.push(ToolGroup { tool, items });
        plan.apps.push(AppPlan {
            app: app.clone(),
            cask,
            login_helpers,
            shared: copy.is_some(),
        });
    }

    plan
}

fn dock_import_path() -> PathBuf {
    std::env::temp_dir().join(format!("taert-dock-{}.plist", std::process::id()))
}

/// macOS keeps the root of an emptied container, so nothing more can be removed.
fn is_container_stub(path: &Path) -> bool {
    let Ok(entries) = fs::read_dir(path) else {
        return false;
    };
    let names: Vec<_> = entries.flatten().map(|entry| entry.file_name()).collect();
    names.len() == 1 && names[0] == CONTAINER_METADATA
}

fn user_can_remove(path: &Path) -> bool {
    let owned =
        fs::symlink_metadata(path).is_ok_and(|meta| meta.uid() == unsafe { libc::getuid() });
    let parent = path
        .parent()
        .and_then(|parent| CString::new(parent.as_os_str().as_bytes()).ok());
    owned && parent.is_some_and(|parent| unsafe { libc::access(parent.as_ptr(), libc::W_OK) } == 0)
}

fn is_gone(path: &Path) -> bool {
    fs::symlink_metadata(path).is_err()
}

fn in_folder_named(path: &Path, folder: &str) -> bool {
    path.parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == folder)
}

fn parse_process_table(table: &str) -> Vec<(u32, u32, String)> {
    table
        .lines()
        .filter_map(|line| {
            let mut fields = line.trim_start().splitn(2, char::is_whitespace);
            let pid = fields.next()?.parse().ok()?;
            let rest = fields.next()?.trim_start();
            let (parent, program) = rest.split_once(char::is_whitespace)?;
            Some((pid, parent.parse().ok()?, program.trim_start().to_string()))
        })
        .collect()
}

/// This process and everything that started it, the terminal included.
fn own_lineage(table: &[(u32, u32, String)]) -> HashSet<u32> {
    let mut lineage = HashSet::new();
    let mut current = std::process::id();
    while lineage.insert(current) {
        match table.iter().find(|(pid, _, _)| *pid == current) {
            Some((_, parent, _)) => current = *parent,
            None => break,
        }
    }
    lineage
}

fn percent_decoded(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let escaped = (bytes[index] == b'%')
            .then(|| text.get(index + 1..index + 3))
            .flatten()
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escaped {
            Some(byte) => {
                decoded.push(byte);
                index += 3;
            }
            None => {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn tile_is_of(tile: &Value, path: &Path, id: Option<&str>) -> bool {
    let Some(data) = tile
        .as_dictionary()
        .and_then(|tile| tile.get("tile-data"))
        .and_then(Value::as_dictionary)
    else {
        return false;
    };
    let same_id = id.is_some() && data.get("bundle-identifier").and_then(Value::as_string) == id;
    let url = data
        .get("file-data")
        .and_then(Value::as_dictionary)
        .and_then(|file| file.get("_CFURLString"))
        .and_then(Value::as_string)
        .unwrap_or_default();
    let target = percent_decoded(url);
    let target = target.strip_prefix("file://").unwrap_or(&target);
    same_id || Path::new(target.trim_end_matches('/')) == path
}

fn mentions(line: &str, needle: &str) -> bool {
    let is_name_char = |c: char| c.is_ascii_alphanumeric() || "_./-".contains(c);
    let ends_name = |c: char| c.is_whitespace() || "/\"')],;}:".contains(c);
    line.match_indices(needle).any(|(start, _)| {
        let before = line[..start].chars().next_back();
        let after = line[start + needle.len()..].chars().next();
        !before.is_some_and(is_name_char) && after.is_none_or(ends_name)
    })
}

fn config_needles(layout: &Layout, removed: &[PathBuf]) -> Vec<String> {
    let mut needles = Vec::new();
    for path in removed {
        needles.push(path.display().to_string());
        if let Ok(relative) = path.strip_prefix(&layout.home) {
            let relative = relative.display();
            needles.push(format!("~/{relative}"));
            needles.push(format!("$HOME/{relative}"));
            needles.push(format!("${{HOME}}/{relative}"));
        }
    }
    needles
}

/// Lines of the shell configuration that still mention removed paths, as `file:line`.
/// Lines themselves stay out of it, because they can hold credentials.
pub fn config_mentions(layout: &Layout, removed: &[PathBuf]) -> Vec<String> {
    let needles = config_needles(layout, removed);
    let mut found = Vec::new();
    for file in CONFIG_FILES {
        let path = layout.user(file);
        let small = fs::metadata(&path).is_ok_and(|meta| meta.len() <= MAX_CONFIG_BYTES);
        let Some(text) = small.then(|| fs::read_to_string(&path).ok()).flatten() else {
            continue;
        };
        for (index, line) in text.lines().enumerate() {
            let line = line.replace("\\/", "/");
            if needles.iter().any(|needle| mentions(&line, needle)) {
                found.push(format!("~/{file}:{}", index + 1));
            }
        }
    }
    found
}

/// One app's selected rows, sorted by who can remove them.
struct Job<'a> {
    plan: &'a AppPlan,
    bundle: Option<&'a ScanItem>,
    own_files: Vec<&'a ScanItem>,
    root_files: Vec<&'a ScanItem>,
    receipts: Vec<(&'a ScanItem, &'a str)>,
    bundle_needs_root: bool,
    /// The app stays, so everything else of it stays too.
    blocked: bool,
}

impl Job<'_> {
    fn bundle_is_gone(&self) -> bool {
        !self.blocked && self.bundle.is_some_and(|bundle| is_gone(&bundle.path))
    }
}

struct Tally<F> {
    report: RemovalReport,
    done: usize,
    total: usize,
    progress: F,
    removed_paths: Vec<PathBuf>,
}

impl<F: FnMut(RemovalProgress)> Tally<F> {
    fn step(&mut self, path: &Path) {
        self.done += 1;
        (self.progress)(RemovalProgress {
            current_index: self.done.min(self.total),
            total_count: self.total,
            path: path.to_path_buf(),
        });
    }

    fn removed(&mut self, item: &ScanItem) {
        self.report.removed_count += 1;
        self.report.freed_bytes = self.report.freed_bytes.saturating_add(item.size_bytes);
        self.removed_paths.push(item.path.clone());
    }

    fn failed(&mut self, path: &Path, error: impl Into<String>) {
        self.report.failures.push(RemovalFailure {
            path: path.to_path_buf(),
            error: error.into(),
        });
    }
}

enum BrewResult {
    Removed,
    NotTracked,
    Failed(String),
}

pub struct Uninstaller<'a> {
    pub host: &'a dyn Host,
    pub layout: &'a Layout,
    pub ignore: &'a IgnoreStore,
}

impl Uninstaller<'_> {
    /// Removes what is selected in `plan`. An app's files go only after the app
    /// itself is gone, so a failed removal never leaves an app without its data.
    pub fn run(
        &self,
        plan: &UninstallPlan,
        progress: impl FnMut(RemovalProgress),
    ) -> RemovalReport {
        let mut tally = Tally {
            report: RemovalReport {
                notes: plan.notes.clone(),
                ..RemovalReport::default()
            },
            done: 0,
            total: plan.scan.selected_items().len(),
            progress,
            removed_paths: Vec::new(),
        };

        let mut jobs = self.sort_out(plan);
        for job in &mut jobs {
            self.prepare(job, &mut tally);
        }
        for job in &mut jobs {
            self.remove_bundle(job, &mut tally);
        }
        self.remove_as_root(&mut jobs, &mut tally);
        for job in &jobs {
            self.remove_own_files(job, &mut tally);
        }
        self.tidy_up(&jobs, &mut tally);
        tally.report
    }

    fn check(&self, path: &Path) -> Result<(), SafetyError> {
        Safety::can_uninstall(path, self.ignore, self.layout).map(|_| ())
    }

    fn sort_out<'p>(&self, plan: &'p UninstallPlan) -> Vec<Job<'p>> {
        plan.apps
            .iter()
            .zip(&plan.scan.groups)
            .map(|(app, group)| {
                let mut job = Job {
                    plan: app,
                    bundle: None,
                    own_files: Vec::new(),
                    root_files: Vec::new(),
                    receipts: Vec::new(),
                    bundle_needs_root: false,
                    blocked: false,
                };
                for item in group.items.iter().filter(|item| item.selected) {
                    match &item.role {
                        Some(UninstallRole::Bundle) => job.bundle = Some(item),
                        Some(UninstallRole::Receipt { package }) => {
                            job.receipts.push((item, package.as_str()));
                        }
                        _ if user_can_remove(&item.path) => job.own_files.push(item),
                        _ => job.root_files.push(item),
                    }
                }
                job
            })
            .collect()
    }

    fn check_bundle(&self, bundle: &ScanItem, app: &InstalledApp) -> Result<(), String> {
        Cleaner::verify(bundle, |path| self.check(path)).map_err(|error| error.to_string())?;
        if bundle_id_of(&app.path) != app.bundle_id {
            return Err("The app changed after the review".to_string());
        }
        Ok(())
    }

    fn prepare<F: FnMut(RemovalProgress)>(&self, job: &mut Job, tally: &mut Tally<F>) {
        if let Some(bundle) = job.bundle
            && let Err(reason) = self.check_bundle(bundle, &job.plan.app)
        {
            tally.step(&bundle.path);
            tally.failed(&bundle.path, reason);
            job.blocked = true;
            return;
        }

        for item in job.own_files.iter().chain(&job.root_files) {
            if in_folder_named(&item.path, "LaunchAgents") {
                let path = item.path.display().to_string();
                self.host
                    .run(LAUNCHCTL, &["unload", &path], TEARDOWN_TIMEOUT);
            }
        }

        let Some(bundle) = job.bundle else {
            return;
        };
        let path = bundle.path.display().to_string();
        self.host.run(LSREGISTER, &["-u", &path], TEARDOWN_TIMEOUT);
        if !job.plan.shared {
            self.remove_login_items(job.plan);
        }
        self.stop_processes(job.plan, tally);
    }

    fn remove_login_items(&self, plan: &AppPlan) {
        let mut args: Vec<&str> = Vec::new();
        for line in LOGIN_ITEM_SCRIPT {
            args.extend(["-e", line]);
        }
        args.push(&plan.app.name);
        self.host.run(OSASCRIPT, &args, TEARDOWN_TIMEOUT);

        let uid = unsafe { libc::getuid() };
        for helper in &plan.login_helpers {
            if !helper.to_ascii_lowercase().starts_with("com.apple.") {
                let target = format!("gui/{uid}/{helper}");
                self.host
                    .run(LAUNCHCTL, &["bootout", &target], TEARDOWN_TIMEOUT);
            }
        }
    }

    /// Finds processes by where their program lives, which a name cannot spoof.
    fn processes_in(&self, bundle: &Path) -> Vec<u32> {
        let listing = self
            .host
            .run(PS, &["-axo", "pid=,ppid=,comm="], TEARDOWN_TIMEOUT);
        let table = parse_process_table(&listing.stdout);
        let lineage = own_lineage(&table);
        let resolved = fs::canonicalize(bundle).unwrap_or_else(|_| bundle.to_path_buf());
        table
            .iter()
            .filter(|(pid, _, _)| !lineage.contains(pid))
            .filter(|(_, _, program)| {
                let program = Path::new(program);
                program.starts_with(bundle) || program.starts_with(&resolved)
            })
            .map(|(pid, _, _)| *pid)
            .collect()
    }

    fn wait_for_exit(&self, bundle: &Path) -> bool {
        for _ in 0..EXIT_POLLS {
            if self.processes_in(bundle).is_empty() {
                return true;
            }
            self.host.pause(EXIT_POLL);
        }
        false
    }

    fn stop_processes<F: FnMut(RemovalProgress)>(&self, plan: &AppPlan, tally: &mut Tally<F>) {
        let bundle = &plan.app.path;
        let mut running = self.processes_in(bundle);
        if running.is_empty() {
            return;
        }

        // A copy that stays answers to the same id, so it must not hear this.
        if let Some(id) = plan.app.bundle_id.as_deref().filter(|_| !plan.shared) {
            let quit = format!("tell application id \"{id}\" to quit");
            self.host.run(OSASCRIPT, &["-e", &quit], TEARDOWN_TIMEOUT);
            if self.wait_for_exit(bundle) {
                return;
            }
            running = self.processes_in(bundle);
        }

        for signal in [libc::SIGTERM, libc::SIGKILL] {
            for pid in &running {
                self.host.signal(*pid, signal);
            }
            if self.wait_for_exit(bundle) {
                return;
            }
            running = self.processes_in(bundle);
        }
        tally.report.notes.push(format!(
            "{} was still running when it was removed.",
            plan.app.name
        ));
    }

    fn brew(&self) -> Option<String> {
        BREW_PATHS
            .iter()
            .map(|path| self.layout.system(path))
            .find(|path| path.exists())
            .map(|path| path.display().to_string())
    }

    fn is_tracked_by_homebrew(&self, token: &str) -> bool {
        CASKROOMS
            .iter()
            .any(|room| self.layout.system(room).join(token).exists())
    }

    /// Without `--zap`: the plan lists what goes, and a zap could reach further.
    fn brew_uninstall(&self, token: &str, bundle: &Path) -> BrewResult {
        let Some(brew) = self.brew() else {
            return BrewResult::NotTracked;
        };
        let args = [
            "HOMEBREW_NO_AUTO_UPDATE=1",
            "HOMEBREW_NO_ENV_HINTS=1",
            "NONINTERACTIVE=1",
            &brew,
            "uninstall",
            "--cask",
            token,
        ];
        let output = self.host.run("/usr/bin/env", &args, BREW_TIMEOUT);
        if output.ok && is_gone(bundle) {
            BrewResult::Removed
        } else if self.is_tracked_by_homebrew(token) {
            BrewResult::Failed(format!(
                "Homebrew could not remove it. Run: brew uninstall --cask {token}"
            ))
        } else {
            BrewResult::NotTracked
        }
    }

    fn remove_bundle<F: FnMut(RemovalProgress)>(&self, job: &mut Job, tally: &mut Tally<F>) {
        let Some(bundle) = job.bundle.filter(|_| !job.blocked) else {
            return;
        };
        tally.step(&bundle.path);

        if let Some(token) = &job.plan.cask {
            match self.brew_uninstall(token, &bundle.path) {
                BrewResult::Removed => return tally.removed(bundle),
                BrewResult::NotTracked => {}
                BrewResult::Failed(reason) => {
                    tally.failed(&bundle.path, reason);
                    job.blocked = true;
                    return;
                }
            }
        }

        if !user_can_remove(&bundle.path) {
            job.bundle_needs_root = true;
            return;
        }
        match Cleaner::remove_verified(bundle, |path| self.check(path)) {
            Ok(()) => tally.removed(bundle),
            Err(error) if error.is_permission_denied() => job.bundle_needs_root = true,
            Err(error) => {
                tally.failed(&bundle.path, error.to_string());
                job.blocked = true;
            }
        }
    }

    /// Drops what root must not touch, then checks the rest right before the script.
    fn vet_for_root<F: FnMut(RemovalProgress)>(&self, job: &mut Job, tally: &mut Tally<F>) {
        if let Some(bundle) = job.bundle.filter(|_| job.bundle_needs_root && !job.blocked)
            && !Safety::is_safe_as_root(&bundle.path, self.layout)
        {
            tally.failed(&bundle.path, NOT_SAFE_AS_ROOT);
            job.bundle_needs_root = false;
            job.blocked = true;
        }

        let files = std::mem::take(&mut job.root_files);
        for item in files {
            if job.blocked {
                continue;
            }
            tally.step(&item.path);
            if let Err(error) = Cleaner::verify(item, |path| self.check(path)) {
                tally.failed(&item.path, error.to_string());
            } else if Safety::is_safe_as_root(&item.path, self.layout) {
                job.root_files.push(item);
            } else {
                tally.failed(&item.path, NOT_SAFE_AS_ROOT);
            }
        }
    }

    fn remove_as_root<F: FnMut(RemovalProgress)>(&self, jobs: &mut [Job], tally: &mut Tally<F>) {
        for job in jobs.iter_mut() {
            self.vet_for_root(job, tally);
        }
        let script = root_script(jobs);
        if script.is_empty() {
            return;
        }

        let outcome = self.host.run_admin(&script, ADMIN_PROMPT);
        let reason = outcome.err();
        let because = |fallback: &str| reason.clone().unwrap_or_else(|| fallback.to_string());

        for job in jobs.iter_mut().filter(|job| !job.blocked) {
            if let Some(bundle) = job.bundle.filter(|_| job.bundle_needs_root) {
                if is_gone(&bundle.path) {
                    tally.removed(bundle);
                } else {
                    tally.failed(&bundle.path, because(APP_MANAGEMENT_HINT));
                    job.blocked = true;
                    continue;
                }
            }
            for item in &job.root_files {
                if is_gone(&item.path) {
                    tally.removed(item);
                } else {
                    tally.failed(&item.path, because(NOT_REMOVED));
                }
            }
            for (item, _) in &job.receipts {
                tally.step(&item.path);
                if is_gone(&item.path) {
                    tally.removed(item);
                } else {
                    tally.failed(&item.path, because(NOT_REMOVED));
                }
            }
        }
    }

    fn remove_own_files<F: FnMut(RemovalProgress)>(&self, job: &Job, tally: &mut Tally<F>) {
        if job.blocked {
            return;
        }
        if job.bundle.is_some() && !job.bundle_is_gone() {
            let name = &job.plan.app.name;
            tally
                .report
                .notes
                .push(format!("{name} is back in place, so its files stayed."));
            return;
        }
        for item in &job.own_files {
            tally.step(&item.path);
            match Cleaner::remove_verified(item, |path| self.check(path)) {
                Ok(()) => tally.removed(item),
                Err(_) if is_container_stub(&item.path) => tally.removed(item),
                Err(error) if error.is_permission_denied() => {
                    tally.failed(&item.path, FULL_DISK_HINT);
                }
                Err(error) => tally.failed(&item.path, error.to_string()),
            }
        }
    }

    fn system_extension_of(&self, id: &str) -> bool {
        let prefix = format!("{id}.");
        folders_in(&self.layout.system("Library/SystemExtensions"))
            .iter()
            .any(|folder| {
                let Ok(entries) = fs::read_dir(folder) else {
                    return false;
                };
                entries.flatten().any(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    name.starts_with(&prefix) && name.ends_with(".systemextension")
                })
            })
    }

    fn clean_dock(&self, removed: &[&AppPlan]) {
        let export = ["export", DOCK_DOMAIN, "-"];
        let exported = self.host.run(DEFAULTS, &export, TEARDOWN_TIMEOUT);
        if !exported.ok {
            return;
        }
        let Some(mut dock) = Value::from_reader(Cursor::new(exported.stdout.as_bytes()))
            .ok()
            .and_then(Value::into_dictionary)
        else {
            return;
        };

        let is_removed = |tile: &Value| {
            removed.iter().any(|plan| {
                let id = plan.app.bundle_id.as_deref().filter(|_| !plan.shared);
                tile_is_of(tile, &plan.app.path, id)
            })
        };
        let mut changed = false;
        for list in DOCK_LISTS {
            if let Some(Value::Array(tiles)) = dock.get_mut(list) {
                let before = tiles.len();
                tiles.retain(|tile| !is_removed(tile));
                changed |= tiles.len() != before;
            }
        }
        if !changed {
            return;
        }

        let import = dock_import_path();
        if Value::Dictionary(dock).to_file_xml(&import).is_ok() {
            let path = import.display().to_string();
            let imported =
                self.host
                    .run(DEFAULTS, &["import", DOCK_DOMAIN, &path], TEARDOWN_TIMEOUT);
            if imported.ok {
                self.host
                    .run("/usr/bin/killall", &["Dock"], TEARDOWN_TIMEOUT);
            }
        }
        let _ = fs::remove_file(&import);
    }

    fn tidy_up<F: FnMut(RemovalProgress)>(&self, jobs: &[Job], tally: &mut Tally<F>) {
        let removed: Vec<&AppPlan> = jobs
            .iter()
            .filter(|job| job.bundle_is_gone())
            .map(|job| job.plan)
            .collect();
        if !removed.is_empty() {
            self.clean_dock(&removed);
        }

        for plan in &removed {
            let extension = plan
                .app
                .bundle_id
                .as_deref()
                .filter(|id| self.system_extension_of(id));
            if extension.is_some() {
                tally.report.notes.push(format!(
                    "{} leaves a system extension. Turn it off in System Settings > General > Login Items & Extensions.",
                    plan.app.name
                ));
            }
        }

        let mentions = config_mentions(self.layout, &tally.removed_paths);
        if !mentions.is_empty() {
            tally.report.notes.push(format!(
                "Still mentioned in your configuration: {}",
                mentions.join(", ")
            ));
        }
    }
}

/// The root part of a run as one script, so macOS asks for the password once.
/// It removes an app's files only once the app is gone.
fn root_script(jobs: &[Job]) -> String {
    let mut lines = Vec::new();
    let active = || jobs.iter().filter(|job| !job.blocked);

    for item in active().flat_map(|job| &job.root_files) {
        if in_folder_named(&item.path, "LaunchDaemons") {
            let path = shell_quote(&item.path.display().to_string());
            lines.push(format!("{LAUNCHCTL} unload {path} >/dev/null 2>&1"));
        }
    }

    for job in active() {
        let removals = job.root_files.iter().map(|item| {
            format!(
                "/bin/rm -rf -- {}",
                shell_quote(&item.path.display().to_string())
            )
        });
        let forgets = job.receipts.iter().map(|(_, package)| {
            format!(
                "/usr/sbin/pkgutil --forget {} >/dev/null 2>&1",
                shell_quote(package)
            )
        });
        let body = removals.chain(forgets);

        match job.bundle.filter(|_| job.bundle_needs_root) {
            Some(bundle) => {
                let path = shell_quote(&bundle.path.display().to_string());
                lines.push(format!("if /bin/rm -rf -- {path}; then"));
                lines.push(":".to_string());
                lines.extend(body);
                lines.push("fi".to_string());
            }
            None => lines.extend(body),
        }
    }

    if lines.is_empty() {
        return String::new();
    }
    lines.push("true".to_string());
    lines.join("\n")
}

#[cfg(test)]
mod plan_tests {
    use super::*;
    use crate::core::apps::testing::{app, fake_app, sandbox, write_info};
    use crate::core::host::fake::FakeHost;
    use crate::core::leftovers::{Evidence, UninstallRole};
    use crate::core::scanner::ScanItem;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn folder(path: PathBuf) -> PathBuf {
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("data"), vec![0_u8; 4096]).unwrap();
        path
    }

    fn installed(layout: &Layout, name: &str, id: &str) -> InstalledApp {
        let path = layout.system("Applications").join(format!("{name}.app"));
        fake_app(&path, id, name);
        InstalledApp {
            path,
            ..app(name, id)
        }
    }

    fn plan_for(apps: &[InstalledApp], layout: &Layout) -> UninstallPlan {
        plan_with(apps, apps, &IgnoreStore::default(), layout)
    }

    fn plan_with(
        selected: &[InstalledApp],
        installed: &[InstalledApp],
        ignore: &IgnoreStore,
        layout: &Layout,
    ) -> UninstallPlan {
        let host = FakeHost::default();
        let request = PlanRequest {
            selected,
            installed,
            ignore,
            host: &host,
            layout,
        };
        build_plan(&request, None, None)
    }

    fn row<'a>(plan: &'a UninstallPlan, path: &Path) -> &'a ScanItem {
        plan.scan
            .groups
            .iter()
            .flat_map(|group| &group.items)
            .find(|item| item.path == path)
            .unwrap_or_else(|| panic!("no row for {path:?}"))
    }

    fn has_row(plan: &UninstallPlan, path: &Path) -> bool {
        plan.scan
            .groups
            .iter()
            .flat_map(|g| &g.items)
            .any(|item| item.path == path)
    }

    #[test]
    fn the_bundle_comes_first_and_is_selected() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        folder(layout.user("Library/Caches/com.acme.foo"));

        let plan = plan_for(std::slice::from_ref(&foo), &layout);

        assert_eq!(plan.scan.id, PLAN_ID);
        assert_eq!(plan.apps.len(), 1);
        assert_eq!(plan.scan.groups.len(), 1);
        let bundle = &plan.scan.groups[0].items[0];
        assert_eq!(bundle.path, foo.path);
        assert_eq!(bundle.role, Some(UninstallRole::Bundle));
        assert!(bundle.selected);
        assert!(bundle.size_bytes > 0);
    }

    #[test]
    fn proof_by_id_or_program_is_selected_and_names_are_judged_by_place() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo Studio", "com.acme.foo");
        let by_id = folder(layout.user("Library/Containers/com.acme.foo"));
        let system_by_id = folder(layout.system("Library/Application Support/com.acme.foo"));
        let library_name = folder(layout.user("Library/Application Support/Foo Studio"));
        let dot_name = folder(layout.user(".config/foo-studio"));
        let system_name = folder(layout.system("Library/Application Support/Foo Studio"));
        let shared_name = folder(layout.system("Users/Shared/Foo Studio"));

        let plan = plan_for(&[foo], &layout);

        assert!(row(&plan, &by_id).selected);
        assert!(row(&plan, &system_by_id).selected);
        assert!(row(&plan, &library_name).selected);
        assert!(!row(&plan, &dot_name).selected);
        assert!(!row(&plan, &system_name).selected);
        assert!(!row(&plan, &shared_name).selected);
    }

    #[test]
    fn a_cache_in_a_dot_folder_is_selected_and_settings_are_not() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Ghostly", "com.acme.ghostly");
        let cache = folder(layout.user(".cache/ghostly"));
        let settings = folder(layout.user(".config/ghostly"));

        let plan = plan_for(&[foo], &layout);

        assert!(row(&plan, &cache).selected);
        assert!(!row(&plan, &settings).selected);
    }

    #[test]
    fn a_system_crash_report_is_not_worth_a_password() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let reports = "Library/Logs/DiagnosticReports";
        let system = layout.system(reports).join("Foo_2026-10-01.diag");
        fs::create_dir_all(system.parent().unwrap()).unwrap();
        fs::write(&system, b"x").unwrap();
        let user = layout.user(reports).join("Foo_2026-10-01.ips");
        fs::create_dir_all(user.parent().unwrap()).unwrap();
        fs::write(&user, b"x").unwrap();

        let plan = plan_for(&[foo], &layout);

        assert!(!row(&plan, &system).selected);
        assert!(row(&plan, &user).selected);
    }

    #[test]
    fn a_name_that_says_little_is_listed_but_not_selected() {
        let (_temp, layout) = sandbox();
        let notes = installed(&layout, "Notes", "com.acme.notes");
        let short = installed(&layout, "Foo", "com.acme.foo");
        let common = folder(layout.user("Library/Application Support/Notes"));
        let brief = folder(layout.user("Library/Application Support/Foo"));

        let plan = plan_for(&[notes, short], &layout);

        assert!(!row(&plan, &common).selected);
        assert!(!row(&plan, &brief).selected);
    }

    #[test]
    fn a_related_name_is_listed_but_not_selected() {
        let (_temp, layout) = sandbox();
        let zed = installed(&layout, "Zed Nightly", "dev.zed.Zed-Nightly");
        let base = folder(layout.user("Library/Application Support/Zed"));

        let plan = plan_for(&[zed], &layout);

        let item = row(&plan, &base);
        assert!(!item.selected);
        assert_eq!(
            item.role,
            Some(UninstallRole::Leftover {
                evidence: Evidence::RelatedName
            })
        );
    }

    #[test]
    fn selected_rows_come_before_the_ones_to_decide_on() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo Studio", "com.acme.foo");
        folder(layout.user(".config/foo-studio"));
        folder(layout.user("Library/Containers/com.acme.foo"));
        folder(layout.user("Library/Caches/com.acme.foo"));

        let plan = plan_for(&[foo], &layout);

        let flags: Vec<bool> = plan.scan.groups[0]
            .items
            .iter()
            .map(|i| i.selected)
            .collect();
        assert_eq!(flags, [true, true, true, false]);
    }

    #[test]
    fn browser_profiles_and_game_libraries_wait_for_a_decision() {
        let (_temp, layout) = sandbox();
        let chrome = installed(&layout, "Google Chrome", "com.google.Chrome");
        let profile = folder(layout.user("Library/Application Support/Google/Chrome"));
        let cache = folder(layout.user("Library/Caches/com.google.Chrome"));
        let emu = installed(&layout, "OpenEmu", "org.openemu.OpenEmu");
        let roms = folder(layout.user("Library/Application Support/OpenEmu"));

        let plan = plan_for(&[chrome, emu], &layout);

        assert!(!row(&plan, &profile).selected);
        assert!(row(&plan, &cache).selected);
        assert!(!row(&plan, &roms).selected);
    }

    #[test]
    fn a_group_container_waits_when_another_app_of_the_vendor_stays() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let group = folder(layout.user("Library/Group Containers/group.com.acme.foo"));
        let sibling = app("Bar", "com.acme.bar");

        let alone = plan_for(std::slice::from_ref(&foo), &layout);
        let beside = plan_with(
            std::slice::from_ref(&foo),
            &[foo.clone(), sibling],
            &IgnoreStore::default(),
            &layout,
        );

        assert!(row(&alone, &group).selected);
        assert!(!row(&beside, &group).selected);
    }

    #[test]
    fn another_copy_of_the_app_keeps_all_shared_data() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let mut copy = foo.clone();
        copy.path = layout.user("Applications/Foo Copy.app");
        copy.name = "Foo Copy".to_string();
        let data = folder(layout.user("Library/Caches/com.acme.foo"));

        let plan = plan_with(
            std::slice::from_ref(&foo),
            &[foo.clone(), copy],
            &IgnoreStore::default(),
            &layout,
        );

        assert!(plan.apps[0].shared);
        assert!(has_row(&plan, &foo.path));
        assert!(!has_row(&plan, &data));
        assert_eq!(plan.scan.groups[0].items.len(), 1);
        assert!(plan.notes[0].contains("Foo Copy"), "{:?}", plan.notes);
    }

    #[test]
    fn selecting_every_copy_lists_shared_data_once() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let mut copy = foo.clone();
        copy.path = layout.system("Applications/Foo 2.app");
        fake_app(&copy.path, "com.acme.foo", "Foo");
        copy.name = "Foo 2".to_string();
        let data = folder(layout.user("Library/Caches/com.acme.foo"));

        let plan = plan_for(&[foo, copy], &layout);

        let rows = plan
            .scan
            .groups
            .iter()
            .flat_map(|group| &group.items)
            .filter(|item| item.path == data);
        assert_eq!(rows.count(), 1);
        assert!(!plan.apps[0].shared);
    }

    #[test]
    fn a_security_agent_is_left_out_with_a_note() {
        let (_temp, layout) = sandbox();
        let falcon = installed(&layout, "Falcon", "com.crowdstrike.falcon.App");

        let plan = plan_for(&[falcon], &layout);

        assert!(plan.apps.is_empty());
        assert!(plan.scan.groups.is_empty());
        assert!(plan.notes[0].contains("CrowdStrike"), "{:?}", plan.notes);
    }

    #[test]
    fn an_app_on_the_ignore_list_is_left_out_with_a_note() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let mut ignore = IgnoreStore::default();
        ignore.add(foo.path.display().to_string());

        let plan = plan_with(
            std::slice::from_ref(&foo),
            std::slice::from_ref(&foo),
            &ignore,
            &layout,
        );

        assert!(plan.apps.is_empty());
        assert!(plan.notes[0].contains("Foo"), "{:?}", plan.notes);
    }

    #[test]
    fn a_protected_leftover_gets_no_row() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let cache = folder(layout.user("Library/Caches/com.acme.foo"));
        let mut ignore = IgnoreStore::default();
        ignore.add(cache.display().to_string());

        let plan = plan_with(
            std::slice::from_ref(&foo),
            std::slice::from_ref(&foo),
            &ignore,
            &layout,
        );

        assert!(!has_row(&plan, &cache));
        assert!(has_row(&plan, &foo.path));
    }

    #[test]
    fn a_homebrew_cask_is_named_on_the_app() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let room = layout.root.join("opt/homebrew/Caskroom/foo-app/1.2.3");
        fs::create_dir_all(&room).unwrap();
        std::os::unix::fs::symlink(&foo.path, room.join("Foo.app")).unwrap();

        let plan = plan_for(&[foo], &layout);

        assert_eq!(plan.apps[0].cask.as_deref(), Some("foo-app"));
        assert_eq!(plan.scan.groups[0].tool, "Foo (Homebrew cask foo-app)");
    }

    #[test]
    fn a_receipt_is_a_row_of_its_own() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let receipts = layout.system("private/var/db/receipts");
        fs::create_dir_all(&receipts).unwrap();
        let receipt = receipts.join("com.acme.foo.pkg.plist");
        fs::write(&receipt, b"x").unwrap();

        let plan = plan_for(&[foo], &layout);

        let item = row(&plan, &receipt);
        assert!(item.selected);
        assert_eq!(
            item.role,
            Some(UninstallRole::Receipt {
                package: "com.acme.foo.pkg".to_string()
            })
        );
    }

    #[test]
    fn login_item_helpers_are_kept_for_stopping_them() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let helper = foo.path.join("Contents/Library/LoginItems/Helper.app");
        write_info(
            &helper,
            &[("CFBundleIdentifier", "com.acme.foo.login".into())],
        );

        let plan = plan_for(&[foo], &layout);

        assert_eq!(plan.apps[0].login_helpers, ["com.acme.foo.login"]);
    }

    #[test]
    fn a_plan_can_be_cancelled() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let host = FakeHost::default();
        let ignore = IgnoreStore::default();
        let apps = [foo];
        let request = PlanRequest {
            selected: &apps,
            installed: &apps,
            ignore: &ignore,
            host: &host,
            layout: &layout,
        };

        let plan = build_plan(&request, None, Some(Arc::new(AtomicBool::new(true))));

        assert!(plan.apps.is_empty());
    }
}

#[cfg(test)]
mod run_tests {
    use super::*;
    use crate::core::apps::testing::{app, fake_app, sandbox};
    use crate::core::host::fake::FakeHost;
    use crate::core::scanner::ToolGroup;
    use std::os::unix::fs::PermissionsExt;

    const RUN_LOOP_EXIT_CHECKS: usize = EXIT_POLLS + 1;

    fn folder(path: PathBuf) -> PathBuf {
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("data"), vec![0_u8; 4096]).unwrap();
        path
    }

    fn file(path: PathBuf) -> PathBuf {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"data").unwrap();
        path
    }

    fn installed(layout: &Layout, name: &str, id: &str) -> InstalledApp {
        let path = layout.system("Applications").join(format!("{name}.app"));
        fake_app(&path, id, name);
        InstalledApp {
            path,
            ..app(name, id)
        }
    }

    fn row(path: &Path, role: UninstallRole) -> ScanItem {
        let mut item = ScanItem::new(path.to_path_buf(), 100, true, false);
        item.role = Some(role);
        item
    }

    fn leftover(path: &Path) -> ScanItem {
        row(
            path,
            UninstallRole::Leftover {
                evidence: Evidence::BundleId,
            },
        )
    }

    fn plan_of(app: &InstalledApp, extra: Vec<ScanItem>) -> UninstallPlan {
        plans_of(vec![(app.clone(), extra)])
    }

    fn plans_of(apps: Vec<(InstalledApp, Vec<ScanItem>)>) -> UninstallPlan {
        let mut plan = UninstallPlan {
            scan: CategoryScan {
                id: PLAN_ID.to_string(),
                title: "Review".to_string(),
                groups: Vec::new(),
            },
            apps: Vec::new(),
            notes: Vec::new(),
        };
        for (app, extra) in apps {
            let mut items = vec![row(&app.path, UninstallRole::Bundle)];
            items.extend(extra);
            plan.scan.groups.push(ToolGroup {
                tool: app.name.clone(),
                items,
            });
            plan.apps.push(AppPlan {
                app,
                cask: None,
                login_helpers: Vec::new(),
                shared: false,
            });
        }
        plan
    }

    fn run(plan: &UninstallPlan, host: &FakeHost, layout: &Layout) -> RemovalReport {
        run_ignoring(plan, host, layout, &IgnoreStore::default())
    }

    fn run_ignoring(
        plan: &UninstallPlan,
        host: &FakeHost,
        layout: &Layout,
        ignore: &IgnoreStore,
    ) -> RemovalReport {
        Uninstaller {
            host,
            layout,
            ignore,
        }
        .run(plan, |_| {})
    }

    fn lock_folder(path: &Path) {
        fs::set_permissions(path, fs::Permissions::from_mode(0o555)).unwrap();
    }

    fn unlock_folder(path: &Path) {
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn the_app_and_selected_files_go_and_the_rest_stays() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let cache = folder(layout.user("Library/Caches/com.acme.foo"));
        let kept = folder(layout.user(".config/foo"));
        let mut unselected = leftover(&kept);
        unselected.selected = false;
        let plan = plan_of(&foo, vec![leftover(&cache), unselected]);

        let report = run(&plan, &FakeHost::default(), &layout);

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(!foo.path.exists());
        assert!(!cache.exists());
        assert!(kept.exists());
        assert_eq!(report.removed_count, 2);
        assert_eq!(report.freed_bytes, 200);
    }

    #[test]
    fn progress_counts_every_selected_row() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let cache = folder(layout.user("Library/Caches/com.acme.foo"));
        let plan = plan_of(&foo, vec![leftover(&cache)]);
        let host = FakeHost::default();
        let ignore = IgnoreStore::default();
        let mut seen = Vec::new();

        Uninstaller {
            host: &host,
            layout: &layout,
            ignore: &ignore,
        }
        .run(&plan, |step| {
            seen.push((step.current_index, step.total_count))
        });

        assert_eq!(seen, [(1, 2), (2, 2)]);
    }

    #[test]
    fn the_app_is_stopped_and_unregistered_before_it_goes() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let agent = file(layout.user("Library/LaunchAgents/com.acme.foo.plist"));
        let plan = plan_of(&foo, vec![leftover(&agent)]);
        let host = FakeHost::default();
        let bundle = foo.path.clone();
        let alive = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = alive.clone();
        host.when(LSREGISTER, move || {
            seen.lock().unwrap().push(bundle.exists())
        });

        let report = run(&plan, &host, &layout);

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(host.ran(&format!("/bin/launchctl unload {}", agent.display())));
        assert_eq!(*alive.lock().unwrap(), [true]);
        assert!(host.ran(&format!("{LSREGISTER} -u {}", foo.path.display())));
        assert!(!agent.exists());
    }

    #[test]
    fn the_login_item_of_the_app_is_removed_by_its_name() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let host = FakeHost::default();

        run(&plan_of(&foo, Vec::new()), &host, &layout);

        let calls = host.calls.lock().unwrap();
        let script = calls
            .iter()
            .find(|line| line.contains("System Events"))
            .unwrap();
        assert!(script.ends_with(" Foo"), "{script}");
    }

    #[test]
    fn login_item_helpers_are_booted_out_and_apple_labels_never() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let mut plan = plan_of(&foo, Vec::new());
        plan.apps[0].login_helpers = vec![
            "com.acme.foo.login".to_string(),
            "com.apple.Dock".to_string(),
        ];
        let host = FakeHost::default();

        run(&plan, &host, &layout);

        let uid = unsafe { libc::getuid() };
        assert!(host.ran(&format!(
            "/bin/launchctl bootout gui/{uid}/com.acme.foo.login"
        )));
        assert!(!host.ran(&format!("/bin/launchctl bootout gui/{uid}/com.apple.Dock")));
    }

    fn running(path: &Path, pid: u32) -> String {
        format!("{pid} 1 {}/Contents/MacOS/foo\n", path.display())
    }

    #[test]
    fn a_running_app_gets_a_chance_to_quit() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let host = FakeHost::default();
        host.reply(PS, &running(&foo.path, 4242));
        host.reply(PS, "");

        let report = run(&plan_of(&foo, Vec::new()), &host, &layout);

        assert!(host.ran("/usr/bin/osascript -e tell application id \"com.acme.foo\" to quit"));
        assert!(host.signals.lock().unwrap().is_empty());
        assert!(report.notes.is_empty(), "{:?}", report.notes);
    }

    #[test]
    fn an_app_that_ignores_quit_is_terminated() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let host = FakeHost::default();
        for _ in 0..=RUN_LOOP_EXIT_CHECKS {
            host.reply(PS, &running(&foo.path, 4242));
        }
        host.reply(PS, "");

        run(&plan_of(&foo, Vec::new()), &host, &layout);

        assert_eq!(*host.signals.lock().unwrap(), [(4242, libc::SIGTERM)]);
    }

    #[test]
    fn an_app_that_ignores_everything_is_killed_and_reported() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let host = FakeHost::default();
        host.reply(PS, &running(&foo.path, 4242));

        let report = run(&plan_of(&foo, Vec::new()), &host, &layout);

        let signals = host.signals.lock().unwrap();
        assert_eq!(*signals, [(4242, libc::SIGTERM), (4242, libc::SIGKILL)]);
        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("still running"))
        );
        assert!(!foo.path.exists());
    }

    #[test]
    fn taert_never_kills_the_terminal_it_runs_in() {
        let (_temp, layout) = sandbox();
        let terminal = installed(&layout, "Foo", "com.acme.foo");
        let host = FakeHost::default();
        let me = std::process::id();
        host.reply(
            PS,
            &format!("{me} 1 {}/Contents/MacOS/foo\n", terminal.path.display()),
        );

        run(&plan_of(&terminal, Vec::new()), &host, &layout);

        assert!(host.signals.lock().unwrap().is_empty());
        assert!(!host.ran("/usr/bin/osascript -e tell application id"));
    }

    #[test]
    fn a_program_elsewhere_is_left_running() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let host = FakeHost::default();
        host.reply(PS, "4242 1 /Applications/Foo Pro.app/Contents/MacOS/foo\n");

        run(&plan_of(&foo, Vec::new()), &host, &layout);

        assert!(host.signals.lock().unwrap().is_empty());
    }

    #[test]
    fn a_copy_that_stays_keeps_its_login_item_and_is_not_asked_to_quit() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let mut plan = plan_of(&foo, Vec::new());
        plan.apps[0].shared = true;
        plan.apps[0].login_helpers = vec!["com.acme.foo.login".to_string()];
        let host = FakeHost::default();
        host.reply(PS, &running(&foo.path, 4242));
        host.reply(PS, "");

        run(&plan, &host, &layout);

        let calls = host.calls.lock().unwrap().join("\n");
        assert!(!calls.contains("System Events"));
        assert!(!calls.contains("bootout"));
        assert!(!calls.contains("to quit"));
        assert_eq!(*host.signals.lock().unwrap(), [(4242, libc::SIGTERM)]);
    }

    #[test]
    fn with_the_bundle_deselected_only_the_chosen_files_go() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let cache = folder(layout.user("Library/Caches/com.acme.foo"));
        let mut plan = plan_of(&foo, vec![leftover(&cache)]);
        plan.scan.groups[0].items[0].selected = false;
        let host = FakeHost::default();

        let report = run(&plan, &host, &layout);

        assert!(foo.path.exists());
        assert!(!cache.exists());
        assert_eq!(report.removed_count, 1);
        assert!(!host.ran(LSREGISTER));
        assert!(!host.ran("/usr/bin/osascript"));
    }

    #[test]
    fn a_bundle_replaced_since_the_review_stays_and_so_do_its_files() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let cache = folder(layout.user("Library/Caches/com.acme.foo"));
        let plan = plan_of(&foo, vec![leftover(&cache)]);
        let aside = layout.user("spacer.app");
        fs::rename(&foo.path, &aside).unwrap();
        fake_app(&foo.path, "com.acme.foo", "Foo");

        let report = run(&plan, &FakeHost::default(), &layout);

        assert_eq!(report.failures.len(), 1);
        assert!(foo.path.exists());
        assert!(cache.exists());
    }

    #[test]
    fn a_bundle_that_changed_its_id_stays() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let plan = plan_of(&foo, Vec::new());
        fake_app(&foo.path, "com.other.app", "Foo");

        let report = run(&plan, &FakeHost::default(), &layout);

        assert_eq!(report.failures.len(), 1);
        assert!(foo.path.exists());
    }

    #[test]
    fn files_of_one_app_do_not_stop_the_next_app() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let bar = installed(&layout, "Bar", "com.acme.bar");
        let mut plan = plans_of(vec![(foo.clone(), Vec::new()), (bar.clone(), Vec::new())]);
        fake_app(&foo.path, "com.other.app", "Foo");
        plan.notes.push("From the plan".to_string());

        let report = run(&plan, &FakeHost::default(), &layout);

        assert!(foo.path.exists());
        assert!(!bar.path.exists());
        assert_eq!(report.notes, ["From the plan"]);
    }

    fn with_cask(layout: &Layout, plan: &mut UninstallPlan, token: &str) -> PathBuf {
        file(layout.system("opt/homebrew/bin/brew"));
        plan.apps[0].cask = Some(token.to_string());
        folder(layout.system("opt/homebrew/Caskroom").join(token))
    }

    fn removing(path: &Path) -> impl Fn() + Send + 'static {
        let path = path.to_path_buf();
        move || {
            let _ = fs::remove_dir_all(&path);
        }
    }

    #[test]
    fn a_cask_goes_through_homebrew_without_zap() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let cache = folder(layout.user("Library/Caches/com.acme.foo"));
        let mut plan = plan_of(&foo, vec![leftover(&cache)]);
        let room = with_cask(&layout, &mut plan, "foo-app");
        let host = FakeHost::default();
        host.reply("/usr/bin/env", "");
        host.when("/usr/bin/env", removing(&foo.path));
        host.when("/usr/bin/env", removing(&room));

        let report = run(&plan, &host, &layout);

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        let calls = host.calls.lock().unwrap().join("\n");
        assert!(calls.contains("uninstall --cask foo-app"), "{calls}");
        assert!(!calls.contains("--zap"), "{calls}");
        assert!(!cache.exists());
        assert_eq!(report.removed_count, 2);
    }

    #[test]
    fn a_cask_homebrew_keeps_is_a_failure_and_the_files_stay() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let cache = folder(layout.user("Library/Caches/com.acme.foo"));
        let mut plan = plan_of(&foo, vec![leftover(&cache)]);
        with_cask(&layout, &mut plan, "foo-app");
        let host = FakeHost::default();
        host.fail("/usr/bin/env", "sudo: a password is required");

        let report = run(&plan, &host, &layout);

        assert!(foo.path.exists());
        assert!(cache.exists());
        assert_eq!(report.failures.len(), 1);
        assert!(
            report.failures[0]
                .error
                .contains("brew uninstall --cask foo-app")
        );
    }

    #[test]
    fn a_cask_homebrew_no_longer_tracks_is_removed_directly() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let mut plan = plan_of(&foo, Vec::new());
        let room = with_cask(&layout, &mut plan, "foo-app");
        fs::remove_dir_all(&room).unwrap();
        let host = FakeHost::default();
        host.fail("/usr/bin/env", "Cask is not installed");

        let report = run(&plan, &host, &layout);

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(!foo.path.exists());
    }

    #[test]
    fn system_files_go_in_one_root_script_with_one_prompt() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let bar = installed(&layout, "Bar", "com.acme.bar");
        let support = layout.system("Library/Application Support");
        let helpers = layout.system("Library/PrivilegedHelperTools");
        let foo_support = folder(support.join("Foo's Data"));
        let bar_helper = file(helpers.join("com.acme.bar.helper"));
        let daemons = layout.system("Library/LaunchDaemons");
        let daemon = file(daemons.join("com.acme.foo.plist"));
        let plan = plans_of(vec![
            (foo.clone(), vec![leftover(&foo_support), leftover(&daemon)]),
            (bar.clone(), vec![leftover(&bar_helper)]),
        ]);
        let host = FakeHost::default();
        let removed = [foo_support.clone(), bar_helper.clone(), daemon.clone()];
        *host.admin_effect.lock().unwrap() = Some(Box::new(move || {
            for path in &removed {
                unlock_folder(path.parent().unwrap());
                let _ = fs::remove_dir_all(path);
                let _ = fs::remove_file(path);
            }
        }));
        lock_folder(&support);
        lock_folder(&helpers);
        lock_folder(&daemons);

        let report = run(&plan, &host, &layout);

        unlock_folder(&support);
        unlock_folder(&helpers);
        unlock_folder(&daemons);
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.removed_count, 5);
        let scripts = host.admin_scripts.lock().unwrap();
        assert_eq!(scripts.len(), 1);
        let script = &scripts[0];
        let unload = script.find("launchctl unload").unwrap();
        let first_removal = script.find("/bin/rm -rf").unwrap();
        assert!(unload < first_removal, "{script}");
        assert!(script.contains(&format!(
            "rm -rf -- {}",
            shell_quote(&bar_helper.display().to_string())
        )));
        assert!(script.contains("Foo'\\''s Data"), "{script}");
    }

    #[test]
    fn a_bundle_root_owns_goes_first_and_guards_the_rest_of_its_app() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let support = layout.system("Library/Application Support");
        let data = folder(support.join("com.acme.foo"));
        let mut receipt = row(
            &layout.system("private/var/db/receipts/com.acme.foo.plist"),
            UninstallRole::Receipt {
                package: "com.acme.foo".to_string(),
            },
        );
        receipt.size_bytes = 1;
        let plan = plan_of(&foo, vec![leftover(&data), receipt]);
        let host = FakeHost::default();
        let applications = layout.system("Applications");
        lock_folder(&applications);
        lock_folder(&support);

        run(&plan, &host, &layout);

        unlock_folder(&applications);
        unlock_folder(&support);
        let scripts = host.admin_scripts.lock().unwrap();
        let script = &scripts[0];
        let guard = script.find(&format!(
            "if /bin/rm -rf -- {}",
            shell_quote(&foo.path.display().to_string())
        ));
        let data_line = script.find(&shell_quote(&data.display().to_string()));
        let forget = script.find("/usr/sbin/pkgutil --forget 'com.acme.foo'");
        assert!(guard.unwrap() < data_line.unwrap(), "{script}");
        assert!(data_line.unwrap() < forget.unwrap(), "{script}");
        assert!(script.contains("\nfi"), "{script}");
    }

    #[test]
    fn a_refused_password_keeps_the_app_and_all_its_files() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let cache = folder(layout.user("Library/Caches/com.acme.foo"));
        let plan = plan_of(&foo, vec![leftover(&cache)]);
        let host = FakeHost::default();
        *host.admin_result.lock().unwrap() = Some("The password was not given".to_string());
        let applications = layout.system("Applications");
        lock_folder(&applications);

        let report = run(&plan, &host, &layout);

        unlock_folder(&applications);
        assert!(foo.path.exists());
        assert!(cache.exists());
        assert_eq!(report.removed_count, 0);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].error, "The password was not given");
    }

    #[test]
    fn a_folder_the_user_could_redirect_never_reaches_root() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let vendor = layout.system("Library/Application Support/Avid");
        let product = folder(vendor.join("Sibelius"));
        let plan = plan_of(&foo, vec![leftover(&product)]);
        let host = FakeHost::default();
        lock_folder(&vendor);

        let report = run(&plan, &host, &layout);

        unlock_folder(&vendor);
        assert!(host.admin_scripts.lock().unwrap().is_empty());
        assert_eq!(report.failures.len(), 1);
        assert!(product.exists());
        assert!(!foo.path.exists());
    }

    #[test]
    fn a_file_macos_keeps_from_the_terminal_says_how_to_allow_it() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let container = folder(layout.user("Library/Containers/com.acme.foo"));
        let locked = CString::new(container.join("data").as_os_str().as_bytes()).unwrap();
        assert_eq!(
            unsafe { libc::chflags(locked.as_ptr(), libc::UF_IMMUTABLE) },
            0
        );
        let plan = plan_of(&foo, vec![leftover(&container)]);

        let report = run(&plan, &FakeHost::default(), &layout);

        assert_eq!(unsafe { libc::chflags(locked.as_ptr(), 0) }, 0);
        assert_eq!(report.failures.len(), 1);
        assert!(report.failures[0].error.contains("Full Disk Access"));
    }

    #[test]
    fn a_system_file_the_user_owns_needs_no_password() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let shared = folder(layout.system("Users/Shared/Foo"));
        let plan = plan_of(&foo, vec![leftover(&shared)]);
        let host = FakeHost::default();

        let report = run(&plan, &host, &layout);

        assert!(host.admin_scripts.lock().unwrap().is_empty());
        assert!(!shared.exists());
        assert_eq!(report.removed_count, 2);
    }

    #[test]
    fn a_path_added_to_the_ignore_list_after_review_stays() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let cache = folder(layout.user("Library/Caches/com.acme.foo"));
        let plan = plan_of(&foo, vec![leftover(&cache)]);
        let mut ignore = IgnoreStore::default();
        ignore.add(cache.display().to_string());

        let report = run_ignoring(&plan, &FakeHost::default(), &layout, &ignore);

        assert!(cache.exists());
        assert_eq!(report.failures.len(), 1);
    }

    fn dock_with(layout: &Layout, app: &InstalledApp) -> String {
        let url = format!("file://{}/", app.path.display()).replace(' ', "%20");
        let tile = |url: &str, id: &str| {
            format!(
                "<dict><key>tile-data</key><dict><key>bundle-identifier</key><string>{id}</string><key>file-data</key><dict><key>_CFURLString</key><string>{url}</string></dict></dict></dict>"
            )
        };
        let _ = layout;
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>autohide</key><true/><key>persistent-apps</key><array>{}{}</array></dict></plist>"#,
            tile(&url, "com.acme.tile"),
            tile("file:///Applications/Other.app/", "com.other.tile"),
        )
    }

    #[test]
    fn the_dock_loses_the_tile_of_a_removed_app_and_keeps_the_rest() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo Bar", "com.acme.foo");
        let host = FakeHost::default();
        host.reply("/usr/bin/defaults export", &dock_with(&layout, &foo));
        let imported = Arc::new(std::sync::Mutex::new(String::new()));
        let seen = imported.clone();
        let temp = dock_import_path();
        host.reply("/usr/bin/defaults import", "");
        host.when("/usr/bin/defaults import", move || {
            *seen.lock().unwrap() = fs::read_to_string(&temp).unwrap();
        });

        run(&plan_of(&foo, Vec::new()), &host, &layout);

        assert!(host.ran("/usr/bin/killall Dock"));
        let written = imported.lock().unwrap();
        assert!(!written.contains("com.acme.tile"), "{written}");
        assert!(written.contains("com.other.tile"), "{written}");
        assert!(written.contains("autohide"), "{written}");
        assert!(!dock_import_path().exists());
    }

    #[test]
    fn the_dock_stays_untouched_when_it_has_no_tile_of_the_app() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let other = app("Other", "com.other.tile");
        let host = FakeHost::default();
        host.reply("/usr/bin/defaults export", &dock_with(&layout, &other));
        let dock = dock_with(&layout, &other).replace("com.acme.tile", "com.third.tile");
        host.reply("/usr/bin/defaults export", &dock);

        run(&plan_of(&foo, Vec::new()), &host, &layout);

        assert!(!host.ran("/usr/bin/defaults import"));
        assert!(!host.ran("/usr/bin/killall"));
    }

    #[test]
    fn a_removed_app_with_a_system_extension_says_so() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        folder(
            layout.system("Library/SystemExtensions/AAAA-1111/com.acme.foo.camera.systemextension"),
        );

        let report = run(&plan_of(&foo, Vec::new()), &FakeHost::default(), &layout);

        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("system extension"))
        );
    }

    #[test]
    fn a_container_with_only_its_metadata_left_is_emptied() {
        let (_temp, layout) = sandbox();
        let emptied = layout.user("Library/Containers/com.acme.emptied");
        fs::create_dir_all(&emptied).unwrap();
        fs::write(emptied.join(CONTAINER_METADATA), b"x").unwrap();
        let full = folder(layout.user("Library/Containers/com.acme.full"));
        fs::write(full.join(CONTAINER_METADATA), b"x").unwrap();

        assert!(is_container_stub(&emptied));
        assert!(!is_container_stub(&full));
    }

    #[test]
    fn configuration_that_mentions_a_removed_path_is_reported_by_line() {
        let (_temp, layout) = sandbox();
        let removed = layout.user(".local/share/foo");
        fs::write(
            layout.user(".zshrc"),
            "export A=1\nsource ~/.local/share/foo/init.zsh\n# ~/.local/share/foobar stays\nPATH=$HOME/.local/share/foo:$PATH\n",
        )
        .unwrap();

        let mentions = config_mentions(&layout, std::slice::from_ref(&removed));

        assert_eq!(mentions, ["~/.zshrc:2", "~/.zshrc:4"]);
    }

    #[test]
    fn configuration_with_escaped_slashes_counts_too() {
        let (_temp, layout) = sandbox();
        let removed = layout.user("Applications/Foo.app");
        fs::create_dir_all(layout.user(".claude")).unwrap();
        let path = removed.display().to_string().replace('/', "\\/");
        fs::write(
            layout.user(".claude.json"),
            format!("{{\"a\":\"{path}\"}}\n"),
        )
        .unwrap();

        let mentions = config_mentions(&layout, &[removed]);

        assert_eq!(mentions, ["~/.claude.json:1"]);
    }

    #[test]
    fn a_removed_app_mentioned_in_configuration_adds_a_note() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        fs::write(
            layout.user(".zshrc"),
            format!("alias f={}\n", foo.path.display()),
        )
        .unwrap();

        let report = run(&plan_of(&foo, Vec::new()), &FakeHost::default(), &layout);

        assert!(
            report.notes.iter().any(|note| note.contains("~/.zshrc:1")),
            "{:?}",
            report.notes
        );
    }
}

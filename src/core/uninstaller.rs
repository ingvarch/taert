//! Plans an uninstall as a reviewable list and carries it out.

use std::collections::HashSet;
use std::ffi::CString;
use std::fs;
use std::io::Cursor;
use std::os::macos::fs::MetadataExt as _;
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
    InstalledApp, bundle_id_of, embedded_bundle_ids, find_apps, find_copies, is_common_name,
    official_uninstaller,
};
use super::cleaner::{Cleaner, RemovalFailure, RemovalMode, RemovalProgress, RemovalReport};
use super::host::{Host, Output, shell_quote};
use super::layout::Layout;
use super::leftovers::{
    Evidence, Leftover, UninstallRole, find_leftovers, has_id_boundary, vendor_token,
    without_channel,
};
use super::path_utils::prettify_path;
use super::safety::{Safety, SafetyError};
use super::scanner::{CategoryScan, ScanItem, ScanProgressEvent, ToolGroup};
use super::sizer::DirSizer;
use super::startup::LOGIN_ITEM_SCRIPT;
use super::stores::IgnoreStore;

pub const PLAN_ID: &str = "uninstall";

const MIN_SELECTED_NAME: usize = 4;
const EXIT_POLLS: usize = 10;
const QUIT_POLLS: usize = 50;
const EXIT_POLL: Duration = Duration::from_millis(200);
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(10);
/// How long an app may keep a save dialog open before the quit request counts as ignored.
const QUIT_TIMEOUT: Duration = Duration::from_secs(30);
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
const WRITE_PROBE: &str = ".taert-write-probe";
const LOCKED_FLAGS: u32 =
    libc::UF_IMMUTABLE | libc::UF_APPEND | libc::SF_IMMUTABLE | libc::SF_APPEND;

const ADMIN_PROMPT: &str = "taert needs your password to remove apps and their system files.";
const APP_MANAGEMENT_HINT: &str = "macOS refused. Allow your terminal in System Settings > Privacy & Security > App Management, or move the app to the Trash in Finder";
const FULL_DISK_HINT: &str = "macOS refused. Give your terminal Full Disk Access in System Settings > Privacy & Security, then run this again";
const NOT_SAFE_AS_ROOT: &str = "A folder on the way could be replaced, so taert will not use administrator rights here. Remove it in Finder";
const NOT_REMOVED: &str = "It was still there after the removal";

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

/// Browser profiles and license or plugin state, compared by the start of the
/// path below the home folder or the root. Removing it takes a decision of its own.
const HELD_BACK_PREFIXES: &[&str] = &[
    "library/application support/bravesoftware",
    "library/application support/codex",
    "library/application support/com.operasoftware.opera",
    "library/application support/google/chrome",
    "library/application support/izotope",
    "library/application support/lasersoft imaging",
    "library/application support/microsoft edge",
    "library/caches/adobe ",
    "library/caches/ms-playwright",
    "library/logs/com.openai.codex",
    "library/preferences/com.avid.mediacomposer",
    "library/preferences/com.fabfilter.",
    "library/preferences/com.native-instruments",
    "library/preferences/com.paceap.",
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

fn base_name(app: &InstalledApp) -> String {
    without_channel(&app.name)
        .unwrap_or(&app.name)
        .to_lowercase()
}

/// Do two apps use the same folders? Copies do, and so do channels of one app,
/// like Foo and Foo Beta, and ids that continue each other.
fn shares_data(app: &InstalledApp, other: &InstalledApp) -> bool {
    let related_ids = match (&app.bundle_id, &other.bundle_id) {
        (Some(one), Some(two)) => has_id_boundary(one, two) || has_id_boundary(two, one),
        _ => false,
    };
    related_ids || base_name(app) == base_name(other)
}

fn group_is_shared(
    path: &Path,
    app: &InstalledApp,
    staying: &[InstalledApp],
    layout: &Layout,
) -> bool {
    let in_group_containers =
        path.parent() == Some(layout.user("Library/Group Containers").as_path());
    let vendor = app.bundle_id.as_deref().and_then(vendor_token);
    in_group_containers
        && vendor.is_some()
        && staying
            .iter()
            .any(|other| other.bundle_id.as_deref().and_then(vendor_token) == vendor)
}

fn is_held_back(path: &Path, layout: &Layout) -> bool {
    let relative = path
        .strip_prefix(&layout.home)
        .or(path.strip_prefix(&layout.root));
    let Ok(relative) = relative else {
        return false;
    };
    let relative = relative.to_string_lossy().to_lowercase();
    HELD_BACK_PREFIXES
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
            let is_launch_entry = in_folder_named(&found.path, "LaunchAgents")
                || in_folder_named(&found.path, "LaunchDaemons");
            in_app_place
                && !is_launch_entry
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

/// Does this cask hold the very bundle, and not only one with the same name?
fn cask_holds(cask: &Path, bundle: &Path, name: &std::ffi::OsStr) -> bool {
    let Ok(target) = fs::canonicalize(bundle) else {
        return false;
    };
    let is_target =
        |folder: &Path| fs::canonicalize(folder.join(name)).is_ok_and(|entry| entry == target);
    folders_in(cask).iter().any(|version| {
        is_target(version) || folders_in(version).iter().any(|folder| is_target(folder))
    })
}

fn app_bundles_in(folder: &Path) -> usize {
    let Ok(entries) = fs::read_dir(folder) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| {
            let path = entry.path();
            path.extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("app"))
        })
        .count()
}

/// `brew uninstall` removes all a cask installed, more than the review showed.
fn cask_has_several_apps(token: &str, layout: &Layout) -> bool {
    CASKROOMS.iter().any(|room| {
        folders_in(&layout.system(room).join(token))
            .iter()
            .any(|version| {
                let nested: usize = folders_in(version).iter().map(|f| app_bundles_in(f)).sum();
                app_bundles_in(version) + nested > 1
            })
    })
}

/// The cask that installed an app. More than one candidate means none, because
/// Homebrew must not uninstall a cask taert only guessed.
fn find_cask(app: &InstalledApp, layout: &Layout) -> Option<String> {
    let name = app.path.file_name()?;
    let mut tokens = HashSet::new();
    for room in CASKROOMS {
        for cask in folders_in(&layout.system(room)) {
            if cask_holds(&cask, &app.path, name) {
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
    let mut plan = UninstallPlan {
        scan: CategoryScan {
            notes: Vec::new(),
            id: PLAN_ID.to_string(),
            title: "Review".to_string(),
            groups: Vec::new(),
        },
        apps: Vec::new(),
        notes: Vec::new(),
    };

    let mut going: Vec<&InstalledApp> = Vec::new();
    for app in request.selected {
        if let Some(vendor) = official_uninstaller(app) {
            plan.notes.push(format!(
                "{} needs the uninstaller from {vendor}, so taert leaves it alone.",
                app.name
            ));
        } else if let Err(reason) = Safety::can_uninstall(&app.path, request.ignore, request.layout)
        {
            plan.notes.push(format!("{} stays: {reason}.", app.name));
        } else {
            going.push(app);
        }
    }

    let staying = apps_staying(request, &going);
    let mut listed = HashSet::new();

    for app in going {
        if cancelled() {
            break;
        }
        if let Some(tx) = &progress {
            let _ = tx.send(ScanProgressEvent::ToolStarted(app.name.clone()));
            let bytes = app.size_bytes;
            let _ = tx.send(ScanProgressEvent::ItemFound { bytes });
        }

        let mut cask = find_cask(app, request.layout);
        if let Some(token) = cask.take_if(|token| cask_has_several_apps(token, request.layout)) {
            plan.notes.push(format!(
                "The Homebrew cask {token} holds more apps, so only {} goes and Homebrew keeps listing the cask.",
                app.name
            ));
        }
        let link_only = app.path.is_symlink() && cask.is_none();
        let size = if link_only { 0 } else { app.size_bytes };
        let mut bundle = ScanItem::new(app.path.clone(), size, true, false);
        bundle.role = Some(UninstallRole::Bundle);
        let mut items = vec![bundle];

        let copy = staying.iter().find(|other| shares_data(app, other));
        if link_only {
            plan.notes.push(format!(
                "{} is a link, so only the link goes. The app it points to stays.",
                app.name
            ));
        } else if let Some(copy) = copy {
            plan.notes.push(format!(
                "{} stays at {}, so the settings and data of {} stay too.",
                copy.name,
                prettify_path(&copy.path),
                app.name
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
            shared: link_only || copy.is_some(),
        });
    }

    plan
}

/// Everything that could still use a file: listed apps that are not going, and
/// copies in places the list leaves out.
fn apps_staying(request: &PlanRequest, going: &[&InstalledApp]) -> Vec<InstalledApp> {
    let is_going = |app: &InstalledApp| going.iter().any(|chosen| chosen.path == app.path);
    let resolved: Vec<PathBuf> = going
        .iter()
        .filter_map(|app| fs::canonicalize(&app.path).ok())
        .collect();
    let is_same_bundle =
        |app: &InstalledApp| fs::canonicalize(&app.path).is_ok_and(|path| resolved.contains(&path));

    request
        .installed
        .iter()
        .filter(|app| !is_going(app))
        .cloned()
        .chain(
            find_copies(request.layout)
                .into_iter()
                .filter(|app| !is_same_bundle(app)),
        )
        .collect()
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

fn can_access(path: &Path, mode: libc::c_int) -> bool {
    CString::new(path.as_os_str().as_bytes())
        .is_ok_and(|path| unsafe { libc::access(path.as_ptr(), mode) } == 0)
}

/// Can the user remove everything below a folder? A removal stops at the first
/// thing it may not touch, with the part before it already gone.
fn tree_is_removable(folder: &Path) -> bool {
    let Ok(entries) = fs::read_dir(folder) else {
        return false;
    };
    can_access(folder, libc::W_OK | libc::X_OK)
        && entries.flatten().all(|entry| {
            entry.metadata().is_ok_and(|meta| {
                let locked = meta.st_flags() & LOCKED_FLAGS != 0;
                !locked && (!meta.is_dir() || tree_is_removable(&entry.path()))
            })
        })
}

pub fn user_can_remove(path: &Path) -> bool {
    let owned =
        fs::symlink_metadata(path).is_ok_and(|meta| meta.uid() == unsafe { libc::getuid() });
    owned
        && path
            .parent()
            .is_some_and(|parent| can_access(parent, libc::W_OK))
}

/// App Management can keep a terminal out of an app it owns. Finding that out
/// now, with one empty file, saves stopping the app and asking for a password twice.
pub fn can_write_inside(bundle: &Path) -> bool {
    let contents = bundle.join("Contents");
    if bundle.is_symlink() || !contents.is_dir() {
        return true;
    }
    let probe = contents.join(WRITE_PROBE);
    let writable = fs::File::create(&probe).is_ok();
    let _ = fs::remove_file(&probe);
    writable
}

/// Determines if removing an item requires root privileges.
pub fn needs_root(item: &ScanItem) -> bool {
    if matches!(item.role, Some(UninstallRole::Receipt { .. })) {
        return true;
    }
    if item.path.symlink_metadata().is_ok() {
        if !user_can_remove(&item.path) {
            return true;
        }
        if matches!(item.role, Some(UninstallRole::Bundle))
            && (!can_write_inside(&item.path) || !tree_is_removable(&item.path))
        {
            return true;
        }
        return false;
    }
    if item.path.starts_with("/Library")
        || item.path.starts_with("/System")
        || item.path.starts_with("/usr")
        || item.path.starts_with("/opt")
    {
        return true;
    }
    false
}

fn program_folders(plan: &AppPlan) -> Vec<PathBuf> {
    let mut folders = vec![plan.app.path.clone()];
    // A link can point anywhere. Only a cask's link is known to point at the app.
    if plan.cask.is_some()
        && let Ok(target) = fs::canonicalize(&plan.app.path)
    {
        folders.push(target);
    }
    folders
}

pub(crate) fn is_inside_any(program: &str, folders: &[PathBuf]) -> bool {
    folders
        .iter()
        .any(|folder| Path::new(program).starts_with(folder))
}

fn is_gone(path: &Path) -> bool {
    fs::symlink_metadata(path).is_err()
}

fn in_folder_named(path: &Path, folder: &str) -> bool {
    path.parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == folder)
}

fn list_processes(host: &dyn Host) -> Output {
    host.run(PS, &["-axo", "pid=,ppid=,comm="], TEARDOWN_TIMEOUT)
}

/// The process table, or `None` when `ps` failed and the list cannot be trusted.
pub(crate) fn checked_process_table(host: &dyn Host) -> Option<Vec<(u32, u32, String)>> {
    let listing = list_processes(host);
    listing.ok.then(|| parse_process_table(&listing.stdout))
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

/// True when the selection holds something only root can remove.
pub fn needs_password(scan: &CategoryScan) -> bool {
    scan.selected_items().iter().any(|item| match item.role {
        Some(UninstallRole::Receipt { .. }) => true,
        Some(_) => !user_can_remove(&item.path),
        None => false,
    })
}

/// One app's selected rows, sorted by who can remove them.
struct Job<'a> {
    plan: &'a AppPlan,
    bundle: Option<&'a ScanItem>,
    own_files: Vec<&'a ScanItem>,
    root_files: Vec<&'a ScanItem>,
    receipts: Vec<(&'a ScanItem, &'a str)>,
    /// System launch agents. Root removes their files, and the session they
    /// run in can stop them only by label after that.
    system_agents: Vec<PathBuf>,
    /// Another copy of the app stays, so only this bundle goes.
    shared: bool,
    bundle_needs_root: bool,
    bundle_done: bool,
    bundle_counted: bool,
    root_files_vetted: bool,
    in_root_script: bool,
    /// Another selected copy that shares these root files has not gone yet.
    hold_root_files: bool,
    /// The app stays, so everything else of it stays too.
    blocked: bool,
}

impl<'a> Job<'a> {
    fn new(plan: &'a AppPlan) -> Self {
        Self {
            plan,
            bundle: None,
            own_files: Vec::new(),
            root_files: Vec::new(),
            receipts: Vec::new(),
            system_agents: Vec::new(),
            shared: plan.shared,
            bundle_needs_root: false,
            bundle_done: false,
            bundle_counted: false,
            root_files_vetted: false,
            in_root_script: false,
            hold_root_files: false,
            blocked: false,
        }
    }

    /// Something that stays needs this app's files, so nothing but the bundle goes.
    fn keep_only_bundle(&mut self) {
        self.shared = true;
        self.own_files.clear();
        self.root_files.clear();
        self.receipts.clear();
        self.system_agents.clear();
    }

    /// The bundle, while it is still there to act on.
    fn pending_bundle(&self) -> Option<&'a ScanItem> {
        self.bundle.filter(|_| !self.blocked && !self.bundle_done)
    }

    fn has_root_files(&self) -> bool {
        !self.root_files.is_empty() || !self.receipts.is_empty()
    }
}

struct Tally<F> {
    report: RemovalReport,
    done: usize,
    total: usize,
    progress: F,
    removed_paths: Vec<PathBuf>,
    /// Why root was refused. One answer holds for the whole run.
    root_refusal: Option<String>,
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

    fn removed(&mut self, item: &ScanItem, mode: RemovalMode) {
        self.report.removed_count += 1;
        match mode {
            RemovalMode::Trash if !item.contents_only => {
                self.report.trashed_bytes =
                    self.report.trashed_bytes.saturating_add(item.size_bytes);
            }
            _ => {
                self.report.freed_bytes = self.report.freed_bytes.saturating_add(item.size_bytes);
            }
        }
        self.removed_paths.push(item.path.clone());
    }

    fn removed_permanent(&mut self, item: &ScanItem) {
        self.removed(item, RemovalMode::Permanent);
    }

    fn failed(&mut self, path: &Path, error: impl Into<String>) {
        self.report.failures.push(RemovalFailure {
            path: path.to_path_buf(),
            error: error.into(),
        });
    }

    fn note(&mut self, note: String) {
        self.report.notes.push(note);
    }
}

/// What runs from an app right now. When taert itself runs from it, in a terminal
/// of the app, quitting or signalling the app would end taert halfway.
struct Running {
    pids: Vec<u32>,
    hosts_taert: bool,
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
    pub mode: RemovalMode,
}

impl Uninstaller<'_> {
    /// Removes what is selected in `plan`.
    ///
    /// Before the password an app is only asked to quit, and Homebrew removes its
    /// casks. Whatever else hurts an app that stays comes after it, so a refused
    /// prompt leaves an app with root files installed and working. If macOS only
    /// refuses during the removal itself, that app is stopped by then, and the
    /// second root step can ask again. An app's files go only after the app is
    /// gone, so a failed removal never leaves an app without its data.
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
            root_refusal: None,
        };

        let mut jobs = self.sort_out(plan);
        self.keep_files_for_copies(&mut jobs, &mut tally);
        for job in &mut jobs {
            self.check_job(job, &mut tally);
        }
        for job in &jobs {
            self.ask_to_quit(job);
        }
        for job in &mut jobs {
            self.remove_cask(job, &mut tally);
        }
        self.remove_as_root(&mut jobs, &mut tally);
        self.keep_files_of_failed_copies(&mut jobs, &mut tally);
        for job in &mut jobs {
            self.stop_app(job, &mut tally);
            self.remove_bundle(job, &mut tally);
        }
        // For bundles macOS kept from the user, and files that waited for a bundle.
        self.remove_as_root(&mut jobs, &mut tally);
        // For files that waited for a copy that went in the step before.
        self.remove_as_root(&mut jobs, &mut tally);
        self.keep_files_of_failed_copies(&mut jobs, &mut tally);
        for job in &jobs {
            self.boot_out_system_agents(job);
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
                let mut job = Job::new(app);
                for item in group.items.iter().filter(|item| item.selected) {
                    match &item.role {
                        Some(UninstallRole::Bundle) => job.bundle = Some(item),
                        Some(UninstallRole::Receipt { package }) => {
                            job.receipts.push((item, package.as_str()));
                        }
                        _ if user_can_remove(&item.path) => job.own_files.push(item),
                        _ => {
                            if in_folder_named(&item.path, "LaunchAgents") {
                                job.system_agents.push(item.path.clone());
                            }
                            job.root_files.push(item);
                        }
                    }
                }
                job
            })
            .collect()
    }

    /// Time passes between the review and the removal, so another copy of an
    /// app can be there by now. Its files then stay, like any app's that stays.
    fn keep_files_for_copies<F: FnMut(RemovalProgress)>(
        &self,
        jobs: &mut [Job],
        tally: &mut Tally<F>,
    ) {
        let going: Vec<PathBuf> = jobs
            .iter()
            .filter(|job| job.bundle.is_some())
            .filter_map(|job| fs::canonicalize(&job.plan.app.path).ok())
            .collect();
        let installed: Vec<InstalledApp> = find_apps(self.layout)
            .into_iter()
            .chain(find_copies(self.layout))
            .chain(
                jobs.iter()
                    .filter(|job| job.bundle.is_none())
                    .map(|job| job.plan.app.clone()),
            )
            .filter(|app| fs::canonicalize(&app.path).is_ok_and(|path| !going.contains(&path)))
            .collect();

        let mut stayed = Vec::new();
        for job in jobs
            .iter_mut()
            .filter(|job| job.bundle.is_some() && !job.shared)
        {
            let app = &job.plan.app;
            if let Some(copy) = installed.iter().find(|other| shares_data(app, other)) {
                tally.note(format!(
                    "{} stays at {}, so the settings and data of {} stay too.",
                    copy.name,
                    prettify_path(&copy.path),
                    app.name
                ));
                stayed.push(copy.path.clone());
                job.keep_only_bundle();
            }
        }
        for job in jobs
            .iter_mut()
            .filter(|job| stayed.contains(&job.plan.app.path))
        {
            job.keep_only_bundle();
        }
    }

    /// A selected copy that did not go is an app that stays, for every step after.
    fn keep_files_of_failed_copies<F: FnMut(RemovalProgress)>(
        &self,
        jobs: &mut [Job],
        tally: &mut Tally<F>,
    ) {
        let failed: Vec<InstalledApp> = jobs
            .iter()
            .filter(|job| job.blocked && job.bundle.is_some())
            .map(|job| job.plan.app.clone())
            .collect();
        for job in jobs.iter_mut().filter(|job| !job.blocked && !job.shared) {
            let app = &job.plan.app;
            let copy = failed
                .iter()
                .find(|other| other.path != app.path && shares_data(app, other));
            if let Some(copy) = copy {
                tally.note(format!(
                    "{} stayed, so the settings and data of {} stay too.",
                    copy.name, app.name
                ));
                job.keep_only_bundle();
            }
        }
    }

    fn check_job<F: FnMut(RemovalProgress)>(&self, job: &mut Job, tally: &mut Tally<F>) {
        let Some(bundle) = job.bundle else {
            return;
        };
        let checked = Cleaner::verify(bundle, |path| self.check(path))
            .map_err(|error| error.to_string())
            .and_then(|()| {
                let unchanged = bundle_id_of(&job.plan.app.path) == job.plan.app.bundle_id;
                unchanged
                    .then_some(())
                    .ok_or_else(|| "The app changed after the review".to_string())
            });
        if let Err(reason) = checked {
            tally.step(&bundle.path);
            tally.failed(&bundle.path, reason);
            job.blocked = true;
        }
    }

    /// The polite part of stopping an app, safe to do before the password. A copy
    /// that stays answers to the same id, so it must not hear this.
    fn ask_to_quit(&self, job: &Job) {
        let id = job.plan.app.bundle_id.as_deref();
        let (Some(_), Some(id), false) = (job.pending_bundle(), id, job.shared) else {
            return;
        };
        let running = self.running(job.plan);
        if running.hosts_taert || running.pids.is_empty() {
            return;
        }
        let quit = format!("tell application id \"{id}\" to quit");
        self.host.run(OSASCRIPT, &["-e", &quit], QUIT_TIMEOUT);
        self.wait_for_exit(job.plan, QUIT_POLLS);
    }

    /// Everything that would hurt an app that stays, so it runs past the point
    /// where the password or Homebrew could still keep the app.
    fn stop_app<F: FnMut(RemovalProgress)>(&self, job: &Job, tally: &mut Tally<F>) {
        if job.blocked {
            return;
        }
        for item in &job.own_files {
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
        if !job.shared {
            self.remove_login_items(job.plan);
        }
        self.force_stop(job.plan, tally);
    }

    fn boot_out_system_agents(&self, job: &Job) {
        let uid = unsafe { libc::getuid() };
        for agent in job.system_agents.iter().filter(|agent| is_gone(agent)) {
            if let Some(label) = agent.file_stem() {
                let target = format!("gui/{uid}/{}", label.to_string_lossy());
                self.host
                    .run(LAUNCHCTL, &["bootout", &target], TEARDOWN_TIMEOUT);
            }
        }
    }

    fn remove_login_items(&self, plan: &AppPlan) {
        let mut args: Vec<&str> = Vec::new();
        for line in LOGIN_ITEM_SCRIPT {
            args.extend(["-e", line]);
        }
        // Without `--` a name that starts with a dash is read as an option.
        args.extend(["--", &plan.app.name]);
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
    fn running(&self, plan: &AppPlan) -> Running {
        let folders = program_folders(plan);
        // Whatever `ps` printed before it failed still names processes to stop.
        let table = parse_process_table(&list_processes(self.host).stdout);
        let lineage = own_lineage(&table);
        let inside = table
            .iter()
            .filter(|(_, _, program)| is_inside_any(program, &folders));
        let (own, others): (Vec<_>, Vec<_>) = inside.partition(|(pid, _, _)| lineage.contains(pid));
        Running {
            pids: others.iter().map(|(pid, _, _)| *pid).collect(),
            hosts_taert: !own.is_empty(),
        }
    }

    fn wait_for_exit(&self, plan: &AppPlan, polls: usize) -> bool {
        for _ in 0..polls {
            if self.running(plan).pids.is_empty() {
                return true;
            }
            self.host.pause(EXIT_POLL);
        }
        false
    }

    fn force_stop<F: FnMut(RemovalProgress)>(&self, plan: &AppPlan, tally: &mut Tally<F>) {
        let name = &plan.app.name;
        let Running {
            mut pids,
            hosts_taert,
        } = self.running(plan);
        if hosts_taert {
            tally.note(format!(
                "{name} runs this terminal, so it stays open until you quit it."
            ));
            return;
        }
        if pids.is_empty() {
            return;
        }
        for signal in [libc::SIGTERM, libc::SIGKILL] {
            for pid in &pids {
                self.host.signal(*pid, signal);
            }
            if self.wait_for_exit(plan, EXIT_POLLS) {
                return;
            }
            pids = self.running(plan).pids;
        }
        tally.note(format!("{name} was still running when it was removed."));
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

    fn count_bundle<F: FnMut(RemovalProgress)>(
        &self,
        job: &mut Job,
        bundle: &ScanItem,
        tally: &mut Tally<F>,
    ) {
        if !job.bundle_counted {
            job.bundle_counted = true;
            tally.step(&bundle.path);
        }
    }

    /// Homebrew takes care of its own rights, so casks go before the root script.
    fn remove_cask<F: FnMut(RemovalProgress)>(&self, job: &mut Job, tally: &mut Tally<F>) {
        let (Some(bundle), Some(token)) = (job.pending_bundle(), job.plan.cask.as_deref()) else {
            return;
        };
        self.count_bundle(job, bundle, tally);
        match self.brew_uninstall(token, &bundle.path) {
            BrewResult::Removed => {
                tally.removed_permanent(bundle);
                job.bundle_done = true;
            }
            BrewResult::NotTracked => {}
            BrewResult::Failed(reason) => {
                tally.failed(&bundle.path, reason);
                job.blocked = true;
            }
        }
    }

    fn remove_bundle<F: FnMut(RemovalProgress)>(&self, job: &mut Job, tally: &mut Tally<F>) {
        let Some(bundle) = job.pending_bundle() else {
            return;
        };
        self.count_bundle(job, bundle, tally);
        match Cleaner::new(self.host, self.mode).remove_verified(bundle, |path| self.check(path)) {
            Ok(()) => {
                tally.removed(bundle, self.mode);
                job.bundle_done = true;
            }
            Err(error) if error.is_permission_denied() => job.bundle_needs_root = true,
            Err(error) => {
                tally.failed(&bundle.path, error.to_string());
                job.blocked = true;
            }
        }
    }

    /// Drops what root must not touch.
    fn vet_root_files<F: FnMut(RemovalProgress)>(&self, job: &mut Job, tally: &mut Tally<F>) {
        if std::mem::replace(&mut job.root_files_vetted, true) {
            return;
        }
        for item in std::mem::take(&mut job.root_files) {
            tally.step(&item.path);
            if is_gone(&item.path) {
                tally.removed_permanent(item);
            } else if let Err(error) = Cleaner::verify(item, |path| self.check(path)) {
                tally.failed(&item.path, error.to_string());
            } else if item.identity.is_some() && Safety::is_safe_as_root(&item.path, self.layout) {
                job.root_files.push(item);
            } else {
                tally.failed(&item.path, NOT_SAFE_AS_ROOT);
            }
        }
    }

    /// Finds what root must do for this app, and blocks it if root must not.
    fn vet_for_root<F: FnMut(RemovalProgress)>(&self, job: &mut Job, tally: &mut Tally<F>) {
        job.in_root_script = false;
        if job.blocked {
            return;
        }
        self.vet_root_files(job, tally);

        let Some(bundle) = job.pending_bundle() else {
            return;
        };
        let must = job.bundle_needs_root
            || !user_can_remove(&bundle.path)
            || !can_write_inside(&bundle.path)
            || !tree_is_removable(&bundle.path);
        if must && !self.root_may_remove(bundle) {
            self.count_bundle(job, bundle, tally);
            tally.failed(&bundle.path, NOT_SAFE_AS_ROOT);
            job.blocked = true;
            return;
        }
        job.bundle_needs_root = must;
    }

    fn root_may_remove(&self, bundle: &ScanItem) -> bool {
        bundle.identity.is_some() && Safety::is_safe_as_root(&bundle.path, self.layout)
    }

    /// Decides what goes in the root script. An app with files only root can
    /// remove goes with them, so a refused password keeps all of it. Files wait
    /// while their app or a copy of it is still there.
    fn choose_root_work(&self, jobs: &mut [Job]) {
        let waiting: Vec<bool> = jobs.iter().map(|job| waits_for_copy(job, jobs)).collect();
        for (job, waits) in jobs.iter_mut().zip(waiting) {
            job.hold_root_files = waits;
            let files_ready = job.has_root_files() && !waits;
            match job.pending_bundle() {
                Some(bundle) => {
                    job.bundle_needs_root |= files_ready && self.root_may_remove(bundle);
                    job.in_root_script = job.bundle_needs_root;
                }
                None => job.in_root_script = !job.blocked && files_ready,
            }
        }
    }

    fn remove_as_root<F: FnMut(RemovalProgress)>(&self, jobs: &mut [Job], tally: &mut Tally<F>) {
        self.keep_files_of_failed_copies(jobs, tally);
        for job in jobs.iter_mut() {
            self.vet_for_root(job, tally);
        }
        self.keep_files_of_failed_copies(jobs, tally);
        self.choose_root_work(jobs);
        let script = root_script(jobs);
        if script.is_empty() {
            return;
        }

        for job in jobs.iter_mut().filter(|job| job.in_root_script) {
            if let Some(bundle) = job.bundle.filter(|_| job.bundle_needs_root) {
                self.count_bundle(job, bundle, tally);
            }
            for (item, _) in job.receipts.iter().filter(|_| !job.hold_root_files) {
                tally.step(&item.path);
            }
        }

        if tally.root_refusal.is_none() {
            tally.root_refusal = self.host.run_admin(&script, ADMIN_PROMPT).err();
        }
        let refusal = tally.root_refusal.clone();
        let because = |fallback: &str| refusal.clone().unwrap_or_else(|| fallback.to_string());

        for job in jobs.iter_mut().filter(|job| job.in_root_script) {
            job.in_root_script = false;
            if let Some(bundle) = job.bundle.filter(|_| job.bundle_needs_root) {
                job.bundle_needs_root = false;
                if is_gone(&bundle.path) {
                    tally.removed_permanent(bundle);
                    job.bundle_done = true;
                } else {
                    tally.failed(&bundle.path, because(APP_MANAGEMENT_HINT));
                    job.blocked = true;
                }
            }

            if job.hold_root_files && !job.blocked {
                continue;
            }
            let files = std::mem::take(&mut job.root_files);
            let receipts = std::mem::take(&mut job.receipts);
            if job.blocked {
                continue;
            }
            for item in files
                .into_iter()
                .chain(receipts.into_iter().map(|(item, _)| item))
            {
                if is_gone(&item.path) {
                    tally.removed_permanent(item);
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
        if job.bundle.is_some_and(|bundle| !is_gone(&bundle.path)) {
            let name = &job.plan.app.name;
            tally.note(format!("{name} is back in place, so its files stayed."));
            return;
        }

        for item in &job.own_files {
            tally.step(&item.path);
            if is_gone(&item.path) {
                tally.removed(item, self.mode);
                continue;
            }
            match Cleaner::new(self.host, self.mode).remove_verified(item, |path| self.check(path))
            {
                Ok(()) => tally.removed(item, self.mode),
                Err(_) if is_container_stub(&item.path) => tally.removed(item, self.mode),
                Err(error) if error.is_permission_denied() => {
                    tally.failed(&item.path, FULL_DISK_HINT);
                }
                Err(error) => tally.failed(&item.path, error.to_string()),
            }
        }
    }

    fn has_system_extension(&self, id: &str) -> bool {
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

    fn has_kernel_extension(&self, name: &str) -> bool {
        self.layout
            .system("Library/Extensions")
            .join(format!("{name}.kext"))
            .exists()
    }

    fn has_loaded_job(&self, plan: &AppPlan) -> bool {
        let uid = unsafe { libc::getuid() };
        plan.app
            .bundle_id
            .iter()
            .chain(&plan.login_helpers)
            .filter(|label| !label.to_ascii_lowercase().starts_with("com.apple."))
            .any(|label| {
                let target = format!("gui/{uid}/{label}");
                self.host
                    .run(LAUNCHCTL, &["print", &target], TEARDOWN_TIMEOUT)
                    .ok
            })
    }

    fn clean_dock(&self, removed: &[&Job]) {
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
            removed.iter().any(|job| {
                let id = job.plan.app.bundle_id.as_deref().filter(|_| !job.shared);
                tile_is_of(tile, &job.plan.app.path, id)
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
            let args = ["import", DOCK_DOMAIN, &path];
            if self.host.run(DEFAULTS, &args, TEARDOWN_TIMEOUT).ok {
                self.host
                    .run("/usr/bin/killall", &["Dock"], TEARDOWN_TIMEOUT);
            }
        }
        let _ = fs::remove_file(&import);
    }

    fn tidy_up<F: FnMut(RemovalProgress)>(&self, jobs: &[Job], tally: &mut Tally<F>) {
        let removed: Vec<&Job> = jobs.iter().filter(|job| job.bundle_done).collect();
        if !removed.is_empty() {
            self.clean_dock(&removed);
        }

        for job in removed.iter().filter(|job| !job.shared) {
            let app = &job.plan.app;
            if app
                .bundle_id
                .as_deref()
                .is_some_and(|id| self.has_system_extension(id))
            {
                tally.note(format!(
                    "{} leaves a system extension. Turn it off in System Settings > General > Login Items & Extensions.",
                    app.name
                ));
            }
            if self.has_kernel_extension(&app.name) {
                tally.note(format!(
                    "{} leaves a kernel extension in /Library/Extensions. taert does not remove those.",
                    app.name
                ));
            }
            if self.has_loaded_job(job.plan) {
                tally.note(format!(
                    "{} still has a background job loaded. It ends at your next login.",
                    app.name
                ));
            }
        }

        let mentions = config_mentions(self.layout, &tally.removed_paths);
        if !mentions.is_empty() {
            tally.note(format!(
                "Still mentioned in your configuration: {}",
                mentions.join(", ")
            ));
        }
    }
}

/// Does another selected copy, which uses the same root files, still have to go?
fn waits_for_copy(job: &Job, jobs: &[Job]) -> bool {
    let app = &job.plan.app;
    jobs.iter().any(|other| {
        other.bundle.is_some()
            && !other.bundle_done
            && other.plan.app.path != app.path
            && shares_data(app, &other.plan.app)
    })
}

/// Removes a path only if it is still the object that was reviewed, right at the
/// moment root acts, however long the password prompt was open.
fn guarded_removal(item: &ScanItem) -> Option<String> {
    let identity = item.identity?;
    let path = shell_quote(&item.path.display().to_string());
    let (device, inode) = (identity.dev, identity.ino);
    Some(format!(
        "[ \"$(/usr/bin/stat -f %d:%i {path} 2>/dev/null)\" = '{device}:{inode}' ] && /bin/rm -rf -- {path}"
    ))
}

/// The root part of a run as one script, so macOS asks for the password once.
/// It removes an app's files only once the app is gone.
fn root_script(jobs: &[Job]) -> String {
    let mut lines = Vec::new();

    for job in jobs.iter().filter(|job| job.in_root_script) {
        let mut body = Vec::new();
        let files = job.root_files.iter().filter(|_| !job.hold_root_files);
        for item in files {
            if in_folder_named(&item.path, "LaunchDaemons") {
                let path = shell_quote(&item.path.display().to_string());
                body.push(format!("{LAUNCHCTL} unload {path} >/dev/null 2>&1"));
            }
            body.extend(guarded_removal(item));
        }
        let receipts = job.receipts.iter().filter(|_| !job.hold_root_files);
        for (_, package) in receipts {
            let package = shell_quote(package);
            body.push(format!(
                "/usr/sbin/pkgutil --forget {package} >/dev/null 2>&1"
            ));
        }

        let bundle = job.bundle.filter(|_| job.bundle_needs_root);
        match bundle.and_then(guarded_removal) {
            Some(removal) => {
                lines.push(format!("if {removal}; then"));
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
    fn a_launch_entry_that_only_mentions_the_name_is_not_selected() {
        let (_temp, layout) = sandbox();
        let slack = installed(&layout, "Slack", "com.tinyspeck.slackmacgap");
        let agent = layout.user("Library/LaunchAgents/com.acme.SlackNotifier.plist");
        fs::create_dir_all(agent.parent().unwrap()).unwrap();
        fs::write(&agent, b"x").unwrap();

        let plan = plan_for(&[slack], &layout);

        assert!(!row(&plan, &agent).selected);
    }

    #[test]
    fn a_group_container_waits_for_a_vendor_under_another_domain_too() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let group = folder(layout.user("Library/Group Containers/group.com.acme.foo"));
        let sibling = app("Bar", "io.acme.bar");

        let plan = plan_with(
            std::slice::from_ref(&foo),
            &[foo.clone(), sibling],
            &IgnoreStore::default(),
            &layout,
        );

        assert!(!row(&plan, &group).selected);
    }

    #[test]
    fn license_and_plugin_state_waits_for_a_decision() {
        let (_temp, layout) = sandbox();
        let tool = installed(&layout, "Pace", "com.paceap.eden");
        let plist = layout.user("Library/Preferences/com.paceap.eden.plist");
        fs::create_dir_all(plist.parent().unwrap()).unwrap();
        fs::write(&plist, b"x").unwrap();
        let codex = installed(&layout, "Codex", "com.openai.codex");
        let state = folder(layout.user("Library/Application Support/Codex"));
        let izotope = installed(&layout, "iZotope", "com.izotope.portal");
        let system = folder(layout.system("Library/Application Support/com.izotope.portal"));
        let vendor = folder(layout.system("Library/Application Support/iZotope"));

        let plan = plan_for(&[tool, codex, izotope], &layout);

        assert!(!row(&plan, &plist).selected);
        assert!(!row(&plan, &state).selected);
        assert!(!row(&plan, &vendor).selected);
        assert!(row(&plan, &system).selected);
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
    fn another_channel_of_the_app_keeps_shared_data() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let beta = installed(&layout, "Foo Beta", "com.acme.foobeta");
        let data = folder(layout.user("Library/Application Support/Foo"));

        let plan = plan_with(
            std::slice::from_ref(&foo),
            &[foo.clone(), beta],
            &IgnoreStore::default(),
            &layout,
        );

        assert!(plan.apps[0].shared);
        assert!(!has_row(&plan, &data));
    }

    #[test]
    fn an_app_whose_id_extends_this_one_keeps_shared_data() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let pro = installed(&layout, "Studio", "com.acme.foo.pro");
        let data = folder(layout.user("Library/Containers/com.acme.foo"));

        let plan = plan_with(
            std::slice::from_ref(&foo),
            &[foo.clone(), pro],
            &IgnoreStore::default(),
            &layout,
        );

        assert!(plan.apps[0].shared);
        assert!(!has_row(&plan, &data));
    }

    #[test]
    fn a_copy_on_a_mounted_volume_keeps_shared_data() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        fake_app(
            &layout.system("Volumes/Backup/Foo.app"),
            "com.acme.foo",
            "Foo",
        );
        let data = folder(layout.user("Library/Caches/com.acme.foo"));

        let plan = plan_for(&[foo], &layout);

        assert!(plan.apps[0].shared);
        assert!(!has_row(&plan, &data));
    }

    #[test]
    fn a_selected_copy_that_cannot_go_still_counts_as_staying() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let kept = installed(&layout, "Foo Old", "com.acme.foo");
        let data = folder(layout.user("Library/Caches/com.acme.foo"));
        let mut ignore = IgnoreStore::default();
        ignore.add(kept.path.display().to_string());
        let both = [kept, foo];

        let plan = plan_with(&both, &both, &ignore, &layout);

        assert_eq!(plan.apps.len(), 1);
        assert!(plan.apps[0].shared);
        assert!(!has_row(&plan, &data));
    }

    #[test]
    fn a_link_to_an_app_goes_alone_and_frees_nothing() {
        let (_temp, layout) = sandbox();
        let real = layout.root.join("opt/homebrew/opt/emacs/Emacs.app");
        fake_app(&real, "org.gnu.Emacs", "Emacs");
        let link = layout.system("Applications/Emacs.app");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let mut emacs = app("Emacs", "org.gnu.Emacs");
        emacs.path = link.clone();
        emacs.size_bytes = 500;
        let data = folder(layout.user("Library/Caches/org.gnu.Emacs"));

        let plan = plan_for(&[emacs], &layout);

        assert!(!has_row(&plan, &data));
        assert_eq!(row(&plan, &link).size_bytes, 0);
        assert!(plan.apps[0].shared);
        assert!(plan.notes[0].contains("link"), "{:?}", plan.notes);
    }

    #[test]
    fn a_cask_with_several_apps_stays_with_homebrew() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let bar = installed(&layout, "Bar", "com.acme.bar");
        let room = layout.root.join("opt/homebrew/Caskroom/suite/1.0");
        fs::create_dir_all(&room).unwrap();
        std::os::unix::fs::symlink(&foo.path, room.join("Foo.app")).unwrap();
        std::os::unix::fs::symlink(&bar.path, room.join("Bar.app")).unwrap();

        let plan = plan_with(
            std::slice::from_ref(&foo),
            &[foo.clone(), bar],
            &IgnoreStore::default(),
            &layout,
        );

        assert_eq!(plan.apps[0].cask, None);
        assert!(
            plan.notes.iter().any(|note| note.contains("suite")),
            "{:?}",
            plan.notes
        );
    }

    #[test]
    fn a_link_homebrew_put_there_is_a_full_uninstall() {
        let (_temp, layout) = sandbox();
        let staged = layout
            .root
            .join("opt/homebrew/Caskroom/foo-app/1.0/Foo.app");
        fake_app(&staged, "com.acme.foo", "Foo");
        let link = layout.system("Applications/Foo.app");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&staged, &link).unwrap();
        let mut foo = app("Foo", "com.acme.foo");
        foo.path = link;
        let data = folder(layout.user("Library/Caches/com.acme.foo"));

        let plan = plan_for(&[foo], &layout);

        assert_eq!(plan.apps[0].cask.as_deref(), Some("foo-app"));
        assert!(!plan.apps[0].shared);
        assert!(has_row(&plan, &data));
    }

    #[test]
    fn a_cask_counts_only_for_the_copy_it_installed() {
        let (_temp, layout) = sandbox();
        let theirs = installed(&layout, "Foo", "com.acme.foo");
        let room = layout.root.join("opt/homebrew/Caskroom/foo-app/1.0");
        fs::create_dir_all(&room).unwrap();
        std::os::unix::fs::symlink(&theirs.path, room.join("Foo.app")).unwrap();
        let mine = layout.user("Applications/Foo.app");
        fake_app(&mine, "com.acme.foo", "Foo");
        let copy = InstalledApp {
            path: mine,
            ..theirs.clone()
        };

        let plan = plan_with(
            std::slice::from_ref(&copy),
            &[theirs, copy.clone()],
            &IgnoreStore::default(),
            &layout,
        );

        assert_eq!(plan.apps[0].cask, None);
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
    use crate::core::safety::FileIdentity;
    use crate::core::scanner::ToolGroup;
    use std::os::unix::fs::PermissionsExt;

    const CHECKS_BEFORE_SIGNALS: usize = QUIT_POLLS + 1;

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
                notes: Vec::new(),
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
        host.simulate_trash();
        Uninstaller {
            host,
            layout,
            ignore,
            mode: RemovalMode::Trash,
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
        assert_eq!(report.trashed_bytes, 200);
        assert_eq!(report.freed_bytes, 0);
    }

    #[test]
    fn progress_counts_every_selected_row() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let cache = folder(layout.user("Library/Caches/com.acme.foo"));
        let plan = plan_of(&foo, vec![leftover(&cache)]);
        let host = FakeHost::default();
        host.simulate_trash();
        let ignore = IgnoreStore::default();
        let mut seen = Vec::new();

        Uninstaller {
            host: &host,
            layout: &layout,
            ignore: &ignore,
            mode: RemovalMode::Trash,
        }
        .run(&plan, |step| {
            seen.push((step.current_index, step.total_count))
        });

        assert_eq!(seen, [(1, 2), (2, 2)]);
    }

    #[test]
    fn permanent_mode_frees_bytes_instead_of_trashing() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let cache = folder(layout.user("Library/Caches/com.acme.foo"));
        let plan = plan_of(&foo, vec![leftover(&cache)]);
        let host = FakeHost::default();
        let ignore = IgnoreStore::default();

        let report = Uninstaller {
            host: &host,
            layout: &layout,
            ignore: &ignore,
            mode: RemovalMode::Permanent,
        }
        .run(&plan, |_| {});

        assert_eq!(report.removed_count, 2);
        assert_eq!(report.freed_bytes, 200);
        assert_eq!(report.trashed_bytes, 0);
        assert!(host.trashed().is_empty());
        assert!(!foo.path.exists());
        assert!(!cache.exists());
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
        assert!(script.ends_with(" -- Foo"), "{script}");
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
        for _ in 0..=CHECKS_BEFORE_SIGNALS {
            host.reply(PS, &running(&foo.path, 4242));
        }
        host.reply(PS, "");

        run(&plan_of(&foo, Vec::new()), &host, &layout);

        assert_eq!(*host.signals.lock().unwrap(), [(4242, libc::SIGTERM)]);
    }

    #[test]
    fn an_app_is_still_stopped_when_ps_lists_it_and_then_fails() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let host = FakeHost::default();
        for _ in 0..=CHECKS_BEFORE_SIGNALS {
            host.fail_after_printing(PS, &running(&foo.path, 4242));
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
    fn an_app_that_hosts_this_terminal_is_left_running_whole() {
        let (_temp, layout) = sandbox();
        let code = installed(&layout, "Foo", "com.acme.foo");
        let host = FakeHost::default();
        let me = std::process::id();
        let table = format!(
            "{me} 1 {path}/Contents/MacOS/shell\n4242 1 {path}/Contents/MacOS/helper\n",
            path = code.path.display()
        );
        host.reply(PS, &table);

        let report = run(&plan_of(&code, Vec::new()), &host, &layout);

        assert!(host.signals.lock().unwrap().is_empty());
        assert!(!host.ran("/usr/bin/osascript -e tell application id"));
        assert!(
            report.notes.iter().any(|note| note.contains("terminal")),
            "{:?}",
            report.notes
        );
        assert!(!code.path.exists());
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
        let removed = [
            foo_support.clone(),
            bar_helper.clone(),
            daemon.clone(),
            foo.path.clone(),
            bar.path.clone(),
        ];
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
        let plist_removal = script
            .find(&format!(
                "rm -rf -- {}",
                shell_quote(&daemon.display().to_string())
            ))
            .unwrap();
        assert!(unload < plist_removal, "{script}");
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
            "&& /bin/rm -rf -- {}; then",
            shell_quote(&foo.path.display().to_string())
        ));
        let data_line = script.find(&shell_quote(&data.display().to_string()));
        let forget = script.find("/usr/sbin/pkgutil --forget 'com.acme.foo'");
        assert!(guard.unwrap() < data_line.unwrap(), "{script}");
        assert!(data_line.unwrap() < forget.unwrap(), "{script}");
        assert!(script.contains("\nfi"), "{script}");
    }

    #[test]
    fn root_removes_a_path_only_if_it_is_still_the_reviewed_one() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let support = layout.system("Library/Application Support");
        let data = folder(support.join("com.acme.foo"));
        let plan = plan_of(&foo, vec![leftover(&data)]);
        let identity = FileIdentity::of(&data).unwrap();
        let host = FakeHost::default();
        lock_folder(&support);

        run(&plan, &host, &layout);

        unlock_folder(&support);
        let scripts = host.admin_scripts.lock().unwrap();
        let path = shell_quote(&data.display().to_string());
        let guard = format!(
            "[ \"$(/usr/bin/stat -f %d:%i {path} 2>/dev/null)\" = '{}:{}' ] && /bin/rm -rf -- {path}",
            identity.dev, identity.ino
        );
        assert!(scripts[0].contains(&guard), "{}", scripts[0]);
    }

    #[test]
    fn a_path_that_cannot_be_identified_never_reaches_root() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let support = layout.system("Library/Application Support");
        let data = folder(support.join("com.acme.foo"));
        let mut unknown = leftover(&data);
        unknown.identity = None;
        let plan = plan_of(&foo, vec![unknown]);
        let host = FakeHost::default();
        lock_folder(&support);

        let report = run(&plan, &host, &layout);

        unlock_folder(&support);
        assert!(host.admin_scripts.lock().unwrap().is_empty());
        assert_eq!(report.failures.len(), 1);
    }

    #[test]
    fn a_refused_password_leaves_a_user_owned_app_running_and_whole() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let cache = folder(layout.user("Library/Caches/com.acme.foo"));
        let agent = file(layout.user("Library/LaunchAgents/com.acme.foo.plist"));
        let support = layout.system("Library/Application Support");
        let data = folder(support.join("com.acme.foo"));
        let mut plan = plan_of(
            &foo,
            vec![leftover(&cache), leftover(&agent), leftover(&data)],
        );
        plan.apps[0].login_helpers = vec!["com.acme.foo.login".to_string()];
        let host = FakeHost::default();
        host.reply(PS, &running(&foo.path, 4242));
        *host.admin_result.lock().unwrap() = Some("The password was not given".to_string());
        lock_folder(&support);

        let report = run(&plan, &host, &layout);

        unlock_folder(&support);
        assert!(foo.path.exists());
        assert!(cache.exists());
        assert!(agent.exists());
        assert_eq!(report.removed_count, 0);
        assert!(host.signals.lock().unwrap().is_empty());
        let calls = host.calls.lock().unwrap().join("\n");
        assert!(calls.contains("to quit"), "{calls}");
        for side_effect in [LSREGISTER, "System Events", "bootout", "launchctl unload"] {
            assert!(!calls.contains(side_effect), "{side_effect} in {calls}");
        }
    }

    #[test]
    fn after_the_password_the_app_is_stopped_and_unregistered() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let helpers = layout.system("Library/PrivilegedHelperTools");
        let helper = file(helpers.join("com.acme.foo.helper"));
        let agents = layout.system("Library/LaunchAgents");
        let agent = file(agents.join("com.acme.foo.plist"));
        let plan = plan_of(&foo, vec![leftover(&helper), leftover(&agent)]);
        let host = FakeHost::default();
        let removed = [helper.clone(), agent.clone(), foo.path.clone()];
        *host.admin_effect.lock().unwrap() = Some(Box::new(move || {
            for path in &removed {
                unlock_folder(path.parent().unwrap());
                let _ = fs::remove_dir_all(path);
                let _ = fs::remove_file(path);
            }
        }));
        lock_folder(&helpers);
        lock_folder(&agents);

        let report = run(&plan, &host, &layout);

        unlock_folder(&helpers);
        unlock_folder(&agents);
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        let uid = unsafe { libc::getuid() };
        assert!(host.ran(&format!("{LSREGISTER} -u {}", foo.path.display())));
        assert!(host.ran(&format!("{LAUNCHCTL} bootout gui/{uid}/com.acme.foo")));
    }

    #[test]
    fn a_cask_homebrew_refuses_keeps_the_app_running_and_registered() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let mut plan = plan_of(&foo, Vec::new());
        plan.apps[0].login_helpers = vec!["com.acme.foo.login".to_string()];
        with_cask(&layout, &mut plan, "foo-app");
        let host = FakeHost::default();
        host.fail("/usr/bin/env", "sudo: a password is required");
        host.reply(PS, &running(&foo.path, 4242));

        run(&plan, &host, &layout);

        assert!(host.signals.lock().unwrap().is_empty());
        let calls = host.calls.lock().unwrap().join("\n");
        for side_effect in [LSREGISTER, "System Events", "bootout"] {
            assert!(!calls.contains(side_effect), "{side_effect} in {calls}");
        }
    }

    #[test]
    fn a_stopped_cask_is_unregistered_after_homebrew_removed_it() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let mut plan = plan_of(&foo, Vec::new());
        let room = with_cask(&layout, &mut plan, "foo-app");
        let host = FakeHost::default();
        host.reply("/usr/bin/env", "");
        host.when("/usr/bin/env", removing(&foo.path));
        host.when("/usr/bin/env", removing(&room));

        run(&plan, &host, &layout);

        let brew = host.position("/usr/bin/env").unwrap();
        let unregister = host.position(LSREGISTER).unwrap();
        assert!(brew < unregister);
    }

    #[test]
    fn a_cask_goes_before_the_root_script() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let helpers = layout.system("Library/PrivilegedHelperTools");
        let helper = file(helpers.join("com.acme.foo.helper"));
        let mut plan = plan_of(&foo, vec![leftover(&helper)]);
        let room = with_cask(&layout, &mut plan, "foo-app");
        let host = FakeHost::default();
        host.reply("/usr/bin/env", "");
        host.when("/usr/bin/env", removing(&foo.path));
        host.when("/usr/bin/env", removing(&room));
        let saw_bundle = Arc::new(std::sync::Mutex::new(None));
        let seen = saw_bundle.clone();
        let bundle = foo.path.clone();
        *host.admin_effect.lock().unwrap() = Some(Box::new(move || {
            *seen.lock().unwrap() = Some(bundle.exists());
        }));
        lock_folder(&helpers);

        run(&plan, &host, &layout);

        unlock_folder(&helpers);
        assert_eq!(*saw_bundle.lock().unwrap(), Some(false));
    }

    #[test]
    fn a_bundle_macos_refuses_to_the_user_goes_as_root() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let locked = foo.path.join("Contents/MacOS/Foo");
        let flagged = CString::new(locked.as_os_str().as_bytes()).unwrap();
        assert_eq!(
            unsafe { libc::chflags(flagged.as_ptr(), libc::UF_IMMUTABLE) },
            0
        );
        let plan = plan_of(&foo, Vec::new());
        let host = FakeHost::default();
        let bundle = foo.path.clone();
        *host.admin_effect.lock().unwrap() = Some(Box::new(move || {
            let flagged = CString::new(locked.as_os_str().as_bytes()).unwrap();
            unsafe { libc::chflags(flagged.as_ptr(), 0) };
            let _ = fs::remove_dir_all(&bundle);
        }));

        let report = run(&plan, &host, &layout);

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(!foo.path.exists());
        assert_eq!(report.removed_count, 1);
    }

    #[test]
    fn root_files_wait_for_a_bundle_root_must_not_touch() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let mut mine = foo.clone();
        mine.path = layout.user("Applications/Mine.app");
        mine.name = "Mine".to_string();
        fake_app(&mine.path, "com.acme.mine", "Mine");
        mine.bundle_id = Some("com.acme.mine".to_string());
        fs::remove_dir_all(&foo.path).unwrap();
        let helpers = layout.system("Library/PrivilegedHelperTools");
        let helper = file(helpers.join("com.acme.mine.helper"));
        let plan = plan_of(&mine, vec![leftover(&helper)]);
        let host = FakeHost::default();
        let bundle_at_prompt = Arc::new(std::sync::Mutex::new(None));
        let seen = bundle_at_prompt.clone();
        let bundle = mine.path.clone();
        *host.admin_effect.lock().unwrap() = Some(Box::new(move || {
            *seen.lock().unwrap() = Some(bundle.exists());
        }));
        *host.admin_result.lock().unwrap() = Some("The password was not given".to_string());
        lock_folder(&helpers);

        let report = run(&plan, &host, &layout);

        unlock_folder(&helpers);
        assert_eq!(*bundle_at_prompt.lock().unwrap(), Some(false));
        assert!(helper.exists());
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].path, helper);
    }

    #[test]
    fn root_files_stay_when_a_bundle_root_must_not_touch_cannot_go() {
        let (_temp, layout) = sandbox();
        let mine = layout.user("Applications/Mine.app");
        fake_app(&mine, "com.acme.mine", "Mine");
        let locked = CString::new(mine.join("Contents/MacOS/Mine").as_os_str().as_bytes()).unwrap();
        assert_eq!(
            unsafe { libc::chflags(locked.as_ptr(), libc::UF_IMMUTABLE) },
            0
        );
        let target = InstalledApp {
            path: mine.clone(),
            ..app("Mine", "com.acme.mine")
        };
        let helpers = layout.system("Library/PrivilegedHelperTools");
        let helper = file(helpers.join("com.acme.mine.helper"));
        let plan = plan_of(&target, vec![leftover(&helper)]);
        let host = FakeHost::default();
        lock_folder(&helpers);

        run(&plan, &host, &layout);

        unlock_folder(&helpers);
        assert_eq!(unsafe { libc::chflags(locked.as_ptr(), 0) }, 0);
        assert!(host.admin_scripts.lock().unwrap().is_empty());
        assert!(helper.exists());
    }

    #[test]
    fn a_bundle_the_user_cannot_write_into_waits_for_the_password() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let host = FakeHost::default();
        host.reply(PS, &running(&foo.path, 4242));
        *host.admin_result.lock().unwrap() = Some("The password was not given".to_string());
        let contents = foo.path.join("Contents");
        lock_folder(&contents);

        let report = run(&plan_of(&foo, Vec::new()), &host, &layout);

        unlock_folder(&contents);
        assert_eq!(host.admin_scripts.lock().unwrap().len(), 1);
        assert!(foo.path.join("Contents/MacOS/Foo").exists());
        assert!(host.signals.lock().unwrap().is_empty());
        assert!(!host.ran(LSREGISTER));
        assert_eq!(report.failures.len(), 1);
    }

    #[test]
    fn a_system_agent_stays_loaded_while_its_file_stays() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let agents = layout.system("Library/LaunchAgents");
        let agent = file(agents.join("com.acme.foo.plist"));
        let mut plan = plan_of(&foo, vec![leftover(&agent)]);
        plan.scan.groups[0].items[0].selected = false;
        let host = FakeHost::default();
        *host.admin_result.lock().unwrap() = Some("The password was not given".to_string());
        lock_folder(&agents);

        run(&plan, &host, &layout);

        unlock_folder(&agents);
        assert!(!host.ran("/bin/launchctl bootout"));
    }

    #[test]
    fn every_row_counts_once_in_progress() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let helpers = layout.system("Library/PrivilegedHelperTools");
        let helper = file(helpers.join("com.acme.foo.helper"));
        let cache = folder(layout.user("Library/Caches/com.acme.foo"));
        let receipt_path = layout.system("private/var/db/receipts/com.acme.foo.plist");
        let receipt = row(
            &receipt_path,
            UninstallRole::Receipt {
                package: "com.acme.foo".to_string(),
            },
        );
        let plan = plan_of(&foo, vec![leftover(&helper), leftover(&cache), receipt]);
        let host = FakeHost::default();
        let removed = [helper.clone(), foo.path.clone()];
        *host.admin_effect.lock().unwrap() = Some(Box::new(move || {
            for path in &removed {
                unlock_folder(path.parent().unwrap());
                let _ = fs::remove_dir_all(path);
                let _ = fs::remove_file(path);
            }
        }));
        let ignore = IgnoreStore::default();
        let mut seen = Vec::new();
        lock_folder(&helpers);

        host.simulate_trash();
        let report = Uninstaller {
            host: &host,
            layout: &layout,
            ignore: &ignore,
            mode: RemovalMode::Trash,
        }
        .run(&plan, |step| seen.push(step.current_index));

        unlock_folder(&helpers);
        assert_eq!(seen, [1, 2, 3, 4]);
        assert_eq!(report.removed_count, 4);
    }

    #[test]
    fn a_bundle_with_a_locked_folder_inside_waits_for_the_password() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let kit = folder(foo.path.join("Contents/Frameworks/Kit"));
        let host = FakeHost::default();
        host.reply(PS, &running(&foo.path, 4242));
        *host.admin_result.lock().unwrap() = Some("The password was not given".to_string());
        lock_folder(&kit);

        let report = run(&plan_of(&foo, Vec::new()), &host, &layout);

        unlock_folder(&kit);
        assert_eq!(host.admin_scripts.lock().unwrap().len(), 1);
        assert!(foo.path.join("Contents/MacOS/Foo").exists());
        assert!(host.signals.lock().unwrap().is_empty());
        assert!(!host.ran(LSREGISTER));
        assert_eq!(report.failures.len(), 1);
    }

    #[test]
    fn a_copy_root_must_not_touch_keeps_shared_root_files() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let mine = layout.user("Applications/Foo Old.app");
        fake_app(&mine, "com.acme.foo", "Foo");
        let copy = InstalledApp {
            path: mine.clone(),
            ..app("Foo Old", "com.acme.foo")
        };
        let helpers = layout.system("Library/PrivilegedHelperTools");
        let helper = file(helpers.join("com.acme.foo.helper"));
        let plan = plans_of(vec![
            (foo.clone(), vec![leftover(&helper)]),
            (copy, Vec::new()),
        ]);
        let host = FakeHost::default();
        let removed = helper.clone();
        *host.admin_effect.lock().unwrap() = Some(Box::new(move || {
            unlock_folder(removed.parent().unwrap());
            let _ = fs::remove_file(&removed);
        }));
        let applications = layout.user("Applications");
        lock_folder(&applications);
        lock_folder(&helpers);

        run(&plan, &host, &layout);

        unlock_folder(&applications);
        unlock_folder(&helpers);
        assert!(mine.exists());
        assert!(helper.exists());
    }

    #[test]
    fn a_copy_that_fails_during_removal_keeps_shared_root_files() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let old = installed(&layout, "Foo Old", "com.acme.foo");
        let helpers = layout.system("Library/PrivilegedHelperTools");
        let helper = file(helpers.join("com.acme.foo.helper"));
        let plan = plans_of(vec![
            (foo.clone(), vec![leftover(&helper)]),
            (old.clone(), Vec::new()),
        ]);
        let host = FakeHost::default();
        let (bundle, aside) = (old.path.clone(), layout.user("aside.app"));
        host.when(LSREGISTER, move || {
            if !aside.exists() {
                fs::rename(&bundle, &aside).unwrap();
                fake_app(&bundle, "com.acme.foo", "Foo");
            }
        });
        let removed = [helper.clone(), foo.path.clone()];
        *host.admin_effect.lock().unwrap() = Some(Box::new(move || {
            for path in &removed {
                unlock_folder(path.parent().unwrap());
                let _ = fs::remove_dir_all(path);
                let _ = fs::remove_file(path);
            }
        }));
        lock_folder(&helpers);

        run(&plan, &host, &layout);

        unlock_folder(&helpers);
        assert!(old.path.exists());
        assert!(helper.exists());
    }

    #[test]
    fn a_refused_password_is_never_asked_for_again() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let mine = layout.user("Applications/Mine.app");
        fake_app(&mine, "com.other.mine", "Mine");
        let target = InstalledApp {
            path: mine.clone(),
            ..app("Mine", "com.other.mine")
        };
        let helpers = layout.system("Library/PrivilegedHelperTools");
        let foo_helper = file(helpers.join("com.acme.foo.helper"));
        let mine_helper = file(helpers.join("com.other.mine.helper"));
        let plan = plans_of(vec![
            (foo.clone(), vec![leftover(&foo_helper)]),
            (target, vec![leftover(&mine_helper)]),
        ]);
        let host = FakeHost::default();
        *host.admin_result.lock().unwrap() = Some("The password was not given".to_string());
        lock_folder(&helpers);

        let report = run(&plan, &host, &layout);

        unlock_folder(&helpers);
        assert_eq!(host.admin_scripts.lock().unwrap().len(), 1);
        assert!(foo.path.exists());
        assert!(mine_helper.exists());
        assert!(
            report
                .failures
                .iter()
                .any(|failure| failure.path == mine_helper),
            "{:?}",
            report.failures
        );
    }

    #[test]
    fn a_failed_copy_keeps_shared_root_files_too() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let old = installed(&layout, "Foo Old", "com.acme.foo");
        let helpers = layout.system("Library/PrivilegedHelperTools");
        let helper = file(helpers.join("com.acme.foo.helper"));
        let plan = plans_of(vec![
            (foo.clone(), vec![leftover(&helper)]),
            (old.clone(), Vec::new()),
        ]);
        fake_app(&old.path, "com.other.changed", "Foo");
        let host = FakeHost::default();
        lock_folder(&helpers);

        let report = run(&plan, &host, &layout);

        unlock_folder(&helpers);
        assert!(host.admin_scripts.lock().unwrap().is_empty());
        assert!(helper.exists());
        assert!(
            report.notes.iter().any(|note| note.contains("Foo Old")),
            "{:?}",
            report.notes
        );
    }

    #[test]
    fn a_failed_copy_keeps_its_dock_tile_and_shared_login_helper() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let old = installed(&layout, "Foo Old", "com.acme.foo");
        let mut plan = plans_of(vec![(foo.clone(), Vec::new()), (old.clone(), Vec::new())]);
        plan.apps[0].login_helpers = vec!["com.acme.foo.login".to_string()];
        fake_app(&old.path, "com.other.changed", "Foo");
        let host = FakeHost::default();
        let tiles = dock_with(&layout, &old).replace("com.acme.tile", "com.acme.foo");
        host.reply("/usr/bin/defaults export", &tiles);

        run(&plan, &host, &layout);

        assert!(!host.ran("/bin/launchctl bootout"));
        assert!(!host.ran("/usr/bin/defaults import"));
    }

    #[test]
    fn a_copy_left_unselected_keeps_the_rows_chosen_for_it() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let old = installed(&layout, "Foo Old", "com.acme.foo");
        let cache = folder(layout.user("Library/Caches/com.acme.foo"));
        let mut plan = plans_of(vec![
            (foo.clone(), vec![leftover(&cache)]),
            (old.clone(), Vec::new()),
        ]);
        plan.scan.groups[0].items[0].selected = false;

        run(&plan, &FakeHost::default(), &layout);

        assert!(foo.path.exists());
        assert!(!old.path.exists());
        assert!(cache.exists());
    }

    #[test]
    fn a_file_that_went_with_the_app_counts_as_removed() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let cache = folder(layout.user("Library/Caches/com.acme.foo"));
        let plan = plan_of(&foo, vec![leftover(&cache)]);
        let host = FakeHost::default();
        host.when(LSREGISTER, removing(&cache));

        let report = run(&plan, &host, &layout);

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.removed_count, 2);
    }

    #[test]
    fn a_copy_that_appeared_after_the_review_keeps_the_files() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let cache = folder(layout.user("Library/Caches/com.acme.foo"));
        let plan = plan_of(&foo, vec![leftover(&cache)]);
        fake_app(
            &layout.user("Applications/Foo Copy.app"),
            "com.acme.foo",
            "Foo",
        );
        let host = FakeHost::default();

        let report = run(&plan, &host, &layout);

        assert!(!foo.path.exists());
        assert!(cache.exists());
        assert!(
            report.notes.iter().any(|note| note.contains("Foo Copy")),
            "{:?}",
            report.notes
        );
        assert!(
            !host
                .calls
                .lock()
                .unwrap()
                .join("\n")
                .contains("System Events")
        );
    }

    #[test]
    fn a_copy_left_unselected_in_the_review_counts_as_staying() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let copy = installed(&layout, "Foo Old", "com.acme.foo");
        let cache = folder(layout.user("Library/Caches/com.acme.foo"));
        let mut plan = plans_of(vec![
            (foo.clone(), vec![leftover(&cache)]),
            (copy.clone(), Vec::new()),
        ]);
        plan.scan.groups[1].items[0].selected = false;

        run(&plan, &FakeHost::default(), &layout);

        assert!(!foo.path.exists());
        assert!(copy.path.exists());
        assert!(cache.exists());
    }

    #[test]
    fn a_selected_copy_that_failed_keeps_the_shared_files() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let copy = installed(&layout, "Foo Old", "com.acme.foo");
        let cache = folder(layout.user("Library/Caches/com.acme.foo"));
        let plan = plans_of(vec![
            (foo.clone(), vec![leftover(&cache)]),
            (copy.clone(), Vec::new()),
        ]);
        fake_app(&copy.path, "com.other.changed", "Foo");

        let report = run(&plan, &FakeHost::default(), &layout);

        assert!(copy.path.exists());
        assert!(cache.exists());
        assert_eq!(report.failures.len(), 1);
    }

    #[test]
    fn a_link_never_reaches_the_processes_of_its_target() {
        let (_temp, layout) = sandbox();
        let target = folder(layout.user("elsewhere"));
        let link = layout.system("Applications/Foo.app");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let mut foo = app("Foo", "com.acme.foo");
        foo.path = link.clone();
        foo.bundle_id = None;
        let mut plan = plan_of(&foo, Vec::new());
        plan.apps[0].shared = true;
        let host = FakeHost::default();
        host.reply(PS, &format!("4242 1 {}/program\n", target.display()));

        run(&plan, &host, &layout);

        assert!(host.signals.lock().unwrap().is_empty());
        assert!(!link.is_symlink());
        assert!(target.exists());
    }

    #[test]
    fn a_background_job_still_loaded_is_mentioned() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let host = FakeHost::default();
        let uid = unsafe { libc::getuid() };
        host.reply(
            &format!("{LAUNCHCTL} print gui/{uid}/com.acme.foo"),
            "state = running",
        );

        let report = run(&plan_of(&foo, Vec::new()), &host, &layout);

        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("background job"))
        );
    }

    #[test]
    fn a_kernel_extension_is_mentioned_and_stays() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo Audio", "com.acme.foo");
        let kext = folder(layout.system("Library/Extensions/Foo Audio.kext"));

        let report = run(&plan_of(&foo, Vec::new()), &FakeHost::default(), &layout);

        assert!(kext.exists());
        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("kernel extension"))
        );
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
    fn the_review_knows_when_macos_will_ask_for_a_password() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let cache = folder(layout.user("Library/Caches/com.acme.foo"));
        let plain = plan_of(&foo, vec![leftover(&cache)]);
        let receipt = row(
            &layout.system("private/var/db/receipts/com.acme.foo.plist"),
            UninstallRole::Receipt {
                package: "com.acme.foo".to_string(),
            },
        );
        let with_receipt = plan_of(&foo, vec![receipt]);

        assert!(!needs_password(&plain.scan));
        assert!(needs_password(&with_receipt.scan));
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

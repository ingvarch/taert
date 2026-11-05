//! Plans an uninstall as a reviewable list and carries it out.

use std::collections::{HashMap, HashSet};
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
    InstalledApp, bundle_id_of, embedded_bundle_ids, find_all_apps, find_copies, is_common_name,
    official_uninstaller, package_holds,
};
use super::cleaner::{
    Cleaner, ItemRemovalError, RemovalFailure, RemovalMode, RemovalProgress, RemovalReport,
};
use super::history::{ItemOutcome, OutcomeKind};
use super::homebrew;
use super::host::{Host, shell_quote};
use super::layout::Layout;
use super::leftovers::{
    CONTAINER_METADATA, Evidence, Leftover, UninstallRole, find_leftovers, has_id_boundary,
    vendor_token, without_channel,
};
use super::path_utils::prettify_path;
use super::processes;
use super::safety::{Safety, SafetyError};
use super::scanner::{CategoryScan, ScanItem, ScanProgressEvent, ToolGroup};
use super::sizer::DirSizer;
use super::startup::remove_login_item_args;
use super::stores::IgnoreStore;

pub const PLAN_ID: &str = "uninstall";

const MIN_SELECTED_NAME: usize = 4;
const EXIT_POLLS: usize = 10;
const QUIT_POLLS: usize = 50;
const EXIT_POLL: Duration = Duration::from_millis(200);
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(10);
/// How long an app may keep a save dialog open before the quit request counts as ignored.
const QUIT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const OSASCRIPT: &str = "/usr/bin/osascript";
const LAUNCHCTL: &str = "/bin/launchctl";
const DEFAULTS: &str = "/usr/bin/defaults";
const LSREGISTER: &str = "/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister";
const DOCK_DOMAIN: &str = "com.apple.dock";
const DOCK_LISTS: &[&str] = &["persistent-apps", "persistent-others", "recent-apps"];
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

/// A Homebrew cask and the prefix whose Caskroom holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cask {
    pub token: String,
    pub prefix: PathBuf,
}

impl Cask {
    /// The cask's folder in the Caskroom of its prefix.
    fn room(&self) -> PathBuf {
        self.prefix.join("Caskroom").join(&self.token)
    }
}

impl Serialize for Cask {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.token)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AppPlan {
    pub app: InstalledApp,
    pub cask: Option<Cask>,
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
fn cask_has_several_apps(cask: &Cask) -> bool {
    folders_in(&cask.room()).iter().any(|version| {
        let nested: usize = folders_in(version).iter().map(|f| app_bundles_in(f)).sum();
        app_bundles_in(version) + nested > 1
    })
}

/// The cask that installed an app. More than one candidate means none, because
/// Homebrew must not uninstall a cask taert only guessed.
fn find_cask(app: &InstalledApp, layout: &Layout) -> Option<Cask> {
    let name = app.path.file_name()?;
    let mut found = Vec::new();
    for prefix in layout.homebrew_prefixes() {
        for folder in folders_in(&prefix.join("Caskroom")) {
            let token = folder.file_name()?.to_string_lossy().into_owned();
            if cask_holds(&folder, &app.path, name) && is_cask_token(&token) {
                found.push(Cask {
                    token,
                    prefix: prefix.clone(),
                });
            }
        }
    }
    match found.len() {
        1 => found.pop(),
        _ => None,
    }
}

/// A package app goes through its own gate; every other bundle through `can_uninstall`.
fn check_app(
    app: &InstalledApp,
    path: &Path,
    ignore: &IgnoreStore,
    layout: &Layout,
) -> Result<(), SafetyError> {
    match &app.package {
        Some(package) => Safety::can_uninstall_package_app(path, package, ignore, layout),
        None => Safety::can_uninstall(path, ignore, layout),
    }
    .map(|_| ())
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

    for package in findings.receipts.into_iter().chain(app.package.clone()) {
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
        } else if let Err(reason) = check_app(app, &app.path, request.ignore, request.layout) {
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
        if let Some(Cask { token, .. }) = cask.take_if(|cask| cask_has_several_apps(cask)) {
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
            Some(cask) => format!("{} (Homebrew cask {})", app.name, cask.token),
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
    names.len() == 1 && CONTAINER_METADATA.iter().any(|name| names[0] == *name)
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

/// Who removes a row of an uninstall plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Homebrew,
    User,
    Root,
}

impl Route {
    pub fn mode(self, permanent: bool) -> RemovalMode {
        match self {
            Self::User if !permanent => RemovalMode::Trash,
            _ => RemovalMode::Permanent,
        }
    }
}

/// Decides who removes a row. Reads the disk and writes nothing.
pub fn route(item: &ScanItem, cask: bool) -> Route {
    match item.role {
        Some(UninstallRole::Receipt { .. }) => Route::Root,
        Some(UninstallRole::Bundle) if cask => Route::Homebrew,
        Some(UninstallRole::Bundle)
            if !user_can_remove(&item.path) || !tree_is_removable(&item.path) =>
        {
            Route::Root
        }
        _ if !user_can_remove(&item.path) => Route::Root,
        _ => Route::User,
    }
}

/// The route of every row of a plan.
pub fn routes(plan: &UninstallPlan) -> HashMap<PathBuf, Route> {
    plan.apps
        .iter()
        .zip(&plan.scan.groups)
        .flat_map(|(app, group)| {
            let cask = app.cask.is_some();
            group
                .items
                .iter()
                .map(move |item| (item.path.clone(), route(item, cask)))
        })
        .collect()
}

/// The route of a row, or `Root` when the plan has none for it.
pub fn route_of(routes: &HashMap<PathBuf, Route>, item: &ScanItem) -> Route {
    routes.get(&item.path).copied().unwrap_or(Route::Root)
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

fn is_gone(path: &Path) -> bool {
    fs::symlink_metadata(path).is_err()
}

fn in_folder_named(path: &Path, folder: &str) -> bool {
    path.parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == folder)
}

/// This process and everything that started it, the terminal included.
fn own_lineage(table: &[(u32, u32, String)], me: u32) -> HashSet<u32> {
    let mut lineage = HashSet::new();
    let mut current = me;
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

/// What taert says an uninstall needs the password for.
pub const ADMIN_REASON: &str = "remove apps and their system files";

/// True when the selection holds a row only root can remove.
pub fn needs_password(scan: &CategoryScan, routes: &HashMap<PathBuf, Route>) -> bool {
    scan.selected_items()
        .iter()
        .any(|item| route_of(routes, item) == Route::Root)
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
    bundle_by_root: bool,
    bundle_done: bool,
    bundle_in_trash: bool,
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
            bundle_by_root: false,
            bundle_done: false,
            bundle_in_trash: false,
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

struct Tally<F, O> {
    report: RemovalReport,
    done: usize,
    total: usize,
    progress: F,
    outcome: O,
    removed_paths: Vec<PathBuf>,
    /// Why root was refused. One answer holds for the whole run.
    root_refusal: Option<String>,
}

impl<F: FnMut(RemovalProgress), O: FnMut(&ItemOutcome)> Tally<F, O> {
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
        let is_trash = matches!(mode, RemovalMode::Trash) && !item.contents_only;
        if is_trash {
            self.report.trashed_bytes = self.report.trashed_bytes.saturating_add(item.size_bytes);
        } else {
            self.report.freed_bytes = self.report.freed_bytes.saturating_add(item.size_bytes);
        }
        self.removed_paths.push(item.path.clone());
        (self.outcome)(&ItemOutcome {
            path: item.path.clone(),
            bytes: item.size_bytes,
            outcome: if is_trash {
                OutcomeKind::Trashed
            } else {
                OutcomeKind::Removed
            },
            error: None,
        });
    }

    fn removed_permanent(&mut self, item: &ScanItem) {
        self.removed(item, RemovalMode::Permanent);
    }

    fn failed(&mut self, path: &Path, error: impl Into<String>) {
        let err_str = error.into();
        (self.outcome)(&ItemOutcome {
            path: path.to_path_buf(),
            bytes: 0,
            outcome: OutcomeKind::Failed,
            error: Some(err_str.clone()),
        });
        self.report.failures.push(RemovalFailure {
            path: path.to_path_buf(),
            error: err_str,
        });
    }

    fn note(&mut self, note: String) {
        self.report.notes.push(note);
    }
}

/// A refused Trash call gives only localized text, so the hint comes with it.
fn failure_text(error: &ItemRemovalError, hint: &str) -> String {
    match error {
        ItemRemovalError::Trash(_) => format!("{error}. {hint}"),
        _ => error.to_string(),
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
    /// Before the first password step an app is only asked to quit, and Homebrew
    /// removes its casks. The bundle goes as the user, to the Trash unless --permanent, before
    /// root files. Only a
    /// bundle the user cannot remove goes as root, before the app is stopped, so
    /// a refused prompt keeps that app installed and working. An app's files go
    /// only after the app is gone, so a failed removal never leaves an app
    /// without its data.
    pub fn run(
        &self,
        plan: &UninstallPlan,
        progress: impl FnMut(RemovalProgress),
    ) -> RemovalReport {
        self.run_with_outcome(plan, progress, |_| {})
    }

    /// Removes what is selected in `plan`, reporting item-level outcomes to `outcome`.
    pub fn run_with_outcome<F, O>(
        &self,
        plan: &UninstallPlan,
        progress: F,
        mut outcome: O,
    ) -> RemovalReport
    where
        F: FnMut(RemovalProgress),
        O: FnMut(&ItemOutcome),
    {
        let mut tally = Tally {
            report: RemovalReport {
                notes: plan.notes.clone(),
                ..RemovalReport::default()
            },
            done: 0,
            total: plan.scan.selected_items().len(),
            progress,
            outcome: &mut outcome,
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
        // For files that waited for their bundle.
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

    fn check_bundle(&self, app: &InstalledApp, path: &Path) -> Result<(), SafetyError> {
        check_app(app, path, self.ignore, self.layout)
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
                        _ if route(item, false) == Route::User => job.own_files.push(item),
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
    fn keep_files_for_copies<F: FnMut(RemovalProgress), O: FnMut(&ItemOutcome)>(
        &self,
        jobs: &mut [Job],
        tally: &mut Tally<F, O>,
    ) {
        let going: Vec<PathBuf> = jobs
            .iter()
            .filter(|job| job.bundle.is_some())
            .filter_map(|job| fs::canonicalize(&job.plan.app.path).ok())
            .collect();
        let installed: Vec<InstalledApp> = find_all_apps(self.layout, self.host)
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
    fn keep_files_of_failed_copies<F: FnMut(RemovalProgress), O: FnMut(&ItemOutcome)>(
        &self,
        jobs: &mut [Job],
        tally: &mut Tally<F, O>,
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

    fn check_job<F: FnMut(RemovalProgress), O: FnMut(&ItemOutcome)>(
        &self,
        job: &mut Job,
        tally: &mut Tally<F, O>,
    ) {
        let Some(bundle) = job.bundle else {
            return;
        };
        let app = &job.plan.app;
        let checked = Cleaner::verify(bundle, |path| {
            self.check_bundle(app, path)?;
            let held = app
                .package
                .as_deref()
                .is_none_or(|package| package_holds(self.host, self.layout, package, path));
            held.then_some(()).ok_or(SafetyError::NotPackageApp)
        })
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
    fn stop_app<F: FnMut(RemovalProgress), O: FnMut(&ItemOutcome)>(
        &self,
        job: &Job,
        tally: &mut Tally<F, O>,
    ) {
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
        if !job.shared
            && let Some(reason) = self.remove_login_items(job.plan)
        {
            let name = &job.plan.app.name;
            tally.note(format!(
                "{name}: could not remove its login item ({reason}). Remove it in System Settings > General > Login Items & Extensions."
            ));
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

    /// Returns why the login item stayed, when it did.
    fn remove_login_items(&self, plan: &AppPlan) -> Option<String> {
        let path = plan.app.path.to_string_lossy();
        let args = remove_login_item_args(&plan.app.name, &path);
        let output = self.host.run(OSASCRIPT, &args, TEARDOWN_TIMEOUT);
        let failure = (!output.ok).then(|| output.stderr.lines().next().unwrap_or("").to_string());

        let uid = unsafe { libc::getuid() };
        for helper in &plan.login_helpers {
            if !helper.to_ascii_lowercase().starts_with("com.apple.") {
                let target = format!("gui/{uid}/{helper}");
                self.host
                    .run(LAUNCHCTL, &["bootout", &target], TEARDOWN_TIMEOUT);
            }
        }
        failure
    }

    /// Finds processes by where their program lives, which a name cannot spoof.
    fn running(&self, plan: &AppPlan) -> Running {
        let folders = program_folders(plan);
        // Whatever `ps` printed before it failed still names processes to stop.
        let table = processes::parse(&processes::list(self.host).stdout);
        let lineage = own_lineage(&table, self.host.pid());
        let inside = table
            .iter()
            .filter(|(_, _, program)| processes::is_inside_any(program, &folders));
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

    fn force_stop<F: FnMut(RemovalProgress), O: FnMut(&ItemOutcome)>(
        &self,
        plan: &AppPlan,
        tally: &mut Tally<F, O>,
    ) {
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
                let _ = self.host.signal(*pid, signal);
            }
            if self.wait_for_exit(plan, EXIT_POLLS) {
                return;
            }
            pids = self.running(plan).pids;
        }
        tally.note(format!("{name} was still running when it was removed."));
    }

    fn is_tracked_by_homebrew(cask: &Cask) -> bool {
        cask.room().exists()
    }

    /// Without `--zap`: the plan lists what goes, and a zap could reach further.
    fn brew_uninstall(&self, cask: &Cask, bundle: &Path) -> BrewResult {
        let Some(brew) = homebrew::brew_in(&cask.prefix) else {
            return BrewResult::NotTracked;
        };
        let token = cask.token.as_str();
        let args = ["uninstall", "--cask", token];
        let output = homebrew::run_removing(self.host, &brew.path, &args);
        if output.ok && is_gone(bundle) {
            BrewResult::Removed
        } else if Self::is_tracked_by_homebrew(cask) {
            BrewResult::Failed(format!(
                "Homebrew could not remove it. Run: brew uninstall --cask {token}"
            ))
        } else {
            BrewResult::NotTracked
        }
    }

    fn count_bundle<F: FnMut(RemovalProgress), O: FnMut(&ItemOutcome)>(
        &self,
        job: &mut Job,
        bundle: &ScanItem,
        tally: &mut Tally<F, O>,
    ) {
        if !job.bundle_counted {
            job.bundle_counted = true;
            tally.step(&bundle.path);
        }
    }

    /// Homebrew takes care of its own rights, so casks go before the root script.
    fn remove_cask<F: FnMut(RemovalProgress), O: FnMut(&ItemOutcome)>(
        &self,
        job: &mut Job,
        tally: &mut Tally<F, O>,
    ) {
        let (Some(bundle), Some(cask)) = (job.pending_bundle(), job.plan.cask.as_ref()) else {
            return;
        };
        self.count_bundle(job, bundle, tally);
        match self.brew_uninstall(cask, &bundle.path) {
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

    fn remove_bundle<F: FnMut(RemovalProgress), O: FnMut(&ItemOutcome)>(
        &self,
        job: &mut Job,
        tally: &mut Tally<F, O>,
    ) {
        let Some(bundle) = job.pending_bundle() else {
            return;
        };
        self.count_bundle(job, bundle, tally);
        let app = &job.plan.app;
        let removal = Cleaner::new(self.host, self.mode)
            .remove_verified(bundle, |path| self.check_bundle(app, path));
        match removal {
            Ok(()) => {
                tally.removed(bundle, self.mode);
                job.bundle_done = true;
                job.bundle_in_trash = self.mode == RemovalMode::Trash;
            }
            Err(error) => {
                tally.failed(&bundle.path, failure_text(&error, APP_MANAGEMENT_HINT));
                job.blocked = true;
            }
        }
    }

    /// Drops what root must not touch.
    fn vet_root_files<F: FnMut(RemovalProgress), O: FnMut(&ItemOutcome)>(
        &self,
        job: &mut Job,
        tally: &mut Tally<F, O>,
    ) {
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
    fn vet_for_root<F: FnMut(RemovalProgress), O: FnMut(&ItemOutcome)>(
        &self,
        job: &mut Job,
        tally: &mut Tally<F, O>,
    ) {
        job.in_root_script = false;
        if job.blocked {
            return;
        }
        self.vet_root_files(job, tally);

        let Some(bundle) = job.pending_bundle() else {
            return;
        };
        let must = job.bundle_by_root || route(bundle, false) == Route::Root;
        if must && !self.root_may_remove(bundle) {
            self.count_bundle(job, bundle, tally);
            tally.failed(&bundle.path, NOT_SAFE_AS_ROOT);
            job.blocked = true;
            return;
        }
        job.bundle_by_root = must;
    }

    fn root_may_remove(&self, bundle: &ScanItem) -> bool {
        bundle.identity.is_some() && Safety::is_safe_as_root(&bundle.path, self.layout)
    }

    /// Decides what goes in the root script. Only a bundle the user cannot
    /// remove goes in the script; files wait while their app or a copy of it is
    /// still there.
    fn choose_root_work(&self, jobs: &mut [Job]) {
        let waiting: Vec<bool> = jobs.iter().map(|job| waits_for_copy(job, jobs)).collect();
        for (job, waits) in jobs.iter_mut().zip(waiting) {
            job.hold_root_files = waits;
            let files_ready = job.has_root_files() && !waits;
            match job.pending_bundle() {
                Some(_) => job.in_root_script = job.bundle_by_root,
                None => job.in_root_script = !job.blocked && files_ready,
            }
        }
    }

    fn remove_as_root<F: FnMut(RemovalProgress), O: FnMut(&ItemOutcome)>(
        &self,
        jobs: &mut [Job],
        tally: &mut Tally<F, O>,
    ) {
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
            if let Some(bundle) = job.bundle.filter(|_| job.bundle_by_root) {
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
            if let Some(bundle) = job.bundle.filter(|_| job.bundle_by_root) {
                job.bundle_by_root = false;
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
            let refusal = refusal.as_ref().filter(|_| job.bundle_done);
            let place = if job.bundle_in_trash {
                "in the Trash"
            } else {
                "removed"
            };
            for item in files
                .into_iter()
                .chain(receipts.into_iter().map(|(item, _)| item))
            {
                if is_gone(&item.path) {
                    tally.removed_permanent(item);
                } else if let Some(reason) = refusal {
                    tally.failed(
                        &item.path,
                        format!("{reason}. The app is already {place}, so only these files stay."),
                    );
                } else {
                    tally.failed(&item.path, because(NOT_REMOVED));
                }
            }
        }
    }

    fn remove_own_files<F: FnMut(RemovalProgress), O: FnMut(&ItemOutcome)>(
        &self,
        job: &Job,
        tally: &mut Tally<F, O>,
    ) {
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
                Err(error) => tally.failed(&item.path, failure_text(&error, FULL_DISK_HINT)),
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

    fn tidy_up<F: FnMut(RemovalProgress), O: FnMut(&ItemOutcome)>(
        &self,
        jobs: &[Job],
        tally: &mut Tally<F, O>,
    ) {
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

/// One root step of a run as one script, so macOS asks for the password once
/// for it. It removes an app's files only once the app is gone.
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

        let bundle = job.bundle.filter(|_| job.bundle_by_root);
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
    use crate::core::apps::testing::{
        ACME_PACKAGE, ACME_PREFIX, app, fake_app, sandbox, write_info,
    };
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

    fn package_app(layout: &Layout) -> InstalledApp {
        package_app_of(layout, ACME_PACKAGE)
    }

    fn package_app_of(layout: &Layout, package: &str) -> InstalledApp {
        let path = layout.system(ACME_PREFIX).join("Acme AutoUpdate.app");
        fake_app(&path, "com.acme.autoupdate2", "autoupdate");
        let receipts = layout.system("private/var/db/receipts");
        fs::create_dir_all(&receipts).unwrap();
        let mut info = plist::Dictionary::new();
        info.insert("InstallPrefixPath".into(), ACME_PREFIX.into());
        Value::Dictionary(info)
            .to_file_xml(receipts.join(format!("{package}.plist")))
            .unwrap();
        InstalledApp {
            path,
            package: Some(package.to_string()),
            ..app("Acme AutoUpdate", "com.acme.autoupdate2")
        }
    }

    #[test]
    fn a_package_app_has_a_bundle_row_and_a_selected_receipt_row() {
        let (_temp, layout) = sandbox();
        let acme = package_app(&layout);
        let receipt = layout
            .system("private/var/db/receipts")
            .join(format!("{ACME_PACKAGE}.plist"));

        let plan = plan_for(std::slice::from_ref(&acme), &layout);

        assert!(plan.notes.is_empty(), "{:?}", plan.notes);
        assert_eq!(plan.apps.len(), 1);
        assert_eq!(row(&plan, &acme.path).role, Some(UninstallRole::Bundle));
        let receipt_row = row(&plan, &receipt);
        assert!(receipt_row.selected);
        assert_eq!(
            receipt_row.role,
            Some(UninstallRole::Receipt {
                package: ACME_PACKAGE.to_string()
            })
        );
    }

    #[test]
    fn a_receipt_that_pkgutil_also_finds_is_one_row() {
        let (_temp, layout) = sandbox();
        let found = "com.acme.autoupdate2.pkg";
        let acme = package_app_of(&layout, found);
        let host = FakeHost::default();
        host.reply(
            &format!("/usr/sbin/pkgutil --file-info {}", acme.path.display()),
            &format!("pkgid: {found}\n"),
        );
        host.reply(
            &format!("/usr/sbin/pkgutil --pkg-info {found}"),
            &format!("package-id: {found}\nvolume: /\nlocation: {ACME_PREFIX}\n"),
        );
        host.reply(
            &format!("/usr/sbin/pkgutil --files {found}"),
            "Acme AutoUpdate.app\n",
        );
        let ignore = IgnoreStore::default();
        let request = PlanRequest {
            selected: std::slice::from_ref(&acme),
            installed: std::slice::from_ref(&acme),
            ignore: &ignore,
            host: &host,
            layout: &layout,
        };
        let findings = find_leftovers(&acme, &[], &host, &layout);
        assert_eq!(findings.receipts, [found]);

        let plan = build_plan(&request, None, None);

        let receipts = plan
            .scan
            .groups
            .iter()
            .flat_map(|group| &group.items)
            .filter(|item| matches!(item.role, Some(UninstallRole::Receipt { .. })))
            .count();
        assert_eq!(receipts, 1);
    }

    #[test]
    fn a_package_app_whose_receipt_is_gone_stays() {
        let (_temp, layout) = sandbox();
        let acme = package_app(&layout);
        fs::remove_file(
            layout
                .system("private/var/db/receipts")
                .join(format!("{ACME_PACKAGE}.plist")),
        )
        .unwrap();

        let plan = plan_for(std::slice::from_ref(&acme), &layout);

        assert!(plan.apps.is_empty());
        assert_eq!(
            plan.notes,
            vec![format!(
                "Acme AutoUpdate stays: {}.",
                SafetyError::NotPackageApp
            )]
        );
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
    fn files_named_after_an_inner_id_of_a_package_app_are_listed_but_not_selected() {
        let (_temp, layout) = sandbox();
        let acme = package_app(&layout);
        fake_app(
            &acme.path.join("Contents/MacOS/Acme Daemon.app"),
            "com.acme.autoupdate.fba",
            "Acme Daemon",
        );
        let files = [
            layout.system("Library/LaunchDaemons/com.acme.autoupdate.helper.plist"),
            layout.system("Library/LaunchAgents/com.acme.autoupdate.agent.plist"),
            layout.user("Library/LaunchAgents/com.acme.autoupdate.agent.plist"),
            layout.system("Library/PrivilegedHelperTools/com.acme.autoupdate.helper"),
            layout.system("Library/Caches/com.acme.autoupdate.fba"),
            layout.user("Library/Caches/com.acme.autoupdate.helper"),
        ];
        for path in &files {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"x").unwrap();
        }

        let plan = plan_for(&[acme], &layout);

        for path in &files {
            let item = row(&plan, path);
            assert!(!item.selected, "{path:?}");
            assert_eq!(
                item.role,
                Some(UninstallRole::Leftover {
                    evidence: Evidence::RelatedName
                })
            );
        }
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

        assert_eq!(plan.apps[0].cask.as_ref().unwrap().token, "foo-app");
        assert!(!plan.apps[0].shared);
        assert!(has_row(&plan, &data));
    }

    fn caskroom_link(layout: &Layout, prefix: &str, app: &InstalledApp) {
        let room = layout.system(prefix).join("Caskroom/foo-app/1.0");
        fs::create_dir_all(&room).unwrap();
        std::os::unix::fs::symlink(&app.path, room.join("Foo.app")).unwrap();
    }

    #[test]
    fn the_plan_json_keeps_the_cask_token() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        caskroom_link(&layout, "usr/local", &foo);

        let plan = plan_for(std::slice::from_ref(&foo), &layout);
        let json = serde_json::to_value(&plan).unwrap();
        assert_eq!(json["apps"][0]["cask"], serde_json::json!("foo-app"));

        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let plan = plan_for(std::slice::from_ref(&foo), &layout);
        let json = serde_json::to_value(&plan).unwrap();
        assert_eq!(json["apps"][0]["cask"], serde_json::Value::Null);
    }

    #[test]
    fn a_cask_in_usr_local_keeps_its_prefix() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        caskroom_link(&layout, "usr/local", &foo);

        let plan = plan_for(std::slice::from_ref(&foo), &layout);

        let cask = plan.apps[0].cask.as_ref().unwrap();
        assert_eq!(cask.token, "foo-app");
        assert_eq!(cask.prefix, layout.system("usr/local"));
    }

    #[test]
    fn a_bundle_both_caskrooms_hold_has_no_cask() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        caskroom_link(&layout, "opt/homebrew", &foo);
        caskroom_link(&layout, "usr/local", &foo);

        let plan = plan_for(std::slice::from_ref(&foo), &layout);

        assert!(plan.apps[0].cask.is_none());
        assert_eq!(plan.scan.groups[0].tool, "Foo");
    }

    #[test]
    fn several_apps_in_another_caskroom_do_not_count() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        caskroom_link(&layout, "opt/homebrew", &foo);
        let other = layout.system("usr/local/Caskroom/foo-app/1.0");
        fs::create_dir_all(other.join("A.app")).unwrap();
        fs::create_dir_all(other.join("B.app")).unwrap();

        let plan = plan_for(std::slice::from_ref(&foo), &layout);

        let cask = plan.apps[0].cask.as_ref().unwrap();
        assert_eq!(cask.prefix, layout.system("opt/homebrew"));
        assert!(
            !plan.notes.iter().any(|n| n.contains("holds more apps")),
            "{:?}",
            plan.notes
        );
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

        assert_eq!(plan.apps[0].cask.as_ref().unwrap().token, "foo-app");
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
    use crate::core::apps::testing::{ACME_PACKAGE, ACME_PREFIX, app, fake_app, sandbox};
    use crate::core::host::fake::FakeHost;
    use crate::core::processes::PS;
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

    fn package_plan(layout: &Layout) -> (UninstallPlan, PathBuf) {
        let path = layout.system(ACME_PREFIX).join("Acme AutoUpdate.app");
        fake_app(&path, "com.acme.autoupdate2", "autoupdate");
        let receipt =
            file(layout.system("private/var/db/receipts/com.acme.package.Acme_AutoUpdate.plist"));
        let mut info = plist::Dictionary::new();
        info.insert("InstallPrefixPath".into(), ACME_PREFIX.into());
        Value::Dictionary(info).to_file_xml(&receipt).unwrap();
        let acme = InstalledApp {
            path: path.clone(),
            package: Some(ACME_PACKAGE.to_string()),
            ..app("Acme AutoUpdate", "com.acme.autoupdate2")
        };
        let rows = vec![row(
            &receipt,
            UninstallRole::Receipt {
                package: ACME_PACKAGE.to_string(),
            },
        )];
        (plan_of(&acme, rows), path)
    }

    #[test]
    fn a_package_app_goes_to_the_trash_and_its_receipt_is_forgotten() {
        let (_temp, layout) = sandbox();
        let (plan, path) = package_plan(&layout);
        let host = FakeHost::default();
        host.reply(
            &format!("/usr/sbin/pkgutil --files {ACME_PACKAGE}"),
            "Acme AutoUpdate.app\nAcme AutoUpdate.app/Contents/Info.plist\n",
        );
        let receipt =
            layout.system("private/var/db/receipts/com.acme.package.Acme_AutoUpdate.plist");
        *host.admin_effect.lock().unwrap() = Some(Box::new(move || {
            fs::remove_file(&receipt).unwrap();
        }));

        let report = run(&plan, &host, &layout);

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(!path.exists());
        assert_eq!(host.trashed(), vec![path]);
        let scripts = host.admin_scripts.lock().unwrap();
        assert!(
            scripts[0].contains("/usr/sbin/pkgutil --forget 'com.acme.package.Acme_AutoUpdate'"),
            "{scripts:?}"
        );
    }

    #[test]
    fn a_package_app_the_package_no_longer_lists_stays() {
        let (_temp, layout) = sandbox();
        let (plan, path) = package_plan(&layout);
        let host = FakeHost::default();
        host.reply(
            &format!("/usr/sbin/pkgutil --files {ACME_PACKAGE}"),
            "Other.app\n",
        );

        let report = run(&plan, &host, &layout);

        assert!(path.exists());
        assert!(host.trashed().is_empty());
        assert!(host.admin_scripts.lock().unwrap().is_empty());
        assert_eq!(report.failures.len(), 1);
        assert_eq!(
            report.failures[0].error,
            "Safety check failed: app is not one an installer package put here"
        );
    }

    #[test]
    fn a_package_app_stays_when_pkgutil_cannot_list_the_package() {
        let (_temp, layout) = sandbox();
        let (plan, path) = package_plan(&layout);
        let host = FakeHost::default();
        host.fail(&format!("/usr/sbin/pkgutil --files {ACME_PACKAGE}"), "no");

        let report = run(&plan, &host, &layout);

        assert!(path.exists());
        assert!(host.trashed().is_empty());
        assert!(host.admin_scripts.lock().unwrap().is_empty());
        assert_eq!(report.failures.len(), 1);
        assert_eq!(
            report.failures[0].error,
            "Safety check failed: app is not one an installer package put here"
        );
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
    fn run_with_outcome_reports_each_item() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let cache = folder(layout.user("Library/Caches/com.acme.foo"));
        let plan = plan_of(&foo, vec![leftover(&cache)]);
        let host = FakeHost::default();
        host.simulate_trash();
        let ignore = IgnoreStore::default();
        let mut outcomes = Vec::new();

        Uninstaller {
            host: &host,
            layout: &layout,
            ignore: &ignore,
            mode: RemovalMode::Trash,
        }
        .run_with_outcome(&plan, |_| {}, |outcome| outcomes.push(outcome.clone()));

        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[0].path, foo.path);
        assert_eq!(outcomes[0].outcome, OutcomeKind::Trashed);
        assert_eq!(outcomes[1].path, cache);
        assert_eq!(outcomes[1].outcome, OutcomeKind::Trashed);
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
    fn the_login_item_of_the_app_is_removed_by_its_name_and_path() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let host = FakeHost::default();

        run(&plan_of(&foo, Vec::new()), &host, &layout);

        let calls = host.calls.lock().unwrap();
        let script = calls
            .iter()
            .find(|line| line.contains("System Events"))
            .unwrap();
        let args = format!(" -- Foo {}", foo.path.display());
        assert!(script.ends_with(&args), "{script}");
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
        host.reply(OSASCRIPT, "");

        let report = run(&plan_of(&foo, Vec::new()), &host, &layout);

        assert!(host.ran("/usr/bin/osascript -e tell application id \"com.acme.foo\" to quit"));
        assert!(host.signals.lock().unwrap().is_empty());
        assert!(report.notes.is_empty(), "{:?}", report.notes);
    }

    #[test]
    fn a_fake_process_with_the_test_pid_is_still_asked_to_quit() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let host = FakeHost::default();
        let me = std::process::id();
        host.reply(PS, &running(&foo.path, me));
        host.reply(PS, "");
        host.reply(OSASCRIPT, "");

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
        host.set_pid(4100);
        let me = host.pid();
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
        host.set_pid(4100);
        let me = host.pid();
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

        let host = FakeHost::default();
        host.reply(OSASCRIPT, "");

        let report = run(&plan, &host, &layout);

        assert!(foo.path.exists());
        assert!(!bar.path.exists());
        assert_eq!(report.notes, ["From the plan"]);
    }

    fn with_cask(layout: &Layout, plan: &mut UninstallPlan, prefix: &str, token: &str) -> PathBuf {
        file(layout.system(prefix).join("bin/brew"));
        plan.apps[0].cask = Some(Cask {
            token: token.to_string(),
            prefix: layout.system(prefix),
        });
        folder(layout.system(prefix).join("Caskroom").join(token))
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
        let room = with_cask(&layout, &mut plan, "opt/homebrew", "foo-app");
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
    fn a_cask_removal_keeps_homebrew_from_removing_unused_formulae() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let mut plan = plan_of(&foo, vec![]);
        let room = with_cask(&layout, &mut plan, "opt/homebrew", "foo-app");
        let host = FakeHost::default();
        host.reply("/usr/bin/env", "");
        host.when("/usr/bin/env", removing(&foo.path));
        host.when("/usr/bin/env", removing(&room));

        run(&plan, &host, &layout);

        let brew = layout.system("opt/homebrew/bin/brew");
        let expected = format!(
            "/usr/bin/env HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_ENV_HINTS=1 NONINTERACTIVE=1 HOMEBREW_NO_AUTOREMOVE=1 {} uninstall --cask foo-app",
            brew.display()
        );
        let calls = host.calls.lock().unwrap();
        let removals: Vec<&String> = calls
            .iter()
            .filter(|call| call.contains("uninstall --cask"))
            .collect();
        assert_eq!(removals, [&expected]);
    }

    fn cask_plan_in(layout: &Layout, foo: &InstalledApp, prefix: &str) -> UninstallPlan {
        let link = layout.system(prefix).join("Caskroom/foo-app/1.0");
        fs::create_dir_all(&link).unwrap();
        std::os::unix::fs::symlink(&foo.path, link.join("Foo.app")).unwrap();
        let host = FakeHost::default();
        let ignore = IgnoreStore::default();
        let apps = std::slice::from_ref(foo);
        let request = PlanRequest {
            selected: apps,
            installed: apps,
            ignore: &ignore,
            host: &host,
            layout,
        };
        build_plan(&request, None, None)
    }

    #[test]
    fn a_cask_goes_through_the_brew_of_its_prefix() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let plan = cask_plan_in(&layout, &foo, "usr/local");
        file(layout.system("opt/homebrew/bin/brew"));
        let brew = file(layout.system("usr/local/bin/brew"));
        let host = FakeHost::default();
        host.reply("/usr/bin/env", "");
        host.when("/usr/bin/env", removing(&foo.path));
        host.when(
            "/usr/bin/env",
            removing(&layout.system("usr/local/Caskroom/foo-app")),
        );

        let report = run(&plan, &host, &layout);

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        let expected = format!(
            "/usr/bin/env HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_ENV_HINTS=1 NONINTERACTIVE=1 HOMEBREW_NO_AUTOREMOVE=1 {} uninstall --cask foo-app",
            brew.display()
        );
        let calls = host.calls.lock().unwrap();
        let removals: Vec<&String> = calls
            .iter()
            .filter(|call| call.contains("uninstall --cask"))
            .collect();
        assert_eq!(removals, [&expected]);
    }

    #[test]
    fn a_cask_whose_prefix_has_no_brew_is_not_tracked() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let plan = cask_plan_in(&layout, &foo, "usr/local");
        file(layout.system("opt/homebrew/bin/brew"));
        let host = FakeHost::default();
        host.fail("/usr/bin/env", "Error: Cask 'foo-app' is not installed.");

        let report = run(&plan, &host, &layout);

        assert!(!host.ran("/usr/bin/env"));
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(!foo.path.exists());
    }

    #[test]
    fn a_failed_brew_looks_only_in_its_own_caskroom() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let plan = cask_plan_in(&layout, &foo, "opt/homebrew");
        let host = FakeHost::default();
        file(layout.system("opt/homebrew/bin/brew"));
        file(layout.system("usr/local/bin/brew"));
        fs::create_dir_all(layout.system("usr/local/Caskroom/foo-app/1.0")).unwrap();
        fs::remove_dir_all(layout.system("opt/homebrew/Caskroom/foo-app")).unwrap();
        host.fail("/usr/bin/env", "Cask is not installed");

        let report = run(&plan, &host, &layout);

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(!foo.path.exists());
    }

    #[test]
    fn a_cask_homebrew_keeps_is_a_failure_and_the_files_stay() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let cache = folder(layout.user("Library/Caches/com.acme.foo"));
        let mut plan = plan_of(&foo, vec![leftover(&cache)]);
        with_cask(&layout, &mut plan, "opt/homebrew", "foo-app");
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
        let room = with_cask(&layout, &mut plan, "opt/homebrew", "foo-app");
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
    fn a_refused_password_leaves_only_the_root_files() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let cache = folder(layout.user("Library/Caches/com.acme.foo"));
        let support = layout.system("Library/Application Support");
        let data = folder(support.join("com.acme.foo"));
        let plan = plan_of(&foo, vec![leftover(&cache), leftover(&data)]);
        let host = FakeHost::default();
        *host.admin_result.lock().unwrap() = Some("The password was not given".to_string());
        lock_folder(&support);

        let report = run(&plan, &host, &layout);

        unlock_folder(&support);
        assert!(host.trashed().contains(&foo.path));
        assert!(!foo.path.exists());
        assert!(!cache.exists());
        assert!(data.exists());
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].path, data);
    }

    #[test]
    fn the_app_is_unregistered_and_its_agent_booted_out_once_gone() {
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
        with_cask(&layout, &mut plan, "opt/homebrew", "foo-app");
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
        let room = with_cask(&layout, &mut plan, "opt/homebrew", "foo-app");
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
        let room = with_cask(&layout, &mut plan, "opt/homebrew", "foo-app");
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
        let contents = foo.path.join("Contents");
        lock_folder(&contents);

        let report = run(&plan, &host, &layout);

        unlock_folder(&helpers);
        unlock_folder(&contents);
        assert_eq!(host.admin_scripts.lock().unwrap().len(), 1);
        assert!(foo.path.exists());
        assert!(foo_helper.exists());
        assert!(host.trashed().contains(&mine));
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
    fn an_app_from_a_package_counts_as_a_copy_that_stays() {
        let (_temp, layout) = sandbox();
        let copy = installed(&layout, "Acme Copy", "com.acme.autoupdate2");
        let cache = folder(layout.user("Library/Caches/com.acme.autoupdate2"));
        let plan = plan_of(&copy, vec![leftover(&cache)]);
        let package_path = crate::core::apps::testing::package_app(&layout);
        let host = FakeHost::default();
        crate::core::apps::testing::package_listing(&host);

        let report = run(&plan, &host, &layout);

        assert!(!copy.path.exists());
        assert!(package_path.exists());
        assert!(cache.exists());
        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("Acme AutoUpdate")),
            "{:?}",
            report.notes
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

        assert!(!needs_password(&plain.scan, &routes(&plain)));
        assert!(needs_password(&with_receipt.scan, &routes(&with_receipt)));
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
        fs::write(emptied.join(CONTAINER_METADATA[0]), b"x").unwrap();
        let full = folder(layout.user("Library/Containers/com.acme.full"));
        fs::write(full.join(CONTAINER_METADATA[0]), b"x").unwrap();

        assert!(is_container_stub(&emptied));
        assert!(!is_container_stub(&full));
    }

    #[test]
    fn a_stub_with_the_older_metadata_name_is_emptied() {
        let (_temp, layout) = sandbox();
        let emptied = layout.user("Library/Containers/com.acme.emptied");
        fs::create_dir_all(&emptied).unwrap();
        fs::write(
            emptied.join(".com.apple.containermanager.metadata.plist"),
            b"x",
        )
        .unwrap();

        assert!(is_container_stub(&emptied));
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

    fn receipt_row(layout: &Layout) -> ScanItem {
        let mut receipt = row(
            &layout.system("private/var/db/receipts/com.acme.foo.plist"),
            UninstallRole::Receipt {
                package: "com.acme.foo".to_string(),
            },
        );
        receipt.size_bytes = 1;
        receipt
    }

    #[test]
    fn a_pkg_app_goes_to_the_trash_as_the_user() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let plan = plan_of(&foo, vec![receipt_row(&layout)]);
        let host = FakeHost::default();

        run(&plan, &host, &layout);

        assert!(host.trashed().contains(&foo.path));
        let scripts = host.admin_scripts.lock().unwrap();
        let script = scripts.join("\n");
        assert!(script.contains("pkgutil --forget"), "{script}");
        let bundle = shell_quote(&foo.path.display().to_string());
        assert!(!script.contains(&format!("rm -rf -- {bundle}")), "{script}");
    }

    #[test]
    fn a_localized_trash_refusal_never_reaches_root() {
        for message in ["Der Vorgang ist nicht erlaubt", "operation not permitted"] {
            let (_temp, layout) = sandbox();
            let foo = installed(&layout, "Foo", "com.acme.foo");
            let plan = plan_of(&foo, vec![receipt_row(&layout)]);
            let host = FakeHost::default();
            host.fail_trash(message);

            let report = run(&plan, &host, &layout);

            assert!(foo.path.exists(), "{message}");
            assert!(host.admin_scripts.lock().unwrap().is_empty(), "{message}");
            let failure = report
                .failures
                .iter()
                .find(|failure| failure.path == foo.path)
                .unwrap();
            assert!(failure.error.ends_with(APP_MANAGEMENT_HINT), "{message}");
        }
    }

    #[test]
    fn a_refused_trash_leaves_a_file_row_and_shows_the_hint() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let data = folder(layout.user("Library/Containers/com.acme.foo"));
        let mut plan = plan_of(&foo, vec![leftover(&data)]);
        let host = FakeHost::default();
        host.fail_trash("operation not permitted");
        plan.scan.groups[0].items[0].selected = false;

        let report = run(&plan, &host, &layout);

        assert!(data.exists());
        let failure = report
            .failures
            .iter()
            .find(|failure| failure.path == data)
            .unwrap();
        assert_eq!(
            failure.error,
            format!("operation not permitted. {FULL_DISK_HINT}")
        );
    }

    fn entries(folder: &Path) -> Vec<std::ffi::OsString> {
        let mut names: Vec<_> = fs::read_dir(folder)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn route_does_not_write_inside_a_bundle() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let contents = foo.path.join("Contents");
        let mtime = || fs::metadata(&contents).unwrap().modified().unwrap();
        let (before_time, before_names) = (mtime(), entries(&contents));

        let went = route(&row(&foo.path, UninstallRole::Bundle), false);

        assert_eq!(went, Route::User);
        assert_eq!(mtime(), before_time);
        assert_eq!(entries(&contents), before_names);
    }

    #[test]
    fn a_cask_bundle_goes_through_homebrew() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let mut plan = plan_of(&foo, Vec::new());
        assert_eq!(routes(&plan)[&foo.path], Route::User);

        with_cask(&layout, &mut plan, "opt/homebrew", "foo-app");

        assert_eq!(routes(&plan)[&foo.path], Route::Homebrew);
        assert_eq!(Route::Homebrew.mode(false), RemovalMode::Permanent);
    }

    #[test]
    fn a_locked_entry_inside_a_bundle_asks_for_a_password() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let inside = foo.path.join("Contents/locked");
        fs::write(&inside, b"data").unwrap();
        let locked = CString::new(inside.as_os_str().as_bytes()).unwrap();
        assert_eq!(
            unsafe { libc::chflags(locked.as_ptr(), libc::UF_IMMUTABLE) },
            0
        );
        let plan = plan_of(&foo, Vec::new());

        let went = routes(&plan);
        let asks = needs_password(&plan.scan, &went);

        assert_eq!(unsafe { libc::chflags(locked.as_ptr(), 0) }, 0);
        assert_eq!(went[&foo.path], Route::Root);
        assert!(asks);
    }

    #[test]
    fn a_user_route_goes_to_the_trash_unless_permanent() {
        assert_eq!(Route::User.mode(false), RemovalMode::Trash);
        assert_eq!(Route::User.mode(true), RemovalMode::Permanent);
        assert_eq!(Route::Root.mode(false), RemovalMode::Permanent);
    }

    #[test]
    fn a_row_with_no_route_goes_through_root() {
        let known = ScanItem::new(PathBuf::from("/known"), 0, true, false);
        let unknown = ScanItem::new(PathBuf::from("/unknown"), 0, true, false);
        let went = HashMap::from([(known.path.clone(), Route::User)]);

        assert_eq!(route_of(&went, &known), Route::User);
        assert_eq!(route_of(&went, &unknown), Route::Root);
    }

    #[test]
    fn a_failed_login_item_removal_is_reported() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let host = FakeHost::default();
        host.fail(
            OSASCRIPT,
            "execution error: Not authorized to send Apple events to System Events (-1743)\nmore",
        );

        let report = run(&plan_of(&foo, Vec::new()), &host, &layout);

        let note = "Foo: could not remove its login item (execution error: Not authorized to send Apple events to System Events (-1743)). Remove it in System Settings > General > Login Items & Extensions.";
        assert!(report.notes.iter().any(|n| n == note), "{:?}", report.notes);
        assert!(!foo.path.exists());
    }

    #[test]
    fn a_removed_login_item_adds_no_note() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");

        let host = FakeHost::default();
        host.reply(OSASCRIPT, "");

        let report = run(&plan_of(&foo, Vec::new()), &host, &layout);

        assert!(
            !report.notes.iter().any(|n| n.contains("login item")),
            "{:?}",
            report.notes
        );
    }

    #[test]
    fn a_refused_password_after_the_trash_says_the_app_is_gone() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let receipt = receipt_row(&layout);
        file(receipt.path.clone());
        let plan = plan_of(&foo, vec![receipt]);
        let host = FakeHost::default();
        *host.admin_result.lock().unwrap() = Some("The password was not given".to_string());

        let report = run(&plan, &host, &layout);

        assert!(host.trashed().contains(&foo.path));
        assert_eq!(report.failures.len(), 1);
        assert_eq!(
            report.failures[0].error,
            "The password was not given. The app is already in the Trash, so only these files stay."
        );
    }

    #[test]
    fn a_refused_password_after_homebrew_says_the_app_is_removed() {
        let (_temp, layout) = sandbox();
        let foo = installed(&layout, "Foo", "com.acme.foo");
        let receipt = receipt_row(&layout);
        file(receipt.path.clone());
        let mut plan = plan_of(&foo, vec![receipt]);
        let room = with_cask(&layout, &mut plan, "opt/homebrew", "foo-app");
        let host = FakeHost::default();
        host.reply("/usr/bin/env", "");
        host.when("/usr/bin/env", removing(&foo.path));
        host.when("/usr/bin/env", removing(&room));
        *host.admin_result.lock().unwrap() = Some("The password was not given".to_string());

        let report = run(&plan, &host, &layout);

        assert!(host.trashed().is_empty());
        assert_eq!(report.failures.len(), 1);
        assert_eq!(
            report.failures[0].error,
            "The password was not given. The app is already removed, so only these files stay."
        );
    }
}

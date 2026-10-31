//! Finds what an app leaves on disk besides its bundle, and how sure that is.
//!
//! Every search here is a list of exact places or one folder level read and
//! compared by name. Nothing walks a tree, so a wrong name cannot reach far.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use plist::Value;
use serde::{Deserialize, Serialize};

use super::apps::{
    InstalledApp, embedded_bundle_ids, is_common_name, is_reverse_dns, jetbrains_selector,
};
use super::host::Host;
use super::layout::Layout;
use super::safety::FileIdentity;

const MIN_NAME: usize = 3;
const MIN_NAME_OUTSIDE_HOME: usize = 5;
const MIN_VENDOR_TOKEN: usize = 3;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const PKGUTIL_TIMEOUT: Duration = Duration::from_secs(15);

const CHANNELS: &[&str] = &[
    "Alpha",
    "Beta",
    "Canary",
    "Dev",
    "Developer Edition",
    "Insider",
    "LTS",
    "Nightly",
    "Preview",
    "RC",
    "Release",
    "Stable",
    "Technology Preview",
];

/// Folder and suffix of what an app keeps under its own name, in any spelling.
const NAMED_IN_LIBRARY: &[(&str, &str)] = &[
    ("Library/Application Support", ""),
    ("Library/Caches", ""),
    ("Library/Logs", ""),
    ("Library/Preferences", ""),
    ("Library/Preferences", ".plist"),
    ("Library/Saved Application State", ".savedState"),
];

const NAMED_EXTENSIONS: &[(&str, &str)] = &[
    ("Library/Accessibility", ".bundle"),
    ("Library/Address Book Plug-Ins", ".bundle"),
    ("Library/Audio/Plug-Ins/Components", ".component"),
    ("Library/Audio/Plug-Ins/Digidesign", ".dpm"),
    ("Library/Audio/Plug-Ins/VST", ".vst"),
    ("Library/Audio/Plug-Ins/VST3", ".vst3"),
    ("Library/ColorPickers", ".colorPicker"),
    ("Library/Contextual Menu Items", ".plugin"),
    ("Library/Frameworks", ".framework"),
    ("Library/Input Methods", ".app"),
    ("Library/Internet Plug-Ins", ".plugin"),
    ("Library/Mail/Bundles", ".mailbundle"),
    ("Library/PreferencePanes", ".prefPane"),
    ("Library/QuickLook", ".qlgenerator"),
    ("Library/Screen Savers", ".saver"),
    ("Library/Services", ".workflow"),
    ("Library/Spotlight", ".mdimporter"),
    ("Library/Workflows", ".workflow"),
];

const NAMED_IN_SYSTEM: &[(&str, &str)] = &[
    ("Library/Application Support", ""),
    ("Library/Audio/Plug-Ins/Components", ".component"),
    ("Library/Audio/Plug-Ins/Digidesign", ".dpm"),
    ("Library/Audio/Plug-Ins/VST", ".vst"),
    ("Library/Audio/Plug-Ins/VST3", ".vst3"),
    ("Library/Caches", ""),
    ("Library/Frameworks", ".framework"),
    ("Library/Input Methods", ".app"),
    ("Library/Internet Plug-Ins", ".plugin"),
    ("Library/Logs", ""),
    ("Library/PreferencePanes", ".prefPane"),
    ("Library/Preferences", ""),
    ("Library/Preferences", ".plist"),
    ("Library/QuickLook", ".qlgenerator"),
    ("Library/Screen Savers", ".saver"),
    ("Library/StartupItems", ""),
];

const BY_ID_IN_LIBRARY: &[(&str, &str)] = &[
    ("Library/Application Scripts", ""),
    ("Library/Application Support", ""),
    ("Library/Application Support/com.bugsnag.Bugsnag", ""),
    ("Library/Autosave Information", ""),
    ("Library/Caches", ""),
    ("Library/Caches", ".ShipIt"),
    ("Library/Caches/com.apple.nsurlsessiond/Downloads", ""),
    ("Library/Caches/com.crashlytics.data", ""),
    ("Library/Caches/com.plausiblelabs.crashreporter.data", ""),
    ("Library/Containers", ""),
    ("Library/Cookies", ".binarycookies"),
    ("Library/HTTPStorages", ""),
    ("Library/HTTPStorages", ".binarycookies"),
    ("Library/Input Methods", ".app"),
    ("Library/Logs", ""),
    ("Library/Preferences", ""),
    ("Library/Preferences", ".plist"),
    ("Library/Saved Application State", ".savedState"),
    ("Library/SyncedPreferences", ".plist"),
    ("Library/WebKit", ""),
    ("Library/WebKit/com.apple.WebKit.WebContent", ""),
];

const BY_ID_IN_SYSTEM: &[(&str, &str)] = &[
    ("Library/Application Support", ""),
    ("Library/Caches", ""),
    ("Library/Input Methods", ".app"),
    ("Library/Logs", ""),
    ("Library/Preferences", ".plist"),
];

const BY_EMBEDDED_ID: &[(&str, &str)] = &[
    ("Library/Application Scripts", ""),
    ("Library/Caches", ""),
    ("Library/Containers", ""),
    ("Library/HTTPStorages", ""),
    ("Library/HTTPStorages", ".binarycookies"),
    ("Library/Preferences", ".plist"),
    ("Library/WebKit", ""),
];

const RECENT_DOCUMENTS: &str = "Library/Application Support/com.apple.sharedfilelist/com.apple.LSSharedFileList.ApplicationRecentDocuments";
const VENDOR_ROOTS: &[&str] = &[
    "Library/Application Support",
    "Library/Caches",
    "Library/Logs",
];
const XDG_FOLDERS: &[&str] = &[".config", ".cache", ".local/share"];
const LAUNCH_FOLDERS_IN_SYSTEM: &[&str] = &["Library/LaunchAgents", "Library/LaunchDaemons"];
const PAYLOAD_FOLDERS: &[&str] = &[
    "Library/LaunchAgents",
    "Library/LaunchDaemons",
    "Library/PrivilegedHelperTools",
];
const REPORT_EXTENSIONS: &[&str] = &["crash", "diag", "ips", "spin"];
const RUNTIME_EXECUTABLE: &str = "electron";

/// Command line tools that keep a dot folder and share a name with some GUI app.
const INDEPENDENT_TOOLS: &[&str] = &["claude", "codex", "gemini", "opencode"];

/// Home folder entries that many tools share, in lower case because APFS folds case.
const SHARED_HOME_FOLDERS: &[&str] = &[
    ".cache",
    ".config",
    ".local",
    ".local/bin",
    ".local/lib",
    ".local/share",
    ".local/state",
    "library/application support/epic",
    "library/unity",
];

/// The user's whole game library lives here, whichever app is being removed.
const STEAM_LIBRARY: &str = "library/application support/steam";

/// Why a file counts as belonging to an app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Evidence {
    /// Named after the bundle id, or a known place of this exact product.
    BundleId,
    /// Its launchd or native host entry runs a program inside the bundle.
    Manifest,
    /// The installer package of the app put it there.
    Receipt,
    /// Named after the app.
    Name,
    /// Named after the app without its channel, like Foo for Foo Beta.
    RelatedName,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leftover {
    pub path: PathBuf,
    pub evidence: Evidence,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Findings {
    pub leftovers: Vec<Leftover>,
    /// Installer receipts of this app, by package id.
    pub receipts: Vec<String>,
}

/// Looks for everything that belongs to `app`. `others` are the apps that stay.
pub fn find_leftovers(
    app: &InstalledApp,
    others: &[InstalledApp],
    host: &dyn Host,
    layout: &Layout,
) -> Findings {
    let mut finder = Finder::new(app, layout);
    finder.by_name();
    finder.by_bundle_id();
    finder.by_vendor();
    finder.launch_entries();
    finder.native_hosts();
    finder.embedded_helpers();
    finder.known_products();
    finder.crash_reports();
    let receipts = finder.receipts(host);

    Findings {
        leftovers: finder.finish(others),
        receipts: receipts.into_iter().collect(),
    }
}

fn entries(folder: &Path) -> Vec<(PathBuf, String)> {
    let Ok(read) = fs::read_dir(folder) else {
        return Vec::new();
    };
    read.flatten()
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            (entry.path(), name)
        })
        .collect()
}

fn spellings(name: &str) -> Vec<String> {
    let mut all = vec![
        name.to_string(),
        name.replace(' ', ""),
        name.replace(' ', "_"),
        name.replace(' ', "-"),
    ];
    all.sort();
    all.dedup();
    all
}

fn lower_spellings(name: &str) -> Vec<String> {
    let mut all: Vec<String> = spellings(name).iter().map(|s| s.to_lowercase()).collect();
    all.sort();
    all.dedup();
    all
}

fn without_channel(name: &str) -> Option<&str> {
    CHANNELS.iter().find_map(|channel| {
        let base = name.strip_suffix(channel)?.strip_suffix(' ')?;
        (base.chars().count() >= MIN_NAME).then_some(base)
    })
}

/// True for `candidate` equal to a spelling or continuing it after a separator.
fn matches_spelling(candidate: &str, spellings: &[String]) -> bool {
    spellings.iter().any(|spelling| {
        candidate
            .strip_prefix(spelling.as_str())
            .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '-', '_', '.']))
    })
}

/// True for a product folder, alone or with a version: `Sibelius`, `Sibelius 2026`.
fn is_product_folder(candidate: &str, spellings: &[String]) -> bool {
    spellings.iter().any(|spelling| {
        candidate
            .strip_prefix(spelling.as_str())
            .is_some_and(|rest| {
                let mut after = rest.chars();
                rest.is_empty()
                    || (after.next() == Some(' ')
                        && after.next().is_some_and(|c| c.is_ascii_digit()))
            })
    })
}

fn has_id_boundary(leaf: &str, id: &str) -> bool {
    let leaf = leaf.to_ascii_lowercase();
    let id = id.to_ascii_lowercase();
    leaf == id || leaf.starts_with(&format!("{id}."))
}

fn contains_id(leaf: &str, id: &str) -> bool {
    let leaf = leaf.to_ascii_lowercase();
    let id = id.to_ascii_lowercase();
    has_id_boundary(&leaf, &id)
        || leaf.ends_with(&format!(".{id}"))
        || leaf.contains(&format!(".{id}."))
}

fn is_uuid(name: &str) -> bool {
    let lengths = [8, 4, 4, 4, 12];
    let groups: Vec<&str> = name.split('-').collect();
    groups.len() == lengths.len()
        && groups.iter().zip(lengths).all(|(group, length)| {
            group.len() == length && group.chars().all(|c| c.is_ascii_hexdigit())
        })
}

/// Splits `DesktopApp` into `Desktop App`.
fn split_words(text: &str) -> String {
    let mut spaced = String::new();
    for (index, c) in text.char_indices() {
        if index > 0 && c.is_ascii_uppercase() {
            spaced.push(' ');
        }
        spaced.push(c);
    }
    spaced
}

/// Forks often name their data after the end of the bundle id: AyuGram ships as
/// `one.ayugram.AyuGramDesktop` and writes to `AyuGram Desktop`. That counts
/// only when the end of the id continues the app name, or a wrapper could claim
/// the data of the app it wraps.
fn id_leaf_spellings(id: &str, name: &str) -> Vec<String> {
    let leaf = id.rsplit('.').next().unwrap_or_default();
    let joined = name.replace(' ', "");
    let continues = leaf.len() >= 8
        && leaf.len() > joined.len()
        && leaf.is_char_boundary(joined.len())
        && leaf[..joined.len()].eq_ignore_ascii_case(&joined);
    if !continues {
        return Vec::new();
    }
    let rest = &leaf[joined.len()..];
    if !rest.starts_with(|c: char| c.is_ascii_uppercase() || c.is_ascii_digit()) {
        return Vec::new();
    }
    vec![leaf.to_string(), format!("{name} {}", split_words(rest))]
}

fn launch_program(plist: &Path) -> Option<String> {
    let info = Value::from_file(plist).ok()?.into_dictionary()?;
    let program = info.get("Program").and_then(Value::as_string);
    let first_argument = || {
        info.get("ProgramArguments")?
            .as_array()?
            .first()?
            .as_string()
    };
    program.or_else(first_argument).map(str::to_string)
}

fn runs_inside(program: &str, bundle: &Path) -> bool {
    let program = Path::new(program);
    program.starts_with(bundle)
        && program != bundle
        && !program
            .components()
            .any(|part| part == Component::ParentDir)
}

fn native_host_program(manifest: &Path) -> Option<String> {
    if fs::metadata(manifest).ok()?.len() > MAX_MANIFEST_BYTES {
        return None;
    }
    let text = fs::read_to_string(manifest).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value.get("path")?.as_str().map(str::to_string)
}

fn container_owner(container: &Path) -> Option<String> {
    let metadata = container.join(".com.apple.containermanagerd.metadata.plist");
    let info = Value::from_file(metadata).ok()?.into_dictionary()?;
    info.get("MCMMetadataIdentifier")?
        .as_string()
        .map(str::to_string)
}

/// APFS folds case, so a searched spelling can differ from the name on disk.
fn name_on_disk(path: &Path) -> PathBuf {
    let (Some(parent), Some(leaf)) = (path.parent(), path.file_name()) else {
        return path.to_path_buf();
    };
    let wanted = leaf.to_string_lossy().to_lowercase();
    let listed = entries(parent);
    let exact = listed.iter().find(|(entry, _)| entry == path);
    let same = || {
        listed
            .iter()
            .find(|(_, name)| name.to_lowercase() == wanted)
    };
    exact
        .or_else(same)
        .map_or_else(|| path.to_path_buf(), |(entry, _)| entry.clone())
}

fn simplified(name: &str) -> String {
    name.chars()
        .filter(|c| !matches!(c, ' ' | '-' | '_'))
        .flat_map(char::to_lowercase)
        .collect()
}

fn relative_lower(path: &Path, home: &Path) -> Option<String> {
    let relative = path.strip_prefix(home).ok()?;
    Some(relative.to_string_lossy().to_lowercase())
}

fn is_independent_tool(path: &Path, home: &Path) -> bool {
    let Some(leaf) = path.file_name() else {
        return false;
    };
    let tool = leaf
        .to_string_lossy()
        .trim_start_matches('.')
        .to_lowercase();
    let in_shared_place = ["", ".cache", ".config", ".local/share"]
        .iter()
        .any(|place| path.parent() == Some(home.join(place).as_path()));
    in_shared_place && INDEPENDENT_TOOLS.contains(&tool.as_str())
}

fn is_shared_by_tools(path: &Path, home: &Path) -> bool {
    relative_lower(path, home).is_some_and(|relative| {
        SHARED_HOME_FOLDERS.contains(&relative.as_str())
            || relative == STEAM_LIBRARY
            || relative.starts_with(&format!("{STEAM_LIBRARY}/"))
    })
}

struct Finder<'a> {
    app: &'a InstalledApp,
    layout: &'a Layout,
    id: Option<&'a str>,
    /// The id, when it is long enough to claim names that continue it.
    /// `com.vendor` would take every app of that vendor.
    family: Option<&'a str>,
    found: Vec<Leftover>,
}

impl<'a> Finder<'a> {
    fn new(app: &'a InstalledApp, layout: &'a Layout) -> Self {
        let id = app.bundle_id.as_deref().filter(|id| is_reverse_dns(id));
        let family = id.filter(|id| id.split('.').count() >= 3);
        Self {
            app,
            layout,
            id,
            family,
            found: Vec::new(),
        }
    }

    fn add(&mut self, path: PathBuf, evidence: Evidence) {
        self.found.push(Leftover { path, evidence });
    }

    fn add_places(&mut self, base: &Path, places: &[(&str, &str)], stem: &str, evidence: Evidence) {
        for (folder, suffix) in places {
            self.add(base.join(folder).join(format!("{stem}{suffix}")), evidence);
        }
    }

    fn name_is_usable(&self) -> bool {
        self.app.name.chars().count() >= MIN_NAME
    }

    fn name_reaches_system(&self) -> bool {
        self.app.name.chars().count() >= MIN_NAME_OUTSIDE_HOME && !is_common_name(&self.app.name)
    }

    fn by_name(&mut self) {
        if !self.name_is_usable() {
            return;
        }
        let layout = self.layout;
        let name = self.app.name.clone();

        for spelling in spellings(&name) {
            self.add_places(&layout.home, NAMED_IN_LIBRARY, &spelling, Evidence::Name);
        }
        self.add_places(&layout.home, NAMED_EXTENSIONS, &name, Evidence::Name);
        self.add_xdg(&name, Evidence::Name);
        self.updater_and_crash_folders(&name);

        if let Some(id) = self.id {
            for spelling in id_leaf_spellings(id, &name) {
                self.add_places(&layout.home, NAMED_IN_LIBRARY, &spelling, Evidence::Name);
            }
        }

        if let Some(base) = without_channel(&name) {
            let base = base.to_string();
            for spelling in spellings(&base) {
                self.add_places(
                    &layout.home,
                    NAMED_IN_LIBRARY,
                    &spelling,
                    Evidence::RelatedName,
                );
            }
            self.add_xdg(&base, Evidence::RelatedName);
        }

        if self.name_reaches_system() {
            self.add_places(&layout.root, NAMED_IN_SYSTEM, &name, Evidence::Name);
            for spelling in spellings(&name) {
                self.add_places(
                    &layout.root,
                    &NAMED_IN_SYSTEM[..1],
                    &spelling,
                    Evidence::Name,
                );
            }
            self.named_helpers_and_shared_files(&name);
        }
    }

    fn add_xdg(&mut self, name: &str, evidence: Evidence) {
        let home = self.layout.home.clone();
        for folder in XDG_FOLDERS {
            for spelling in spellings(name).into_iter().chain(lower_spellings(name)) {
                self.add(home.join(folder).join(spelling), evidence);
            }
        }
    }

    fn updater_and_crash_folders(&mut self, name: &str) {
        let layout = self.layout;
        let is_generic = is_common_name(name) || name.eq_ignore_ascii_case(RUNTIME_EXECUTABLE);
        if !is_generic {
            for crash_kit in ["SentryCrash", "KSCrash"] {
                let path = layout.user("Library/Caches").join(crash_kit).join(name);
                self.add(path, Evidence::Name);
            }
            for spelling in spellings(name).into_iter().chain(lower_spellings(name)) {
                let updater = format!("{spelling}-updater");
                self.add(layout.user("Library/Caches").join(&updater), Evidence::Name);
                let support_caches = layout.user("Library/Application Support/Caches");
                self.add(support_caches.join(&updater), Evidence::Name);
            }
        }

        let joined = name.replace(' ', "");
        for (path, leaf) in entries(&layout.user("Library/Application Support/CrashReporter")) {
            let from_app =
                leaf.starts_with(&format!("{name}_")) || leaf.starts_with(&format!("{joined}_"));
            if from_app && leaf.ends_with(".plist") {
                self.add(path, Evidence::Name);
            }
        }
    }

    fn named_helpers_and_shared_files(&mut self, name: &str) {
        let layout = self.layout;
        let mut names: Vec<String> = lower_spellings(name)
            .into_iter()
            .filter(|spelling| spelling.len() >= MIN_NAME_OUTSIDE_HOME)
            .collect();
        for (path, leaf) in entries(&layout.system("Library/PrivilegedHelperTools")) {
            let lower = leaf.to_lowercase();
            if !lower.starts_with("com.apple.") && matches_spelling(&lower, &names) {
                self.add(path, Evidence::Name);
            }
        }

        names.extend(self.product_token());
        for (path, leaf) in entries(&layout.system("Users/Shared")) {
            if matches_spelling(&leaf.to_lowercase(), &names) {
                self.add(path, Evidence::Name);
            }
        }
    }

    fn vendor_and_product(&self) -> Option<(&'a str, &'a str)> {
        let mut parts = self.id?.rsplit('.');
        let product = parts.next()?;
        let vendor = parts.next()?;
        let long_enough = |token: &str| token.len() >= MIN_VENDOR_TOKEN;
        (long_enough(product) && long_enough(vendor)).then_some((vendor, product))
    }

    fn product_token(&self) -> Option<String> {
        let (_, product) = self.vendor_and_product()?;
        (product.len() >= MIN_NAME_OUTSIDE_HOME).then(|| product.to_lowercase())
    }

    fn by_bundle_id(&mut self) {
        let Some(id) = self.id else {
            return;
        };
        let layout = self.layout;
        self.add_places(&layout.home, BY_ID_IN_LIBRARY, id, Evidence::BundleId);
        self.add_places(&layout.root, BY_ID_IN_SYSTEM, id, Evidence::BundleId);
        if let Some(cache) = layout.user_cache.clone() {
            self.add(cache.join(id), Evidence::BundleId);
        }
        for version in ["sfl2", "sfl3", "sfl4"] {
            let recent = layout
                .user(RECENT_DOCUMENTS)
                .join(format!("{id}.{version}"));
            self.add(recent, Evidence::BundleId);
        }

        let containers = entries(&layout.user("Library/Containers"));
        for (path, leaf) in &containers {
            if is_uuid(leaf) && container_owner(path).as_deref() == Some(id) {
                self.add(path.clone(), Evidence::BundleId);
            }
        }

        let Some(family) = self.family else {
            return;
        };
        for (path, leaf) in containers
            .into_iter()
            .chain(entries(&layout.user("Library/Application Scripts")))
            .chain(entries(&layout.user("Library/Group Containers")))
        {
            if contains_id(&leaf, family) {
                self.add(path, Evidence::BundleId);
            }
        }
        for (path, leaf) in entries(&layout.user("Library/Preferences/ByHost")) {
            if has_id_boundary(&leaf, family) && leaf.ends_with(".plist") {
                self.add(path, Evidence::BundleId);
            }
        }
        for (path, leaf) in entries(&layout.system("Library/PrivilegedHelperTools")) {
            if has_id_boundary(&leaf, family) {
                self.add(path, Evidence::BundleId);
            }
        }
    }

    /// `Application Support/Avid/Sibelius`: the id names the vendor, so the
    /// product folder can be told apart from its siblings.
    fn by_vendor(&mut self) {
        let Some((vendor, product)) = self.vendor_and_product() else {
            return;
        };
        if self.app.name.chars().count() < 4 || is_common_name(&self.app.name) {
            return;
        }
        let mut names = lower_spellings(&self.app.name);
        names.push(product.to_lowercase());

        let bases = [self.layout.home.clone(), self.layout.root.clone()];
        for base in &bases {
            for root in VENDOR_ROOTS {
                for (vendor_folder, leaf) in entries(&base.join(root)) {
                    if !leaf.eq_ignore_ascii_case(vendor) {
                        continue;
                    }
                    for (path, child) in entries(&vendor_folder) {
                        if path.is_dir() && is_product_folder(&child.to_lowercase(), &names) {
                            self.add(path, Evidence::Name);
                        }
                    }
                }
            }
        }
    }

    fn launch_entries(&mut self) {
        let layout = self.layout;
        let mut folders = vec![layout.user("Library/LaunchAgents")];
        folders.extend(
            LAUNCH_FOLDERS_IN_SYSTEM
                .iter()
                .map(|rel| layout.system(rel)),
        );
        let quotable = self.name_reaches_system();

        for folder in folders {
            for (path, leaf) in entries(&folder) {
                let Some(stem) = leaf.strip_suffix(".plist") else {
                    continue;
                };
                if leaf.starts_with("com.apple.") || !path.is_file() || path.is_symlink() {
                    continue;
                }
                if self.is_own_label(stem) {
                    self.add(path, Evidence::BundleId);
                } else if launch_program(&path).is_some_and(|p| runs_inside(&p, &self.app.path)) {
                    self.add(path, Evidence::Manifest);
                } else if quotable && leaf.contains(&self.app.name) {
                    self.add(path, Evidence::Name);
                }
            }
        }
    }

    fn is_own_label(&self, label: &str) -> bool {
        let exact = self.id.is_some_and(|id| label.eq_ignore_ascii_case(id));
        exact
            || self
                .family
                .is_some_and(|family| has_id_boundary(label, family))
    }

    fn native_hosts(&mut self) {
        let support = self.layout.user("Library/Application Support");
        let mut folders = Vec::new();
        for (browser, _) in entries(&support) {
            folders.push(browser.join("NativeMessagingHosts"));
            for (profile, _) in entries(&browser) {
                folders.push(profile.join("NativeMessagingHosts"));
            }
        }
        for folder in folders {
            for (path, leaf) in entries(&folder) {
                let starts_app = leaf.ends_with(".json")
                    && !path.is_symlink()
                    && native_host_program(&path)
                        .is_some_and(|program| runs_inside(&program, &self.app.path));
                if starts_app {
                    self.add(path, Evidence::Manifest);
                }
            }
        }
    }

    fn embedded_helpers(&mut self) {
        let layout = self.layout;
        let (embedded, _) = embedded_bundle_ids(&self.app.path, self.id);
        for id in embedded {
            self.add_places(&layout.home, BY_EMBEDDED_ID, &id, Evidence::BundleId);
            let prefix = format!("{id}.");
            for (path, leaf) in entries(&layout.user("Library/Preferences/ByHost")) {
                if leaf.starts_with(&prefix) && leaf.ends_with(".plist") {
                    self.add(path, Evidence::BundleId);
                }
            }
        }
    }

    fn add_home_paths(&mut self, relative: &[&str]) {
        for path in relative {
            self.add(self.layout.user(path), Evidence::BundleId);
        }
    }

    /// Products that keep data where neither their name nor their id says so.
    /// Only rebuildable or plainly own folders: never projects, keys or SDKs.
    fn known_products(&mut self) {
        let Some(id) = self.id.map(str::to_ascii_lowercase) else {
            return;
        };
        match id.as_str() {
            "com.microsoft.vscode" => self.add_home_paths(&[
                ".vscode",
                "Library/Application Support/Code",
                "Library/Caches/com.microsoft.VSCode",
                "Library/Caches/com.microsoft.VSCode.ShipIt",
            ]),
            "com.microsoft.vscodeinsiders" => self.add_home_paths(&[
                ".vscode-insiders",
                "Library/Application Support/Code - Insiders",
                "Library/Caches/com.microsoft.VSCodeInsiders",
                "Library/Caches/com.microsoft.VSCodeInsiders.ShipIt",
            ]),
            "com.apple.dt.xcode" => self.add_home_paths(&[
                ".Xcode",
                "Library/Developer/CoreSimulator/Caches",
                "Library/Developer/Xcode/DerivedData",
                "Library/Developer/Xcode/iOS DeviceSupport",
                "Library/Developer/Xcode/macOS DeviceSupport",
                "Library/Developer/Xcode/tvOS DeviceSupport",
                "Library/Developer/Xcode/visionOS DeviceSupport",
                "Library/Developer/Xcode/watchOS DeviceSupport",
                "Library/Developer/Xcode/xrOS DeviceSupport",
            ]),
            "org.godotengine.godot" => self.add_home_paths(&["Library/Application Support/Godot"]),
            "net.ankiweb.anki" => {
                self.add_home_paths(&["Library/Application Support/AnkiProgramFiles"])
            }
            "com.anthropic.claudefordesktop" => self.add_home_paths(&[
                "Library/Application Support/Claude-3p",
                "Library/Logs/Claude-3p",
            ]),
            _ => {}
        }

        if id.starts_with("com.google.android.studio") {
            self.android_studio();
        }
        if id.starts_with("com.jetbrains.") {
            self.jetbrains_ide();
        }
    }

    fn android_studio(&mut self) {
        self.add_home_paths(&[
            ".android/breakpad",
            ".android/build-cache",
            ".android/cache",
        ]);
        let google = self.layout.user("Library/Application Support/Google");
        for (path, leaf) in entries(&google) {
            let version = leaf.strip_prefix("AndroidStudio");
            if version.is_some_and(|v| v.chars().next().is_some_and(|c| c.is_ascii_digit())) {
                self.add(path, Evidence::BundleId);
            }
        }
    }

    fn jetbrains_ide(&mut self) {
        let Some(selector) = jetbrains_selector(&self.app.path) else {
            return;
        };
        for root in [
            "Library/Application Support/JetBrains",
            "Library/Caches/JetBrains",
            "Library/Logs/JetBrains",
            "Library/Preferences",
        ] {
            self.add(self.layout.user(root).join(&selector), Evidence::BundleId);
        }
    }

    fn crash_report_prefix(&self) -> Option<String> {
        let executable = self.app.executable.clone();
        let prefix = executable
            .filter(|name| !name.eq_ignore_ascii_case(RUNTIME_EXECUTABLE))
            .unwrap_or_else(|| self.app.name.replace(' ', ""));
        (prefix.chars().count() >= MIN_NAME && !prefix.eq_ignore_ascii_case(RUNTIME_EXECUTABLE))
            .then_some(prefix)
    }

    fn crash_reports(&mut self) {
        let Some(prefix) = self.crash_report_prefix() else {
            return;
        };
        let names = [prefix.clone(), format!("{prefix} Helper")];
        let reports = "Library/Logs/DiagnosticReports";
        for folder in [self.layout.user(reports), self.layout.system(reports)] {
            for (path, leaf) in entries(&folder) {
                let is_report = Path::new(&leaf)
                    .extension()
                    .is_some_and(|ext| REPORT_EXTENSIONS.iter().any(|known| ext == *known));
                let from_app = names.iter().any(|name| {
                    leaf.strip_prefix(name.as_str())
                        .is_some_and(|rest| rest.starts_with(['.', '_', '-']))
                });
                if is_report && from_app {
                    self.add(path, Evidence::BundleId);
                }
            }
        }
    }

    fn receipts(&mut self, host: &dyn Host) -> BTreeSet<String> {
        let mut receipts = BTreeSet::new();
        if let Some(family) = self.family {
            for (_, leaf) in entries(&self.layout.system("private/var/db/receipts")) {
                if let Some(id) = leaf.strip_suffix(".plist")
                    && has_id_boundary(id, family)
                    && is_reverse_dns(id)
                {
                    receipts.insert(id.to_string());
                }
            }
        }

        let path = self.app.path.display().to_string();
        let args = ["--file-info", path.as_str()];
        let owners = host.run("/usr/sbin/pkgutil", &args, PKGUTIL_TIMEOUT).stdout;
        for id in owners
            .lines()
            .filter_map(|line| line.strip_prefix("pkgid: "))
        {
            let id = id.trim();
            let is_apple = id.to_ascii_lowercase().starts_with("com.apple.");
            if is_reverse_dns(id) && !is_apple && self.package_payload(host, id) {
                receipts.insert(id.to_string());
            }
        }
        receipts
    }

    /// Adds the launch and helper files of a package and tells if it is this
    /// app's alone. A package that holds another app says nothing about this one.
    fn package_payload(&mut self, host: &dyn Host, id: &str) -> bool {
        let info = host.run("/usr/sbin/pkgutil", &["--pkg-info", id], PKGUTIL_TIMEOUT);
        let location = info
            .stdout
            .lines()
            .find_map(|line| line.strip_prefix("location: "))
            .unwrap_or_default()
            .trim()
            .trim_start_matches('/')
            .to_string();
        let listing = host.run("/usr/sbin/pkgutil", &["--files", id], PKGUTIL_TIMEOUT);
        if !listing.ok {
            return false;
        }

        let root = self.layout.root.join(location);
        let is_top_level_app = |relative: &&str| {
            let parts: Vec<&str> = relative.split('/').collect();
            let is_app = |part: &&str| part.to_ascii_lowercase().ends_with(".app");
            parts.last().is_some_and(&is_app) && !parts[..parts.len() - 1].iter().any(is_app)
        };
        let holds_another_app = listing
            .stdout
            .lines()
            .filter(is_top_level_app)
            .any(|relative| root.join(relative) != self.app.path);
        if holds_another_app {
            return false;
        }

        let places: Vec<PathBuf> = PAYLOAD_FOLDERS
            .iter()
            .map(|folder| self.layout.system(folder))
            .collect();
        for relative in listing.stdout.lines() {
            let path = root.join(relative);
            if path
                .parent()
                .is_some_and(|parent| places.iter().any(|place| place == parent))
            {
                self.add(path, Evidence::Receipt);
            }
        }
        true
    }

    fn claimed_by_another_app(&self, found: &Leftover, others: &[InstalledApp]) -> bool {
        let Some(leaf) = found.path.file_name().map(|leaf| leaf.to_string_lossy()) else {
            return false;
        };
        match found.evidence {
            Evidence::Name | Evidence::RelatedName => {
                let stem = Path::new(leaf.as_ref())
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .unwrap_or_default();
                others.iter().any(|other| {
                    let name = simplified(&other.name);
                    simplified(&leaf) == name || simplified(&stem) == name
                })
            }
            _ => {
                let own_length = self.id.map_or(0, str::len);
                others
                    .iter()
                    .filter_map(|other| other.bundle_id.as_deref())
                    .filter(|id| id.len() > own_length)
                    .any(|id| contains_id(&leaf, id))
            }
        }
    }

    fn finish(self, others: &[InstalledApp]) -> Vec<Leftover> {
        let home = &self.layout.home;
        let mut found: Vec<Leftover> = self
            .found
            .iter()
            .filter(|item| fs::symlink_metadata(&item.path).is_ok())
            .filter(|item| !item.path.starts_with(&self.app.path))
            .filter(|item| !is_independent_tool(&item.path, home))
            .filter(|item| !is_shared_by_tools(&item.path, home))
            .filter(|item| !self.claimed_by_another_app(item, others))
            .cloned()
            .collect();

        // One folder can come up under several spellings. The strongest reason stays.
        found.sort_by_key(|item| item.evidence);
        let mut seen = HashSet::new();
        found.retain(|item| seen.insert(FileIdentity::of(&item.path)));
        for item in &mut found {
            item.path = name_on_disk(&item.path);
        }

        found.sort_by(|a, b| a.path.cmp(&b.path));
        let mut kept: Vec<Leftover> = Vec::new();
        for item in found {
            if !kept
                .iter()
                .any(|parent| item.path.starts_with(&parent.path))
            {
                kept.push(item);
            }
        }
        kept
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::apps::testing::{app, fake_app, sandbox};
    use crate::core::host::fake::FakeHost;
    use std::collections::HashMap;
    use std::fs;
    use std::path::Path;

    fn folder(path: PathBuf) -> PathBuf {
        fs::create_dir_all(&path).unwrap();
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

    fn find(target: &InstalledApp, layout: &Layout) -> HashMap<PathBuf, Evidence> {
        find_with(target, &[], layout)
    }

    fn find_with(
        target: &InstalledApp,
        others: &[InstalledApp],
        layout: &Layout,
    ) -> HashMap<PathBuf, Evidence> {
        find_leftovers(target, others, &FakeHost::default(), layout)
            .leftovers
            .into_iter()
            .map(|found| (found.path, found.evidence))
            .collect()
    }

    #[track_caller]
    fn assert_found(found: &HashMap<PathBuf, Evidence>, path: &Path, evidence: Evidence) {
        assert_eq!(found.get(path), Some(&evidence), "{path:?} in {found:#?}");
    }

    #[track_caller]
    fn assert_absent(found: &HashMap<PathBuf, Evidence>, path: &Path) {
        assert!(!found.contains_key(path), "{path:?} in {found:#?}");
    }

    #[test]
    fn places_named_after_the_bundle_id_are_found() {
        let (_temp, layout) = sandbox();
        let target = installed(&layout, "Foo", "com.acme.foo");
        let paths = [
            folder(layout.user("Library/Application Support/com.acme.foo")),
            folder(layout.user("Library/Caches/com.acme.foo")),
            folder(layout.user("Library/Caches/com.acme.foo.ShipIt")),
            folder(layout.user("Library/Logs/com.acme.foo")),
            file(layout.user("Library/Preferences/com.acme.foo.plist")),
            file(layout.user("Library/Preferences/ByHost/com.acme.foo.ABC-123.plist")),
            folder(layout.user("Library/Saved Application State/com.acme.foo.savedState")),
            folder(layout.user("Library/Containers/com.acme.foo")),
            folder(layout.user("Library/Containers/com.acme.foo.share")),
            folder(layout.user("Library/Group Containers/TEAM123.com.acme.foo")),
            folder(layout.user("Library/Group Containers/group.com.acme.foo.data")),
            folder(layout.user("Library/Application Scripts/com.acme.foo")),
            folder(layout.user("Library/WebKit/com.acme.foo")),
            folder(layout.user("Library/HTTPStorages/com.acme.foo")),
            file(layout.user("Library/HTTPStorages/com.acme.foo.binarycookies")),
            file(layout.user("Library/Cookies/com.acme.foo.binarycookies")),
            folder(layout.user("Library/Autosave Information/com.acme.foo")),
            file(layout.user(
                "Library/Application Support/com.apple.sharedfilelist/com.apple.LSSharedFileList.ApplicationRecentDocuments/com.acme.foo.sfl3",
            )),
        ];

        let found = find(&target, &layout);

        for path in &paths {
            assert_found(&found, path, Evidence::BundleId);
        }
    }

    #[test]
    fn places_named_after_the_app_are_found_in_every_spelling() {
        let (_temp, layout) = sandbox();
        let target = installed(&layout, "Maestro Studio", "com.acme.maestro");
        let paths = [
            folder(layout.user("Library/Application Support/Maestro Studio")),
            folder(layout.user("Library/Application Support/MaestroStudio")),
            folder(layout.user("Library/Application Support/Maestro-Studio")),
            folder(layout.user("Library/Caches/Maestro_Studio")),
            folder(layout.user("Library/Logs/Maestro Studio")),
            file(layout.user("Library/Preferences/MaestroStudio.plist")),
            folder(layout.user("Library/Saved Application State/Maestro Studio.savedState")),
            folder(layout.user(".config/maestro-studio")),
            folder(layout.user(".cache/maestrostudio")),
            folder(layout.user(".local/share/maestro_studio")),
            file(layout.user("Library/Audio/Plug-Ins/VST3/Maestro Studio.vst3")),
            folder(layout.user("Library/Caches/maestro-studio-updater")),
        ];

        let found = find(&target, &layout);

        for path in &paths {
            assert_found(&found, path, Evidence::Name);
        }
    }

    #[test]
    fn a_bundle_id_claims_only_up_to_a_dot() {
        let (_temp, layout) = sandbox();
        let target = installed(&layout, "Foo", "com.acme.foo");
        let helper = folder(layout.user("Library/Containers/com.acme.foo.helper"));
        let unrelated = folder(layout.user("Library/Containers/com.acme.foobar"));

        let found = find(&target, &layout);

        assert_found(&found, &helper, Evidence::BundleId);
        assert_absent(&found, &unrelated);
    }

    #[test]
    fn a_two_part_bundle_id_claims_only_its_exact_name() {
        let (_temp, layout) = sandbox();
        let target = installed(&layout, "Foo", "com.apple");
        let own = folder(layout.user("Library/Caches/com.apple"));
        let foreign = folder(layout.user("Library/Containers/com.apple.Safari"));
        let plist = file(layout.user("Library/Preferences/com.apple.finder.plist"));

        let found = find(&target, &layout);

        assert_found(&found, &own, Evidence::BundleId);
        assert_absent(&found, &foreign);
        assert_absent(&found, &plist);
    }

    #[test]
    fn data_of_another_installed_app_stays() {
        let (_temp, layout) = sandbox();
        let target = installed(&layout, "Foo", "com.acme.foo");
        let pro = app("Foo Pro", "com.acme.foo.pro");
        let own = folder(layout.user("Library/Containers/com.acme.foo.share"));
        let theirs = folder(layout.user("Library/Containers/com.acme.foo.pro"));
        let their_extension = folder(layout.user("Library/Containers/com.acme.foo.pro.share"));

        let found = find_with(&target, &[pro], &layout);

        assert_found(&found, &own, Evidence::BundleId);
        assert_absent(&found, &theirs);
        assert_absent(&found, &their_extension);
    }

    #[test]
    fn a_folder_named_after_the_app_without_its_channel_is_only_related() {
        let (_temp, layout) = sandbox();
        let target = installed(&layout, "Zed Nightly", "dev.zed.Zed-Nightly");
        let base = folder(layout.user("Library/Application Support/Zed"));

        let found = find(&target, &layout);

        assert_found(&found, &base, Evidence::RelatedName);
    }

    #[test]
    fn a_folder_named_after_another_installed_app_stays() {
        let (_temp, layout) = sandbox();
        let target = installed(&layout, "Zed Nightly", "dev.zed.Zed-Nightly");
        let stable = app("Zed", "dev.zed.Zed");
        let base = folder(layout.user("Library/Application Support/Zed"));

        let found = find_with(&target, &[stable], &layout);

        assert_absent(&found, &base);
    }

    #[test]
    fn edge_is_part_of_a_name_and_never_a_channel() {
        let (_temp, layout) = sandbox();
        let target = installed(&layout, "Microsoft Edge", "com.microsoft.edgemac");
        let vendor = folder(layout.user("Library/Application Support/Microsoft"));
        let own = folder(layout.user("Library/Application Support/Microsoft Edge"));

        let found = find(&target, &layout);

        assert_absent(&found, &vendor);
        assert_found(&found, &own, Evidence::Name);
    }

    #[test]
    fn a_product_folder_under_its_vendor_is_found_and_the_vendor_folder_stays() {
        let (_temp, layout) = sandbox();
        let target = installed(&layout, "Sibelius", "com.avid.Sibelius");
        let product = folder(layout.user("Library/Application Support/Avid/Sibelius"));
        let versioned = folder(layout.system("Library/Application Support/Avid/Sibelius 2026"));
        let sibling = folder(layout.user("Library/Application Support/Avid/Pro Tools"));

        let found = find(&target, &layout);

        assert_found(&found, &product, Evidence::Name);
        assert_found(&found, &versioned, Evidence::Name);
        assert_absent(&found, &sibling);
    }

    #[test]
    fn a_sibling_product_of_the_vendor_is_not_a_version_of_this_one() {
        let (_temp, layout) = sandbox();
        let target = installed(&layout, "Google Chrome", "com.google.Chrome");
        let chrome = folder(layout.user("Library/Application Support/Google/Chrome"));
        let canary = folder(layout.user("Library/Application Support/Google/Chrome Canary"));

        let found = find(&target, &layout);

        assert_found(&found, &chrome, Evidence::Name);
        assert_absent(&found, &canary);
    }

    fn launch_plist(path: PathBuf, program: &str) -> PathBuf {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut dict = plist::Dictionary::new();
        dict.insert("Label".to_string(), "any".into());
        dict.insert(
            "ProgramArguments".to_string(),
            plist::Value::Array(vec![program.into()]),
        );
        plist::to_file_xml(&path, &plist::Value::Dictionary(dict)).unwrap();
        path
    }

    #[test]
    fn launch_agents_belong_by_name_or_by_the_program_they_start() {
        let (_temp, layout) = sandbox();
        let target = installed(&layout, "Foo", "com.acme.foo");
        let inside = format!("{}/Contents/MacOS/agent", target.path.display());
        let exact = launch_plist(layout.user("Library/LaunchAgents/com.acme.foo.plist"), "/x");
        let derived = launch_plist(
            layout.user("Library/LaunchAgents/com.acme.foo.helper.plist"),
            "/x",
        );
        let by_program = launch_plist(layout.user("Library/LaunchAgents/io.other.plist"), &inside);
        let daemon = launch_plist(layout.system("Library/LaunchDaemons/io.d.plist"), &inside);
        let unrelated = launch_plist(layout.user("Library/LaunchAgents/io.unrelated.plist"), "/x");

        let found = find(&target, &layout);

        assert_found(&found, &exact, Evidence::BundleId);
        assert_found(&found, &derived, Evidence::BundleId);
        assert_found(&found, &by_program, Evidence::Manifest);
        assert_found(&found, &daemon, Evidence::Manifest);
        assert_absent(&found, &unrelated);
    }

    #[test]
    fn a_launch_entry_pointing_out_of_the_bundle_with_dots_is_not_owned() {
        let (_temp, layout) = sandbox();
        let target = installed(&layout, "Foo", "com.acme.foo");
        let escape = format!("{}/../Other.app/run", target.path.display());
        let plist = launch_plist(layout.user("Library/LaunchAgents/io.other.plist"), &escape);

        assert_absent(&find(&target, &layout), &plist);
    }

    #[test]
    fn apple_launch_entries_are_never_claimed() {
        let (_temp, layout) = sandbox();
        let target = installed(&layout, "Foo", "com.acme.foo");
        let inside = format!("{}/Contents/MacOS/agent", target.path.display());
        let plist = launch_plist(
            layout.user("Library/LaunchAgents/com.apple.foo.plist"),
            &inside,
        );

        assert_absent(&find(&target, &layout), &plist);
    }

    #[test]
    fn data_of_extensions_inside_the_app_is_found() {
        let (_temp, layout) = sandbox();
        let target = installed(&layout, "Foo", "com.acme.foo");
        let share = target.path.join("Contents/PlugIns/Share.appex");
        fake_app(&share, "net.other.share", "share");
        let container = folder(layout.user("Library/Containers/net.other.share"));
        let prefs = file(layout.user("Library/Preferences/net.other.share.plist"));
        let by_host = file(layout.user("Library/Preferences/ByHost/net.other.share.ABC.plist"));

        let found = find(&target, &layout);

        assert_found(&found, &container, Evidence::BundleId);
        assert_found(&found, &prefs, Evidence::BundleId);
        assert_found(&found, &by_host, Evidence::BundleId);
    }

    #[test]
    fn a_container_with_a_generated_name_belongs_by_its_metadata() {
        let (_temp, layout) = sandbox();
        let target = installed(&layout, "Foo", "com.acme.foo");
        let write_owner = |name: &str, owner: &str| {
            let container = folder(layout.user("Library/Containers").join(name));
            let mut dict = plist::Dictionary::new();
            dict.insert("MCMMetadataIdentifier".to_string(), owner.into());
            let metadata = container.join(".com.apple.containermanagerd.metadata.plist");
            plist::to_file_xml(metadata, &plist::Value::Dictionary(dict)).unwrap();
            container
        };
        let mine = write_owner("6F9619FF-8B86-D011-B42D-00C04FC964FF", "com.acme.foo");
        let theirs = write_owner("7A9619FF-8B86-D011-B42D-00C04FC964FF", "com.other.app");

        let found = find(&target, &layout);

        assert_found(&found, &mine, Evidence::BundleId);
        assert_absent(&found, &theirs);
    }

    #[test]
    fn a_browser_native_host_belongs_when_it_starts_this_app() {
        let (_temp, layout) = sandbox();
        let target = installed(&layout, "Foo", "com.acme.foo");
        let write_host = |relative: &str, program: String| {
            let path = file(layout.user(relative));
            let manifest = serde_json::json!({ "name": "foo", "path": program });
            fs::write(&path, manifest.to_string()).unwrap();
            path
        };
        let inside = format!("{}/Contents/MacOS/host", target.path.display());
        let firefox = write_host(
            "Library/Application Support/Mozilla/NativeMessagingHosts/foo.json",
            inside.clone(),
        );
        let chrome = write_host(
            "Library/Application Support/Google/Chrome/NativeMessagingHosts/foo.json",
            inside,
        );
        let other = write_host(
            "Library/Application Support/Mozilla/NativeMessagingHosts/bar.json",
            "/Applications/Other.app/host".to_string(),
        );

        let found = find(&target, &layout);

        assert_found(&found, &firefox, Evidence::Manifest);
        assert_found(&found, &chrome, Evidence::Manifest);
        assert_absent(&found, &other);
    }

    #[test]
    fn system_files_are_found_by_id_and_by_name() {
        let (_temp, layout) = sandbox();
        let target = installed(&layout, "Foo Studio", "com.acme.foo");
        let by_id = [
            folder(layout.system("Library/Application Support/com.acme.foo")),
            file(layout.system("Library/Preferences/com.acme.foo.plist")),
            file(layout.system("Library/PrivilegedHelperTools/com.acme.foo.helper")),
            file(layout.system("Library/LaunchDaemons/com.acme.foo.helper.plist")),
            file(layout.system("Library/LaunchAgents/com.acme.foo.plist")),
            folder(layout.system("Library/Caches/com.acme.foo")),
            folder(layout.system("Library/Logs/com.acme.foo")),
        ];
        let by_name = [
            folder(layout.system("Library/Application Support/Foo Studio")),
            folder(layout.system("Library/Application Support/FooStudio")),
            file(layout.system("Library/PrivilegedHelperTools/foostudio-helper")),
            file(layout.system("Library/LaunchDaemons/io.vendor.Foo Studio.plist")),
            folder(layout.system("Users/Shared/Foo Studio")),
        ];

        let found = find(&target, &layout);

        for path in &by_id {
            assert_found(&found, path, Evidence::BundleId);
        }
        for path in &by_name {
            assert_found(&found, path, Evidence::Name);
        }
    }

    #[test]
    fn a_short_or_common_name_does_not_reach_system_or_shared_folders() {
        let (_temp, layout) = sandbox();
        let target = installed(&layout, "Notes", "com.acme.notes");
        let helper = file(layout.system("Library/PrivilegedHelperTools/notes-helper"));
        let shared = folder(layout.system("Users/Shared/Notes"));
        let daemon = file(layout.system("Library/LaunchDaemons/io.vendor.Notes.plist"));

        let found = find(&target, &layout);

        assert_absent(&found, &helper);
        assert_absent(&found, &shared);
        assert_absent(&found, &daemon);
    }

    #[test]
    fn a_two_letter_name_finds_nothing_by_name() {
        let (_temp, layout) = sandbox();
        let target = installed(&layout, "Qt", "com.acme.qt");
        let folder_named = folder(layout.user("Library/Application Support/Qt"));

        assert_absent(&find(&target, &layout), &folder_named);
    }

    #[test]
    fn tools_that_share_a_name_with_an_app_keep_their_folders() {
        let (_temp, layout) = sandbox();
        let target = installed(&layout, "Claude", "com.anthropic.claudefordesktop");
        let code = folder(layout.user(".claude"));
        let config = folder(layout.user(".config/claude"));
        let share = folder(layout.user(".local/share/claude"));
        let own = folder(layout.user("Library/Application Support/Claude"));
        let third_party = folder(layout.user("Library/Application Support/Claude-3p"));

        let found = find(&target, &layout);

        assert_absent(&found, &code);
        assert_absent(&found, &config);
        assert_absent(&found, &share);
        assert_found(&found, &own, Evidence::Name);
        assert_found(&found, &third_party, Evidence::BundleId);
    }

    #[test]
    fn a_steam_library_is_never_claimed() {
        let (_temp, layout) = sandbox();
        let target = installed(&layout, "Steam", "com.valvesoftware.steam");
        let library = folder(layout.user("Library/Application Support/Steam"));
        let by_id = folder(layout.user("Library/Caches/com.valvesoftware.steam"));

        let found = find(&target, &layout);

        assert_absent(&found, &library);
        assert_found(&found, &by_id, Evidence::BundleId);
    }

    #[test]
    fn crash_reports_are_found_by_executable() {
        let (_temp, layout) = sandbox();
        let target = installed(&layout, "Foo", "com.acme.foo");
        let reports = "Library/Logs/DiagnosticReports";
        let user = file(layout.user(reports).join("Foo-2026-10-01-101010.ips"));
        let helper = file(layout.user(reports).join("Foo Helper_2026-10-01.crash"));
        let system = file(layout.system(reports).join("Foo.diag"));
        let other = file(layout.user(reports).join("Foobar-2026-10-01.ips"));
        let text = file(layout.user(reports).join("Foo-notes.txt"));

        let found = find(&target, &layout);

        assert_found(&found, &user, Evidence::BundleId);
        assert_found(&found, &helper, Evidence::BundleId);
        assert_found(&found, &system, Evidence::BundleId);
        assert_absent(&found, &other);
        assert_absent(&found, &text);
    }

    #[test]
    fn an_electron_executable_says_nothing_about_which_app_crashed() {
        let (_temp, layout) = sandbox();
        let mut target = installed(&layout, "Foo", "com.acme.foo");
        target.executable = Some("Electron".to_string());
        let reports = "Library/Logs/DiagnosticReports";
        let shared = file(layout.user(reports).join("Electron-2026-10-01.ips"));
        let own = file(layout.user(reports).join("Foo-2026-10-01.ips"));

        let found = find(&target, &layout);

        assert_absent(&found, &shared);
        assert_found(&found, &own, Evidence::BundleId);
    }

    #[test]
    fn something_inside_a_found_folder_is_not_listed_again() {
        let (_temp, layout) = sandbox();
        let target = installed(&layout, "Foo", "com.acme.foo");
        let parent = folder(layout.user("Library/Caches/com.acme.foo"));
        folder(layout.user("Library/Caches/com.acme.foo/com.acme.foo"));
        folder(layout.user("Library/Caches/com.crashlytics.data/com.acme.foo"));

        let found = find(&target, &layout);

        assert_found(&found, &parent, Evidence::BundleId);
        let inside = found
            .keys()
            .filter(|path| path.starts_with(&parent))
            .count();
        assert_eq!(inside, 1);
    }

    #[test]
    fn two_spellings_of_one_folder_count_once() {
        let (_temp, layout) = sandbox();
        let target = installed(&layout, "Foo Bar", "com.acme.foo");
        folder(layout.user(".cache/foo-bar"));
        let spelled = layout.user(".cache/Foo-Bar");
        if spelled.exists() {
            let found = find(&target, &layout);
            let rows = found
                .keys()
                .filter(|path| path.starts_with(layout.user(".cache")));
            assert_eq!(rows.count(), 1);
        }
    }

    #[test]
    fn an_app_without_a_bundle_id_is_searched_by_name_only() {
        let (_temp, layout) = sandbox();
        let mut target = installed(&layout, "Foo", "com.acme.foo");
        target.bundle_id = None;
        let named = folder(layout.user("Library/Application Support/Foo"));
        let by_id = folder(layout.user("Library/Containers/com.acme.foo"));

        let found = find(&target, &layout);

        assert_found(&found, &named, Evidence::Name);
        assert_absent(&found, &by_id);
    }

    #[test]
    fn the_app_bundle_itself_is_not_a_leftover() {
        let (_temp, layout) = sandbox();
        let target = installed(&layout, "Foo", "com.acme.foo");

        assert!(find(&target, &layout).is_empty());
    }

    #[test]
    fn known_product_folders_are_found() {
        let (_temp, layout) = sandbox();
        let code = installed(&layout, "Visual Studio Code", "com.microsoft.VSCode");
        let user_data = folder(layout.user("Library/Application Support/Code"));
        let extensions = folder(layout.user(".vscode"));
        let insiders = folder(layout.user(".vscode-insiders"));

        let found = find(&code, &layout);

        assert_found(&found, &user_data, Evidence::BundleId);
        assert_found(&found, &extensions, Evidence::BundleId);
        assert_absent(&found, &insiders);

        let xcode = installed(&layout, "Xcode", "com.apple.dt.Xcode");
        let derived = folder(layout.user("Library/Developer/Xcode/DerivedData"));
        let archives = folder(layout.user("Library/Developer/Xcode/Archives"));

        let found = find(&xcode, &layout);

        assert_found(&found, &derived, Evidence::BundleId);
        assert_absent(&found, &archives);
    }

    #[test]
    fn a_jetbrains_ide_owns_the_folders_of_its_own_version() {
        let (_temp, layout) = sandbox();
        let ide = installed(&layout, "GoLand", "com.jetbrains.goland");
        let properties =
            crate::core::apps::testing::dict(&[("idea.paths.selector", "GoLand2026.2".into())]);
        let options = crate::core::apps::testing::dict(&[("Properties", properties)]);
        crate::core::apps::testing::write_info(
            &ide.path,
            &[
                ("CFBundleIdentifier", "com.jetbrains.goland".into()),
                ("JVMOptions", options),
            ],
        );
        let own = folder(layout.user("Library/Application Support/JetBrains/GoLand2026.2"));
        let other = folder(layout.user("Library/Application Support/JetBrains/PyCharm2026.2"));

        let found = find(&ide, &layout);

        assert_found(&found, &own, Evidence::BundleId);
        assert_absent(&found, &other);
    }

    fn pkg_host(id: &str, location: &str, files: &str) -> FakeHost {
        let host = FakeHost::default();
        host.reply(
            "/usr/sbin/pkgutil --file-info",
            &format!("volume: /\npath: x\n\npkgid: {id}\npkg-version: 1\n"),
        );
        host.reply(
            &format!("/usr/sbin/pkgutil --pkg-info {id}"),
            &format!("package-id: {id}\nvolume: /\nlocation: {location}\n"),
        );
        host.reply(&format!("/usr/sbin/pkgutil --files {id}"), files);
        host
    }

    #[test]
    fn the_installer_package_gives_receipts_and_launch_files() {
        let (_temp, layout) = sandbox();
        let target = installed(&layout, "Foo", "us.acme.foo");
        let daemon = file(layout.system("Library/LaunchDaemons/io.vendor.helper.plist"));
        let tool = file(layout.system("Library/PrivilegedHelperTools/io.vendor.tool"));
        let host = pkg_host(
            "us.acme.pkg.foo",
            "/",
            "Applications/Foo.app\nApplications/Foo.app/Contents\nLibrary/LaunchDaemons/io.vendor.helper.plist\nLibrary/PrivilegedHelperTools/io.vendor.tool\nLibrary/Fonts/Shared.ttf\n",
        );

        let findings = find_leftovers(&target, &[], &host, &layout);

        assert_eq!(findings.receipts, ["us.acme.pkg.foo"]);
        let found: Vec<_> = findings
            .leftovers
            .iter()
            .map(|l| (&l.path, l.evidence))
            .collect();
        assert!(found.contains(&(&daemon, Evidence::Receipt)), "{found:?}");
        assert!(found.contains(&(&tool, Evidence::Receipt)), "{found:?}");
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn a_package_that_also_installed_another_app_keeps_its_receipt() {
        let (_temp, layout) = sandbox();
        let target = installed(&layout, "Foo", "us.acme.foo");
        let daemon = file(layout.system("Library/LaunchDaemons/io.vendor.helper.plist"));
        let host = pkg_host(
            "us.acme.suite",
            "/",
            "Applications/Foo.app\nApplications/Bar.app\nLibrary/LaunchDaemons/io.vendor.helper.plist\n",
        );

        let findings = find_leftovers(&target, &[], &host, &layout);

        assert!(findings.receipts.is_empty());
        assert!(!findings.leftovers.iter().any(|l| l.path == daemon));
    }

    #[test]
    fn a_package_installed_right_into_applications_reads_the_same() {
        let (_temp, layout) = sandbox();
        let target = installed(&layout, "Foo", "us.acme.foo");
        let host = pkg_host(
            "us.acme.foo.pkg",
            "Applications",
            "Foo.app\nFoo.app/Contents\nFoo.app/Contents/Library/LoginItems/Helper.app\n",
        );

        let findings = find_leftovers(&target, &[], &host, &layout);

        assert_eq!(findings.receipts, ["us.acme.foo.pkg"]);
    }

    #[test]
    fn apple_packages_are_never_forgotten() {
        let (_temp, layout) = sandbox();
        let target = installed(&layout, "Foo", "us.acme.foo");
        let host = pkg_host(
            "com.apple.files.data-template",
            "/",
            "Applications/Foo.app\n",
        );

        let findings = find_leftovers(&target, &[], &host, &layout);

        assert!(findings.receipts.is_empty());
    }

    #[test]
    fn receipts_named_after_the_bundle_id_are_found_without_pkgutil() {
        let (_temp, layout) = sandbox();
        let target = installed(&layout, "Foo", "com.acme.foo");
        let receipts = "private/var/db/receipts";
        file(layout.system(receipts).join("com.acme.foo.pkg.plist"));
        file(layout.system(receipts).join("com.acme.foo.pkg.bom"));
        file(layout.system(receipts).join("com.acme.foobar.plist"));

        let findings = find_leftovers(&target, &[], &FakeHost::default(), &layout);

        assert_eq!(findings.receipts, ["com.acme.foo.pkg"]);
    }
}

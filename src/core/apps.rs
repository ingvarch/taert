//! Apps installed on this Mac and what their bundles say about them.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, SystemTime};

use plist::{Dictionary, Value};
use rayon::prelude::*;
use serde::Serialize;

use super::date_utils::parse_rfc3339;
use super::host::Host;
use super::layout::Layout;
use super::path_utils::prettify_path;
use super::scanner::{CategoryScan, ScanItem, ScanProgressEvent, ToolGroup};
use super::sizer::DirSizer;

pub const LIST_ID: &str = "apps";

const MDLS_TIMEOUT: Duration = Duration::from_secs(30);
const NO_DATE: &str = "(null)";
const BUNDLE_SEARCH_DEPTH: usize = 3;

/// Apple apps people install themselves. Every other `com.apple.` id is part of macOS.
const REMOVABLE_APPLE_APPS: &[&str] = &[
    "com.apple.compressor",
    "com.apple.configurator",
    "com.apple.dt.",
    "com.apple.finalcut",
    "com.apple.garageband",
    "com.apple.imovie",
    "com.apple.iwork.",
    "com.apple.logic",
    "com.apple.mainstage",
    "com.apple.motion",
    "com.apple.playgrounds",
    "com.apple.server.",
    "com.apple.testflight",
    "com.apple.transporter",
];

/// Vendor, bundle id prefixes, parts of the app name.
const OFFICIAL_UNINSTALLERS: &[(&str, &[&str], &[&str])] = &[
    (
        "ESET",
        &["com.eset."],
        &[
            "eset management agent",
            "eset remote administrator agent",
            "eset endpoint security",
            "eset endpoint antivirus",
        ],
    ),
    (
        "Jamf",
        &["com.jamf.", "com.jamfsoftware."],
        &["jamf connect", "jamf protect", "jamf self service"],
    ),
    (
        "CrowdStrike",
        &["com.crowdstrike."],
        &["crowdstrike", "falcon sensor"],
    ),
    (
        "SentinelOne",
        &["com.sentinelone.", "com.sentinel-labs."],
        &["sentinelone", "sentinel agent"],
    ),
    (
        "GlobalProtect",
        &["com.paloaltonetworks."],
        &["globalprotect"],
    ),
    (
        "Cisco",
        &["com.cisco.anyconnect", "com.cisco.secureclient"],
        &["cisco secure client", "cisco anyconnect"],
    ),
    ("Alibaba Cloud Security", &["com.alibaba.security."], &[]),
];

const COMMON_NAMES: &[&str] = &[
    "agent",
    "backup",
    "books",
    "calendar",
    "client",
    "clock",
    "cloud",
    "contacts",
    "daemon",
    "driver",
    "extension",
    "files",
    "finder",
    "helper",
    "launcher",
    "manager",
    "messages",
    "monitor",
    "music",
    "news",
    "notes",
    "photos",
    "plugin",
    "podcasts",
    "preview",
    "reminders",
    "runner",
    "safari",
    "server",
    "service",
    "stocks",
    "store",
    "sync",
    "system",
    "update",
    "utility",
    "voice",
    "weather",
    "widget",
    "worker",
];

/// Where a bundle may hold helpers with their own ids.
const EMBEDDED_FOLDERS: &[&str] = &[
    "Contents/PlugIns",
    "Contents/Extensions",
    "Contents/XPCServices",
];
const LOGIN_ITEMS_FOLDER: &str = "Contents/Library/LoginItems";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstalledApp {
    pub name: String,
    pub path: PathBuf,
    pub bundle_id: Option<String>,
    pub executable: Option<String>,
    pub size_bytes: u64,
    pub last_used: Option<SystemTime>,
}

/// `com.vendor.app`, the only shape a bundle id may have before it builds a path.
pub fn is_reverse_dns(id: &str) -> bool {
    let mut parts = id.split('.');
    let valid = |part: &str| {
        part.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
            && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    };
    id.contains('.') && parts.all(valid)
}

fn is_part_of_macos(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    let is_apple = id == "com.apple" || id.starts_with("com.apple.");
    is_apple
        && !REMOVABLE_APPLE_APPS
            .iter()
            .any(|prefix| id.starts_with(prefix))
}

fn read_info(bundle: &Path) -> Option<Dictionary> {
    [
        "Contents/Info.plist",
        "WrappedBundle/Info.plist",
        "Info.plist",
    ]
    .iter()
    .find_map(|relative| Value::from_file(bundle.join(relative)).ok())?
    .into_dictionary()
}

fn text(info: &Dictionary, key: &str) -> Option<String> {
    info.get(key)?.as_string().map(str::to_string)
}

fn is_true(value: &Value) -> bool {
    match value {
        Value::Boolean(flag) => *flag,
        Value::Integer(number) => number.as_signed() == Some(1),
        Value::String(word) => matches!(word.to_lowercase().as_str(), "1" | "yes" | "true"),
        _ => false,
    }
}

fn is_link_into_system(path: &Path) -> bool {
    fs::read_link(path).is_ok_and(|target| {
        [
            "/System",
            "/usr/bin",
            "/usr/lib",
            "/bin",
            "/sbin",
            "/private/etc",
        ]
        .iter()
        .any(|place| target.starts_with(place))
    })
}

fn has_app_extension(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("app"))
}

fn bundles_in(folder: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(folder) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if has_app_extension(&path) {
            if !is_link_into_system(&path) {
                found.push(path);
            }
            continue;
        }
        // A hidden folder holds an updater's staging copy, never an install.
        let hidden = entry.file_name().to_string_lossy().starts_with('.');
        let is_folder = entry.file_type().is_ok_and(|kind| kind.is_dir());
        if is_folder && !hidden && depth > 1 {
            bundles_in(&path, depth - 1, found);
        }
    }
}

fn read_app(path: &Path, root: &Path) -> Option<InstalledApp> {
    let info = read_info(path).unwrap_or_default();
    let bundle_id = text(&info, "CFBundleIdentifier").filter(|id| is_reverse_dns(id));
    if bundle_id.as_deref().is_some_and(is_part_of_macos) {
        return None;
    }
    let background = info.get("LSBackgroundOnly").is_some_and(is_true);
    if background && path.parent() != Some(root) {
        return None;
    }

    Some(InstalledApp {
        name: path.file_stem()?.to_string_lossy().into_owned(),
        path: path.to_path_buf(),
        bundle_id,
        executable: text(&info, "CFBundleExecutable"),
        size_bytes: 0,
        last_used: None,
    })
}

fn parse_spotlight_date(value: &str) -> Option<SystemTime> {
    parse_rfc3339(&format!("{}Z", value.trim().strip_suffix(" +0000")?))
}

fn last_used_dates(host: &dyn Host, apps: &mut [InstalledApp]) {
    if apps.is_empty() {
        return;
    }
    let paths: Vec<String> = apps
        .iter()
        .map(|app| app.path.display().to_string())
        .collect();
    let mut args = vec![
        "-raw",
        "-nullMarker",
        NO_DATE,
        "-name",
        "kMDItemLastUsedDate",
    ];
    args.extend(paths.iter().map(String::as_str));

    let output = host.run("/usr/bin/mdls", &args, MDLS_TIMEOUT);
    let values: Vec<&str> = output.stdout.trim_end_matches('\0').split('\0').collect();
    // Without one value per app the dates could land on the wrong apps.
    if values.len() == apps.len() {
        for (app, value) in apps.iter_mut().zip(values) {
            app.last_used = parse_spotlight_date(value);
        }
    }
}

/// The bundle id an app has on disk right now.
pub fn bundle_id_of(bundle: &Path) -> Option<String> {
    text(&read_info(bundle)?, "CFBundleIdentifier").filter(|id| is_reverse_dns(id))
}

/// Finds apps without measuring them, which takes a moment and no more.
pub fn find_apps(layout: &Layout) -> Vec<InstalledApp> {
    let mut bundles = Vec::new();
    for root in layout.app_roots() {
        let mut found = Vec::new();
        bundles_in(&root, BUNDLE_SEARCH_DEPTH, &mut found);
        bundles.extend(found.into_iter().map(|path| (root.clone(), path)));
    }

    let mut apps: Vec<InstalledApp> = bundles
        .par_iter()
        .filter_map(|(root, path)| read_app(path, root))
        .collect();
    apps.sort_by_key(|app| app.name.to_lowercase());
    apps
}

fn measure(app: &mut InstalledApp) {
    let target = if app.path.is_symlink() {
        fs::canonicalize(&app.path).unwrap_or_else(|_| app.path.clone())
    } else {
        app.path.clone()
    };
    app.size_bytes = DirSizer::size_of(&target);
}

/// Adds sizes and dates. Sizes take the longest, so progress goes out for each app.
pub fn measure_apps(
    apps: &mut [InstalledApp],
    host: &dyn Host,
    progress: Option<Sender<ScanProgressEvent>>,
    cancel: Option<Arc<AtomicBool>>,
) {
    let cancelled = || {
        cancel
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
    };
    apps.par_iter_mut().for_each(|app| {
        if cancelled() {
            return;
        }
        if let Some(tx) = &progress {
            let _ = tx.send(ScanProgressEvent::ScanningPath(prettify_path(&app.path)));
        }
        measure(app);
        if let Some(tx) = &progress {
            let bytes = app.size_bytes;
            let _ = tx.send(ScanProgressEvent::ItemFound { bytes });
        }
    });
    last_used_dates(host, apps);
}

/// Every app with its size and last use.
pub fn installed_apps(
    layout: &Layout,
    host: &dyn Host,
    progress: Option<Sender<ScanProgressEvent>>,
    cancel: Option<Arc<AtomicBool>>,
) -> Vec<InstalledApp> {
    let mut apps = find_apps(layout);
    measure_apps(&mut apps, host, progress, cancel.clone());
    if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
        return Vec::new();
    }
    apps
}

/// The list of apps to pick from, largest first in each folder.
pub fn app_scan(apps: &[InstalledApp], layout: &Layout) -> CategoryScan {
    let groups = layout
        .app_roots()
        .iter()
        .filter_map(|root| {
            let mut items: Vec<ScanItem> = apps
                .iter()
                .filter(|app| app.path.starts_with(root))
                .map(|app| {
                    let mut item = ScanItem::new(app.path.clone(), app.size_bytes, false, false);
                    item.modified = app.last_used;
                    item
                })
                .collect();
            items.sort_by_key(|item| std::cmp::Reverse(item.size_bytes));
            (!items.is_empty()).then(|| ToolGroup {
                tool: prettify_path(root),
                items,
            })
        })
        .collect();

    CategoryScan {
        id: LIST_ID.to_string(),
        title: "Uninstall apps".to_string(),
        groups,
    }
}

fn is_named(app: &InstalledApp, term: &str) -> bool {
    app.name.to_lowercase() == term.to_lowercase()
        || app.path.as_os_str() == term
        || app
            .bundle_id
            .as_deref()
            .is_some_and(|id| id.eq_ignore_ascii_case(term))
}

fn pick<'a>(
    apps: &'a [InstalledApp],
    term: &str,
    matching: Matching,
) -> Result<&'a InstalledApp, String> {
    let exact: Vec<&InstalledApp> = apps.iter().filter(|app| is_named(app, term)).collect();
    let matches = if exact.is_empty() && matching == Matching::Partial {
        let lower = term.to_lowercase();
        apps.iter()
            .filter(|app| app.name.to_lowercase().contains(&lower))
            .collect()
    } else {
        exact
    };

    match matches.as_slice() {
        [] => Err(format!("No installed app matches '{term}'")),
        [app] => Ok(app),
        several => {
            let paths: Vec<String> = several
                .iter()
                .map(|app| app.path.display().to_string())
                .collect();
            Err(format!(
                "'{term}' matches several apps. Use a full name or path: {}",
                paths.join(", ")
            ))
        }
    }
}

/// How far a name given on the command line may be from the app it picks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Matching {
    Exact,
    Partial,
}

/// Picks apps by name, bundle file or bundle id. A name that fits several apps
/// is an error, because a guess here would delete the wrong app.
pub fn find_by_names(
    apps: &[InstalledApp],
    terms: &[String],
    matching: Matching,
) -> Result<Vec<InstalledApp>, String> {
    let names_an_app = |term: &String| apps.iter().any(|app| is_named(app, term));
    if terms.len() > 1 && !terms.iter().all(names_an_app) {
        // `taert uninstall Tor Browser` arrives as two words.
        let joined = terms.join(" ");
        if let Some(app) = apps.iter().find(|app| is_named(app, &joined)) {
            return Ok(vec![app.clone()]);
        }
    }

    let mut picked: Vec<InstalledApp> = Vec::new();
    for term in terms {
        let app = pick(apps, term, matching)?;
        if !picked.contains(app) {
            picked.push(app.clone());
        }
    }
    Ok(picked)
}

/// The vendor whose own uninstaller an app needs. Security agents treat outside
/// removal as tampering.
pub fn official_uninstaller(app: &InstalledApp) -> Option<&'static str> {
    let id = app
        .bundle_id
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let name = app.name.to_lowercase();
    OFFICIAL_UNINSTALLERS
        .iter()
        .find(|(_, prefixes, fragments)| {
            prefixes.iter().any(|prefix| id.starts_with(prefix))
                || fragments.iter().any(|fragment| name.contains(fragment))
        })
        .map(|(vendor, _, _)| *vendor)
}

/// Generic words that say nothing about which app a file belongs to.
pub fn is_common_name(name: &str) -> bool {
    COMMON_NAMES.contains(&name.to_lowercase().as_str())
}

fn bundle_ids_in(folder: &Path, own_id: Option<&str>) -> Vec<String> {
    let Ok(entries) = fs::read_dir(folder) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| text(&read_info(&entry.path())?, "CFBundleIdentifier"))
        .filter(|id| is_reverse_dns(id) && Some(id.as_str()) != own_id)
        // Sparkle ships in many apps, so these ids belong to none of them.
        .filter(|id| !id.starts_with("org.sparkle-project."))
        .collect()
}

/// Bundle ids of the extensions and helpers inside an app, and which of those
/// are login items.
pub fn embedded_bundle_ids(app: &Path, own_id: Option<&str>) -> (Vec<String>, Vec<String>) {
    let login_items = bundle_ids_in(&app.join(LOGIN_ITEMS_FOLDER), own_id);
    let mut embedded: Vec<String> = EMBEDDED_FOLDERS
        .iter()
        .flat_map(|folder| bundle_ids_in(&app.join(folder), own_id))
        .chain(login_items.iter().cloned())
        .collect();
    embedded.sort();
    embedded.dedup();
    (embedded, login_items)
}

fn is_jetbrains_selector(value: &str) -> bool {
    if !value.is_ascii() {
        return false;
    }
    let Some((product, minor)) = value
        .rsplit_once('.')
        .filter(|(product, _)| product.len() > 4)
    else {
        return false;
    };
    let (name, year) = product.split_at(product.len().saturating_sub(4));
    let starts_with_letter = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic());
    let is_digits = |part: &str| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit());
    starts_with_letter
        && name.chars().all(|c| c.is_ascii_alphanumeric())
        && is_digits(year)
        && is_digits(minor)
}

fn selector_in_plist(app: &Path) -> Option<String> {
    let selector = read_info(app)?
        .get("JVMOptions")?
        .as_dictionary()?
        .get("Properties")?
        .as_dictionary()?
        .get("idea.paths.selector")?
        .as_string()?
        .to_string();
    Some(selector)
}

fn selector_in_product_info(app: &Path) -> Option<String> {
    let text = fs::read_to_string(app.join("Contents/Resources/product-info.json")).ok()?;
    let info: serde_json::Value = serde_json::from_str(&text).ok()?;
    info.get("dataDirectoryName")?.as_str().map(str::to_string)
}

/// The name a JetBrains IDE gives its data folders, like `GoLand2026.2`.
pub fn jetbrains_selector(app: &Path) -> Option<String> {
    selector_in_plist(app)
        .or_else(|| selector_in_product_info(app))
        .filter(|selector| is_jetbrains_selector(selector))
}

#[cfg(test)]
pub mod testing {
    use super::*;
    use std::fs;

    pub fn sandbox() -> (tempfile::TempDir, Layout) {
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

    pub fn dict(entries: &[(&str, plist::Value)]) -> plist::Value {
        let dict: plist::Dictionary = entries
            .iter()
            .map(|(key, value)| (key.to_string(), value.clone()))
            .collect();
        plist::Value::Dictionary(dict)
    }

    pub fn write_info(bundle: &Path, entries: &[(&str, plist::Value)]) {
        let contents = bundle.join("Contents");
        fs::create_dir_all(&contents).unwrap();
        plist::to_file_xml(contents.join("Info.plist"), &dict(entries)).unwrap();
    }

    pub fn fake_app(path: &Path, id: &str, executable: &str) {
        let binary = path.join("Contents/MacOS").join(executable);
        fs::create_dir_all(binary.parent().unwrap()).unwrap();
        fs::write(&binary, vec![0_u8; 8192]).unwrap();
        write_info(
            path,
            &[
                ("CFBundleIdentifier", id.into()),
                ("CFBundleExecutable", executable.into()),
            ],
        );
    }

    pub fn app(name: &str, id: &str) -> InstalledApp {
        InstalledApp {
            name: name.to_string(),
            path: PathBuf::from(format!("/Applications/{name}.app")),
            bundle_id: Some(id.to_string()),
            executable: Some(name.to_string()),
            size_bytes: 1,
            last_used: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use crate::core::host::fake::FakeHost;
    use std::fs;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, UNIX_EPOCH};

    fn list(layout: &Layout) -> Vec<InstalledApp> {
        installed_apps(layout, &FakeHost::default(), None, None)
    }

    fn names(apps: &[InstalledApp]) -> Vec<&str> {
        apps.iter().map(|app| app.name.as_str()).collect()
    }

    #[test]
    fn a_bundle_id_must_look_like_reverse_dns() {
        for id in ["com.foo", "com.foo.Bar", "dev.zed.Zed-Nightly", "9to5.app"] {
            assert!(is_reverse_dns(id), "{id}");
        }
        for id in [
            "",
            "foo",
            "com..foo",
            "../etc",
            "com.foo/bar",
            "com.*",
            "-a.b",
        ] {
            assert!(!is_reverse_dns(id), "{id}");
        }
    }

    #[test]
    fn the_applications_link_of_a_disk_image_is_no_app_folder() {
        let (_temp, layout) = sandbox();
        fake_app(&layout.system("Applications/A.app"), "com.a.one", "a");
        let volume = layout.system("Volumes/Installer");
        fs::create_dir_all(&volume).unwrap();
        let applications = layout.system("Applications");
        std::os::unix::fs::symlink(&applications, volume.join("Applications")).unwrap();

        assert_eq!(names(&list(&layout)), ["A"]);
    }

    #[test]
    fn apps_are_read_from_their_info_plist() {
        let (_temp, layout) = sandbox();
        fake_app(
            &layout.system("Applications/Foo Bar.app"),
            "com.foo.bar",
            "foo",
        );

        let apps = list(&layout);

        assert_eq!(names(&apps), ["Foo Bar"]);
        assert_eq!(apps[0].bundle_id.as_deref(), Some("com.foo.bar"));
        assert_eq!(apps[0].executable.as_deref(), Some("foo"));
        assert!(apps[0].size_bytes >= 8192);
    }

    #[test]
    fn a_binary_info_plist_reads_the_same() {
        let (_temp, layout) = sandbox();
        let bundle = layout.system("Applications/Foo.app");
        fs::create_dir_all(bundle.join("Contents")).unwrap();
        let value = dict(&[("CFBundleIdentifier", "com.foo.bin".into())]);
        plist::to_file_binary(bundle.join("Contents/Info.plist"), &value).unwrap();

        assert_eq!(list(&layout)[0].bundle_id.as_deref(), Some("com.foo.bin"));
    }

    #[test]
    fn every_app_folder_is_searched_and_vendor_folders_too() {
        let (_temp, layout) = sandbox();
        fake_app(&layout.system("Applications/A.app"), "com.a.one", "a");
        fake_app(&layout.user("Applications/B.app"), "com.b.one", "b");
        fake_app(&layout.system("Applications/Suite/C.app"), "com.c.one", "c");
        fake_app(
            &layout.user("Library/Input Methods/D.app"),
            "com.d.one",
            "d",
        );
        fake_app(
            &layout.system("Volumes/Ext/Applications/E.app"),
            "com.e.one",
            "e",
        );

        let apps = list(&layout);

        assert_eq!(names(&apps), ["A", "B", "C", "D", "E"]);
    }

    #[test]
    fn helpers_inside_an_app_and_updater_copies_are_not_apps() {
        let (_temp, layout) = sandbox();
        let foo = layout.system("Applications/Foo.app");
        fake_app(&foo, "com.foo.app", "foo");
        fake_app(
            &foo.join("Contents/Library/LoginItems/Helper.app"),
            "com.foo.helper",
            "h",
        );
        fake_app(
            &layout.system("Applications/.foo-update/Foo.app"),
            "com.foo.app",
            "foo",
        );

        assert_eq!(names(&list(&layout)), ["Foo"]);
    }

    #[test]
    fn apple_system_apps_stay_out_and_removable_apple_apps_are_listed() {
        let (_temp, layout) = sandbox();
        fake_app(
            &layout.system("Applications/Safari.app"),
            "com.apple.Safari",
            "Safari",
        );
        fake_app(
            &layout.system("Applications/Music.app"),
            "com.apple.Music",
            "Music",
        );
        fake_app(
            &layout.system("Applications/Xcode.app"),
            "com.apple.dt.Xcode",
            "Xcode",
        );
        fake_app(
            &layout.system("Applications/Pages.app"),
            "com.apple.iWork.Pages",
            "Pages",
        );

        assert_eq!(names(&list(&layout)), ["Pages", "Xcode"]);
    }

    #[test]
    fn a_background_helper_counts_only_directly_in_an_app_folder() {
        let (_temp, layout) = sandbox();
        let direct = layout.system("Applications/Agent.app");
        let nested = layout.system("Applications/Suite/Nested.app");
        for (bundle, id) in [(&direct, "com.agent.one"), (&nested, "com.nested.one")] {
            write_info(
                bundle,
                &[
                    ("CFBundleIdentifier", id.into()),
                    ("LSBackgroundOnly", true.into()),
                ],
            );
        }

        assert_eq!(names(&list(&layout)), ["Agent"]);
    }

    #[test]
    fn a_link_into_the_system_is_not_an_app_and_other_links_are() {
        let (_temp, layout) = sandbox();
        let apps_dir = layout.system("Applications");
        fs::create_dir_all(&apps_dir).unwrap();
        std::os::unix::fs::symlink("/System/Applications/Chess.app", apps_dir.join("Chess.app"))
            .unwrap();
        let cask = layout.root.join("opt/homebrew/Caskroom/foo/1.0/Foo.app");
        fake_app(&cask, "com.foo.cask", "foo");
        std::os::unix::fs::symlink(&cask, apps_dir.join("Foo.app")).unwrap();

        let apps = list(&layout);

        assert_eq!(names(&apps), ["Foo"]);
        assert!(apps[0].size_bytes >= 8192);
    }

    #[test]
    fn a_broken_bundle_id_is_dropped_and_the_app_stays_listed() {
        let (_temp, layout) = sandbox();
        write_info(
            &layout.system("Applications/Odd.app"),
            &[("CFBundleIdentifier", "../../Library".into())],
        );
        fs::create_dir_all(layout.system("Applications/Bare.app")).unwrap();

        let apps = list(&layout);

        assert_eq!(names(&apps), ["Bare", "Odd"]);
        assert!(apps.iter().all(|app| app.bundle_id.is_none()));
    }

    #[test]
    fn last_used_dates_come_from_spotlight_in_one_call() {
        let (_temp, layout) = sandbox();
        fake_app(&layout.system("Applications/A.app"), "com.a.one", "a");
        fake_app(&layout.system("Applications/B.app"), "com.b.one", "b");
        let host = FakeHost::default();
        host.reply("/usr/bin/mdls", "2026-10-14 14:04:07 +0000\0(null)\0");

        let apps = installed_apps(&layout, &host, None, None);

        let expected = UNIX_EPOCH + Duration::from_secs(1_791_986_647);
        assert_eq!(apps[0].last_used, Some(expected));
        assert_eq!(apps[1].last_used, None);
        assert_eq!(host.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_cancelled_search_stops() {
        let (_temp, layout) = sandbox();
        fake_app(&layout.system("Applications/A.app"), "com.a.one", "a");
        let cancel = Arc::new(AtomicBool::new(true));

        let apps = installed_apps(&layout, &FakeHost::default(), None, Some(cancel.clone()));

        assert!(apps.is_empty());
        assert!(cancel.load(Ordering::Relaxed));
    }

    #[test]
    fn the_list_is_grouped_by_folder_with_the_largest_first_and_nothing_selected() {
        let (_temp, layout) = sandbox();
        let mut small = app("Small", "com.small.one");
        small.path = layout.system("Applications/Small.app");
        small.size_bytes = 10;
        let mut big = app("Big", "com.big.one");
        big.path = layout.system("Applications/Big.app");
        big.size_bytes = 500;
        big.last_used = Some(UNIX_EPOCH);
        let mut mine = app("Mine", "com.mine.one");
        mine.path = layout.user("Applications/Mine.app");

        let scan = app_scan(&[small, big, mine], &layout);

        assert_eq!(scan.id, LIST_ID);
        assert_eq!(scan.groups.len(), 2);
        let first = &scan.groups[0];
        assert!(first.tool.ends_with("Applications"));
        let leaf: Vec<_> = first
            .items
            .iter()
            .map(|i| i.path.file_name().unwrap())
            .collect();
        assert_eq!(leaf, ["Big.app", "Small.app"]);
        assert_eq!(first.items[0].size_bytes, 500);
        assert_eq!(first.items[0].modified, Some(UNIX_EPOCH));
        assert!(scan.selected_items().is_empty());
    }

    fn terms(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| word.to_string()).collect()
    }

    fn sample() -> Vec<InstalledApp> {
        vec![
            app("Zoom", "us.zoom.xos"),
            app("Google Chrome", "com.google.Chrome"),
            app("Google Drive", "com.google.drivefs"),
            app("Tor Browser", "org.torproject.torbrowser"),
            app("WebStorm", "com.jetbrains.WebStorm"),
        ]
    }

    #[test]
    fn names_match_exactly_without_regard_to_case() {
        let found = find_by_names(&sample(), &terms(&["zoom"]), Matching::Partial).unwrap();

        assert_eq!(names(&found), ["Zoom"]);
    }

    #[test]
    fn a_bundle_id_names_an_app() {
        let found =
            find_by_names(&sample(), &terms(&["com.google.Chrome"]), Matching::Partial).unwrap();

        assert_eq!(names(&found), ["Google Chrome"]);
    }

    #[test]
    fn part_of_a_name_works_when_only_one_app_has_it() {
        let found = find_by_names(&sample(), &terms(&["chrome"]), Matching::Partial).unwrap();

        assert_eq!(names(&found), ["Google Chrome"]);
    }

    #[test]
    fn part_of_a_name_shared_by_apps_is_an_error_that_lists_them() {
        let error = find_by_names(&sample(), &terms(&["google"]), Matching::Partial).unwrap_err();

        assert!(error.contains("Google Chrome"), "{error}");
        assert!(error.contains("Google Drive"), "{error}");
    }

    #[test]
    fn unattended_removal_takes_only_whole_names() {
        let error = find_by_names(&sample(), &terms(&["chrome"]), Matching::Exact).unwrap_err();

        assert!(error.contains("chrome"), "{error}");
        assert!(find_by_names(&sample(), &terms(&["zoom"]), Matching::Exact).is_ok());
    }

    #[test]
    fn an_unknown_name_is_an_error() {
        let error = find_by_names(&sample(), &terms(&["nothing"]), Matching::Partial).unwrap_err();

        assert!(error.contains("nothing"), "{error}");
    }

    #[test]
    fn words_of_one_app_name_given_apart_mean_that_app() {
        let found =
            find_by_names(&sample(), &terms(&["Tor", "Browser"]), Matching::Partial).unwrap();

        assert_eq!(names(&found), ["Tor Browser"]);
    }

    #[test]
    fn several_names_pick_several_apps_once_each() {
        let found = find_by_names(
            &sample(),
            &terms(&["zoom", "Zoom", "WebStorm"]),
            Matching::Partial,
        )
        .unwrap();

        assert_eq!(names(&found), ["Zoom", "WebStorm"]);
    }

    #[test]
    fn security_agents_need_their_own_uninstaller() {
        let falcon = app("Falcon", "com.crowdstrike.falcon.App");
        let jamf = app("Jamf Connect", "com.example.other");

        assert_eq!(official_uninstaller(&falcon), Some("CrowdStrike"));
        assert_eq!(official_uninstaller(&jamf), Some("Jamf"));
        assert_eq!(official_uninstaller(&app("Zoom", "us.zoom.xos")), None);
    }

    #[test]
    fn common_words_are_not_distinctive_names() {
        assert!(is_common_name("Notes"));
        assert!(is_common_name("HELPER"));
        assert!(!is_common_name("Zoom"));
    }

    #[test]
    fn extensions_and_login_items_inside_an_app_are_found() {
        let (_temp, layout) = sandbox();
        let foo = layout.system("Applications/Foo.app");
        fake_app(&foo, "com.foo.app", "foo");
        fake_app(
            &foo.join("Contents/PlugIns/Share.appex"),
            "com.foo.app.share",
            "s",
        );
        fake_app(
            &foo.join("Contents/XPCServices/Net.xpc"),
            "com.foo.net",
            "n",
        );
        fake_app(
            &foo.join("Contents/Library/LoginItems/Login.app"),
            "com.foo.login",
            "l",
        );
        fake_app(
            &foo.join("Contents/XPCServices/Downloader.xpc"),
            "org.sparkle-project.Downloader",
            "d",
        );
        fake_app(
            &foo.join("Contents/XPCServices/Self.xpc"),
            "com.foo.app",
            "d",
        );

        let (embedded, login_items) = embedded_bundle_ids(&foo, Some("com.foo.app"));

        assert_eq!(
            embedded,
            ["com.foo.app.share", "com.foo.login", "com.foo.net"]
        );
        assert_eq!(login_items, ["com.foo.login"]);
    }

    #[test]
    fn a_newer_jetbrains_ide_gives_its_folder_name_in_product_info() {
        let (_temp, layout) = sandbox();
        let ide = layout.system("Applications/DataGrip.app");
        let resources = ide.join("Contents/Resources");
        fs::create_dir_all(&resources).unwrap();
        let info = r#"{"name":"DataGrip","dataDirectoryName":"DataGrip2025.3"}"#;
        fs::write(resources.join("product-info.json"), info).unwrap();

        assert_eq!(jetbrains_selector(&ide).as_deref(), Some("DataGrip2025.3"));
    }

    #[test]
    fn a_jetbrains_data_folder_name_comes_from_the_ide() {
        let (_temp, layout) = sandbox();
        let selector = |value: &str| {
            let properties = dict(&[("idea.paths.selector", value.into())]);
            dict(&[("Properties", properties)])
        };
        let ide = layout.system("Applications/GoLand.app");
        write_info(&ide, &[("JVMOptions", selector("GoLand2026.2"))]);
        let spoofed = layout.system("Applications/Bad.app");
        write_info(&spoofed, &[("JVMOptions", selector("../.."))]);

        assert_eq!(jetbrains_selector(&ide).as_deref(), Some("GoLand2026.2"));
        assert_eq!(jetbrains_selector(&spoofed), None);

        write_info(&spoofed, &[("JVMOptions", selector("Яabc.1"))]);
        assert_eq!(jetbrains_selector(&spoofed), None);
    }
}

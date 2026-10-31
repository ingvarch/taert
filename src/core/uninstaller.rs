//! Plans an uninstall as a reviewable list and carries it out.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;

use serde::Serialize;

use super::apps::{InstalledApp, embedded_bundle_ids, is_common_name, official_uninstaller};
use super::host::Host;
use super::layout::Layout;
use super::leftovers::{Evidence, Leftover, UninstallRole, find_leftovers};
use super::path_utils::prettify_path;
use super::safety::Safety;
use super::scanner::{CategoryScan, ScanItem, ScanProgressEvent, ToolGroup};
use super::sizer::DirSizer;
use super::stores::IgnoreStore;

pub const PLAN_ID: &str = "uninstall";

const MIN_SELECTED_NAME: usize = 4;
const RECEIPTS: &str = "private/var/db/receipts";
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
    match found.evidence {
        Evidence::BundleId | Evidence::Manifest | Evidence::Receipt => true,
        Evidence::RelatedName => false,
        // A name can be a coincidence, so it needs to be distinctive and in the
        // place an app would use. Dot folders and system folders stay a decision.
        Evidence::Name => {
            found.path.starts_with(layout.user("Library"))
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

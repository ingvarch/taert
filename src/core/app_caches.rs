//! Rows for the rest of `~/Library/Caches` and for browser caches.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rayon::prelude::*;

use super::apps::find_apps;
use super::host::Host;
use super::layout::Layout;
use super::path_utils::prettify_path;
use super::uninstaller::process_table;

pub const APP_CACHES: &str = "App caches";
pub const BROWSER_CACHES: &str = "Browser caches";

const CACHES: &str = "Library/Caches";
const STALE_AFTER: Duration = Duration::from_secs(30 * 24 * 60 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheRow {
    pub path: PathBuf,
    pub selected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheGroup {
    pub tool: &'static str,
    pub rows: Vec<CacheRow>,
}

#[derive(Debug, Default)]
pub struct CachePlan {
    pub groups: Vec<CacheGroup>,
    pub notes: Vec<String>,
}

struct Browser {
    name: &'static str,
    bundle: &'static str,
    caches: &'static [&'static str],
}

const BROWSERS: &[Browser] = &[
    Browser {
        name: "Safari",
        bundle: "Safari.app",
        caches: &[
            "Library/Caches/com.apple.Safari",
            "Library/Containers/com.apple.Safari/Data/Library/Caches",
        ],
    },
    Browser {
        name: "Chrome",
        bundle: "Google Chrome.app",
        caches: &["Library/Caches/Google/Chrome"],
    },
    Browser {
        name: "Edge",
        bundle: "Microsoft Edge.app",
        caches: &["Library/Caches/Microsoft Edge"],
    },
    Browser {
        name: "Brave",
        bundle: "Brave Browser.app",
        caches: &["Library/Caches/BraveSoftware/Brave-Browser"],
    },
    Browser {
        name: "Firefox",
        bundle: "Firefox.app",
        caches: &["Library/Caches/Firefox"],
    },
    Browser {
        name: "Arc",
        bundle: "Arc.app",
        caches: &["Library/Caches/company.thebrowser.Browser"],
    },
];

/// These hold sync state, and macOS refuses to remove some of them.
const SYNC_STATE: &[&str] = &["CloudKit", "FamilyCircle"];

type Processes = [(u32, u32, String)];

/// A cache starts selected when nothing changed in it for a while and its app is not running.
pub fn starts_selected(stale: bool, running: bool) -> bool {
    stale && !running
}

/// Did anything in the tree change after `cutoff`? What cannot be read counts as changed.
fn changed_since(path: &Path, cutoff: SystemTime) -> bool {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return true;
    };
    if meta.modified().is_ok_and(|time| time > cutoff) {
        return true;
    }
    if !meta.is_dir() {
        return false;
    }
    match fs::read_dir(path) {
        Ok(entries) => entries
            .flatten()
            .any(|entry| changed_since(&entry.path(), cutoff)),
        Err(_) => true,
    }
}

fn runs_inside(processes: &Processes, bundle: &Path) -> bool {
    processes
        .iter()
        .any(|(_, _, program)| Path::new(program).starts_with(bundle))
}

fn runs_bundle_named(processes: &Processes, bundle: &str) -> bool {
    processes.iter().any(|(_, _, program)| {
        Path::new(program)
            .components()
            .any(|part| part.as_os_str() == bundle)
    })
}

fn is_left_out(name: &str) -> bool {
    name.to_ascii_lowercase().starts_with("com.apple.") || SYNC_STATE.contains(&name)
}

fn overlaps(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn app_cache_group(
    layout: &Layout,
    processes: &Processes,
    covered: &[PathBuf],
    now: SystemTime,
) -> CacheGroup {
    let apps = find_apps(layout);
    let cutoff = now - STALE_AFTER;
    let Ok(entries) = fs::read_dir(layout.user(CACHES)) else {
        return CacheGroup {
            tool: APP_CACHES,
            rows: Vec::new(),
        };
    };
    let candidates: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| !is_left_out(&entry.file_name().to_string_lossy()))
        .map(|entry| entry.path())
        .filter(|path| !covered.iter().any(|known| overlaps(path, known)))
        .collect();

    let rows = candidates
        .into_par_iter()
        .map(|path| {
            let name = path.file_name().map(|name| name.to_string_lossy());
            let running = apps
                .iter()
                .filter(|app| {
                    app.bundle_id
                        .as_deref()
                        .zip(name.as_deref())
                        .is_some_and(|(id, name)| id.eq_ignore_ascii_case(name))
                })
                .any(|app| runs_inside(processes, &app.path));
            let selected = starts_selected(!changed_since(&path, cutoff), running);
            CacheRow { path, selected }
        })
        .collect();
    CacheGroup {
        tool: APP_CACHES,
        rows,
    }
}

fn browser_group(layout: &Layout, processes: &Processes, notes: &mut Vec<String>) -> CacheGroup {
    let mut rows = Vec::new();
    for browser in BROWSERS {
        let running = runs_bundle_named(processes, browser.bundle);
        for relative in browser.caches {
            let path = layout.user(relative);
            match fs::read_dir(&path) {
                Ok(_) => rows.push(CacheRow {
                    path,
                    selected: starts_selected(true, running),
                }),
                Err(error) if error.kind() == ErrorKind::PermissionDenied => {
                    notes.push(format!(
                        "{} keeps a cache that macOS hides from this terminal ({}). \
                         Give the terminal Full Disk Access in System Settings to include it.",
                        browser.name,
                        prettify_path(&path),
                    ));
                }
                Err(_) => {}
            }
        }
    }
    CacheGroup {
        tool: BROWSER_CACHES,
        rows,
    }
}

/// `named` lists the paths other targets already cover.
pub fn plan(layout: &Layout, host: &dyn Host, named: &[PathBuf], now: SystemTime) -> CachePlan {
    let processes = process_table(host);
    let mut notes = Vec::new();
    let browsers = browser_group(layout, &processes, &mut notes);

    let mut covered = named.to_vec();
    covered.extend(
        BROWSERS
            .iter()
            .flat_map(|browser| browser.caches)
            .map(|relative| layout.user(relative)),
    );
    let apps = app_cache_group(layout, &processes, &covered, now);

    let groups = [apps, browsers]
        .into_iter()
        .filter(|group| !group.rows.is_empty())
        .collect();
    CachePlan { groups, notes }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::apps::testing::{fake_app, sandbox};
    use crate::core::host::fake::FakeHost;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    fn cache(layout: &Layout, relative: &str, days_old: u64) -> PathBuf {
        let folder = layout.user("Library/Caches").join(relative);
        fs::create_dir_all(&folder).unwrap();
        let file = folder.join("data");
        fs::write(&file, vec![0_u8; 4096]).unwrap();
        let then = SystemTime::now() - DAY * days_old as u32;
        fs::File::open(&file).unwrap().set_modified(then).unwrap();
        fs::File::open(&folder).unwrap().set_modified(then).unwrap();
        folder
    }

    fn running(host: &FakeHost, program: &str) {
        host.reply("/bin/ps", &format!("  501 1 {program}\n"));
    }

    fn plan_for(layout: &Layout, host: &FakeHost, named: &[PathBuf]) -> CachePlan {
        plan(layout, host, named, SystemTime::now())
    }

    fn group<'a>(plan: &'a CachePlan, tool: &str) -> &'a [CacheRow] {
        plan.groups
            .iter()
            .find(|group| group.tool == tool)
            .map_or(&[][..], |group| &group.rows)
    }

    fn row<'a>(rows: &'a [CacheRow], path: &PathBuf) -> &'a CacheRow {
        rows.iter()
            .find(|row| &row.path == path)
            .unwrap_or_else(|| panic!("no row for {}: {rows:?}", path.display()))
    }

    #[test]
    fn only_a_stale_cache_of_a_closed_app_starts_selected() {
        assert!(starts_selected(true, false));
        assert!(!starts_selected(false, false));
        assert!(!starts_selected(true, true));
        assert!(!starts_selected(false, true));
    }

    #[test]
    fn an_old_cache_of_an_unknown_app_starts_selected() {
        let (_temp, layout) = sandbox();
        let old = cache(&layout, "com.acme.old", 40);
        let plan = plan_for(&layout, &FakeHost::default(), &[]);
        assert!(row(group(&plan, APP_CACHES), &old).selected);
    }

    #[test]
    fn a_cache_changed_in_the_last_30_days_starts_unselected() {
        let (_temp, layout) = sandbox();
        let recent = cache(&layout, "com.acme.recent", 5);
        let plan = plan_for(&layout, &FakeHost::default(), &[]);
        assert!(!row(group(&plan, APP_CACHES), &recent).selected);
    }

    #[test]
    fn a_new_file_deep_inside_makes_the_cache_recent() {
        let (_temp, layout) = sandbox();
        let folder = cache(&layout, "com.acme.deep", 60);
        let nested = folder.join("a/b");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("fresh"), b"x").unwrap();
        let old = SystemTime::now() - DAY * 60;
        for dir in [folder.join("a/b"), folder.join("a"), folder.clone()] {
            fs::File::open(dir).unwrap().set_modified(old).unwrap();
        }
        let plan = plan_for(&layout, &FakeHost::default(), &[]);
        assert!(!row(group(&plan, APP_CACHES), &folder).selected);
    }

    #[test]
    fn the_cache_of_a_running_app_starts_unselected() {
        let (_temp, layout) = sandbox();
        let bundle = layout.system("Applications/Acme.app");
        fake_app(&bundle, "com.acme.app", "Acme");
        let old = cache(&layout, "com.acme.app", 90);
        let host = FakeHost::default();
        running(&host, &format!("{}/Contents/MacOS/Acme", bundle.display()));
        let plan = plan_for(&layout, &host, &[]);
        assert!(!row(group(&plan, APP_CACHES), &old).selected);
    }

    #[test]
    fn the_cache_of_an_app_that_is_closed_starts_selected() {
        let (_temp, layout) = sandbox();
        let bundle = layout.system("Applications/Acme.app");
        fake_app(&bundle, "com.acme.app", "Acme");
        let old = cache(&layout, "com.acme.app", 90);
        let host = FakeHost::default();
        running(&host, "/usr/sbin/cfprefsd");
        let plan = plan_for(&layout, &host, &[]);
        assert!(row(group(&plan, APP_CACHES), &old).selected);
    }

    #[test]
    fn a_folder_a_named_target_covers_is_no_app_cache() {
        let (_temp, layout) = sandbox();
        let covered = cache(&layout, "Homebrew", 90);
        let other = cache(&layout, "com.acme.other", 90);
        let plan = plan_for(
            &layout,
            &FakeHost::default(),
            std::slice::from_ref(&covered),
        );
        let rows = group(&plan, APP_CACHES);
        assert!(rows.iter().all(|row| row.path != covered), "{rows:?}");
        row(rows, &other);
    }

    #[test]
    fn the_parent_of_a_named_target_is_no_app_cache() {
        let (_temp, layout) = sandbox();
        let inner = cache(&layout, "Vendor/Tool", 90);
        let parent = inner.parent().unwrap().to_path_buf();
        let plan = plan_for(&layout, &FakeHost::default(), &[inner]);
        let rows = group(&plan, APP_CACHES);
        assert!(rows.iter().all(|row| row.path != parent), "{rows:?}");
    }

    #[test]
    fn apple_folders_and_sync_state_are_left_out() {
        let (_temp, layout) = sandbox();
        for name in [
            "com.apple.Maps",
            "COM.APPLE.Mail",
            "CloudKit",
            "FamilyCircle",
        ] {
            cache(&layout, name, 90);
        }
        let kept = cache(&layout, "com.acme.kept", 90);
        let plan = plan_for(&layout, &FakeHost::default(), &[]);
        let paths: Vec<_> = group(&plan, APP_CACHES).iter().map(|r| &r.path).collect();
        assert_eq!(paths, [&kept]);
    }

    #[test]
    fn a_browser_folder_is_no_app_cache() {
        let (_temp, layout) = sandbox();
        for name in [
            "Google/Chrome",
            "Microsoft Edge",
            "Firefox",
            "BraveSoftware/Brave-Browser",
            "company.thebrowser.Browser",
        ] {
            cache(&layout, name, 90);
        }
        let plan = plan_for(&layout, &FakeHost::default(), &[]);
        assert!(group(&plan, APP_CACHES).is_empty());
    }

    #[test]
    fn a_closed_browser_cache_starts_selected_even_when_recent() {
        let (_temp, layout) = sandbox();
        let chrome = cache(&layout, "Google/Chrome", 0);
        let plan = plan_for(&layout, &FakeHost::default(), &[]);
        assert!(row(group(&plan, BROWSER_CACHES), &chrome).selected);
    }

    #[test]
    fn a_running_browser_cache_starts_unselected() {
        let (_temp, layout) = sandbox();
        let chrome = cache(&layout, "Google/Chrome", 90);
        let firefox = cache(&layout, "Firefox", 90);
        let host = FakeHost::default();
        running(
            &host,
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        );
        let plan = plan_for(&layout, &host, &[]);
        let rows = group(&plan, BROWSER_CACHES);
        assert!(!row(rows, &chrome).selected);
        assert!(row(rows, &firefox).selected);
    }

    #[test]
    fn safari_has_a_row_for_each_of_its_two_folders() {
        let (_temp, layout) = sandbox();
        let outer = cache(&layout, "com.apple.Safari", 90);
        let container = layout.user("Library/Containers/com.apple.Safari/Data/Library/Caches");
        fs::create_dir_all(&container).unwrap();
        fs::write(container.join("data"), vec![0_u8; 4096]).unwrap();
        let host = FakeHost::default();
        running(
            &host,
            "/System/Volumes/Preboot/Cryptexes/App/System/Applications/Safari.app/Contents/MacOS/Safari",
        );
        let plan = plan_for(&layout, &host, &[]);
        let rows = group(&plan, BROWSER_CACHES);
        assert!(!row(rows, &outer).selected);
        assert!(!row(rows, &container).selected);
    }

    #[test]
    fn browser_profiles_are_never_rows() {
        let (_temp, layout) = sandbox();
        let profile = layout.user("Library/Application Support/Google/Chrome/Default");
        fs::create_dir_all(&profile).unwrap();
        fs::write(profile.join("Cookies"), vec![0_u8; 4096]).unwrap();
        cache(&layout, "Google/Chrome", 90);
        let plan = plan_for(&layout, &FakeHost::default(), &[]);
        let all = plan.groups.iter().flat_map(|g| &g.rows);
        assert!(all.into_iter().all(|row| !row.path.starts_with(&profile)));
    }

    #[test]
    fn a_refused_read_is_told_in_a_note() {
        let (_temp, layout) = sandbox();
        let container = layout.user("Library/Containers/com.apple.Safari/Data/Library/Caches");
        fs::create_dir_all(&container).unwrap();
        fs::set_permissions(&container, fs::Permissions::from_mode(0o000)).unwrap();
        let plan = plan_for(&layout, &FakeHost::default(), &[]);
        fs::set_permissions(&container, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            plan.notes
                .iter()
                .any(|note| note.contains("Full Disk Access")),
            "{:?}",
            plan.notes
        );
        assert!(group(&plan, BROWSER_CACHES).is_empty());
    }
}

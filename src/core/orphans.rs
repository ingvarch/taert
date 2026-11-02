//! Scans for leftover files and containers belonging to apps that are no longer installed.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, SystemTime};

use crate::core::apps;
use crate::core::host::Host;
use crate::core::layout::Layout;
use crate::core::leftovers::{self, Evidence, UninstallRole};
use crate::core::path_utils::prettify_path;
use crate::core::scanner::{self, CategoryScan, ScanProgressEvent, ToolGroup};
use crate::core::stores::IgnoreStore;

pub const SCAN_ID: &str = "orphans";

const THIRTY_DAYS: Duration = Duration::from_secs(30 * 24 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateKind {
    Exact,
    Shared,
}

struct Candidate {
    id: String,
    path: PathBuf,
    kind: CandidateKind,
}

fn is_permission_denied(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::PermissionDenied
        || matches!(err.raw_os_error(), Some(libc::EACCES | libc::EPERM))
}

fn cut_extension(name: &str) -> &str {
    for suffix in [".plist", ".binarycookies", ".savedState"] {
        if name.len() >= suffix.len()
            && name[name.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
        {
            return &name[..name.len() - suffix.len()];
        }
    }
    name
}

fn cut_group_container_name(name: &str) -> &str {
    if let Some(rest) = name.strip_prefix("group.") {
        return rest;
    }
    let bytes = name.as_bytes();
    if bytes.len() > 11 && bytes[10] == b'.' {
        let team_id = &bytes[0..10];
        if team_id
            .iter()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        {
            return &name[11..];
        }
    }
    name
}

fn read_container_identifier(container: &Path) -> Option<String> {
    let p1 = container.join(".com.apple.containermanagerd.metadata.plist");
    let p2 = container.join(".com.apple.containermanager.metadata.plist");
    let path = if p1.exists() {
        p1
    } else if p2.exists() {
        p2
    } else {
        return None;
    };
    let val = plist::Value::from_file(path).ok()?;
    val.as_dictionary()?
        .get("MCMMetadataIdentifier")?
        .as_string()
        .map(str::to_string)
}

fn newest_modified(path: &Path, now: SystemTime) -> Option<SystemTime> {
    let mut newest = fs::symlink_metadata(path).ok()?.modified().ok()?;
    if newest > now {
        newest = now;
    }
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            if let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) {
                let effective = if mtime > now { now } else { mtime };
                if effective > newest {
                    newest = effective;
                }
            }
        }
    }
    Some(newest)
}

fn is_old(path: &Path, now: SystemTime) -> bool {
    newest_modified(path, now).is_some_and(|mtime| {
        now.duration_since(mtime)
            .is_ok_and(|age| age >= THIRTY_DAYS)
    })
}

pub fn scan(
    layout: &Layout,
    host: &dyn Host,
    ignore: &IgnoreStore,
    now: SystemTime,
    progress: Option<Sender<ScanProgressEvent>>,
    cancel: Option<Arc<AtomicBool>>,
) -> CategoryScan {
    let mut scan = CategoryScan {
        id: SCAN_ID.to_string(),
        title: "Orphaned App Files".to_string(),
        groups: Vec::new(),
        notes: Vec::new(),
    };

    let cancelled = || cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed));
    if cancelled() {
        return scan;
    }

    let folders = [
        ("Application Scripts", false, false),
        ("Application Support", false, false),
        ("Caches", false, false),
        ("Containers", true, false),
        ("Cookies", false, false),
        ("Group Containers", false, true),
        ("HTTPStorages", false, false),
        ("Logs", false, false),
        ("Preferences", false, false),
        ("Saved Application State", false, false),
        ("WebKit", false, false),
    ];

    let mut raw_candidates: Vec<Candidate> = Vec::new();
    let mut permission_denied_notes: Vec<String> = Vec::new();

    for (folder_name, is_containers, is_group_containers) in folders {
        if cancelled() {
            return scan;
        }

        let folder_path = layout.user(&format!("Library/{folder_name}"));
        if let Some(ref tx) = progress {
            let _ = tx.send(ScanProgressEvent::ScanningPath(prettify_path(&folder_path)));
        }

        let entries = match fs::read_dir(&folder_path) {
            Ok(entries) => entries,
            Err(e) => {
                if (is_containers || is_group_containers) && is_permission_denied(&e) {
                    permission_denied_notes.push(format!(
                        "Could not read ~/Library/{folder_name}. Give your terminal Full Disk Access in System Settings > Privacy & Security to include it."
                    ));
                }
                continue;
            }
        };

        for entry in entries.flatten() {
            let entry_path = entry.path();

            if entry.file_type().is_ok_and(|ft| ft.is_symlink())
                || fs::symlink_metadata(&entry_path).is_ok_and(|m| m.file_type().is_symlink())
            {
                continue;
            }

            let file_name = entry.file_name();
            let name_str = file_name.to_string_lossy();

            let (id, kind) = if is_containers {
                let Some(id) = read_container_identifier(&entry_path) else {
                    continue;
                };
                (id, CandidateKind::Exact)
            } else if is_group_containers {
                let id = cut_group_container_name(&name_str).to_string();
                (id, CandidateKind::Shared)
            } else {
                let id = cut_extension(&name_str).to_string();
                (id, CandidateKind::Exact)
            };

            if !apps::is_reverse_dns(&id) {
                continue;
            }

            if id.to_ascii_lowercase().starts_with("com.apple.") {
                continue;
            }

            raw_candidates.push(Candidate {
                id,
                path: entry_path,
                kind,
            });
        }
    }

    if cancelled() {
        return scan;
    }

    // Step 4.2: Collect installed IDs
    let mut installed_ids: Vec<String> = Vec::new();
    let all_apps = apps::find_apps(layout)
        .into_iter()
        .chain(apps::find_copies(layout));
    for app in all_apps {
        if let Some(ref bid) = app.bundle_id {
            installed_ids.push(bid.clone());
        }
        let (embedded, _) = apps::embedded_bundle_ids(&app.path, app.bundle_id.as_deref());
        installed_ids.extend(embedded);
    }

    let is_installed = |candidate_id: &str| -> bool {
        let c_lower = candidate_id.to_ascii_lowercase();
        let c_prefix = format!("{c_lower}.");
        installed_ids.iter().any(|inst| {
            let inst_lower = inst.to_ascii_lowercase();
            leftovers::has_id_boundary(candidate_id, inst) || inst_lower.starts_with(&c_prefix)
        })
    };

    // Step 4.2 and 4.3: Filter candidates
    let mut surviving_candidates: Vec<Candidate> = Vec::new();
    for candidate in raw_candidates {
        if is_installed(&candidate.id) {
            continue;
        }

        if candidate.kind == CandidateKind::Shared {
            let c_vendor = leftovers::vendor_token(&candidate.id);
            if c_vendor.is_some()
                && installed_ids
                    .iter()
                    .any(|inst| leftovers::vendor_token(inst) == c_vendor)
            {
                continue;
            }
        }

        surviving_candidates.push(candidate);
    }

    if cancelled() {
        return scan;
    }

    // Step 4.4: Spotlight check per distinct surviving ID
    let mut distinct_ids: Vec<String> = surviving_candidates.iter().map(|c| c.id.clone()).collect();
    distinct_ids.sort();
    distinct_ids.dedup();

    let mut spotlight_dropped: HashMap<String, bool> = HashMap::new();
    let mut spotlight_failed: HashMap<String, bool> = HashMap::new();

    for id in distinct_ids {
        if cancelled() {
            return scan;
        }
        let out = host.run(
            "mdfind",
            &[&format!("kMDItemCFBundleIdentifier == '{id}'")],
            Duration::from_secs(5),
        );
        if out.ok {
            if !out.stdout.trim().is_empty() {
                spotlight_dropped.insert(id, true);
            } else {
                spotlight_dropped.insert(id, false);
            }
        } else {
            spotlight_dropped.insert(id.clone(), false);
            spotlight_failed.insert(id, true);
        }
    }

    surviving_candidates.retain(|c| !spotlight_dropped.get(&c.id).copied().unwrap_or(false));

    // Step 5 & 6: Build rows
    let mut groups_by_id: HashMap<String, Vec<scanner::ScanItem>> = HashMap::new();

    for candidate in surviving_candidates {
        if cancelled() {
            return scan;
        }

        let failed = spotlight_failed
            .get(&candidate.id)
            .copied()
            .unwrap_or(false);
        let selected =
            candidate.kind == CandidateKind::Exact && !failed && is_old(&candidate.path, now);

        if let Some(mut item) = scanner::scan_folder(candidate.path, selected, ignore, &progress) {
            let evidence = match candidate.kind {
                CandidateKind::Exact => Evidence::BundleId,
                CandidateKind::Shared => Evidence::Name,
            };
            item.role = Some(UninstallRole::Leftover { evidence });
            groups_by_id.entry(candidate.id).or_default().push(item);
        }
    }

    let mut groups: Vec<ToolGroup> = groups_by_id
        .into_iter()
        .map(|(tool, mut items)| {
            items.sort_by(|a, b| a.path.cmp(&b.path));
            ToolGroup { tool, items }
        })
        .collect();

    // Groups sort by total bytes, largest first; tie break on tool name
    groups.sort_by(|a, b| {
        b.total_all_bytes()
            .cmp(&a.total_all_bytes())
            .then_with(|| a.tool.cmp(&b.tool))
    });

    // Notes:
    // (a) permission denied on Containers or Group Containers
    scan.notes.extend(permission_denied_notes);

    // (b) if any listed row is under Containers or Group Containers
    let containers_dir = layout.user("Library/Containers");
    let group_containers_dir = layout.user("Library/Group Containers");
    let has_containers_row = groups.iter().any(|g| {
        g.items.iter().any(|item| {
            item.path.starts_with(&containers_dir) || item.path.starts_with(&group_containers_dir)
        })
    });

    if has_containers_row {
        scan.notes.push(
            "Removing items in Containers needs Full Disk Access for your terminal.".to_string(),
        );
    }

    // (c) always
    scan.notes.push(
        "Apps on a drive that is not connected look removed. Rows changed in the last 30 days start unselected."
            .to_string(),
    );

    scan.groups = groups;
    scan
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::apps::testing::{fake_app, sandbox, write_info};
    use crate::core::host::fake::FakeHost;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::time::Duration;

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    fn old_file(path: &Path, then: SystemTime) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, vec![0_u8; 4096]).unwrap();
        fs::File::open(path).unwrap().set_modified(then).unwrap();
    }

    fn old_folder(folder: &Path, then: SystemTime) {
        fs::create_dir_all(folder).unwrap();
        let child = folder.join("data");
        fs::write(&child, vec![0_u8; 4096]).unwrap();
        fs::File::open(&child).unwrap().set_modified(then).unwrap();
        fs::File::open(folder).unwrap().set_modified(then).unwrap();
    }

    fn write_container_metadata(container: &Path, id: &str) {
        fs::create_dir_all(container).unwrap();
        let mut d = plist::Dictionary::new();
        d.insert("MCMMetadataIdentifier".to_string(), id.into());
        let meta = container.join(".com.apple.containermanagerd.metadata.plist");
        plist::to_file_xml(&meta, &plist::Value::Dictionary(d.clone())).unwrap();
        let meta2 = container.join(".com.apple.containermanager.metadata.plist");
        plist::to_file_xml(&meta2, &plist::Value::Dictionary(d)).unwrap();
    }

    #[test]
    fn files_of_an_installed_app_are_not_listed() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        host.reply("mdfind", "");
        let app = layout.system("Applications/Foo.app");
        fake_app(&app, "com.acme.foo", "Foo");

        let now = SystemTime::now();
        let then = now - 40 * DAY;

        old_folder(&layout.user("Library/Caches/com.acme.foo"), then);
        old_file(&layout.user("Library/Preferences/com.acme.foo.plist"), then);
        old_folder(
            &layout.user("Library/Saved Application State/com.acme.foo.savedState"),
            then,
        );

        let res = scan(&layout, &host, &IgnoreStore::default(), now, None, None);
        assert!(
            res.groups.is_empty(),
            "expected no rows, got {:?}",
            res.groups
        );
    }

    #[test]
    fn a_helper_of_an_installed_app_is_not_listed() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        host.reply("mdfind", "");
        let app = layout.system("Applications/Foo.app");
        fake_app(&app, "com.acme.foo", "Foo");

        let helper = app.join("Contents/Library/LoginItems/Helper.app");
        let helper_exe = helper.join("Contents/MacOS/Helper");
        fs::create_dir_all(helper_exe.parent().unwrap()).unwrap();
        fs::write(&helper_exe, vec![0_u8; 4096]).unwrap();
        write_info(
            &helper,
            &[
                ("CFBundleIdentifier", "com.acme.foo.helper".into()),
                ("CFBundleExecutable", "Helper".into()),
            ],
        );

        let now = SystemTime::now();
        let then = now - 40 * DAY;
        old_folder(
            &layout.user("Library/Application Support/com.acme.foo.helper"),
            then,
        );

        let res = scan(&layout, &host, &IgnoreStore::default(), now, None, None);
        assert!(
            res.groups.is_empty(),
            "expected no rows, got {:?}",
            res.groups
        );
    }

    #[test]
    fn a_prefix_is_not_the_same_id() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        host.reply("mdfind", "");
        let app = layout.system("Applications/Foobar.app");
        fake_app(&app, "com.acme.foobar", "Foobar");

        let now = SystemTime::now();
        let then = now - 40 * DAY;
        let cache = layout.user("Library/Caches/com.acme.foo");
        old_folder(&cache, then);

        let res = scan(&layout, &host, &IgnoreStore::default(), now, None, None);
        assert_eq!(res.groups.len(), 1);
        assert_eq!(res.groups[0].tool, "com.acme.foo");

        // And the other way: app com.acme.foo.beta installed, Caches/com.acme.foo exists: not listed
        let (_temp2, layout2) = sandbox();
        let host2 = FakeHost::default();
        host2.reply("mdfind", "");
        let beta = layout2.system("Applications/Beta.app");
        fake_app(&beta, "com.acme.foo.beta", "Beta");
        let cache2 = layout2.user("Library/Caches/com.acme.foo");
        old_folder(&cache2, then);

        let res2 = scan(&layout2, &host2, &IgnoreStore::default(), now, None, None);
        assert!(
            res2.groups.is_empty(),
            "expected no rows, got {:?}",
            res2.groups
        );
    }

    #[test]
    fn com_apple_is_never_listed() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        host.reply("mdfind", "");

        let now = SystemTime::now();
        let then = now - 40 * DAY;
        old_folder(&layout.user("Library/Caches/com.apple.Safari"), then);
        old_file(
            &layout.user("Library/Preferences/com.apple.finder.plist"),
            then,
        );

        let res = scan(&layout, &host, &IgnoreStore::default(), now, None, None);
        assert!(res.groups.is_empty());
        assert!(!host.ran("mdfind"));
    }

    #[test]
    fn a_free_form_name_is_never_listed() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        host.reply("mdfind", "");

        let now = SystemTime::now();
        let then = now - 40 * DAY;
        old_folder(&layout.user("Library/Application Support/Foo Editor"), then);

        let res = scan(&layout, &host, &IgnoreStore::default(), now, None, None);
        assert!(res.groups.is_empty());
    }

    #[test]
    fn a_container_is_identified_from_its_metadata_plist() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        host.reply("mdfind", "");

        let app = layout.system("Applications/Foo.app");
        fake_app(&app, "com.acme.foo", "Foo");

        let now = SystemTime::now();
        let then = now - 40 * DAY;

        // (a) Containers/ABCD-1234 with metadata MCMMetadataIdentifier = com.gone.app: row for com.gone.app
        let cont_a = layout.user("Library/Containers/ABCD-1234");
        old_folder(&cont_a, then);
        write_container_metadata(&cont_a, "com.gone.app");

        // (b) Containers/com.gone.other whose metadata says com.acme.foo with com.acme.foo installed: no row
        let cont_b = layout.user("Library/Containers/com.gone.other");
        old_folder(&cont_b, then);
        write_container_metadata(&cont_b, "com.acme.foo");

        // (c) Containers/com.gone.third with no metadata file: no row
        let cont_c = layout.user("Library/Containers/com.gone.third");
        old_folder(&cont_c, then);

        let res = scan(&layout, &host, &IgnoreStore::default(), now, None, None);
        assert_eq!(res.groups.len(), 1);
        assert_eq!(res.groups[0].tool, "com.gone.app");
        assert_eq!(res.groups[0].items.len(), 1);
        assert_eq!(res.groups[0].items[0].path, cont_a);
    }

    #[test]
    fn a_spotlight_hit_removes_the_row() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        host.reply(
            "mdfind kMDItemCFBundleIdentifier == 'com.gone.app'",
            "/Volumes/Ext/Gone.app\n",
        );
        host.reply("mdfind", "");

        let now = SystemTime::now();
        let then = now - 40 * DAY;
        old_folder(&layout.user("Library/Caches/com.gone.app"), then);
        old_folder(&layout.user("Library/Caches/com.other.gone"), then);

        let res = scan(&layout, &host, &IgnoreStore::default(), now, None, None);
        assert_eq!(res.groups.len(), 1);
        assert_eq!(res.groups[0].tool, "com.other.gone");
    }

    #[test]
    fn a_failed_mdfind_keeps_the_row_unselected() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        host.fail("mdfind", "");

        let now = SystemTime::now();
        let then = now - 40 * DAY;
        old_folder(&layout.user("Library/Caches/com.gone.app"), then);

        let res = scan(&layout, &host, &IgnoreStore::default(), now, None, None);
        assert_eq!(res.groups.len(), 1);
        assert_eq!(res.groups[0].tool, "com.gone.app");
        assert!(!res.groups[0].items[0].selected);
    }

    #[test]
    fn an_old_exact_id_starts_selected() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        host.reply("mdfind", "");

        let now = SystemTime::now();
        let then = now - 40 * DAY;
        old_folder(&layout.user("Library/Caches/com.gone.app"), then);

        let res = scan(&layout, &host, &IgnoreStore::default(), now, None, None);
        assert_eq!(res.groups.len(), 1);
        assert!(res.groups[0].items[0].selected);
        assert_eq!(
            res.groups[0].items[0].role,
            Some(UninstallRole::Leftover {
                evidence: Evidence::BundleId
            })
        );
    }

    #[test]
    fn a_recent_exact_id_starts_unselected() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        host.reply("mdfind", "");

        let now = SystemTime::now();
        let then = now - 5 * DAY;
        old_folder(&layout.user("Library/Caches/com.gone.app"), then);

        let res = scan(&layout, &host, &IgnoreStore::default(), now, None, None);
        assert_eq!(res.groups.len(), 1);
        assert!(!res.groups[0].items[0].selected);
    }

    #[test]
    fn a_folder_with_a_recent_child_starts_unselected() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        host.reply("mdfind", "");

        let now = SystemTime::now();
        let folder = layout.user("Library/Caches/com.gone.app");
        fs::create_dir_all(&folder).unwrap();
        let child = folder.join("recent.data");
        fs::write(&child, vec![0_u8; 4096]).unwrap();
        fs::File::open(&child)
            .unwrap()
            .set_modified(now - 5 * DAY)
            .unwrap();
        fs::File::open(&folder)
            .unwrap()
            .set_modified(now - 40 * DAY)
            .unwrap();

        let res = scan(&layout, &host, &IgnoreStore::default(), now, None, None);
        assert_eq!(res.groups.len(), 1);
        assert!(!res.groups[0].items[0].selected);
    }

    #[test]
    fn a_group_container_is_listed_unselected() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        host.reply("mdfind", "");

        let now = SystemTime::now();
        let then = now - 40 * DAY;
        let gc = layout.user("Library/Group Containers/ABCDE12345.com.gone.shared");
        old_folder(&gc, then);

        let res = scan(&layout, &host, &IgnoreStore::default(), now, None, None);
        assert_eq!(res.groups.len(), 1);
        assert_eq!(res.groups[0].tool, "com.gone.shared");
        let item = &res.groups[0].items[0];
        assert!(!item.selected);
        assert_eq!(
            item.role,
            Some(UninstallRole::Leftover {
                evidence: Evidence::Name
            })
        );
    }

    #[test]
    fn a_group_container_of_an_installed_vendor_is_dropped() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        host.reply("mdfind", "");

        let app = layout.system("Applications/Other.app");
        fake_app(&app, "com.gone.other", "Other");

        let now = SystemTime::now();
        let then = now - 40 * DAY;
        let gc = layout.user("Library/Group Containers/ABCDE12345.com.gone.shared");
        old_folder(&gc, then);

        let res = scan(&layout, &host, &IgnoreStore::default(), now, None, None);
        assert!(
            res.groups.is_empty(),
            "expected group container to be dropped"
        );
    }

    #[test]
    fn group_prefix_is_removed() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        host.reply("mdfind", "");

        let now = SystemTime::now();
        let then = now - 40 * DAY;
        let gc = layout.user("Library/Group Containers/group.com.gone.app");
        old_folder(&gc, then);

        let res = scan(&layout, &host, &IgnoreStore::default(), now, None, None);
        assert_eq!(res.groups.len(), 1);
        assert_eq!(res.groups[0].tool, "com.gone.app");
    }

    #[test]
    fn a_path_on_the_ignore_list_is_left_out() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        host.reply("mdfind", "");

        let now = SystemTime::now();
        let then = now - 40 * DAY;
        let cache = layout.user("Library/Caches/com.gone.app");
        old_folder(&cache, then);

        let mut ignore = IgnoreStore::default();
        ignore.add(cache.to_string_lossy().to_string());

        let res = scan(&layout, &host, &ignore, now, None, None);
        assert!(res.groups.is_empty());
    }

    #[test]
    fn rows_group_by_missing_app() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        host.reply("mdfind", "");

        let now = SystemTime::now();
        let then = now - 40 * DAY;
        old_folder(&layout.user("Library/Caches/com.gone.app"), then);
        old_file(&layout.user("Library/Preferences/com.gone.app.plist"), then);
        old_folder(&layout.user("Library/Caches/com.other.gone"), then);

        let res = scan(&layout, &host, &IgnoreStore::default(), now, None, None);
        assert_eq!(res.groups.len(), 2);
        let g1 = res
            .groups
            .iter()
            .find(|g| g.tool == "com.gone.app")
            .unwrap();
        assert_eq!(g1.items.len(), 2);
        let g2 = res
            .groups
            .iter()
            .find(|g| g.tool == "com.other.gone")
            .unwrap();
        assert_eq!(g2.items.len(), 1);
    }

    #[test]
    fn a_symlink_entry_is_skipped() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        host.reply("mdfind", "");

        let now = SystemTime::now();
        let then = now - 40 * DAY;
        let real_folder = layout.home.join("real_folder");
        old_folder(&real_folder, then);
        let cache_symlink = layout.user("Library/Caches/com.gone.app");
        fs::create_dir_all(cache_symlink.parent().unwrap()).unwrap();
        symlink(&real_folder, &cache_symlink).unwrap();

        let res = scan(&layout, &host, &IgnoreStore::default(), now, None, None);
        assert!(res.groups.is_empty());
    }

    #[test]
    fn notes_explain_full_disk_access_and_unplugged_drives() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        host.reply("mdfind", "");

        let now = SystemTime::now();
        let then = now - 40 * DAY;
        old_folder(&layout.user("Library/Caches/com.gone.app"), then);

        let res = scan(&layout, &host, &IgnoreStore::default(), now, None, None);
        assert_eq!(res.notes.len(), 1);
        assert_eq!(
            res.notes[0],
            "Apps on a drive that is not connected look removed. Rows changed in the last 30 days start unselected."
        );

        // With Containers row:
        let cont = layout.user("Library/Containers/ABCD-1234");
        old_folder(&cont, then);
        write_container_metadata(&cont, "com.gone.other");

        let res2 = scan(&layout, &host, &IgnoreStore::default(), now, None, None);
        assert_eq!(res2.notes.len(), 2);
        assert_eq!(
            res2.notes[0],
            "Removing items in Containers needs Full Disk Access for your terminal."
        );
        assert_eq!(
            res2.notes[1],
            "Apps on a drive that is not connected look removed. Rows changed in the last 30 days start unselected."
        );
    }
}

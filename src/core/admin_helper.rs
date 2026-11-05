//! The root side of the admin helper: `taert admin-helper compress` runs here as root.

use std::fs;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::sync::atomic::AtomicBool;

use super::compress::{self, CompressOptions, CompressReport, Compressor};
use super::host::{Host, MacHost};
use super::layout::Layout;
use super::safety::Safety;
use super::slim;
use super::stores::IgnoreStore;

pub const NOT_ROOT: &str = "admin-helper must run as root";
const NOT_APP: &str = "Not an app bundle in an app folder";
const UNSAFE_FOLDER: &str = "A folder above the app can be changed by others";
const OPEN_FOLDER: &str = "A folder in the app can be changed by others";
const ACL_FOLDER: &str =
    "a folder in the app has an access control list, so taert will not work in it as root";
const MOVED: &str = "The app changed while taert worked on it, so taert stopped";
const LINKED_BUNDLE: &str = "The app is a link. Compress the app it points to";
const IGNORE_UNREADABLE: &str = "Could not read the ignore list";
const BAD_HOME: &str = "--home must be the full path of a folder that is not a link";

/// The device and inode the bundle had when it was checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BundleId {
    dev: u64,
    ino: u64,
}

impl BundleId {
    fn of(bundle: &Path) -> Result<Self, String> {
        let meta = fs::symlink_metadata(bundle)
            .map_err(|e| format!("Could not read {}: {e}", bundle.display()))?;
        if meta.is_symlink() {
            return Err(LINKED_BUNDLE.to_string());
        }
        Ok(Self {
            dev: meta.dev(),
            ino: meta.ino(),
        })
    }

    /// Errors when the path is now a link or another folder than the one that was checked.
    pub fn verify(&self, bundle: &Path) -> Result<(), String> {
        match Self::of(bundle) {
            Ok(now) if now == *self => Ok(()),
            _ => Err(MOVED.to_string()),
        }
    }
}

/// Whether a folder with this mode and group lets others, or a group besides wheel, write in it.
fn opens_folder(mode: u32, gid: u32) -> bool {
    mode & 0o002 != 0 || (mode & 0o020 != 0 && gid != 0)
}

fn check_folders(bundle: &Path) -> Result<(), String> {
    let entries = slim::walk_entries(bundle)
        .map_err(|e| format!("Could not read {}: {e}", bundle.display()))?;
    for (folder, meta) in entries.iter().filter(|(_, meta)| meta.is_dir()) {
        if opens_folder(meta.mode(), meta.gid()) {
            return Err(OPEN_FOLDER.to_string());
        }
        let has_acl = compress::has_acl(folder)
            .map_err(|e| format!("Could not read {}: {e}", folder.display()))?;
        if has_acl {
            return Err(ACL_FOLDER.to_string());
        }
    }
    Ok(())
}

/// Reads the ignore list in the home of the user as root. The home must be a full path to a
/// folder that is not a link.
pub fn read_ignore(layout: &Layout) -> Result<IgnoreStore, String> {
    let home = &layout.home;
    match fs::symlink_metadata(home) {
        Ok(meta) if home.is_absolute() && meta.is_dir() => {
            read_ignore_file(&layout.ignore_path(), meta.uid())
        }
        _ => Err(BAD_HOME.to_string()),
    }
}

/// Reads the ignore file only when it is a regular file of `owner`, not a link. Only a missing
/// file means no list: root must not work without the list the user made. No error quotes
/// the file, which root may read where the user cannot.
fn read_ignore_file(path: &Path, owner: u32) -> Result<IgnoreStore, String> {
    fn unreadable<E>(_: E) -> String {
        IGNORE_UNREADABLE.to_string()
    }
    // O_NONBLOCK keeps a pipe in place of the file from blocking the open.
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
            return Ok(IgnoreStore::default());
        }
        Err(error) => return Err(unreadable(error)),
    };
    let meta = file.metadata().map_err(unreadable)?;
    if !meta.is_file() || meta.uid() != owner {
        return Err(unreadable(()));
    }
    let mut content = String::new();
    (&file).read_to_string(&mut content).map_err(unreadable)?;
    toml::from_str(&content).map_err(unreadable)
}

/// Checks the bundle again, as root, before anything is written. `system_owner` is the uid
/// that must own every entry: 0 in production.
pub fn helper_check(
    bundle: &Path,
    layout: &Layout,
    ignore: &IgnoreStore,
    euid: u32,
    system_owner: u32,
    host: &dyn Host,
) -> Result<BundleId, String> {
    if euid != 0 {
        return Err(NOT_ROOT.to_string());
    }
    let id = BundleId::of(bundle)?;
    if bundle.canonicalize().ok().as_deref() != Some(bundle)
        || !Safety::is_app_bundle(bundle, layout)
    {
        return Err(NOT_APP.to_string());
    }
    if !Safety::is_safe_as_root(bundle, layout) {
        return Err(UNSAFE_FOLDER.to_string());
    }
    slim::check_ownership(bundle, system_owner)?;
    check_folders(bundle)?;
    id.verify(bundle)?;
    Safety::can_compress_with_host(bundle, ignore, layout, host, system_owner)
        .map_err(|e| e.to_string())?;
    Ok(id)
}

/// Compresses or unpacks one checked bundle. It stops when the bundle is no longer the folder
/// that was checked.
pub fn compress_bundle(
    layout: &Layout,
    bundle: &Path,
    undo: bool,
    host: &dyn Host,
    euid: u32,
    system_owner: u32,
) -> Result<CompressReport, String> {
    if euid != 0 {
        return Err(NOT_ROOT.to_string());
    }
    let ignore = read_ignore(layout)?;
    let id = helper_check(bundle, layout, &ignore, euid, system_owner, host)?;
    compress_checked(layout, &ignore, bundle, id, system_owner, undo, host)
}

/// Compresses or unpacks the files of `owner` in a bundle that `helper_check` passed, as long
/// as it is still the folder with `id`.
fn compress_checked(
    layout: &Layout,
    ignore: &IgnoreStore,
    bundle: &Path,
    id: BundleId,
    owner: u32,
    undo: bool,
    host: &dyn Host,
) -> Result<CompressReport, String> {
    let compressor = Compressor {
        host,
        layout,
        ignore,
        system_owner: owner,
    };
    let options = CompressOptions {
        dry_run: false,
        undo,
    };
    let mut moved = None;
    let report = compressor
        .process_path_guarded(
            bundle,
            &options,
            owner,
            &AtomicBool::new(false),
            |_| id.verify(bundle).inspect_err(|e| moved = Some(e.clone())),
            |_, _| {},
        )
        .map_err(|e| e.to_string())?;
    moved.map_or(Ok(report), Err)
}

/// What `taert admin-helper compress` prints: the report, or `{"error": ...}`.
pub fn run_compress(home: &Path, bundle: &Path, undo: bool) -> String {
    let layout = Layout {
        home: home.to_path_buf(),
        root: "/".into(),
        user_cache: None,
    };
    let euid = unsafe { libc::geteuid() };
    let result = compress_bundle(&layout, bundle, undo, &MacHost, euid, 0)
        .and_then(|report| serde_json::to_string(&report).map_err(|e| e.to_string()));
    result.unwrap_or_else(|error| error_answer(&error))
}

/// The `{"error": ...}` answer of the helper.
/// Runs the helper and turns a panic into an error answer, so the parent always gets JSON.
pub fn answer_or_error(run: impl FnOnce() -> String) -> String {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).unwrap_or_else(|panic| {
        let text = panic
            .downcast_ref::<&str>()
            .map(|text| text.to_string())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        error_answer(&format!("admin-helper stopped: {text}"))
    })
}

pub fn error_answer(error: &str) -> String {
    serde_json::json!({ "error": error }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::apps::testing::sandbox;
    use crate::core::compress::testing::{host_with, is_compressed, make_repetitive_file};
    use crate::core::host::fake::{FakeHost, real_uid};
    use crate::core::safety::SafetyError;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    const IDLE: &str = "    1     0 /sbin/launchd\n";

    fn host() -> FakeHost {
        host_with(IDLE, "")
    }

    fn bundle_at(bundle: PathBuf) -> PathBuf {
        let macos = bundle.join("Contents/MacOS");
        fs::create_dir_all(&macos).unwrap();
        fs::write(macos.join("tool"), "x").unwrap();
        for folder in [&bundle, &bundle.join("Contents"), &macos] {
            fs::set_permissions(folder, fs::Permissions::from_mode(0o755)).unwrap();
        }
        bundle
    }

    fn check(layout: &Layout, bundle: &Path) -> Result<BundleId, String> {
        helper_check(
            bundle,
            layout,
            &IgnoreStore::default(),
            0,
            real_uid(),
            &host(),
        )
    }

    #[test]
    fn a_user_process_is_refused() {
        let (_temp, layout) = sandbox();
        let bundle = bundle_at(layout.system("Applications/Foo.app"));

        let error = helper_check(
            &bundle,
            &layout,
            &IgnoreStore::default(),
            501,
            real_uid(),
            &host(),
        );

        assert_eq!(error.unwrap_err(), NOT_ROOT);
    }

    #[test]
    fn a_plain_bundle_in_the_app_folder_passes() {
        let (_temp, layout) = sandbox();
        let bundle = bundle_at(layout.system("Applications/Foo.app"));

        assert!(check(&layout, &bundle).is_ok());
    }

    #[test]
    fn a_bundle_outside_the_app_folders_is_refused() {
        let (_temp, layout) = sandbox();
        let bundle = bundle_at(layout.system("Library/Foo.app"));
        let not_an_app = layout.system("Applications/Foo");
        fs::create_dir_all(&not_an_app).unwrap();

        assert_eq!(check(&layout, &bundle).unwrap_err(), NOT_APP);
        assert_eq!(check(&layout, &not_an_app).unwrap_err(), NOT_APP);
    }

    #[test]
    fn a_path_that_is_not_in_its_final_form_is_refused() {
        let (_temp, layout) = sandbox();
        bundle_at(layout.system("Foo.app"));
        let dotted = layout.system("Applications/../Foo.app");
        fs::create_dir_all(layout.system("Applications")).unwrap();

        assert_eq!(check(&layout, &dotted).unwrap_err(), NOT_APP);
    }

    #[test]
    fn a_home_bundle_is_refused() {
        let (_temp, layout) = sandbox();
        let bundle = bundle_at(layout.user("Applications/Foo.app"));

        assert_eq!(check(&layout, &bundle).unwrap_err(), UNSAFE_FOLDER);
    }

    #[test]
    fn a_vendor_folder_the_user_owns_is_refused() {
        let (_temp, layout) = sandbox();
        let bundle = bundle_at(layout.system("Applications/Vendor/Foo.app"));

        assert_eq!(check(&layout, &bundle).unwrap_err(), UNSAFE_FOLDER);
    }

    #[test]
    fn an_entry_of_another_owner_is_refused() {
        let (_temp, layout) = sandbox();
        let bundle = bundle_at(layout.system("Applications/Foo.app"));

        let error = helper_check(
            &bundle,
            &layout,
            &IgnoreStore::default(),
            0,
            real_uid() + 1,
            &host(),
        );

        assert_eq!(error.unwrap_err(), "Belongs to another user");
    }

    #[test]
    fn a_link_is_refused() {
        let (_temp, layout) = sandbox();
        let real = bundle_at(layout.system("Applications/Real.app"));
        let link = layout.system("Applications/Link.app");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        assert_eq!(check(&layout, &link).unwrap_err(), LINKED_BUNDLE);
    }

    #[test]
    fn a_folder_others_can_write_is_refused() {
        let (_temp, layout) = sandbox();
        let bundle = bundle_at(layout.system("Applications/Foo.app"));
        let folder = bundle.join("Contents/MacOS");

        fs::set_permissions(&folder, fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(check(&layout, &bundle).unwrap_err(), OPEN_FOLDER);

        fs::set_permissions(&folder, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&bundle, fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(check(&layout, &bundle).unwrap_err(), OPEN_FOLDER);
    }

    #[test]
    fn a_folder_a_group_besides_wheel_can_write_is_refused() {
        let (_temp, layout) = sandbox();
        let bundle = bundle_at(layout.system("Applications/Foo.app"));
        let folder = bundle.join("Contents/MacOS");
        assert_ne!(fs::metadata(&folder).unwrap().gid(), 0);

        fs::set_permissions(&folder, fs::Permissions::from_mode(0o775)).unwrap();

        assert_eq!(check(&layout, &bundle).unwrap_err(), OPEN_FOLDER);
    }

    fn add_acl(folder: &Path) {
        let status = std::process::Command::new("/bin/chmod")
            .args(["+a", "everyone allow add_file"])
            .arg(folder)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn a_folder_with_an_access_control_list_is_refused() {
        let (_temp, layout) = sandbox();
        let inner = bundle_at(layout.system("Applications/Inner.app"));
        let outer = bundle_at(layout.system("Applications/Outer.app"));
        assert!(check(&layout, &inner).is_ok());

        add_acl(&inner.join("Contents/MacOS"));
        add_acl(&outer);

        assert_eq!(check(&layout, &inner).unwrap_err(), ACL_FOLDER);
        assert_eq!(check(&layout, &outer).unwrap_err(), ACL_FOLDER);
    }

    #[test]
    fn the_wheel_group_may_write_where_others_may_not() {
        assert!(!opens_folder(0o775, 0));
        assert!(opens_folder(0o775, 20));
        assert!(opens_folder(0o777, 0));
        assert!(opens_folder(0o757, 0));
        assert!(!opens_folder(0o755, 20));
    }

    #[test]
    fn a_running_app_is_refused() {
        let (_temp, layout) = sandbox();
        let bundle = bundle_at(layout.system("Applications/Foo.app"));
        let ps = format!(
            "{IDLE}  500     1 {}/Contents/MacOS/tool\n",
            bundle.display()
        );
        let host = host_with(&ps, "");

        let error = helper_check(
            &bundle,
            &layout,
            &IgnoreStore::default(),
            0,
            real_uid(),
            &host,
        );

        assert_eq!(error.unwrap_err(), "app is currently running");
    }

    #[test]
    fn an_app_that_cannot_be_written_gives_the_app_management_text() {
        let (_temp, layout) = sandbox();
        let bundle = bundle_at(layout.system("Applications/Foo.app"));
        let contents = bundle.join("Contents");
        fs::set_permissions(&contents, fs::Permissions::from_mode(0o555)).unwrap();

        let error = check(&layout, &bundle).unwrap_err();

        fs::set_permissions(&contents, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(error.contains("App Management"), "{error}");
    }

    #[test]
    fn the_id_holds_while_the_bundle_stays_and_fails_when_it_is_replaced() {
        let (_temp, layout) = sandbox();
        let bundle = bundle_at(layout.system("Applications/Foo.app"));
        let id = check(&layout, &bundle).unwrap();
        assert_eq!(id.verify(&bundle), Ok(()));

        let aside = layout.system("Applications/Aside.app");
        fs::rename(&bundle, &aside).unwrap();
        fs::create_dir(&bundle).unwrap();
        assert_eq!(id.verify(&bundle), Err(MOVED.to_string()));

        fs::remove_dir(&bundle).unwrap();
        std::os::unix::fs::symlink(&aside, &bundle).unwrap();
        assert_eq!(id.verify(&bundle), Err(MOVED.to_string()));
    }

    fn big_file(bundle: &Path) -> PathBuf {
        let resources = bundle.join("Contents/Resources");
        fs::create_dir_all(&resources).unwrap();
        make_repetitive_file(&resources, "data.txt", 300_000)
    }

    #[test]
    fn a_checked_bundle_is_compressed_and_then_unpacked() {
        let (_temp, layout) = sandbox();
        let bundle = bundle_at(layout.system("Applications/Foo.app"));
        let file = big_file(&bundle);
        let original = fs::read(&file).unwrap();

        let report = compress_bundle(&layout, &bundle, false, &host(), 0, real_uid()).unwrap();
        assert_eq!(report.processed_files, 1, "{:?}", report.failures);
        assert!(!report.undo);
        assert!(is_compressed(&file));

        let report = compress_bundle(&layout, &bundle, true, &host(), 0, real_uid()).unwrap();
        assert!(report.undo);
        assert!(!is_compressed(&file));
        assert_eq!(fs::read(&file).unwrap(), original);
    }

    #[test]
    fn the_files_of_the_system_owner_are_compressed_whoever_the_host_user_is() {
        let (_temp, layout) = sandbox();
        let bundle = bundle_at(layout.system("Applications/Foo.app"));
        let file = big_file(&bundle);
        let host = host();
        host.set_uid(real_uid() + 1);

        let report = compress_bundle(&layout, &bundle, false, &host, 0, real_uid()).unwrap();
        assert_eq!(report.processed_files, 1, "{report:?}");
        assert!(is_compressed(&file));

        let report = compress_bundle(&layout, &bundle, true, &host, 0, real_uid()).unwrap();
        assert_eq!(report.processed_files, 1, "{report:?}");
        assert!(!is_compressed(&file));
    }

    #[test]
    fn a_bundle_replaced_after_the_check_is_left_alone() {
        let (_temp, layout) = sandbox();
        let bundle = bundle_at(layout.system("Applications/Foo.app"));
        let id = check(&layout, &bundle).unwrap();
        fs::rename(&bundle, layout.system("Applications/Aside.app")).unwrap();
        let file = big_file(&bundle_at(bundle.clone()));

        let error = compress_checked(
            &layout,
            &IgnoreStore::default(),
            &bundle,
            id,
            real_uid(),
            false,
            &host(),
        );

        assert_eq!(error.unwrap_err(), MOVED);
        assert!(!is_compressed(&file));
    }

    #[test]
    fn a_refused_bundle_is_left_alone() {
        let (_temp, layout) = sandbox();
        let bundle = bundle_at(layout.system("Applications/Foo.app"));
        let file = big_file(&bundle);

        let error = compress_bundle(&layout, &bundle, false, &host(), 501, real_uid());

        assert_eq!(error.unwrap_err(), NOT_ROOT);
        assert!(!is_compressed(&file));
    }

    #[test]
    fn a_user_process_hears_it_must_run_as_root_before_any_home_check() {
        let (_temp, layout) = sandbox();
        let bundle = bundle_at(layout.system("Applications/Foo.app"));
        let layout = Layout {
            home: "not/a/full/path".into(),
            ..layout
        };

        let error = compress_bundle(&layout, &bundle, false, &host(), 501, real_uid());

        assert_eq!(error.unwrap_err(), NOT_ROOT);
    }

    #[test]
    fn a_panic_in_the_helper_comes_back_as_a_json_error() {
        let answer = answer_or_error(|| panic!("boom"));

        let value: serde_json::Value = serde_json::from_str(&answer).unwrap();
        assert_eq!(
            value,
            serde_json::json!({ "error": "admin-helper stopped: boom" })
        );
    }

    #[test]
    fn the_ignore_list_of_the_home_still_applies() {
        let (_temp, layout) = sandbox();
        let bundle = bundle_at(layout.system("Applications/Foo.app"));
        let file = big_file(&bundle);
        let ignore = layout.ignore_path();
        fs::create_dir_all(ignore.parent().unwrap()).unwrap();
        fs::write(&ignore, format!("roots = [\"{}\"]\n", bundle.display())).unwrap();

        let error = compress_bundle(&layout, &bundle, false, &host(), 0, real_uid());

        assert_eq!(error.unwrap_err(), SafetyError::Ignored.to_string());
        assert!(!is_compressed(&file));
    }

    #[test]
    fn an_ignore_list_that_cannot_be_read_stops_the_helper() {
        let (_temp, layout) = sandbox();
        let bundle = bundle_at(layout.system("Applications/Foo.app"));
        let file = big_file(&bundle);
        let ignore = layout.ignore_path();
        fs::create_dir_all(ignore.parent().unwrap()).unwrap();
        fs::write(&ignore, "roots = [not toml\n").unwrap();

        let error = compress_bundle(&layout, &bundle, false, &host(), 0, real_uid());

        assert_eq!(
            error.unwrap_err(),
            IGNORE_UNREADABLE,
            "the helper must not run without the user's ignore list"
        );
        assert!(!is_compressed(&file));
    }

    /// The ignore path of `layout`, with its folder made.
    fn ignore_path_in(layout: &Layout) -> PathBuf {
        let path = layout.ignore_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        path
    }

    #[test]
    fn a_missing_ignore_file_is_an_empty_list() {
        let (_temp, layout) = sandbox();

        assert!(read_ignore(&layout).unwrap().roots.is_empty());
    }

    #[test]
    fn the_ignore_file_of_the_home_is_read() {
        let (_temp, layout) = sandbox();
        fs::write(
            ignore_path_in(&layout),
            "roots = [\"/Applications/A.app\"]\n",
        )
        .unwrap();

        assert_eq!(read_ignore(&layout).unwrap().roots, ["/Applications/A.app"]);
    }

    #[test]
    fn a_linked_ignore_file_stops_the_helper() {
        let (_temp, layout) = sandbox();
        let elsewhere = layout.user("elsewhere.toml");
        fs::write(&elsewhere, "roots = []\n").unwrap();
        std::os::unix::fs::symlink(&elsewhere, ignore_path_in(&layout)).unwrap();

        assert_eq!(read_ignore(&layout).unwrap_err(), IGNORE_UNREADABLE);
    }

    #[test]
    fn a_dangling_linked_ignore_file_stops_the_helper() {
        let (_temp, layout) = sandbox();
        std::os::unix::fs::symlink(layout.user("gone.toml"), ignore_path_in(&layout)).unwrap();

        assert_eq!(read_ignore(&layout).unwrap_err(), IGNORE_UNREADABLE);
    }

    #[test]
    fn a_broken_ignore_file_is_not_quoted_back() {
        let (_temp, layout) = sandbox();
        fs::write(ignore_path_in(&layout), "secret = \"s3cr3t\" broken\n").unwrap();

        let error = read_ignore(&layout).unwrap_err();

        assert!(!error.contains("s3cr3t"), "{error}");
        assert_eq!(error, IGNORE_UNREADABLE);
    }

    /// Reads the ignore list on another thread, so a read that blocks fails the test.
    fn read_ignore_in_time(layout: &Layout) -> Result<IgnoreStore, String> {
        let (send, receive) = std::sync::mpsc::channel();
        let layout = layout.clone();
        std::thread::spawn(move || send.send(read_ignore(&layout)));
        receive
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("reading the ignore list blocked")
    }

    #[test]
    fn a_folder_in_place_of_the_ignore_file_stops_the_helper() {
        let (_temp, layout) = sandbox();
        fs::create_dir(ignore_path_in(&layout)).unwrap();

        assert_eq!(read_ignore(&layout).unwrap_err(), IGNORE_UNREADABLE);
    }

    #[test]
    fn a_pipe_in_place_of_the_ignore_file_stops_the_helper_at_once() {
        let (_temp, layout) = sandbox();
        let path = ignore_path_in(&layout);
        let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);

        assert_eq!(read_ignore_in_time(&layout).unwrap_err(), IGNORE_UNREADABLE);
    }

    #[test]
    fn an_ignore_file_of_another_owner_stops_the_helper() {
        let (_temp, layout) = sandbox();
        let path = ignore_path_in(&layout);
        fs::write(&path, "roots = []\n").unwrap();

        assert!(read_ignore_file(&path, real_uid()).is_ok());
        assert_eq!(
            read_ignore_file(&path, real_uid() + 1).unwrap_err(),
            IGNORE_UNREADABLE
        );
    }

    #[test]
    fn a_home_that_is_not_a_full_path_to_a_folder_is_refused() {
        let (_temp, layout) = sandbox();
        let at = |home: PathBuf| Layout {
            home,
            ..layout.clone()
        };
        let linked = layout.system("linked-home");
        std::os::unix::fs::symlink(&layout.home, &linked).unwrap();
        let file = layout.system("a-file");
        fs::write(&file, "").unwrap();

        for home in [
            PathBuf::from("home"),
            linked,
            file,
            layout.system("missing"),
        ] {
            assert_eq!(
                read_ignore(&at(home.clone())).unwrap_err(),
                BAD_HOME,
                "{home:?}"
            );
        }
    }

    #[test]
    fn the_command_answers_a_user_process_with_a_json_error() {
        let (_temp, layout) = sandbox();
        let bundle = bundle_at(layout.system("Applications/Foo.app"));
        let file = big_file(&bundle);

        let answer = run_compress(&layout.home, &bundle, false);

        let value: serde_json::Value = serde_json::from_str(&answer).unwrap();
        assert_eq!(value, serde_json::json!({ "error": NOT_ROOT }));
        assert!(!is_compressed(&file));
    }
}

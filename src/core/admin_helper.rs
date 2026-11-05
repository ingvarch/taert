//! The root side of the admin helper: `taert admin-helper compress` and `slim` run here as
//! root.

use std::collections::HashSet;
use std::ffi::{CStr, CString, OsStr};
use std::fs;
use std::io::Read;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use super::apps::{self, InstalledApp};
use super::compress::{self, CompressOptions, CompressReport, Compressor};
use super::host::{Host, MacHost};
use super::layout::Layout;
use super::safety::Safety;
use super::slim::{self, open_folder};
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
const ROSETTA_UNREADABLE: &str = "Could not read the Rosetta settings";
const HARD_LINK: &str =
    "A file in the app has another name (a hard link), so taert will not hand the old app to you";
const COPY_HARD_LINK: &str =
    "A file in the copy of the app has another name (a hard link), so taert stopped";
const OLD_COPY_CHANGED: &str =
    "The old app in the holding folder is not the app taert checked, so taert left it there";
const BAD_HOME: &str = "--home must be the full path of a folder that is not a link";
const NOT_IN_APPLICATIONS: &str =
    "taert slims apps that belong to root only directly in /Applications";
const UNSAFE_HOLDING: &str =
    "The folder taert copies the app to is a link or can be changed by others";
const OTHER_VOLUME: &str = "The app is on another volume than the folder taert copies it to";
const HOLDING_ACL: &str =
    "The folder taert copies the app to has an access control list, which the copy would take on";

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

    /// Whether the open folder is the one that was checked.
    fn is(&self, folder: &OwnedFd) -> bool {
        stat_of(folder).is_ok_and(|stat| (stat.st_dev as u64, stat.st_ino) == (self.dev, self.ino))
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

/// The uid of the home folder, which must be a full path to a folder that is not a link.
fn home_owner(layout: &Layout) -> Result<u32, String> {
    let home = &layout.home;
    match fs::symlink_metadata(home) {
        Ok(meta) if home.is_absolute() && meta.is_dir() => Ok(meta.uid()),
        _ => Err(BAD_HOME.to_string()),
    }
}

/// Reads the ignore list in the home of the user as root.
pub fn read_ignore(layout: &Layout) -> Result<IgnoreStore, String> {
    read_ignore_file(&layout.ignore_path(), home_owner(layout)?)
}

/// Reads a file of the user as root: `None` when it is missing, an error unless it is a regular
/// file of `owner` that is not a link. The error says nothing about the file, which root may
/// read where the user cannot.
fn read_user_file(path: &Path, owner: u32) -> Result<Option<Vec<u8>>, ()> {
    // O_NONBLOCK keeps a pipe in place of the file from blocking the open.
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => return Ok(None),
        Err(_) => return Err(()),
    };
    let meta = file.metadata().map_err(|_| ())?;
    if !meta.is_file() || meta.uid() != owner {
        return Err(());
    }
    let mut content = Vec::new();
    (&file).read_to_end(&mut content).map_err(|_| ())?;
    Ok(Some(content))
}

/// Reads the ignore file. Only a missing file means no list: root must not work without the
/// list the user made.
fn read_ignore_file(path: &Path, owner: u32) -> Result<IgnoreStore, String> {
    let unreadable = || IGNORE_UNREADABLE.to_string();
    match read_user_file(path, owner).map_err(|()| unreadable())? {
        None => Ok(IgnoreStore::default()),
        Some(content) => {
            let text = String::from_utf8(content).map_err(|_| unreadable())?;
            toml::from_str(&text).map_err(|_| unreadable())
        }
    }
}

/// The apps the user set to open using Rosetta, read as root.
pub fn read_rosetta_ids(layout: &Layout) -> Result<HashSet<String>, String> {
    read_rosetta_ids_file(&slim::rosetta_plist_path(layout), home_owner(layout)?)
}

fn read_rosetta_ids_file(path: &Path, owner: u32) -> Result<HashSet<String>, String> {
    let unreadable = || ROSETTA_UNREADABLE.to_string();
    match read_user_file(path, owner).map_err(|()| unreadable())? {
        None => Ok(HashSet::new()),
        Some(content) => plist::Value::from_reader(std::io::Cursor::new(content))
            .map(|value| slim::rosetta_ids_in(&value))
            .map_err(|_| unreadable()),
    }
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

/// What `taert admin-helper slim` reports after the swap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlimmedByRoot {
    pub saved: u64,
    pub old_app: PathBuf,
    /// Why the old app could not be given to the user. It is then still owned by root, in
    /// `/Applications` or in the holding folder.
    pub handover_error: Option<String>,
}

/// Slims one root-owned bundle in `/Applications` as root, in a holding folder only root can
/// reach. The copy is cloned from the checked bundle, never from what its path names later.
/// Then it hands the old app to `owner` and `group` and moves it to
/// `/Applications/<name>.taert-old.app`. `Err` holds the step that failed and why; the bundle
/// is then as it was.
pub fn slim_bundle(
    layout: &Layout,
    bundle: &Path,
    owner: u32,
    group: u32,
    host: &dyn Host,
    euid: u32,
    system_owner: u32,
) -> Result<SlimmedByRoot, (usize, String)> {
    let first = |error: String| (1, error);
    if euid != 0 {
        return Err(first(NOT_ROOT.to_string()));
    }
    // The swap resolves the folders above the app by path. `/Applications` is the only one the
    // user cannot replace with a link.
    let applications = layout.system("Applications");
    if bundle.parent() != Some(applications.as_path()) {
        return Err(first(NOT_IN_APPLICATIONS.to_string()));
    }
    let ignore = read_ignore(layout).map_err(first)?;
    let id = helper_check(bundle, layout, &ignore, euid, system_owner, host).map_err(first)?;
    // The rules that read the app run here, so a refusal comes before any write, and again on
    // the copy, because a process can put another folder at the path while they read it.
    check_single_names(bundle).map_err(first)?;
    let app = app_at(bundle);
    let rosetta = read_rosetta_ids(layout).map_err(first)?;
    slim::check_app_rules(&app, &rosetta, None).map_err(first)?;
    let binaries = slim::check_signed_universal(&app, host, false).map_err(first)?;
    let parent = bundle.parent().unwrap_or(bundle);
    let app_device = fs::symlink_metadata(parent)
        .map_err(|e| first(format!("Could not read {}: {e}", parent.display())))?
        .dev();
    let holding = new_holding(layout, system_owner, app_device).map_err(first)?;
    let copy = |holding: BorrowedFd<'_>, name: &CStr| clone_checked(bundle, id, holding, name);
    let check_copy = |copy: &Path| check_root_copy(copy, system_owner);
    let binaries_of_copy = |copy: &Path| {
        let copied = app_at(copy);
        slim::check_app_rules(&copied, &rosetta, None)?;
        slim::signed_universal_binaries(&copied, host, false)
    };
    let guard = || id.verify(bundle);
    let root = slim::RootSteps {
        copy: &copy,
        check_copy: &check_copy,
        binaries_of_copy: &binaries_of_copy,
        guard: &guard,
    };
    let staged = slim::slim_to_swap(
        &app,
        &binaries,
        host,
        false,
        system_owner,
        holding,
        Some(&root),
    )?;
    let (old_app, handover_error) =
        release_old_copy(&staged.old_app, id, owner, group, &applications, &app.name);
    if old_app != staged.old_app {
        // Only root can reach the holding folder, so an empty one left there harms nothing.
        let _ = fs::remove_dir(&staged.holding);
    }
    Ok(SlimmedByRoot {
        saved: staged.saved,
        old_app,
        handover_error,
    })
}

/// The app at `bundle`, with what its `Info.plist` says now.
fn app_at(bundle: &Path) -> InstalledApp {
    InstalledApp {
        name: bundle
            .file_stem()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        path: bundle.to_path_buf(),
        bundle_id: apps::bundle_id_of(bundle),
        executable: None,
        size_bytes: 0,
        last_used: None,
        package: None,
    }
}

/// After the swap: gives the old copy to `owner` and `group` and moves it to `applications`.
/// Returns where the old app is, and why it could not be given to the user. An old copy that is
/// not the bundle with `id` stays where it is, untouched. The checked old copy is moved even
/// when the hand-over fails, so it does not stay where only root can reach it.
fn release_old_copy(
    old_copy: &Path,
    id: BundleId,
    owner: u32,
    group: u32,
    applications: &Path,
    name: &str,
) -> (PathBuf, Option<String>) {
    let old = match open_checked(old_copy, id) {
        Some(old) => old,
        None => return (old_copy.to_path_buf(), Some(OLD_COPY_CHANGED.to_string())),
    };
    let handed = hand_over(&old, old_copy, owner, group).err();
    match move_old_copy(old_copy, applications, name) {
        Ok(moved) => (moved, handed),
        Err(error) => {
            let errors: Vec<_> = handed.into_iter().chain([error]).collect();
            (old_copy.to_path_buf(), Some(errors.join("; ")))
        }
    }
}

/// The folder at `path`, opened without following a link, when it is the bundle with `id`.
fn open_checked(path: &Path, id: BundleId) -> Option<OwnedFd> {
    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    open_folder(None, &path).ok().filter(|folder| id.is(folder))
}

/// The folder that holds the holding folders of root.
pub(crate) fn holdings(layout: &Layout) -> PathBuf {
    layout.system("Library/Application Support/taert/slim")
}

/// Opens `Library/Application Support/taert/slim` through descriptors, following no link,
/// and makes `taert` (0755) and `slim` (0700) when they are missing. Refuses unless
/// `Application Support` and each folder below it belongs to `system_owner` and no group or
/// others can write in it, and unless `slim` is on `app_device`. Returns a free path in `slim`
/// for the holding folder.
fn new_holding(layout: &Layout, system_owner: u32, app_device: u64) -> Result<PathBuf, String> {
    let root = CString::new(layout.root.as_os_str().as_bytes())
        .map_err(|_| format!("Could not open {}", layout.root.display()))?;
    let root = open_folder(None, &root)
        .map_err(|e| format!("Could not open {}: {e}", layout.root.display()))?;
    let library = open_holding_part(&root, c"Library", &layout.system("Library"))?;
    let mut path = layout.system("Library/Application Support");
    let mut folder = open_holding_part(&library, c"Application Support", &path)?;
    check_holding_part(&folder, &path, system_owner)?;
    for (name, mode) in [(c"taert", 0o755), (c"slim", 0o700)] {
        path.push(OsStr::from_bytes(name.to_bytes()));
        if unsafe { libc::mkdirat(folder.as_raw_fd(), name.as_ptr(), mode) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EEXIST) {
                return Err(format!("Could not create {}: {error}", path.display()));
            }
        }
        folder = open_holding_part(&folder, name, &path)?;
        check_holding_part(&folder, &path, system_owner)?;
    }
    if stat_of(&folder)
        .map_err(|e| format!("Could not read {}: {e}", path.display()))?
        .st_dev as u64
        != app_device
    {
        return Err(OTHER_VOLUME.to_string());
    }
    let pid = std::process::id();
    let mut n = 0;
    loop {
        let holding = path.join(format!("{pid}.{n}"));
        if fs::symlink_metadata(&holding).is_err() {
            return Ok(holding);
        }
        n += 1;
    }
}

fn open_holding_part(parent: &OwnedFd, name: &CStr, path: &Path) -> Result<OwnedFd, String> {
    open_folder(Some(parent.as_raw_fd()), name).map_err(|e| {
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        let found = unsafe {
            libc::fstatat(
                parent.as_raw_fd(),
                name.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if found == 0 && stat.st_mode & libc::S_IFMT == libc::S_IFLNK {
            UNSAFE_HOLDING.to_string()
        } else {
            format!("Could not open {}: {e}", path.display())
        }
    })
}

/// Refuses a folder on the way to the holding folder unless it belongs to `system_owner`, no
/// group or others can write in it, and it has no access control list, which the copy would
/// take on.
fn check_holding_part(folder: &OwnedFd, path: &Path, system_owner: u32) -> Result<(), String> {
    match stat_of(folder) {
        Ok(stat) if stat.st_uid == system_owner && stat.st_mode & 0o022 == 0 => {}
        _ => return Err(UNSAFE_HOLDING.to_string()),
    }
    match compress::has_acl(path) {
        Ok(false) => Ok(()),
        Ok(true) => Err(HOLDING_ACL.to_string()),
        Err(error) => Err(format!("Could not read {}: {error}", path.display())),
    }
}

fn stat_of(fd: &OwnedFd) -> std::io::Result<libc::stat> {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(stat)
}

/// Moves the old copy to `<applications>/<name>.taert-old.app`, or to the first free name
/// with ` 2`, ` 3`, ... before `.app`. It never replaces an entry.
fn move_old_copy(old_copy: &Path, applications: &Path, name: &str) -> Result<PathBuf, String> {
    let failed = |error: &dyn std::fmt::Display| {
        format!(
            "Could not move {} to {}: {error}",
            old_copy.display(),
            applications.display()
        )
    };
    let from = CString::new(old_copy.as_os_str().as_bytes()).map_err(|e| failed(&e))?;
    let mut n = 1;
    loop {
        let target = applications.join(slim::old_copy_name(name, n));
        let to = CString::new(target.as_os_str().as_bytes()).map_err(|e| failed(&e))?;
        if unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) } == 0 {
            return Ok(target);
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EEXIST) {
            return Err(failed(&error));
        }
        n += 1;
    }
}

/// Refuses a bundle with an entry that has more than one name. The hand-over changes the owner
/// of the entry, so it would give the user the file under its other names too, and they can be
/// outside the bundle.
fn check_single_names(bundle: &Path) -> Result<(), String> {
    if has_hard_link(bundle)? {
        return Err(HARD_LINK.to_string());
    }
    Ok(())
}

/// Whether an entry below `root` that is not a folder has more than one name.
fn has_hard_link(root: &Path) -> Result<bool, String> {
    let entries =
        slim::walk_entries(root).map_err(|e| format!("Could not read {}: {e}", root.display()))?;
    Ok(entries
        .iter()
        .any(|(_, meta)| !meta.is_dir() && meta.nlink() > 1))
}

/// Clones the bundle into the open holding folder as `name`. The source is a descriptor of the
/// bundle that still has the device and inode it had at the check, never what the path names
/// later. As root, the clone keeps owners, modes, flags, and extended attributes.
fn clone_checked(
    bundle: &Path,
    id: BundleId,
    holding: BorrowedFd<'_>,
    name: &CStr,
) -> Result<(), String> {
    let source = open_checked(bundle, id).ok_or_else(|| MOVED.to_string())?;
    let cloned =
        unsafe { libc::fclonefileat(source.as_raw_fd(), holding.as_raw_fd(), name.as_ptr(), 0) };
    if cloned != 0 {
        let error = std::io::Error::last_os_error();
        return Err(format!("Could not copy {}: {error}", bundle.display()));
    }
    Ok(())
}

/// Checks the copy in the holding folder of root: every entry belongs to `system_owner`, none
/// is setuid or setgid, and no file has another name.
fn check_root_copy(copy: &Path, system_owner: u32) -> Result<(), String> {
    slim::check_ownership(copy, system_owner)?;
    if has_hard_link(copy)? {
        return Err(COPY_HARD_LINK.to_string());
    }
    Ok(())
}

/// The names in the folder `fd` holds, read through a copy of the descriptor.
fn names_in(fd: RawFd) -> std::io::Result<Vec<CString>> {
    let copy = unsafe { libc::dup(fd) };
    if copy < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let dir = unsafe { libc::fdopendir(copy) };
    if dir.is_null() {
        let error = std::io::Error::last_os_error();
        unsafe { libc::close(copy) };
        return Err(error);
    }
    let mut names = Vec::new();
    loop {
        let entry = unsafe { libc::readdir(dir) };
        if entry.is_null() {
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() != b"." && name.to_bytes() != b".." {
            names.push(name.to_owned());
        }
    }
    unsafe { libc::closedir(dir) };
    Ok(names)
}

/// What a hand-over does to one thing: the entry `name` in the folder `RawFd`, or, with no
/// name, the folder itself, as `stat` found it. The path is for messages.
type Apply<'a> = dyn FnMut(RawFd, Option<&CStr>, &Path, &libc::stat) -> std::io::Result<()> + 'a;

fn walk_folder(fd: RawFd, path: &Path, apply: &mut Apply) -> Result<(), (PathBuf, std::io::Error)> {
    for name in names_in(fd).map_err(|e| (path.to_path_buf(), e))? {
        let entry = path.join(OsStr::from_bytes(name.to_bytes()));
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        let found =
            unsafe { libc::fstatat(fd, name.as_ptr(), &mut stat, libc::AT_SYMLINK_NOFOLLOW) };
        if found != 0 {
            return Err((entry, std::io::Error::last_os_error()));
        }
        if stat.st_mode & libc::S_IFMT == libc::S_IFDIR {
            let inner = open_folder(Some(fd), &name).map_err(|e| (entry.clone(), e))?;
            walk_folder(inner.as_raw_fd(), &entry, apply)?;
        }
        apply(fd, Some(&name), &entry, &stat).map_err(|e| (entry, e))?;
    }
    Ok(())
}

/// Visits every entry in the open folder `top`, then `top`, without following a link. The walk
/// goes down through descriptors, so a folder renamed or replaced meanwhile cannot send it
/// elsewhere. `path` names `top` in messages.
fn walk_tree(top: &OwnedFd, path: &Path, apply: &mut Apply) -> Result<(), String> {
    let failed = |path: &Path, error: &dyn std::fmt::Display| {
        format!("Could not hand {} to you: {error}", path.display())
    };
    walk_folder(top.as_raw_fd(), path, apply).map_err(|(path, e)| failed(&path, &e))?;
    let stat = stat_of(top).map_err(|e| failed(path, &e))?;
    apply(top.as_raw_fd(), None, path, &stat).map_err(|e| failed(path, &e))
}

/// Gives the open old copy and every entry in it to `owner` and `group` without following
/// links, so the user can move it to the Trash. It stops at a file with another name, because
/// that name can be outside the old copy.
fn hand_over(old_copy: &OwnedFd, path: &Path, owner: u32, group: u32) -> Result<(), String> {
    walk_tree(old_copy, path, &mut |fd, name, _, stat| {
        if stat.st_mode & libc::S_IFMT != libc::S_IFDIR && stat.st_nlink > 1 {
            return Err(std::io::Error::other("it has another name (a hard link)"));
        }
        let done = match name {
            Some(name) => unsafe {
                libc::fchownat(fd, name.as_ptr(), owner, group, libc::AT_SYMLINK_NOFOLLOW)
            },
            None => unsafe { libc::fchown(fd, owner, group) },
        };
        if done == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    })
}

fn helper_layout(home: &Path) -> Layout {
    Layout {
        home: home.to_path_buf(),
        root: "/".into(),
        user_cache: None,
    }
}

/// What `taert admin-helper slim` prints.
pub fn run_slim(home: &Path, owner: u32, group: u32, bundle: &Path) -> String {
    let euid = unsafe { libc::geteuid() };
    slim_answer(slim_bundle(
        &helper_layout(home),
        bundle,
        owner,
        group,
        &MacHost,
        euid,
        0,
    ))
}

fn slim_answer(result: Result<SlimmedByRoot, (usize, String)>) -> String {
    match result {
        Ok(done) => {
            let mut answer = serde_json::json!({
                "saved": done.saved,
                "old_app": done.old_app,
            });
            if let Some(error) = done.handover_error {
                answer["handover_error"] = error.into();
            }
            answer.to_string()
        }
        Err((step, error)) => serde_json::json!({ "error": error, "step": step }).to_string(),
    }
}

/// What `taert admin-helper compress` prints: the report, or `{"error": ...}`.
pub fn run_compress(home: &Path, bundle: &Path, undo: bool) -> String {
    let layout = helper_layout(home);
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
        add_acl_entry(folder, "everyone allow add_file");
    }

    fn add_acl_entry(folder: &Path, entry: &str) {
        let status = std::process::Command::new("/bin/chmod")
            .args(["+a", entry])
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

    fn rosetta_path_in(layout: &Layout) -> PathBuf {
        let path = slim::rosetta_plist_path(layout);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        path
    }

    const ROSETTA_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>LSArchitecturesForX86_64</key><dict>
<key>com.vendor.old</key><array><string>x86_64</string></array></dict></dict></plist>"#;

    #[test]
    fn a_missing_rosetta_file_is_an_empty_set() {
        let (_temp, layout) = sandbox();

        assert!(read_rosetta_ids(&layout).unwrap().is_empty());
    }

    #[test]
    fn the_rosetta_file_of_the_home_is_read() {
        let (_temp, layout) = sandbox();
        fs::write(rosetta_path_in(&layout), ROSETTA_PLIST).unwrap();

        let ids = read_rosetta_ids(&layout).unwrap();

        assert_eq!(ids, ["com.vendor.old".to_string()].into());
    }

    #[test]
    fn a_rosetta_file_that_cannot_be_trusted_stops_the_helper() {
        let (_temp, layout) = sandbox();
        let path = rosetta_path_in(&layout);
        let elsewhere = layout.user("elsewhere.plist");
        fs::write(&elsewhere, ROSETTA_PLIST).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &path).unwrap();
        assert_eq!(read_rosetta_ids(&layout), Err(ROSETTA_UNREADABLE.into()));

        fs::remove_file(&path).unwrap();
        fs::write(&path, "not a plist").unwrap();
        assert_eq!(read_rosetta_ids(&layout), Err(ROSETTA_UNREADABLE.into()));

        fs::write(&path, ROSETTA_PLIST).unwrap();
        assert!(read_rosetta_ids_file(&path, real_uid()).is_ok());
        assert_eq!(
            read_rosetta_ids_file(&path, real_uid() + 1),
            Err(ROSETTA_UNREADABLE.into())
        );
    }

    #[test]
    fn a_pipe_in_place_of_the_rosetta_file_stops_the_helper_at_once() {
        let (_temp, layout) = sandbox();
        let path = rosetta_path_in(&layout);
        let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || send.send(read_rosetta_ids(&layout)));

        let answer = receive
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("reading the Rosetta settings blocked");

        assert_eq!(answer, Err(ROSETTA_UNREADABLE.into()));
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

    use crate::core::slim::testing::write_test_app;

    const DEVELOPER_ID: &str = "Authority=Developer ID Application: Acme Corp (123)\n";

    /// A universal app that passes the checks of root, with the folder modes the helper wants.
    fn slim_bundle_at(bundle: PathBuf) -> PathBuf {
        write_test_app(&bundle, "com.vendor.slimme");
        for (path, meta) in slim::walk_entries(&bundle).unwrap() {
            if meta.is_dir() {
                fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        bundle
    }

    /// A sandbox with the `Library/Application Support` of a Mac.
    fn slim_sandbox() -> (tempfile::TempDir, Layout) {
        let (temp, layout) = sandbox();
        let support = layout.system("Library/Application Support");
        fs::create_dir_all(&support).unwrap();
        fs::set_permissions(&support, fs::Permissions::from_mode(0o755)).unwrap();
        (temp, layout)
    }

    fn holdings_in(layout: &Layout) -> Vec<PathBuf> {
        fs::read_dir(holdings(layout))
            .map(|entries| entries.map(|entry| entry.unwrap().path()).collect())
            .unwrap_or_default()
    }

    fn slim_host() -> FakeHost {
        slim_host_signed(DEVELOPER_ID)
    }

    /// A host where every tool says yes. Root copies the app itself, without `ditto`.
    fn slim_host_signed(authority: &str) -> FakeHost {
        let host = host();
        host.reply("/usr/bin/codesign -dv", authority);
        host.reply("/usr/bin/codesign", "");
        host.reply("/usr/sbin/spctl", "");
        host
    }

    fn slim_as_root(
        layout: &Layout,
        bundle: &Path,
        host: &FakeHost,
        system_owner: u32,
    ) -> Result<SlimmedByRoot, (usize, String)> {
        slim_bundle(
            layout,
            bundle,
            real_uid(),
            unsafe { libc::getgid() },
            host,
            0,
            system_owner,
        )
    }

    fn binary_of(bundle: &Path) -> PathBuf {
        bundle.join("Contents/MacOS/testapp")
    }

    fn mode_of(folder: &Path) -> u32 {
        fs::symlink_metadata(folder).unwrap().mode() & 0o777
    }

    #[test]
    fn the_old_copy_ends_in_applications_and_the_holding_folder_is_gone() {
        let (_temp, layout) = slim_sandbox();
        let bundle = slim_bundle_at(layout.system("Applications/Foo.app"));
        let before = fs::read(binary_of(&bundle)).unwrap();
        let host = slim_host();

        let done = slim_as_root(&layout, &bundle, &host, real_uid()).unwrap();

        let old_app = layout.system("Applications/Foo.taert-old.app");
        assert_eq!(done.old_app, old_app);
        assert_eq!(done.handover_error, None);
        assert!(!host.ran("/usr/bin/ditto"));
        assert_eq!(fs::read(binary_of(&old_app)).unwrap(), before);
        let header = slim::parse_fat_header_from_file(&binary_of(&bundle)).unwrap();
        let cpus: Vec<_> = header.unwrap().slices.iter().map(|s| s.cputype).collect();
        assert_eq!(cpus, [slim::CPU_TYPE_ARM64]);
        assert!(holdings_in(&layout).is_empty());
        assert_eq!(
            mode_of(&layout.system("Library/Application Support/taert")),
            0o755
        );
        assert_eq!(mode_of(&holdings(&layout)), 0o700);
    }

    #[test]
    fn a_taken_old_copy_name_gets_the_next_number_and_is_not_replaced() {
        let (_temp, layout) = slim_sandbox();
        let bundle = slim_bundle_at(layout.system("Applications/Foo.app"));
        let taken = layout.system("Applications/Foo.taert-old.app");
        fs::create_dir(&taken).unwrap();
        fs::write(taken.join("kept"), "x").unwrap();
        let host = slim_host();

        let done = slim_as_root(&layout, &bundle, &host, real_uid()).unwrap();

        let next = layout.system("Applications/Foo.taert-old 2.app");
        assert_eq!((done.old_app, done.handover_error), (next.clone(), None));
        assert!(binary_of(&next).exists());
        assert_eq!(fs::read_dir(&taken).unwrap().count(), 1);
        assert!(taken.join("kept").exists());
        assert!(holdings_in(&layout).is_empty());
    }

    /// Spoils the holding place in one way that must stop the helper.
    fn spoil_holding_place(layout: &Layout, elsewhere: &Path, how: usize) {
        let support = layout.system("Library/Application Support");
        let taert = support.join("taert");
        let link = |path: &Path| {
            if path.exists() {
                fs::remove_dir(path).unwrap();
            }
            std::os::unix::fs::symlink(elsewhere, path).unwrap();
        };
        let set_mode = |path: &Path, mode| {
            fs::create_dir_all(path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
        };
        match how {
            0 => link(&support),
            1 => set_mode(&support, 0o775),
            2 => set_mode(&support, 0o757),
            3 => link(&taert),
            4 => set_mode(&taert, 0o775),
            5 => set_mode(&taert.join("slim"), 0o770),
            _ => {
                set_mode(&taert, 0o755);
                link(&taert.join("slim"));
            }
        }
    }

    #[test]
    fn a_holding_place_that_is_a_link_or_that_others_can_write_is_refused_before_the_copy() {
        for how in 0..7 {
            let (temp, layout) = slim_sandbox();
            let elsewhere = temp.path().join("elsewhere");
            fs::create_dir(&elsewhere).unwrap();
            let bundle = slim_bundle_at(layout.system("Applications/Foo.app"));
            let before = fs::read(binary_of(&bundle)).unwrap();
            spoil_holding_place(&layout, &elsewhere, how);
            let host = slim_host();

            let failure = slim_as_root(&layout, &bundle, &host, real_uid());

            assert_eq!(
                failure.unwrap_err(),
                (1, UNSAFE_HOLDING.to_string()),
                "case {how}"
            );
            assert!(!host.ran("/usr/bin/ditto"), "case {how}");
            assert_eq!(fs::read(binary_of(&bundle)).unwrap(), before);
            assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0, "case {how}");
        }
    }

    #[test]
    fn a_holding_place_of_another_owner_or_on_another_volume_is_refused() {
        let (_temp, layout) = slim_sandbox();
        let device = fs::metadata(&layout.root).unwrap().dev();

        assert_eq!(
            new_holding(&layout, real_uid() + 1, device),
            Err(UNSAFE_HOLDING.to_string())
        );
        assert_eq!(
            new_holding(&layout, real_uid(), device + 1),
            Err(OTHER_VOLUME.to_string())
        );
        let holding = new_holding(&layout, real_uid(), device).unwrap();
        assert_eq!(holding.parent(), Some(holdings(&layout).as_path()));
        assert!(!holding.exists());
    }

    #[test]
    fn every_check_of_root_refuses_before_anything_is_written() {
        let (_temp, layout) = slim_sandbox();
        let owned = slim_bundle_at(layout.system("Applications/Owned.app"));
        let setuid = slim_bundle_at(layout.system("Applications/Setuid.app"));
        fs::set_permissions(binary_of(&setuid), fs::Permissions::from_mode(0o4755)).unwrap();
        let not_an_app = slim_bundle_at(layout.system("Applications/Plain"));
        let below_user = slim_bundle_at(layout.system("Applications/Vendor/Below.app"));

        for (bundle, system_owner, error) in [
            (&owned, real_uid() + 1, "Belongs to another user"),
            (&setuid, real_uid(), "Contains a setuid or setgid file"),
            (&not_an_app, real_uid(), NOT_APP),
            (&below_user, real_uid(), NOT_IN_APPLICATIONS),
        ] {
            let before = slim::fingerprint(bundle).unwrap();
            let host = slim_host();

            let failure = slim_as_root(&layout, bundle, &host, system_owner);

            assert_eq!(failure.unwrap_err(), (1, error.to_string()), "{bundle:?}");
            assert_eq!(slim::fingerprint(bundle).unwrap(), before);
            assert!(!layout.system("Library/Application Support/taert").exists());
            assert!(!host.ran("/usr/bin/ditto"));
        }
    }

    #[test]
    fn a_bundle_outside_applications_is_refused_before_anything_is_written() {
        let (_temp, layout) = slim_sandbox();
        let bundle = slim_bundle_at(layout.system("Library/Input Methods/Foo.app"));
        let before = fs::read(binary_of(&bundle)).unwrap();
        let host = slim_host();

        let failure = slim_as_root(&layout, &bundle, &host, real_uid());

        assert_eq!(failure.unwrap_err(), (1, NOT_IN_APPLICATIONS.to_string()));
        assert_eq!(fs::read(binary_of(&bundle)).unwrap(), before);
        assert!(!host.ran("/usr/bin/ditto"));
    }

    #[test]
    fn an_app_in_a_vendor_folder_is_refused_before_anything_is_written() {
        let (_temp, layout) = slim_sandbox();
        let bundle = slim_bundle_at(layout.system("Applications/Vendor/Foo.app"));
        let before = fs::read(binary_of(&bundle)).unwrap();
        let host = slim_host();

        let failure = slim_as_root(&layout, &bundle, &host, real_uid());

        assert_eq!(
            failure.unwrap_err(),
            (
                1,
                "taert slims apps that belong to root only directly in /Applications".to_string()
            )
        );
        assert_eq!(fs::read(binary_of(&bundle)).unwrap(), before);
        assert!(!layout.system("Library/Application Support/taert").exists());
        assert!(host.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn a_holding_place_with_an_access_control_list_is_refused_before_the_copy() {
        for part in ["", "taert", "taert/slim"] {
            let (_temp, layout) = slim_sandbox();
            let bundle = slim_bundle_at(layout.system("Applications/Foo.app"));
            let before = fs::read(binary_of(&bundle)).unwrap();
            let folder = layout.system("Library/Application Support").join(part);
            fs::create_dir_all(&folder).unwrap();
            add_acl_entry(
                &folder,
                "everyone allow list,file_inherit,directory_inherit",
            );
            let host = slim_host();

            let failure = slim_as_root(&layout, &bundle, &host, real_uid());

            assert_eq!(
                failure.unwrap_err(),
                (1, HOLDING_ACL.to_string()),
                "{part:?}"
            );
            assert_eq!(fs::read(binary_of(&bundle)).unwrap(), before);
            assert!(holdings_in(&layout).is_empty(), "{part:?}");
        }
    }

    /// Puts a plain app of theirs at `bundle` when root first lists processes, in its checks,
    /// and puts the checked bundle back at the last check before the copy.
    fn swap_during_the_checks(host: &FakeHost, bundle: &Path, theirs: fn(&Path)) {
        let aside = bundle.with_file_name("Aside.app");
        let (bundle, back, out) = (bundle.to_path_buf(), aside.clone(), std::sync::Once::new());
        let path = bundle.clone();
        host.when("/bin/ps", move || {
            out.call_once(|| {
                fs::rename(&path, &aside).unwrap();
                write_test_app(&path, "com.vendor.theirs");
                theirs(&path);
            })
        });
        let back_once = std::sync::Once::new();
        host.when("/usr/sbin/spctl", move || {
            back_once.call_once(|| {
                fs::remove_dir_all(&bundle).unwrap();
                fs::rename(&back, &bundle).unwrap();
            })
        });
    }

    #[test]
    fn the_rules_that_read_the_app_run_again_on_the_copy() {
        let store = |bundle: &Path| fs::create_dir(bundle.join("Contents/_MASReceipt")).unwrap();
        let apple = |bundle: &Path| write_test_app(bundle, "com.apple.x");
        let rosetta = |bundle: &Path| write_test_app(bundle, "com.vendor.rosetta");
        for (make, error) in [
            (
                store as fn(&Path),
                "App Store apps are managed by the App Store",
            ),
            (apple, "Apple apps are part of macOS"),
            (rosetta, "Set to open using Rosetta"),
        ] {
            let (_temp, layout) = slim_sandbox();
            let bundle = slim_bundle_at(layout.system("Applications/Foo.app"));
            make(&bundle);
            let plist = ROSETTA_PLIST.replace("com.vendor.old", "com.vendor.rosetta");
            fs::write(rosetta_path_in(&layout), plist).unwrap();
            let before = fs::read(binary_of(&bundle)).unwrap();
            let host = slim_host();
            swap_during_the_checks(&host, &bundle, |_| {});

            let failure = slim_as_root(&layout, &bundle, &host, real_uid());

            assert_eq!(failure.unwrap_err(), (1, error.to_string()), "{error}");
            assert_eq!(fs::read(binary_of(&bundle)).unwrap(), before);
            assert!(holdings_in(&layout).is_empty());
        }
    }

    #[test]
    fn the_developer_id_of_the_copy_is_checked() {
        let (_temp, layout) = slim_sandbox();
        let bundle = slim_bundle_at(layout.system("Applications/Foo.app"));
        let before = fs::read(binary_of(&bundle)).unwrap();
        let host = slim_host();
        // The first answer is for the app, every later one for the copy.
        host.reply("/usr/bin/codesign -dv", "Authority=Mac Developer\n");

        let failure = slim_as_root(&layout, &bundle, &host, real_uid());

        assert_eq!(
            failure.unwrap_err(),
            (1, "Not signed with a Developer ID".to_string())
        );
        assert_eq!(fs::read(binary_of(&bundle)).unwrap(), before);
        assert!(holdings_in(&layout).is_empty());
    }

    #[test]
    fn the_binaries_that_step_two_slims_come_from_the_copy() {
        let (_temp, layout) = slim_sandbox();
        let bundle = slim_bundle_at(layout.system("Applications/Foo.app"));
        let host = slim_host();
        // Their app has its universal binary under another name.
        swap_during_the_checks(&host, &bundle, |theirs| {
            fs::rename(binary_of(theirs), theirs.join("Contents/MacOS/other")).unwrap();
        });

        let done = slim_as_root(&layout, &bundle, &host, real_uid()).unwrap();

        assert_eq!(done.handover_error, None);
        let header = slim::parse_fat_header_from_file(&binary_of(&bundle)).unwrap();
        let cpus: Vec<_> = header.unwrap().slices.iter().map(|s| s.cputype).collect();
        assert_eq!(cpus, [slim::CPU_TYPE_ARM64]);
        let copy_signs = format!(
            "/usr/bin/codesign -dv --verbose=2 {}/",
            holdings(&layout).display()
        );
        assert!(host.ran(&copy_signs), "{:?}", host.calls.lock().unwrap());
    }

    #[test]
    fn a_bundle_with_a_hard_link_is_refused_before_the_copy() {
        let (temp, layout) = slim_sandbox();
        let bundle = slim_bundle_at(layout.system("Applications/Foo.app"));
        let elsewhere = temp.path().join("elsewhere.txt");
        fs::write(&elsewhere, "x").unwrap();
        fs::hard_link(&elsewhere, bundle.join("Contents/MacOS/linked")).unwrap();
        let host = slim_host();

        let failure = slim_as_root(&layout, &bundle, &host, real_uid());

        assert_eq!(failure.unwrap_err(), (1, HARD_LINK.to_string()));
        assert!(holdings_in(&layout).is_empty());
        assert!(!host.ran("/usr/bin/ditto"));
    }

    #[test]
    fn a_user_process_cannot_slim() {
        let (_temp, layout) = slim_sandbox();
        let bundle = slim_bundle_at(layout.system("Applications/Foo.app"));
        let host = slim_host();

        let failure = slim_bundle(&layout, &bundle, 501, 20, &host, 501, real_uid());

        assert_eq!(failure.unwrap_err(), (1, NOT_ROOT.to_string()));
        assert!(holdings_in(&layout).is_empty());
    }

    #[test]
    fn the_rules_that_do_not_depend_on_the_owner_still_apply() {
        let (_temp, layout) = slim_sandbox();
        let apple = slim_bundle_at(layout.system("Applications/Apple.app"));
        write_test_app(&apple, "com.apple.x");
        let store = slim_bundle_at(layout.system("Applications/Store.app"));
        fs::create_dir(store.join("Contents/_MASReceipt")).unwrap();
        let rosetta = slim_bundle_at(layout.system("Applications/Rosetta.app"));
        write_test_app(&rosetta, "com.vendor.rosetta");
        let plist = ROSETTA_PLIST.replace("com.vendor.old", "com.vendor.rosetta");
        fs::write(rosetta_path_in(&layout), plist).unwrap();
        let unsigned = slim_bundle_at(layout.system("Applications/Unsigned.app"));
        let thin = slim_bundle_at(layout.system("Applications/Thin.app"));
        fs::write(binary_of(&thin), b"not a fat file").unwrap();
        let broken = slim_bundle_at(layout.system("Applications/Broken.app"));
        let rejected = slim_bundle_at(layout.system("Applications/Rejected.app"));
        // A host where `command` fails the first time and every other check passes.
        let failing = |command: &str| {
            let host = host();
            host.reply("/usr/bin/codesign -dv", DEVELOPER_ID);
            host.fail(command, "rejected\n");
            host.reply("/usr/bin/codesign", "");
            host.reply("/usr/sbin/spctl", "");
            host
        };

        for (bundle, host, error) in [
            (&apple, slim_host(), "Apple apps are part of macOS"),
            (
                &store,
                slim_host(),
                "App Store apps are managed by the App Store",
            ),
            (&rosetta, slim_host(), "Set to open using Rosetta"),
            (
                &unsigned,
                slim_host_signed("Authority=Mac Developer\n"),
                "Not signed with a Developer ID",
            ),
            (&thin, slim_host(), "Already Apple Silicon only"),
            (
                &broken,
                failing("/usr/bin/codesign --verify"),
                "Already fails the signature check",
            ),
            (
                &rejected,
                failing("/usr/sbin/spctl"),
                "Already fails the Gatekeeper check",
            ),
        ] {
            let failure = slim_as_root(&layout, bundle, &host, real_uid());

            assert_eq!(failure.unwrap_err(), (1, error.to_string()), "{bundle:?}");
            assert!(holdings_in(&layout).is_empty());
        }
    }

    #[test]
    fn a_bundle_replaced_before_the_swap_is_left_alone() {
        let (_temp, layout) = slim_sandbox();
        let bundle = slim_bundle_at(layout.system("Applications/Foo.app"));
        let host = slim_host();
        let (aside, swapped_in) = (layout.system("Applications/Aside.app"), bundle.clone());
        let process_lists = std::sync::atomic::AtomicUsize::new(0);
        // The second list is the last check before the swap.
        host.when("/bin/ps", move || {
            if process_lists.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
                fs::rename(&swapped_in, &aside).unwrap();
                fs::create_dir(&swapped_in).unwrap();
            }
        });

        let failure = slim_as_root(&layout, &bundle, &host, real_uid());

        assert_eq!(failure.unwrap_err(), (6, MOVED.to_string()));
        assert_eq!(fs::read_dir(&bundle).unwrap().count(), 0);
        assert!(holdings_in(&layout).is_empty());
    }

    #[test]
    fn a_folder_swapped_in_before_the_copy_does_not_reach_the_copy() {
        let (_temp, layout) = slim_sandbox();
        let bundle = slim_bundle_at(layout.system("Applications/Foo.app"));
        let host = slim_host();
        let (aside, theirs) = (layout.system("Applications/Aside.app"), bundle.clone());
        let once = std::sync::Once::new();
        // The first `spctl` is the last check of root before the copy.
        host.when("/usr/sbin/spctl", move || {
            once.call_once(|| {
                fs::rename(&theirs, &aside).unwrap();
                write_test_app(&theirs, "com.vendor.theirs");
            })
        });

        let failure = slim_as_root(&layout, &bundle, &host, real_uid());

        assert_eq!(failure.unwrap_err(), (1, MOVED.to_string()));
        assert!(!host.ran("/usr/bin/ditto"));
        assert_eq!(host.count("/usr/bin/codesign --verify"), 1);
        assert!(holdings_in(&layout).is_empty());
    }

    /// The copy of the app in the holding folder, once there is one.
    fn copy_in(layout: &Layout) -> Option<PathBuf> {
        let copy = holdings_in(layout).first()?.join("Foo.app");
        copy.exists().then_some(copy)
    }

    #[test]
    fn a_copy_with_a_setuid_file_or_a_hard_link_is_refused_before_the_swap() {
        for (case, error) in [(0, "Contains a setuid or setgid file"), (1, COPY_HARD_LINK)] {
            let (temp, layout) = slim_sandbox();
            let bundle = slim_bundle_at(layout.system("Applications/Foo.app"));
            let before = fs::read(binary_of(&bundle)).unwrap();
            let outside = temp.path().join("outside.txt");
            fs::write(&outside, "x").unwrap();
            let host = slim_host();
            let (sandbox, linked) = (layout.clone(), outside.clone());
            // Changes the copy while `codesign` checks it.
            host.when("/usr/bin/codesign --verify", move || {
                let Some(copy) = copy_in(&sandbox) else {
                    return;
                };
                let added = copy.join("Contents/MacOS/added");
                if added.exists() {
                    return;
                }
                if case == 0 {
                    fs::write(&added, "x").unwrap();
                    fs::set_permissions(&added, fs::Permissions::from_mode(0o4755)).unwrap();
                } else {
                    fs::hard_link(&linked, &added).unwrap();
                }
            });

            let failure = slim_as_root(&layout, &bundle, &host, real_uid());

            assert_eq!(failure.unwrap_err(), (6, error.to_string()), "case {case}");
            assert_eq!(fs::read(binary_of(&bundle)).unwrap(), before, "case {case}");
            assert!(holdings_in(&layout).is_empty(), "case {case}");
            assert_eq!(fs::read_to_string(&outside).unwrap(), "x");
        }
    }

    /// The path and inode of each thing a hand-over of `top` would change. The inode is read
    /// through the descriptor and name the walk passes, so it is the thing that would change.
    fn handed(top: &Path) -> Result<Vec<(PathBuf, u64)>, String> {
        let c_top = CString::new(top.as_os_str().as_bytes()).unwrap();
        let folder = open_folder(None, &c_top).map_err(|e| e.to_string())?;
        let mut handed = Vec::new();
        walk_tree(&folder, top, &mut |fd, name, path, _| {
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            let name = name.map_or(c".".as_ptr(), CStr::as_ptr);
            if unsafe { libc::fstatat(fd, name, &mut stat, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            handed.push((path.to_path_buf(), stat.st_ino));
            Ok(())
        })?;
        Ok(handed)
    }

    /// Hands over the folder at `old`, opened as the hand-over after a slim opens it.
    fn hand_over_at(old: &Path, owner: u32, group: u32) -> Result<(), String> {
        let folder = open_checked(old, BundleId::of(old)?).ok_or(OLD_COPY_CHANGED)?;
        hand_over(&folder, old, owner, group)
    }

    fn changed_at(path: &Path) -> (i64, i64) {
        let meta = fs::metadata(path).unwrap();
        (meta.ctime(), meta.ctime_nsec())
    }

    #[test]
    fn the_hand_over_changes_the_old_copy_and_follows_no_link() {
        let (temp, layout) = slim_sandbox();
        let old = slim_bundle_at(holdings(&layout).join("1.0/Foo.app"));
        let elsewhere = temp.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        let outside = elsewhere.join("outside.txt");
        fs::write(&outside, "x").unwrap();
        std::os::unix::fs::symlink(&outside, old.join("Contents/to-file")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, old.join("Contents/to-folder")).unwrap();
        std::os::unix::fs::symlink(temp.path().join("gone"), old.join("Contents/dangling"))
            .unwrap();
        let before = [changed_at(&elsewhere), changed_at(&outside)];

        let handed = handed(&old).unwrap();
        let gid = unsafe { libc::getgid() };
        hand_over_at(&old, real_uid(), gid).unwrap();

        let paths: Vec<_> = handed.iter().map(|(path, _)| path.clone()).collect();
        assert!(paths.contains(&old.join("Contents/to-file")));
        assert!(paths.contains(&old.join("Contents/to-folder")));
        assert!(paths.contains(&old.join("Contents/MacOS/testapp")));
        assert_eq!(paths.last(), Some(&old));
        let inodes: Vec<_> = handed.iter().map(|(_, ino)| *ino).collect();
        for target in [&elsewhere, &outside] {
            let ino = fs::metadata(target).unwrap().ino();
            assert!(!inodes.contains(&ino), "{target:?} was walked");
        }
        assert_eq!(
            [changed_at(&elsewhere), changed_at(&outside)],
            before,
            "the hand-over changed a file a link points to"
        );
    }

    #[test]
    fn the_hand_over_works_for_the_own_group_and_fails_for_another() {
        let (_temp, layout) = slim_sandbox();
        let old = slim_bundle_at(holdings(&layout).join("1.0/Foo.app"));
        std::os::unix::fs::symlink("/nonexistent", old.join("Contents/dangling")).unwrap();
        let gid = unsafe { libc::getgid() };

        assert_eq!(hand_over_at(&old, real_uid(), gid), Ok(()));
        let error = hand_over_at(&old, real_uid(), 4_000_000);

        assert!(error.is_err());
    }

    #[test]
    fn an_old_copy_that_is_a_link_is_not_handed_over() {
        let (temp, layout) = slim_sandbox();
        let target = slim_bundle_at(temp.path().join("target/Foo.app"));
        let old = holdings(&layout).join("1.0/Foo.app");
        fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &old).unwrap();
        let before = changed_at(&target);

        let handed = handed(&old);
        let gid = unsafe { libc::getgid() };

        assert!(handed.is_err(), "{handed:?}");
        assert!(hand_over_at(&old, real_uid(), gid).is_err());
        assert_eq!(changed_at(&target), before);
    }

    #[test]
    fn a_hand_over_that_fails_still_moves_the_checked_old_copy_to_applications() {
        let (_temp, layout) = slim_sandbox();
        let bundle = slim_bundle_at(layout.system("Applications/Foo.app"));
        let host = slim_host();

        let done = slim_bundle(
            &layout,
            &bundle,
            real_uid(),
            4_000_000,
            &host,
            0,
            real_uid(),
        )
        .unwrap();

        let old_app = layout.system("Applications/Foo.taert-old.app");
        assert_eq!(done.old_app, old_app);
        assert!(done.handover_error.is_some());
        assert!(binary_of(&old_app).exists());
        assert!(holdings_in(&layout).is_empty());
    }

    fn applications(layout: &Layout) -> PathBuf {
        layout.system("Applications")
    }

    #[test]
    fn an_old_copy_that_is_not_the_checked_app_is_not_handed_over_or_moved() {
        let (temp, layout) = slim_sandbox();
        let checked = slim_bundle_at(layout.system("Applications/Foo.app"));
        let id = BundleId::of(&checked).unwrap();
        let theirs = slim_bundle_at(holdings(&layout).join("1.0/Foo.app"));
        let outside = temp.path().join("outside.txt");
        fs::write(&outside, "x").unwrap();
        fs::hard_link(&outside, theirs.join("Contents/MacOS/linked")).unwrap();
        let before = changed_at(&outside);
        let gid = unsafe { libc::getgid() };

        let left = release_old_copy(&theirs, id, real_uid(), gid, &applications(&layout), "Foo");

        assert_eq!(left, (theirs.clone(), Some(OLD_COPY_CHANGED.to_string())));
        assert_eq!(changed_at(&outside), before);
        assert!(binary_of(&theirs).exists());
        assert!(!layout.system("Applications/Foo.taert-old.app").exists());
    }

    #[test]
    fn a_hard_link_stops_the_hand_over_and_its_other_name_is_not_changed() {
        let (temp, layout) = slim_sandbox();
        let old = slim_bundle_at(holdings(&layout).join("1.0/Foo.app"));
        let id = BundleId::of(&old).unwrap();
        let outside = temp.path().join("outside.txt");
        fs::write(&outside, "x").unwrap();
        let linked = old.join("Contents/MacOS/linked");
        fs::hard_link(&outside, &linked).unwrap();
        let before = changed_at(&outside);
        let gid = unsafe { libc::getgid() };
        fs::create_dir(applications(&layout)).unwrap();

        let left = release_old_copy(&old, id, real_uid(), gid, &applications(&layout), "Foo");

        let error = format!(
            "Could not hand {} to you: it has another name (a hard link)",
            linked.display()
        );
        let moved = layout.system("Applications/Foo.taert-old.app");
        assert_eq!(left, (moved.clone(), Some(error)));
        assert_eq!(changed_at(&outside), before);
        assert!(binary_of(&moved).exists());
    }

    #[test]
    fn an_old_copy_that_cannot_be_moved_stays_in_the_holding_folder() {
        let (temp, layout) = slim_sandbox();
        let old = slim_bundle_at(holdings(&layout).join("1.0/Foo.app"));
        let id = BundleId::of(&old).unwrap();
        let closed = temp.path().join("closed");
        fs::create_dir(&closed).unwrap();
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o555)).unwrap();
        let gid = unsafe { libc::getgid() };

        let (left, error) = release_old_copy(&old, id, real_uid(), gid, &closed, "Foo");

        fs::set_permissions(&closed, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(left, old);
        assert!(error.unwrap().starts_with("Could not move"));
        assert!(binary_of(&old).exists());
    }

    #[test]
    fn a_move_that_fails_is_reported_and_the_old_copy_stays() {
        let (temp, layout) = slim_sandbox();
        let old = slim_bundle_at(holdings(&layout).join("1.0/Foo.app"));
        let closed = temp.path().join("closed");
        fs::create_dir(&closed).unwrap();
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o555)).unwrap();

        let moved = move_old_copy(&old, &closed, "Foo");

        fs::set_permissions(&closed, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(moved.unwrap_err().contains("Could not move"));
        assert!(binary_of(&old).exists());
        assert_eq!(fs::read_dir(&closed).unwrap().count(), 0);
    }

    #[test]
    fn the_slim_answer_is_one_json_object() {
        let (_temp, layout) = slim_sandbox();
        let bundle = slim_bundle_at(layout.system("Applications/Foo.app"));

        let done = SlimmedByRoot {
            saved: 7,
            old_app: PathBuf::from("/Applications/Foo.taert-old.app"),
            handover_error: None,
        };
        let value = |answer: String| serde_json::from_str::<serde_json::Value>(&answer).unwrap();

        assert_eq!(
            value(slim_answer(Ok(done.clone()))),
            serde_json::json!({
                "saved": 7,
                "old_app": "/Applications/Foo.taert-old.app",
            })
        );
        let with_error = SlimmedByRoot {
            handover_error: Some("denied".into()),
            ..done
        };
        assert_eq!(
            value(slim_answer(Ok(with_error)))["handover_error"],
            "denied"
        );
        assert_eq!(
            value(slim_answer(Err((3, "no arm".into())))),
            serde_json::json!({ "error": "no arm", "step": 3 })
        );
        let answer = run_slim(&layout.home, real_uid(), 20, &bundle);
        assert_eq!(
            value(answer),
            serde_json::json!({ "error": NOT_ROOT, "step": 1 })
        );
    }

    #[test]
    fn the_command_answers_a_user_process_with_a_json_error() {
        let (_temp, layout) = slim_sandbox();
        let bundle = bundle_at(layout.system("Applications/Foo.app"));
        let file = big_file(&bundle);

        let answer = run_compress(&layout.home, &bundle, false);

        let value: serde_json::Value = serde_json::from_str(&answer).unwrap();
        assert_eq!(value, serde_json::json!({ "error": NOT_ROOT }));
        assert!(!is_compressed(&file));
    }
}

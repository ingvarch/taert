//! Safety rules and path verification.
//!
//! Designed to prevent accidental deletion of critical system files,
//! root directories, credentials, cloud files, and symlinks.

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

use super::layout::Layout;
use super::stores::IgnoreStore;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    pub dev: u64,
    pub ino: u64,
}

impl FileIdentity {
    pub fn of(path: &Path) -> Option<Self> {
        let meta = fs::symlink_metadata(path).ok()?;
        Some(Self {
            dev: meta.dev(),
            ino: meta.ino(),
        })
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SafetyError {
    #[error("path is empty or root filesystem")]
    RootPath,
    #[error("path contains a symbolic link")]
    SymlinkDetected,
    #[error("path is a protected system root")]
    ProtectedRoot,
    #[error("path is within a protected system directory")]
    ProtectedSystemPath,
    #[error("path belongs to another user")]
    AnotherUserFiles,
    #[error("path is a credential or sensitive config directory")]
    CredentialRoot,
    #[error("path is managed by iCloud, CloudStorage, or is dataless")]
    CloudOrDataless,
    #[error("path is ignored or protected by an ignore-list ancestor")]
    Ignored,
    #[error("path does not exist")]
    NotFound,
    #[error("not a directory: {0}")]
    NotADirectory(String),
    #[error("path name is not safe to pass to a removal")]
    UnsafeName,
    #[error("path is not an app or a place apps keep their files")]
    OutsideUninstallScope,
}

/// Where a path allowed for uninstalling sits, which decides how it is removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UninstallScope {
    Bundle,
    Home,
    System,
}

/// Folders outside the home folder that hold one entry per app.
const SYSTEM_APP_FOLDERS: &[&str] = &[
    "Library/Application Support",
    "Library/Audio/Plug-Ins/Components",
    "Library/Audio/Plug-Ins/Digidesign",
    "Library/Audio/Plug-Ins/VST",
    "Library/Audio/Plug-Ins/VST3",
    "Library/Caches",
    "Library/Frameworks",
    "Library/Input Methods",
    "Library/Internet Plug-Ins",
    "Library/LaunchAgents",
    "Library/LaunchDaemons",
    "Library/Logs",
    "Library/Logs/DiagnosticReports",
    "Library/PreferencePanes",
    "Library/Preferences",
    "Library/PrivilegedHelperTools",
    "Library/QuickLook",
    "Library/Screen Savers",
    "Library/StartupItems",
    "Users/Shared",
];

/// Of those, the ones where a vendor keeps a folder per product.
const VENDOR_FOLDERS: &[&str] = &[
    "Library/Application Support",
    "Library/Caches",
    "Library/Logs",
];

/// Home folder locations that hold entries of many apps and never go themselves.
const HOME_APP_FOLDERS: &[&str] = &[
    "Applications",
    "Library/Application Scripts",
    "Library/Autosave Information",
    "Library/Cookies",
    "Library/Group Containers",
    "Library/HTTPStorages",
    "Library/LaunchAgents",
    "Library/Logs/DiagnosticReports",
    "Library/Preferences/ByHost",
    "Library/Saved Application State",
    "Library/WebKit",
    ".local",
    ".local/share",
];

/// Most nested an app sits below an app folder: `Vendor/Suite/Foo.app`.
const MAX_BUNDLE_DEPTH: usize = 3;

pub struct Safety;

impl Safety {
    pub fn home_dir() -> PathBuf {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
    }

    /// Denied roots for cloud/sync storage providers
    pub fn denied_cloud_roots() -> Vec<PathBuf> {
        Self::cloud_roots_in(&Self::home_dir())
    }

    fn cloud_roots_in(home: &Path) -> Vec<PathBuf> {
        vec![
            home.join("Library/Mobile Documents"),
            home.join("Library/CloudStorage"),
            home.join("Library/Application Support/FileProvider"),
            home.join("Library/Application Support/CloudDocs"),
            home.join("Library/Daemon Containers"),
            home.join("Library/Caches/CloudKit"),
            home.join("Library/Caches/com.apple.bird"),
            home.join("Library/Caches/com.apple.cloudkit"),
            home.join("Library/Caches/com.apple.cloudd"),
            home.join("Library/Caches/com.apple.FileProvider"),
        ]
    }

    /// Critical system and user roots that can NEVER be deleted
    pub fn critical_roots() -> HashSet<PathBuf> {
        Self::critical_roots_in(&Self::home_dir())
    }

    fn critical_roots_in(home: &Path) -> HashSet<PathBuf> {
        let mut roots = HashSet::new();

        let system_paths = [
            "/",
            "/System",
            "/Library",
            "/Applications",
            "/Users",
            "/usr",
            "/bin",
            "/sbin",
            "/etc",
            "/var",
            "/private",
        ];
        for p in system_paths {
            roots.insert(PathBuf::from(p));
        }

        roots.insert(home.to_path_buf());
        let home_roots = [
            "Library",
            "Library/Caches",
            "Library/Application Support",
            "Library/Containers",
            "Library/Preferences",
            "Library/Logs",
            "Library/Developer",
            "Library/Developer/Xcode",
            ".config",
            ".cache",
            "Documents",
            "Desktop",
            "Downloads",
        ];
        for hr in home_roots {
            roots.insert(home.join(hr));
        }

        roots
    }

    /// Denied user-level credential and sensitive config directories
    pub fn denied_user_roots() -> Vec<PathBuf> {
        Self::user_roots_in(&Self::home_dir())
    }

    fn user_roots_in(home: &Path) -> Vec<PathBuf> {
        let dirs = [
            ".ssh",
            ".aws",
            ".gnupg",
            ".gpg",
            ".kube",
            ".docker",
            ".claude",
            ".config",
            ".cargo",
            ".rustup",
            ".gem",
            ".nvm",
            ".pyenv",
            ".rbenv",
            ".ollama",
            ".lmstudio",
        ];
        dirs.iter().map(|d| home.join(d)).collect()
    }

    /// Whitelisted cache paths under credential roots (e.g. ~/.cargo/registry/cache is ok, ~/.cargo itself is not)
    pub fn allowed_credential_subpaths() -> Vec<PathBuf> {
        Self::credential_subpaths_in(&Self::home_dir())
    }

    fn credential_subpaths_in(home: &Path) -> Vec<PathBuf> {
        vec![
            home.join(".docker/cli-plugins/.cache"),
            home.join(".docker/buildx/cache"),
            home.join(".cargo/registry/cache"),
            home.join(".cargo/registry/index"),
            home.join(".cargo/git/db"),
            home.join(".cargo/git/checkouts"),
            home.join(".ollama/logs"),
            home.join(".lmstudio/server-logs"),
        ]
    }

    /// Check if path or any of its parent components is a symlink (excluding /tmp and /var)
    pub fn has_symlink_component(path: &Path) -> bool {
        let mut curr = path.to_path_buf();
        while let Some(parent) = curr.parent() {
            if curr == Path::new("/") || curr == Path::new("/tmp") || curr == Path::new("/var") {
                break;
            }
            if let Ok(meta) = fs::symlink_metadata(&curr)
                && meta.file_type().is_symlink()
            {
                return true;
            }
            curr = parent.to_path_buf();
        }
        false
    }

    /// Check if path is inside cloud storage or has dataless flag (SF_DATALESS on APFS)
    pub fn is_cloud_or_dataless(path: &Path) -> bool {
        Self::is_cloud_or_dataless_in(path, &Self::home_dir())
    }

    fn is_cloud_or_dataless_in(path: &Path, home: &Path) -> bool {
        let path_str = path.to_string_lossy();
        if path_str.contains("com~apple~") {
            return true;
        }

        for cloud_root in Self::cloud_roots_in(home) {
            if path == cloud_root || path.starts_with(&cloud_root) {
                return true;
            }
        }

        #[cfg(target_os = "macos")]
        {
            use std::ffi::CString;
            use std::mem::MaybeUninit;
            if let Ok(c_path) = CString::new(path_str.as_bytes()) {
                let mut stat_buf = MaybeUninit::<libc::stat>::uninit();
                let res = unsafe { libc::lstat(c_path.as_ptr(), stat_buf.as_mut_ptr()) };
                if res == 0 {
                    let stat = unsafe { stat_buf.assume_init() };
                    // SF_DATALESS flag on macOS is 0x40000000
                    if stat.st_flags & 0x4000_0000 != 0 {
                        return true;
                    }
                }
            }
        }

        false
    }

    /// Check if path is a credential root (or sensitive dir) and not whitelisted
    pub fn is_credential_root(path: &Path) -> bool {
        Self::is_credential_root_in(path, &Self::home_dir())
    }

    fn is_credential_root_in(path: &Path, home: &Path) -> bool {
        for allowed in Self::credential_subpaths_in(home) {
            if path == allowed || path.starts_with(&allowed) {
                return false;
            }
        }

        for denied in Self::user_roots_in(home) {
            if path == denied || path.starts_with(&denied) {
                return true;
            }
        }

        false
    }

    /// Check if an ignore store protects the candidate path (ancestor or descendant)
    pub fn is_ignored_or_protected(path: &Path, ignore: &IgnoreStore) -> bool {
        let path_buf = path.to_path_buf();
        for ignored_str in &ignore.roots {
            let ignored_path = PathBuf::from(ignored_str);
            if path_buf == ignored_path
                || path_buf.starts_with(&ignored_path)
                || ignored_path.starts_with(&path_buf)
            {
                return true;
            }
        }
        false
    }

    /// Primary safety validator: can this path be safely cleaned/removed?
    pub fn can_remove(path: &Path, ignore: &IgnoreStore) -> Result<(), SafetyError> {
        if path.as_os_str().is_empty() || path == Path::new("/") {
            return Err(SafetyError::RootPath);
        }

        if !path.exists() {
            return Err(SafetyError::NotFound);
        }

        if Self::has_symlink_component(path) {
            return Err(SafetyError::SymlinkDetected);
        }

        let canonical = match path.canonicalize() {
            Ok(c) => c,
            Err(_) => path.to_path_buf(),
        };

        if canonical == Path::new("/") {
            return Err(SafetyError::RootPath);
        }

        let critical = Self::critical_roots();
        if critical.contains(path) || critical.contains(&canonical) {
            return Err(SafetyError::ProtectedRoot);
        }

        let denied_trees = [
            "/System",
            "/Library",
            "/Applications",
            "/usr",
            "/bin",
            "/sbin",
            "/etc",
            "/private/etc",
            "/cores",
        ];
        for tree in denied_trees {
            let tree_path = Path::new(tree);
            if canonical == tree_path || canonical.starts_with(tree_path) {
                return Err(SafetyError::ProtectedSystemPath);
            }
        }

        // Another user check: under /Users/ but not current home
        let home = Self::home_dir();
        if canonical.starts_with("/Users/") && canonical != home && !canonical.starts_with(&home) {
            return Err(SafetyError::AnotherUserFiles);
        }

        if Self::is_credential_root(path) || Self::is_credential_root(&canonical) {
            return Err(SafetyError::CredentialRoot);
        }

        if Self::is_cloud_or_dataless(path) || Self::is_cloud_or_dataless(&canonical) {
            return Err(SafetyError::CloudOrDataless);
        }

        if Self::is_ignored_or_protected(path, ignore)
            || Self::is_ignored_or_protected(&canonical, ignore)
        {
            return Err(SafetyError::Ignored);
        }

        Ok(())
    }

    /// Can an uninstall remove this path? Stricter than `can_remove` in what it
    /// accepts outside the home folder, and it opens /Applications and /Library.
    pub fn can_uninstall(
        path: &Path,
        ignore: &IgnoreStore,
        layout: &Layout,
    ) -> Result<UninstallScope, SafetyError> {
        if path.as_os_str().is_empty() || path == Path::new("/") {
            return Err(SafetyError::RootPath);
        }
        let climbs = path.components().any(|c| c == Component::ParentDir);
        let has_control = path.to_string_lossy().chars().any(char::is_control);
        if !path.is_absolute() || climbs || has_control {
            return Err(SafetyError::UnsafeName);
        }
        if fs::symlink_metadata(path).is_err() {
            return Err(SafetyError::NotFound);
        }

        let is_bundle = Self::is_app_bundle(path, layout);
        let checked_for_links = if is_bundle {
            path.parent().unwrap_or(path)
        } else {
            path
        };
        if Self::has_symlink_component(checked_for_links) {
            return Err(SafetyError::SymlinkDetected);
        }

        let home = &layout.home;
        let shared_folder = HOME_APP_FOLDERS.iter().any(|rel| path == home.join(rel));
        if shared_folder || Self::critical_roots_in(home).contains(path) {
            return Err(SafetyError::ProtectedRoot);
        }
        // ~/.config/<app> is that app's own. Everything deeper keeps its protection.
        let in_xdg_config = path.parent() == Some(home.join(".config").as_path());
        if !in_xdg_config && Self::is_credential_root_in(path, home) {
            return Err(SafetyError::CredentialRoot);
        }
        if Self::is_cloud_or_dataless_in(path, home) {
            return Err(SafetyError::CloudOrDataless);
        }
        if Self::is_ignored_or_protected(path, ignore) {
            return Err(SafetyError::Ignored);
        }

        if is_bundle {
            Ok(UninstallScope::Bundle)
        } else if layout.is_user_path(path) {
            Ok(UninstallScope::Home)
        } else {
            Self::system_scope(path, layout)
        }
    }

    fn is_app_bundle(path: &Path, layout: &Layout) -> bool {
        let is_app = |part: &std::ffi::OsStr| {
            Path::new(part)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("app"))
        };
        is_app(path.as_os_str())
            && layout.app_roots().iter().any(|root| {
                path.strip_prefix(root).is_ok_and(|inside| {
                    let parts: Vec<_> = inside.iter().collect();
                    let folders = &parts[..parts.len().saturating_sub(1)];
                    parts.len() <= MAX_BUNDLE_DEPTH && !folders.iter().any(|part| is_app(part))
                })
            })
    }

    fn system_scope(path: &Path, layout: &Layout) -> Result<UninstallScope, SafetyError> {
        let inside = |rel: &&str| path.parent() == Some(layout.system(rel).as_path());
        let in_vendor =
            |rel: &&str| path.parent().and_then(Path::parent) == Some(layout.system(rel).as_path());
        let is_folder = SYSTEM_APP_FOLDERS
            .iter()
            .any(|rel| path == layout.system(rel));
        let allowed = SYSTEM_APP_FOLDERS.iter().any(inside) || VENDOR_FOLDERS.iter().any(in_vendor);
        if is_folder || !allowed {
            return Err(SafetyError::OutsideUninstallScope);
        }

        let below_root = path.strip_prefix(&layout.root).unwrap_or(path);
        let is_apple = below_root
            .iter()
            .any(|part| part.to_string_lossy().starts_with("com.apple."));
        if is_apple {
            return Err(SafetyError::ProtectedSystemPath);
        }
        Ok(UninstallScope::System)
    }

    /// Check if path is valid to scan as a project root (for purge and analyze)
    pub fn validate_scan_root(path: &Path) -> Result<PathBuf, SafetyError> {
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(path)
        };

        if Self::has_symlink_component(&abs) {
            return Err(SafetyError::SymlinkDetected);
        }

        if !abs.is_dir() {
            return Err(SafetyError::NotADirectory(abs.display().to_string()));
        }

        let canonical = abs.canonicalize().unwrap_or_else(|_| abs.clone());
        let home = Self::home_dir();

        let mut forbidden_roots = vec![
            PathBuf::from("/"),
            PathBuf::from("/System"),
            PathBuf::from("/Library"),
            PathBuf::from("/Applications"),
            PathBuf::from("/Users"),
            PathBuf::from("/private"),
            PathBuf::from("/usr"),
            PathBuf::from("/bin"),
            PathBuf::from("/sbin"),
            PathBuf::from("/etc"),
            PathBuf::from("/var"),
            PathBuf::from("/opt"),
            PathBuf::from("/cores"),
            PathBuf::from("/Volumes"),
            home.clone(),
        ];

        for standard in ["Library", "Pictures", "Music", "Movies", "Public"] {
            forbidden_roots.push(home.join(standard));
        }

        for forbidden in forbidden_roots {
            if canonical == forbidden {
                return Err(SafetyError::ProtectedRoot);
            }
        }

        let denied_trees = [
            PathBuf::from("/System"),
            PathBuf::from("/Library"),
            PathBuf::from("/Applications"),
            PathBuf::from("/usr"),
            PathBuf::from("/bin"),
            PathBuf::from("/sbin"),
            PathBuf::from("/etc"),
            PathBuf::from("/cores"),
            home.join("Library"),
            home.join("Pictures"),
            home.join("Music"),
            home.join("Movies"),
            home.join("Public"),
        ];

        for tree in denied_trees {
            if canonical == tree || canonical.starts_with(&tree) {
                return Err(SafetyError::ProtectedSystemPath);
            }
        }

        if Self::is_cloud_or_dataless(&canonical) {
            return Err(SafetyError::CloudOrDataless);
        }

        Ok(canonical)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::tempdir;

    #[test]
    fn test_critical_roots_cannot_be_removed() {
        let ignore = IgnoreStore::default();
        assert_eq!(
            Safety::can_remove(Path::new("/"), &ignore),
            Err(SafetyError::RootPath)
        );
        assert_eq!(
            Safety::can_remove(Path::new("/System"), &ignore),
            Err(SafetyError::ProtectedRoot)
        );
        assert_eq!(
            Safety::can_remove(&Safety::home_dir(), &ignore),
            Err(SafetyError::ProtectedRoot)
        );
    }

    #[test]
    fn test_can_remove_temp_file() {
        let temp = tempdir().unwrap();
        let file_path = temp.path().join("cache_file.tmp");
        File::create(&file_path).unwrap();

        let ignore = IgnoreStore::default();
        assert!(Safety::can_remove(&file_path, &ignore).is_ok());
    }

    #[test]
    fn test_ignored_paths_are_protected() {
        let temp = tempdir().unwrap();
        let target_dir = temp.path().join("build_output");
        fs::create_dir_all(&target_dir).unwrap();

        let mut ignore = IgnoreStore::default();
        ignore.add(target_dir.to_str().unwrap().to_string());

        assert_eq!(
            Safety::can_remove(&target_dir, &ignore),
            Err(SafetyError::Ignored)
        );

        let sub_file = target_dir.join("artifact.bin");
        File::create(&sub_file).unwrap();
        assert_eq!(
            Safety::can_remove(&sub_file, &ignore),
            Err(SafetyError::Ignored)
        );
    }

    #[test]
    fn test_symlink_protection() {
        let temp = tempdir().unwrap();
        let real_dir = temp.path().join("real_dir");
        fs::create_dir_all(&real_dir).unwrap();

        let symlink_path = temp.path().join("link_to_dir");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real_dir, &symlink_path).unwrap();

        let ignore = IgnoreStore::default();
        assert_eq!(
            Safety::can_remove(&symlink_path, &ignore),
            Err(SafetyError::SymlinkDetected)
        );
    }

    fn sandbox() -> (tempfile::TempDir, Layout) {
        let temp = tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let layout = Layout {
            home: base.join("home"),
            root: base.join("root"),
        };
        fs::create_dir_all(&layout.home).unwrap();
        fs::create_dir_all(&layout.root).unwrap();
        (temp, layout)
    }

    fn make(path: &Path) -> PathBuf {
        fs::create_dir_all(path).unwrap();
        path.to_path_buf()
    }

    fn uninstall_check(path: &Path, layout: &Layout) -> Result<UninstallScope, SafetyError> {
        Safety::can_uninstall(path, &IgnoreStore::default(), layout)
    }

    #[test]
    fn an_app_bundle_in_an_app_folder_can_be_uninstalled() {
        let (_temp, layout) = sandbox();
        let app = make(&layout.system("Applications/Foo.app"));
        let user_app = make(&layout.user("Applications/Bar.app"));
        let nested = make(&layout.system("Applications/Vendor/Baz.app"));

        assert_eq!(uninstall_check(&app, &layout), Ok(UninstallScope::Bundle));
        assert_eq!(
            uninstall_check(&user_app, &layout),
            Ok(UninstallScope::Bundle)
        );
        assert_eq!(
            uninstall_check(&nested, &layout),
            Ok(UninstallScope::Bundle)
        );
    }

    #[test]
    fn a_link_to_an_app_can_go_but_only_as_a_bundle() {
        let (_temp, layout) = sandbox();
        let target = make(&layout.user("elsewhere/Foo.app"));
        make(&layout.system("Applications"));
        let link = layout.system("Applications/Foo.app");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let data = layout.user("Library/Caches/Foo");
        make(&layout.user("Library/Caches"));
        std::os::unix::fs::symlink(&target, &data).unwrap();

        assert_eq!(uninstall_check(&link, &layout), Ok(UninstallScope::Bundle));
        assert_eq!(
            uninstall_check(&data, &layout),
            Err(SafetyError::SymlinkDetected)
        );
    }

    #[test]
    fn a_folder_of_apps_or_anything_else_in_it_cannot_be_uninstalled() {
        let (_temp, layout) = sandbox();
        let vendor = make(&layout.system("Applications/Vendor"));
        let inner = make(&layout.system("Applications/Foo.app/Contents/Helper.app"));
        let plain = make(&layout.system("Applications/notes.txt.d"));

        for path in [&vendor, &inner, &plain] {
            assert_eq!(
                uninstall_check(path, &layout),
                Err(SafetyError::OutsideUninstallScope),
                "{path:?}"
            );
        }
    }

    #[test]
    fn system_files_are_allowed_only_right_below_the_folders_apps_use() {
        let (_temp, layout) = sandbox();
        let support = make(&layout.system("Library/Application Support/Foo"));
        let helper = make(&layout.system("Library/PrivilegedHelperTools/com.foo.helper"));
        let shared = make(&layout.system("Users/Shared/Foo"));
        let vendor_product = make(&layout.system("Library/Application Support/Avid/Sibelius"));
        let deep = make(&layout.system("Library/Application Support/Avid/Sibelius/Data"));
        let other = make(&layout.system("Library/Keychains/foo"));
        let parent = layout.system("Library/Application Support");

        for path in [&support, &helper, &shared, &vendor_product] {
            assert_eq!(
                uninstall_check(path, &layout),
                Ok(UninstallScope::System),
                "{path:?}"
            );
        }
        for path in [&deep, &other, &parent] {
            assert_eq!(
                uninstall_check(path, &layout),
                Err(SafetyError::OutsideUninstallScope),
                "{path:?}"
            );
        }
    }

    #[test]
    fn apple_files_outside_the_home_folder_stay() {
        let (_temp, layout) = sandbox();
        let apple = make(&layout.system("Library/Application Support/com.apple.TCC"));

        assert_eq!(
            uninstall_check(&apple, &layout),
            Err(SafetyError::ProtectedSystemPath)
        );
    }

    #[test]
    fn a_home_folder_item_keeps_the_rules_for_credentials_and_cloud_files() {
        let (_temp, layout) = sandbox();
        let cache = make(&layout.user("Library/Caches/Foo"));
        let ssh = make(&layout.user(".ssh/keys"));
        let icloud = make(&layout.user("Library/Mobile Documents/foo"));
        let xdg = make(&layout.user(".config/foo"));
        let xdg_root = layout.user(".config");
        let library = layout.user("Library/Caches");

        assert_eq!(uninstall_check(&cache, &layout), Ok(UninstallScope::Home));
        assert_eq!(uninstall_check(&xdg, &layout), Ok(UninstallScope::Home));
        assert_eq!(
            uninstall_check(&ssh, &layout),
            Err(SafetyError::CredentialRoot)
        );
        assert_eq!(
            uninstall_check(&icloud, &layout),
            Err(SafetyError::CloudOrDataless)
        );
        assert_eq!(
            uninstall_check(&xdg_root, &layout),
            Err(SafetyError::ProtectedRoot)
        );
        assert_eq!(
            uninstall_check(&library, &layout),
            Err(SafetyError::ProtectedRoot)
        );
    }

    #[test]
    fn the_ignore_list_protects_an_app_and_its_files() {
        let (_temp, layout) = sandbox();
        let app = make(&layout.system("Applications/Foo.app"));
        let mut ignore = IgnoreStore::default();
        ignore.add(app.display().to_string());

        assert_eq!(
            Safety::can_uninstall(&app, &ignore, &layout),
            Err(SafetyError::Ignored)
        );
    }

    #[test]
    fn a_name_that_could_break_a_command_is_refused() {
        let (_temp, layout) = sandbox();
        let newline = make(&layout.user("Library/Caches/foo\nbar"));
        let control = layout.user("Library/Caches/foo\u{7}bar");
        fs::create_dir_all(&control).unwrap();
        let climbing = layout.user("Library/Caches/../../root");

        assert_eq!(
            uninstall_check(&newline, &layout),
            Err(SafetyError::UnsafeName)
        );
        assert_eq!(
            uninstall_check(&control, &layout),
            Err(SafetyError::UnsafeName)
        );
        assert_eq!(
            uninstall_check(&climbing, &layout),
            Err(SafetyError::UnsafeName)
        );
    }

    #[test]
    fn nothing_outside_the_known_places_can_be_uninstalled() {
        let (_temp, layout) = sandbox();
        let stray = make(&layout.root.join("opt/foo"));

        assert_eq!(
            uninstall_check(&stray, &layout),
            Err(SafetyError::OutsideUninstallScope)
        );
        assert_eq!(
            uninstall_check(&layout.system("Applications/Missing.app"), &layout),
            Err(SafetyError::NotFound)
        );
    }

    #[test]
    fn test_credential_root_protection() {
        let home = Safety::home_dir();
        let ssh_dir = home.join(".ssh");
        let ignore = IgnoreStore::default();
        if ssh_dir.exists() {
            assert!(matches!(
                Safety::can_remove(&ssh_dir, &ignore),
                Err(SafetyError::CredentialRoot) | Err(SafetyError::ProtectedRoot)
            ));
        }
    }
}

//! Safety rules and path verification.
//!
//! Designed to prevent accidental deletion of critical system files,
//! root directories, credentials, cloud files, and symlinks.

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use thiserror::Error;

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
}

pub struct Safety;

impl Safety {
    pub fn home_dir() -> PathBuf {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
    }

    /// Denied roots for cloud/sync storage providers
    pub fn denied_cloud_roots() -> Vec<PathBuf> {
        let home = Self::home_dir();
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
        let home = Self::home_dir();
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

        roots.insert(home.clone());
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
        let home = Self::home_dir();
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
        let home = Self::home_dir();
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
        let path_str = path.to_string_lossy();
        if path_str.contains("com~apple~") {
            return true;
        }

        for cloud_root in Self::denied_cloud_roots() {
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
        for allowed in Self::allowed_credential_subpaths() {
            if path == allowed || path.starts_with(&allowed) {
                return false;
            }
        }

        for denied in Self::denied_user_roots() {
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

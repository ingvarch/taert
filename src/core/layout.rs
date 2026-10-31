//! Where a Mac keeps apps and their files. Tests point it at a temporary folder.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::safety::Safety;

fn user_cache_dir() -> Option<PathBuf> {
    let output = Command::new("/usr/bin/getconf")
        .arg("DARWIN_USER_CACHE_DIR")
        .output()
        .ok()?;
    let printed = String::from_utf8(output.stdout).ok()?;
    PathBuf::from(printed.trim()).canonicalize().ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub home: PathBuf,
    pub root: PathBuf,
    /// The per-user cache folder of macOS, under /private/var/folders.
    pub user_cache: Option<PathBuf>,
}

impl Layout {
    pub fn current() -> Self {
        Self {
            home: Safety::home_dir(),
            root: PathBuf::from("/"),
            user_cache: user_cache_dir(),
        }
    }

    pub fn user(&self, relative: &str) -> PathBuf {
        self.home.join(relative)
    }

    pub fn system(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    /// Folders that hold app bundles, external volumes included.
    pub fn app_roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![
            self.system("Applications"),
            self.user("Applications"),
            self.system("Library/Input Methods"),
            self.user("Library/Input Methods"),
        ];
        roots.extend(self.volume_app_roots());
        roots
    }

    /// Folders that can hold a copy of an app without it being listed, with how
    /// deep to look in each.
    pub fn copy_roots(&self) -> Vec<(PathBuf, usize)> {
        let mut roots = vec![
            (
                self.user("Library/Application Support/Setapp/Applications"),
                1,
            ),
            (self.system("opt/homebrew/Caskroom"), 3),
            (self.system("usr/local/Caskroom"), 3),
        ];
        roots.extend(self.volumes().into_iter().map(|volume| (volume, 1)));
        roots
    }

    fn volumes(&self) -> Vec<PathBuf> {
        let Ok(volumes) = std::fs::read_dir(self.system("Volumes")) else {
            return Vec::new();
        };
        volumes
            .flatten()
            .map(|volume| volume.path())
            // The boot volume is linked here and only repeats /Applications.
            .filter(|volume| !volume.is_symlink())
            .collect()
    }

    fn volume_app_roots(&self) -> Vec<PathBuf> {
        self.volumes()
            .into_iter()
            .map(|volume| volume.join("Applications"))
            // Disk images link to /Applications for dragging an app in.
            .filter(|apps| apps.is_dir() && !apps.is_symlink())
            .collect()
    }

    pub fn is_user_path(&self, path: &Path) -> bool {
        path.starts_with(&self.home) || self.is_user_cache_entry(path)
    }

    fn is_user_cache_entry(&self, path: &Path) -> bool {
        self.user_cache
            .as_deref()
            .is_some_and(|cache| path.parent() == Some(cache))
    }
}

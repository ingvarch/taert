//! Where a Mac keeps apps and their files. Tests point it at a temporary folder.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::safety::Safety;
use super::stores::Config;

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

    fn data_dir(&self) -> PathBuf {
        self.user("Library/Application Support/taert")
    }

    pub fn config_path(&self) -> PathBuf {
        Config::path_in(&self.data_dir())
    }

    pub fn ignore_path(&self) -> PathBuf {
        self.data_dir().join("ignore.toml")
    }

    pub fn projects_path(&self) -> PathBuf {
        self.data_dir().join("projects")
    }

    pub fn history_path(&self) -> PathBuf {
        self.user(".config/taert/history.jsonl")
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
        let mut roots = vec![(
            self.user("Library/Application Support/Setapp/Applications"),
            1,
        )];
        roots.extend(
            self.homebrew_prefixes()
                .into_iter()
                .map(|prefix| (prefix.join("Caskroom"), 3)),
        );
        roots.extend(self.volumes().into_iter().map(|volume| (volume, 1)));
        roots
    }

    /// Where Homebrew can live, Apple Silicon first.
    pub fn homebrew_prefixes(&self) -> Vec<PathBuf> {
        vec![self.system("opt/homebrew"), self.system("usr/local")]
    }

    pub fn volumes(&self) -> Vec<PathBuf> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::history::HistoryLog;
    use crate::core::projects_store::ProjectFoldersStore;
    use crate::core::stores::{Config, IgnoreStore};

    fn layout_at(home: &str) -> Layout {
        Layout {
            home: PathBuf::from(home),
            root: PathBuf::from("/"),
            user_cache: None,
        }
    }

    #[test]
    fn copy_roots_hold_the_caskroom_of_each_prefix() {
        let (_temp, layout) = crate::core::apps::testing::sandbox();

        assert_eq!(
            layout.copy_roots(),
            [
                (
                    layout
                        .home
                        .join("Library/Application Support/Setapp/Applications"),
                    1
                ),
                (layout.root.join("opt/homebrew/Caskroom"), 3),
                (layout.root.join("usr/local/Caskroom"), 3),
            ]
        );
    }

    #[test]
    fn config_and_history_live_under_the_home_of_the_layout() {
        let layout = layout_at("/sandbox");

        assert_eq!(
            layout.config_path(),
            PathBuf::from("/sandbox/Library/Application Support/taert/config.toml")
        );
        assert_eq!(
            layout.history_path(),
            PathBuf::from("/sandbox/.config/taert/history.jsonl")
        );
    }

    #[test]
    fn ignore_list_and_project_folders_live_next_to_the_config() {
        let layout = layout_at("/sandbox");

        assert_eq!(
            layout.ignore_path(),
            PathBuf::from("/sandbox/Library/Application Support/taert/ignore.toml")
        );
        assert_eq!(
            layout.projects_path(),
            PathBuf::from("/sandbox/Library/Application Support/taert/projects")
        );
    }

    #[test]
    fn a_config_json_is_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_at(temp.path().to_str().unwrap());
        let dir = layout.config_path().parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.json"), r#"{"older_than_days": 3}"#).unwrap();

        assert!(layout.config_path().ends_with("config.toml"));
        let config = Config::load_from(&layout.config_path()).unwrap();
        assert_eq!(config.older_than_days, 7);
    }

    #[test]
    fn the_current_layout_agrees_with_the_default_paths() {
        let layout = Layout::current();

        assert_eq!(Some(layout.config_path()), Config::default_path());
        assert_eq!(layout.history_path(), HistoryLog::default_path());
        assert_eq!(Some(layout.ignore_path()), IgnoreStore::default_path());
        assert_eq!(
            Some(layout.projects_path()),
            ProjectFoldersStore::default_path()
        );
    }
}

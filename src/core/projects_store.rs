//! Project folders store.
//! Persists project search roots in ~/.config/taert/projects (one per line).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::path_utils::expand_tilde;
use super::safety::Safety;

#[derive(Debug, Clone)]
pub struct ProjectFoldersStore {
    pub roots: Vec<String>,
}

impl Default for ProjectFoldersStore {
    fn default() -> Self {
        Self::new(Self::default_roots())
    }
}

impl ProjectFoldersStore {
    pub fn new(roots: Vec<String>) -> Self {
        Self { roots }
    }

    pub fn default_roots() -> Vec<String> {
        let home = Safety::home_dir();
        ["Projects", "Code", "dev", "GitHub", "Workspace", "Repos"]
            .iter()
            .map(|folder| home.join(folder).display().to_string())
            .collect()
    }

    pub fn default_path() -> Option<PathBuf> {
        dirs::config_dir().map(|p| p.join("taert").join("projects"))
    }

    pub fn load_or_default() -> Self {
        if let Some(p) = Self::default_path()
            && p.exists()
            && let Ok(store) = Self::load_from(&p)
        {
            return store;
        }
        Self::default()
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read project folders from {}", path.display()))?;
        let roots: Vec<String> = content
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .map(|l| expand_tilde(l).display().to_string())
            .collect();

        if roots.is_empty() {
            Ok(Self::default())
        } else {
            Ok(Self { roots })
        }
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = self.roots.join("\n") + "\n";
        let temp_path = path.with_extension("tmp");
        fs::write(&temp_path, content)?;
        fs::rename(&temp_path, path)?;
        Ok(())
    }

    pub fn add(&mut self, path: String) -> bool {
        let expanded = expand_tilde(&path).display().to_string();
        let trimmed = expanded.trim_end_matches('/').to_string();
        if !self.roots.contains(&trimmed) {
            self.roots.push(trimmed);
            true
        } else {
            false
        }
    }

    pub fn remove(&mut self, path: &str) -> bool {
        let trimmed = path.trim_end_matches('/');
        let initial_len = self.roots.len();
        self.roots
            .retain(|r| r.as_str() != path && r.as_str() != trimmed);
        self.roots.len() < initial_len
    }

    pub fn existing_roots(&self) -> Vec<PathBuf> {
        self.roots
            .iter()
            .map(|r| expand_tilde(r))
            .filter(|p| p.is_dir())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_project_folders_store_crud() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("projects");

        let mut store = ProjectFoldersStore::new(vec!["/tmp/proj1".to_string()]);
        assert!(store.add("/tmp/proj2".to_string()));
        assert!(!store.add("/tmp/proj2".to_string())); // duplicate
        assert_eq!(store.roots.len(), 2);

        store.save_to(&file).unwrap();

        let loaded = ProjectFoldersStore::load_from(&file).unwrap();
        assert_eq!(loaded.roots, vec!["/tmp/proj1", "/tmp/proj2"]);

        let mut store2 = loaded;
        assert!(store2.remove("/tmp/proj1"));
        assert!(!store2.remove("/tmp/proj1"));
        assert_eq!(store2.roots, vec!["/tmp/proj2"]);
    }

    #[test]
    fn test_project_folders_tilde_expansion() {
        let home = Safety::home_dir();
        let mut store = ProjectFoldersStore::new(vec!["~/MyProjects".to_string()]);
        assert_eq!(store.existing_roots().len(), 0); // does not exist yet

        // Add with tilde
        store.add("~/OtherProjects".to_string());
        assert_eq!(
            store.roots[1],
            home.join("OtherProjects").display().to_string()
        );
    }
}

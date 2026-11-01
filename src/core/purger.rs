//! Project artifact scanner (purger).
//! Finds build output directories, dependencies, and caches (e.g. node_modules, target, .venv)
//! and groups them by project with customizable age threshold.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, SystemTime};

use super::path_utils::{expand_tilde, prettify_path};
use super::scanner::{CategoryScan, ScanItem, ToolGroup};
use super::sizer::{DirSizer, walk_parallelism};
use super::stores::IgnoreStore;

pub const SCAN_ID: &str = "purge";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PurgeScanEvent {
    Root(String),
    Walking(String),
    Found { bytes: u64 },
}

const STANDARD_ARTIFACTS: &[&str] = &[
    "node_modules",
    ".next",
    ".nuxt",
    ".turbo",
    ".svelte-kit",
    ".angular",
    ".parcel-cache",
    "target",
    ".build",
    "DerivedData",
    "Pods",
    ".venv",
    "venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".tox",
    ".ruff_cache",
    ".gradle",
    "build",
    "dist",
    "coverage",
];

const ARTIFACT_PREFIXES: &[&str] = &["cmake-build"];

pub struct Purger;

impl Purger {
    pub fn is_artifact(name: &str, artifact_names: &[String], parent: &Path) -> bool {
        if ARTIFACT_PREFIXES.iter().any(|&p| name.starts_with(p)) {
            return true;
        }
        if artifact_names.iter().any(|a| a == name) {
            return Self::is_valid_artifact_marker(name, parent);
        }
        false
    }

    /// Check whether a folder looks like an intentional artifact directory.
    /// Only generic names like `build` and `dist` require build system marker files nearby.
    fn is_valid_artifact_marker(artifact_name: &str, parent: &Path) -> bool {
        match artifact_name {
            "build" | "dist" => {
                parent.join("package.json").exists()
                    || parent.join("CMakeLists.txt").exists()
                    || parent.join("Makefile").exists()
                    || parent.join("build.gradle").exists()
                    || parent.join("build.gradle.kts").exists()
            }
            _ => true,
        }
    }

    fn project_label(path: &Path, root: &Path) -> String {
        if let Ok(rel) = path.strip_prefix(root)
            && let Some(first_comp) = rel.components().next()
        {
            let s = first_comp.as_os_str().to_string_lossy();
            if s == path.file_name().unwrap_or_default().to_string_lossy() {
                return root
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "Projects".to_string());
            }
            return s.to_string();
        }
        path.parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "Projects".to_string())
    }

    pub fn scan_projects(
        root: &Path,
        older_than_days: u64,
        extra_artifacts: &[String],
        ignore: &IgnoreStore,
    ) -> CategoryScan {
        Self::scan_multiple_roots(
            &[root.to_path_buf()],
            older_than_days,
            extra_artifacts,
            ignore,
            None,
            None,
        )
    }

    pub fn scan_multiple_roots(
        roots: &[PathBuf],
        older_than_days: u64,
        extra_artifacts: &[String],
        ignore: &IgnoreStore,
        progress: Option<Sender<PurgeScanEvent>>,
        is_cancelled: Option<Arc<AtomicBool>>,
    ) -> CategoryScan {
        let mut artifact_names: Vec<String> =
            STANDARD_ARTIFACTS.iter().map(|s| s.to_string()).collect();
        for extra in extra_artifacts {
            if !artifact_names.contains(extra) {
                artifact_names.push(extra.clone());
            }
        }

        let now = SystemTime::now();
        let threshold = Duration::from_secs(older_than_days * 86400);

        let mut projects_map: HashMap<String, Vec<ScanItem>> = HashMap::new();

        let clean_roots: Vec<PathBuf> = roots
            .iter()
            .map(|r| expand_tilde(&r.to_string_lossy()))
            .filter(|r| r.exists())
            .collect();

        'root_loop: for root in &clean_roots {
            if let Some(ref cancelled) = is_cancelled
                && cancelled.load(Ordering::Relaxed)
            {
                break 'root_loop;
            }

            if let Some(ref tx) = progress {
                let _ = tx.send(PurgeScanEvent::Root(prettify_path(root)));
            }

            let artifact_names_filter = artifact_names.clone();
            let walker = jwalk::WalkDir::new(root)
                .parallelism(walk_parallelism())
                .skip_hidden(false)
                .follow_links(false)
                .max_depth(8)
                .process_read_dir(move |_depth, _path, _read_dir_state, children| {
                    children.retain(|entry_res| match entry_res {
                        Ok(entry) => {
                            if entry.file_type.is_symlink() {
                                return false;
                            }
                            let name = entry.file_name.to_string_lossy();
                            if name == ".git" {
                                return false;
                            }
                            true
                        }
                        Err(_) => false,
                    });

                    // If any child is an artifact directory, don't recurse into it
                    for entry in children.iter_mut().flatten() {
                        if entry.file_type.is_dir() {
                            let name = entry.file_name.to_string_lossy();
                            let parent = entry.parent_path();
                            if Purger::is_artifact(&name, &artifact_names_filter, parent) {
                                entry.read_children_path = None;
                            }
                        }
                    }
                });

            for entry_res in walker {
                if let Some(ref cancelled) = is_cancelled
                    && cancelled.load(Ordering::Relaxed)
                {
                    break 'root_loop;
                }

                let entry = match entry_res {
                    Ok(e) => e,
                    Err(_) => continue,
                };

                if !entry.file_type.is_dir() {
                    continue;
                }

                let path = entry.path();
                if let Some(ref tx) = progress {
                    let _ = tx.send(PurgeScanEvent::Walking(prettify_path(&path)));
                }

                let file_name = entry.file_name.to_string_lossy();
                let parent = match path.parent() {
                    Some(p) => p,
                    None => continue,
                };

                if Self::is_artifact(&file_name, &artifact_names, parent) {
                    if ignore.contains(&path.to_string_lossy()) {
                        continue;
                    }

                    let size = DirSizer::size_of(&path);
                    if size == 0 {
                        continue;
                    }

                    let modified = fs::metadata(&path).and_then(|m| m.modified()).ok();
                    let selected_by_age = match modified {
                        Some(m) => match now.duration_since(m) {
                            Ok(age) => age >= threshold,
                            Err(_) => false,
                        },
                        None => false,
                    };

                    let project_name = Self::project_label(&path, root);
                    let item = ScanItem::new(path, size, selected_by_age, false);
                    if let Some(ref tx) = progress {
                        let _ = tx.send(PurgeScanEvent::Found { bytes: size });
                    }
                    projects_map.entry(project_name).or_default().push(item);
                }
            }
        }

        let mut groups: Vec<ToolGroup> = projects_map
            .into_iter()
            .map(|(tool, items)| ToolGroup { tool, items })
            .collect();

        groups.sort_by(|a, b| a.tool.cmp(&b.tool));

        CategoryScan {
            notes: Vec::new(),
            id: SCAN_ID.to_string(),
            title: "Project Artifacts".to_string(),
            groups,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_purge_discovers_artifacts() {
        let temp = tempdir().unwrap();
        let project_dir = temp.path().join("my-rust-project");
        let target_dir = project_dir.join("target");
        fs::create_dir_all(&target_dir).unwrap();
        fs::write(project_dir.join("Cargo.toml"), "[package]").unwrap();
        fs::write(target_dir.join("binary"), b"123456").unwrap();

        let ignore = IgnoreStore::default();
        let scan = Purger::scan_projects(temp.path(), 0, &[], &ignore);

        assert_eq!(scan.groups.len(), 1);
        assert_eq!(scan.groups[0].tool, "my-rust-project");
        assert_eq!(scan.groups[0].items.len(), 1);
        assert!(scan.groups[0].items[0].size_bytes > 0);
    }
}

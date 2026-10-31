//! Scan item and category models.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::SystemTime;

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use super::catalog::Catalog;
use super::docker::DockerObject;
use super::leftovers::UninstallRole;
use super::path_utils::prettify_path;
use super::safety::{FileIdentity, Safety};
use super::sizer::DirSizer;
use super::stores::IgnoreStore;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanItem {
    pub path: PathBuf,
    pub size_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<SystemTime>,
    pub selected: bool,
    pub contents_only: bool,
    #[serde(skip)]
    pub identity: Option<FileIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker: Option<DockerObject>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<UninstallRole>,
}

impl ScanItem {
    pub fn new(path: PathBuf, size_bytes: u64, selected: bool, contents_only: bool) -> Self {
        let identity = FileIdentity::of(&path);
        let modified = fs::metadata(&path).and_then(|m| m.modified()).ok();
        Self {
            path,
            size_bytes,
            modified,
            selected,
            contents_only,
            identity,
            docker: None,
            role: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolGroup {
    pub tool: String,
    pub items: Vec<ScanItem>,
}

impl ToolGroup {
    pub fn total_selected_bytes(&self) -> u64 {
        self.items
            .iter()
            .filter(|i| i.selected)
            .map(|i| i.size_bytes)
            .sum()
    }

    pub fn total_all_bytes(&self) -> u64 {
        self.items.iter().map(|i| i.size_bytes).sum()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategoryScan {
    pub id: String,
    pub title: String,
    pub groups: Vec<ToolGroup>,
}

impl CategoryScan {
    pub fn total_selected_bytes(&self) -> u64 {
        self.groups.iter().map(|g| g.total_selected_bytes()).sum()
    }

    pub fn total_all_bytes(&self) -> u64 {
        self.groups.iter().map(|g| g.total_all_bytes()).sum()
    }

    pub fn selected_items(&self) -> Vec<&ScanItem> {
        self.groups
            .iter()
            .flat_map(|g| &g.items)
            .filter(|i| i.selected)
            .collect()
    }

    pub fn selected_items_mut(&mut self) -> Vec<&mut ScanItem> {
        self.groups
            .iter_mut()
            .flat_map(|g| &mut g.items)
            .filter(|i| i.selected)
            .collect()
    }
}

#[derive(Debug, Clone)]
pub enum ScanProgressEvent {
    CategoryStarted {
        title: String,
        index: usize,
        total: usize,
    },
    ToolStarted(String),
    ScanningPath(String),
    ItemFound {
        bytes: u64,
    },
    ToolFinished {
        tool: String,
        bytes_found: u64,
    },
}

pub struct CategoryScanner;

impl CategoryScanner {
    pub fn scan(
        category_id: &str,
        category_title: &str,
        ignore: &IgnoreStore,
        progress_tx: Option<Sender<ScanProgressEvent>>,
        is_cancelled: Option<Arc<AtomicBool>>,
    ) -> CategoryScan {
        let targets = Catalog::targets_for(category_id);

        let groups: Vec<ToolGroup> = targets
            .into_par_iter()
            .filter_map(|target| {
                if let Some(ref flag) = is_cancelled
                    && flag.load(Ordering::Relaxed)
                {
                    return None;
                }

                if let Some(ref tx) = progress_tx {
                    let _ = tx.send(ScanProgressEvent::ToolStarted(target.tool.clone()));
                }

                let paths_to_scan = if target.contents_only {
                    let mut expanded = Vec::new();
                    for p in &target.paths {
                        if !p.exists()
                            || Safety::has_symlink_component(p)
                            || Safety::is_cloud_or_dataless(p)
                            || Safety::is_ignored_or_protected(p, ignore)
                        {
                            continue;
                        }
                        if let Ok(entries) = fs::read_dir(p) {
                            for entry in entries.flatten() {
                                expanded.push(entry.path());
                            }
                        }
                    }
                    expanded
                } else {
                    target.paths.clone()
                };

                let mut items = Vec::new();
                for path in paths_to_scan {
                    if let Some(ref flag) = is_cancelled
                        && flag.load(Ordering::Relaxed)
                    {
                        break;
                    }

                    if !path.exists() {
                        continue;
                    }

                    if let Some(ref tx) = progress_tx {
                        let _ = tx.send(ScanProgressEvent::ScanningPath(prettify_path(&path)));
                    }

                    if Safety::has_symlink_component(&path)
                        || Safety::is_cloud_or_dataless(&path)
                        || Safety::is_ignored_or_protected(&path, ignore)
                    {
                        continue;
                    }

                    // Check safety before scanning
                    if Safety::can_remove(&path, ignore).is_err() {
                        continue;
                    }

                    let size = DirSizer::size_of(&path);
                    if size >= 1024 {
                        if let Some(ref tx) = progress_tx {
                            let _ = tx.send(ScanProgressEvent::ItemFound { bytes: size });
                        }
                        items.push(ScanItem::new(path, size, target.selected_by_default, false));
                    }
                }

                let tool_bytes: u64 = items.iter().map(|i| i.size_bytes).sum();
                if let Some(ref tx) = progress_tx {
                    let _ = tx.send(ScanProgressEvent::ToolFinished {
                        tool: target.tool.clone(),
                        bytes_found: tool_bytes,
                    });
                }

                if items.is_empty() {
                    None
                } else {
                    Some(ToolGroup {
                        tool: target.tool,
                        items,
                    })
                }
            })
            .collect();

        CategoryScan {
            id: category_id.to_string(),
            title: category_title.to_string(),
            groups,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scan_junk_includes_user_logs() {
        let ignore = IgnoreStore::default();
        let scan = CategoryScanner::scan("junk", "System Junk", &ignore, None, None);
        if let Some(home) = dirs::home_dir()
            && home.join("Library/Logs").exists()
        {
            let user_logs = scan.groups.iter().find(|g| g.tool == "User logs");
            assert!(user_logs.is_some(), "User logs group should be discovered");
            assert!(!user_logs.unwrap().items.is_empty());
        }
    }
}

//! Interactive Select Items Screen.
//! Includes modal confirmation dialog, live removal progress dialog with spinner,
//! and completion outcome dialog.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::SystemTime;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use super::analyze::SortOrder;
use super::components::button::Button;
use super::components::chrome::AppChrome;
use super::components::empty::EmptyState;
use super::components::frame::{FRAME_SIDES, frame_bottom, frame_top, framed};
use super::components::gauge::bar;
use super::components::hints::{Hint, PrimaryAction};
use super::components::list::{Column, ColumnTitles, ListRow, SIZE_W, row_path_spans};
use super::components::modal::Modal;
use super::components::text::plural;
use super::components::toast::Toast;
use super::theme::Theme;
use crate::core::apps;
use crate::core::bytes::format_bytes;
use crate::core::cleaner::{RemovalMode, RemovalProgress, RemovalReport, item_removal_mode};
use crate::core::compress;
use crate::core::date_utils::format_age;
use crate::core::docker::{DockerObject, SCAN_ID};
use crate::core::homebrew;
use crate::core::large_files;
use crate::core::leftovers::UninstallRole;
use crate::core::path_utils::{middle_truncate, prettify_path};
use crate::core::purger;
use crate::core::scanner::{CategoryScan, ScanItem, ToolGroup};
use crate::core::slim;
use crate::core::uninstaller::{self, Route};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmButton {
    Cancel,
    Remove,
}

pub enum ModalOverlay {
    None,
    Confirm(ConfirmButton),
    Removing {
        progress: RemovalProgress,
        spinner_idx: usize,
        /// Set when the run can be stopped with Esc.
        cancel: Option<Arc<AtomicBool>>,
    },
    Outcome(RemovalReport),
}

pub struct SelectState {
    pub scan: CategoryScan,
    pub focused_index: usize,
    pub path_scroll: usize,
    pub search_query: String,
    pub is_searching: bool,
    pub overlay: ModalOverlay,
    pub problem: Option<String>,
    /// Lists that can be sorted are drawn as tables.
    pub sort_order: Option<SortOrder>,
    /// Told before the removal starts.
    pub notes: Vec<String>,
    pub asks_password: bool,
    permanent: bool,
    /// Worked out once per uninstall plan, because a bundle check reads the disk.
    routes: HashMap<PathBuf, Route>,
    /// Why the Homebrew list leaves a row alone, by the kind and label of the row.
    pub refusals: HashMap<(homebrew::PackageKind, PathBuf), String>,
    /// What the Homebrew list was built from, so a removal can update it without reading brew.
    pub homebrew: Option<homebrew::Inventory>,
    /// The rows of the Homebrew tree that are open, by their path of names.
    pub expanded: HashSet<Vec<String>>,
    pub toast: Option<Toast>,
    /// The Homebrew list is waiting for `brew update` and `brew outdated`.
    pub checking_updates: bool,
    /// The Homebrew list is reading brew again after an upgrade.
    pub refreshing: bool,
    /// The upgrade the question asks about or the run is doing, until its outcome closes.
    pub upgrade: Option<homebrew::UpgradePlan>,
}

const BAR_MIN_WIDTH: usize = 100;
const MAX_LISTED_FAILURES: usize = 3;
const MAX_LISTED_NOTES: usize = 3;
// Fits "About an hour ago", the longest age that lasts more than a few seconds.
const AGE_W: usize = 17;
// Fits "on request" and "dependency".
const ORIGIN_W: usize = 10;
const UPDATE_TITLE: &str = "UPDATE";
const UPDATE_MAX_W: usize = 14;
const BREW_BAR_CAP: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Item,
    Section,
    Columns { date: &'static str },
    SectionEnd,
    Gap,
}

/// Where a row of the Homebrew tree sits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRow {
    /// The names from the top level down to this row.
    pub path: Vec<String>,
    pub expandable: bool,
    pub expanded: bool,
}

impl TreeRow {
    /// 0 at the top level.
    pub fn depth(&self) -> usize {
        self.path.len() - 1
    }
}

#[derive(Debug, Clone)]
pub struct FlattenedRow {
    pub group_index: usize,
    pub item_index: Option<usize>,
    pub kind: RowKind,
    pub group_name: String,
    pub size_bytes: u64,
    pub display_path: String,
    pub is_selected: bool,
    pub full_path: Option<PathBuf>,
    pub item_count: usize,
    pub created: Option<SystemTime>,
    pub more_folders: usize,
    /// Set on the rows of the Homebrew tree, not while searching.
    pub tree: Option<TreeRow>,
}

impl FlattenedRow {
    pub fn is_item(&self) -> bool {
        self.kind == RowKind::Item
    }
}

/// A heading and the items under it, as the item at `(group, item)` of the scan.
struct Section {
    group_index: usize,
    entries: Vec<(usize, usize, Option<TreeRow>)>,
}

impl SelectState {
    pub fn new(mut scan: CategoryScan) -> Self {
        for group in &mut scan.groups {
            for item in &mut group.items {
                item.selected = false;
            }
        }
        Self::with_preserved_selection(scan)
    }

    /// Slim and Compress start with Enter; the other lists start with a removal key.
    pub fn starts_with_enter(&self) -> bool {
        matches!(self.scan.id.as_str(), slim::SCAN_ID | compress::SCAN_ID)
    }

    pub fn action_key<'a>(&self, enter: &'a str) -> &'a str {
        if self.starts_with_enter() { enter } else { "d" }
    }

    /// A scan that could not run, with the reason to show instead of a list.
    pub fn unavailable(id: &str, title: &str, problem: String) -> Self {
        let mut state = Self::new(CategoryScan {
            notes: Vec::new(),
            id: id.to_string(),
            title: title.to_string(),
            groups: Vec::new(),
        });
        state.problem = Some(problem);
        state
    }

    pub fn with_preserved_selection(scan: CategoryScan) -> Self {
        let is_table =
            scan.id == SCAN_ID || scan.id == apps::LIST_ID || scan.id == large_files::SCAN_ID;
        let sort_order = is_table.then(SortOrder::default);
        let notes = scan.notes.clone();
        let mut state = Self {
            scan,
            focused_index: 0,
            path_scroll: 0,
            search_query: String::new(),
            is_searching: false,
            overlay: ModalOverlay::None,
            problem: None,
            sort_order,
            notes,
            asks_password: false,
            permanent: false,
            routes: HashMap::new(),
            refusals: HashMap::new(),
            homebrew: None,
            expanded: HashSet::new(),
            toast: None,
            checking_updates: false,
            refreshing: false,
            upgrade: None,
        };
        state.move_to_start();
        state
    }

    /// The routes of an uninstall plan, worked out when it was built.
    pub fn with_routes(mut self, routes: HashMap<PathBuf, Route>) -> Self {
        self.routes = routes;
        self
    }

    /// Where an item goes. A row of an uninstall plan goes by its route.
    fn removal_mode(&self, item: &ScanItem) -> RemovalMode {
        if self.scan.id == uninstaller::PLAN_ID {
            uninstaller::route_of(&self.routes, item).mode(self.permanent)
        } else {
            item_removal_mode(item, &self.scan.id, self.permanent)
        }
    }

    /// The Homebrew list, with the reason each row cannot be uninstalled.
    pub fn homebrew_list(inventory: &homebrew::Inventory) -> Self {
        let mut state = Self::new(homebrew::list_scan(inventory));
        state.refusals = homebrew::list_refusals(inventory);
        state.homebrew = Some(inventory.clone());
        state.set_sort(SortOrder::default());
        state.move_to_start();
        state
    }

    /// Takes the result of the check for updates: the rows show the new versions, and a failure
    /// shows as a toast. Selection, sort, search, open rows, and the focused package stay as they are.
    pub fn finish_update_check(
        &mut self,
        update_error: Option<String>,
        latest: Result<homebrew::Latest, String>,
    ) {
        self.checking_updates = false;
        let failure = update_error.or_else(|| latest.as_ref().err().cloned());
        if let Some(failure) = failure {
            self.toast = Some(Toast::error(format!(
                "Could not check for updates: {failure}"
            )));
        }
        let Ok(latest) = latest else {
            return;
        };
        let focused = self.flattened_rows().get(self.focused_index).cloned();
        let Some(inventory) = self.homebrew.as_mut() else {
            return;
        };
        let changed = inventory.apply_latest(&latest);
        for (kind, item) in self.scan.groups.iter_mut().flat_map(|group| {
            let kind = homebrew::group_kind(&group.tool);
            group.items.iter_mut().map(move |item| (kind, item))
        }) {
            if let Some(label) = changed.get(&(kind, item.path.clone())) {
                item.path = label.clone();
            }
        }
        self.refusals = std::mem::take(&mut self.refusals)
            .into_iter()
            .map(|(key, reason)| match changed.get(&key) {
                Some(label) => ((key.0, label.clone()), reason),
                None => (key, reason),
            })
            .collect();
        if let Some(focused) = focused {
            self.refocus_renamed(&focused, &changed);
        }
    }

    /// Puts the focus back on the row that was focused before its label changed.
    fn refocus_renamed(
        &mut self,
        focused: &FlattenedRow,
        changed: &HashMap<(homebrew::PackageKind, PathBuf), PathBuf>,
    ) {
        let kind = homebrew::group_kind(&self.scan.groups[focused.group_index].tool);
        let wanted = focused
            .full_path
            .as_ref()
            .map(|old| changed.get(&(kind, old.clone())).unwrap_or(old));
        if !self.focus_same_item(focused, wanted) {
            self.focus_item_at_or_before(self.focused_index);
        }
    }

    /// Closes the outcome of a removal on the Homebrew list. The formulae that went leave the
    /// list and the ones they used are updated, with the sort, the search, and the focus kept.
    pub fn close_homebrew_outcome(&mut self) {
        let overlay = std::mem::replace(&mut self.overlay, ModalOverlay::None);
        let ModalOverlay::Outcome(report) = overlay else {
            return;
        };
        let Some(mut inventory) = self.homebrew.take() else {
            return;
        };
        let removed: Vec<&str> = self
            .scan
            .selected_items()
            .into_iter()
            .filter_map(|item| {
                let name = uninstall_name(item)?;
                let failed = report.failures.iter().any(|failure| {
                    homebrew::label_name(&item.path.to_string_lossy())
                        == failure.path.to_string_lossy()
                });
                (!failed).then_some(name)
            })
            .collect();
        if removed.is_empty() {
            self.homebrew = Some(inventory);
            for item in self.scan.selected_items_mut() {
                item.selected = false;
            }
            return;
        }
        for name in removed {
            inventory.forget(name);
        }
        let mut fresh = self.rebuilt(&inventory);
        fresh.focus_item_at_or_before(self.focused_index);
        *self = fresh;
    }

    /// A new Homebrew list for `inventory` that keeps the sort, the search, the open rows, and
    /// the toast.
    fn rebuilt(&mut self, inventory: &homebrew::Inventory) -> Self {
        let mut fresh = Self::homebrew_list(inventory).with_permanent(self.permanent);
        if let Some(order) = self.sort_order {
            fresh.set_sort(order);
        }
        fresh.checking_updates = self.checking_updates;
        fresh.refreshing = self.refreshing;
        fresh.is_searching = self.is_searching;
        fresh.search_query = std::mem::take(&mut self.search_query);
        fresh.expanded = std::mem::take(&mut self.expanded);
        fresh.toast = self.toast.take();
        if !fresh.is_searching && fresh.flattened_rows().is_empty() {
            // A search that matched only what went would show nothing.
            fresh.search_query.clear();
        }
        fresh
    }

    /// Closes the outcome of an upgrade. Returns the list to read again.
    pub fn close_upgrade_outcome(&mut self) -> Option<homebrew::Inventory> {
        self.overlay = ModalOverlay::None;
        self.upgrade.take()?;
        let request = self.refresh_request();
        self.refreshing = request.is_some();
        request
    }

    /// The list to read again, when it has a brew to read it with.
    pub fn refresh_request(&self) -> Option<homebrew::Inventory> {
        self.homebrew
            .clone()
            .filter(|inventory| inventory.brew.is_some())
    }

    /// Shows the list read again after an upgrade, with the focus on the same package.
    pub fn finish_refresh(&mut self, inventory: &homebrew::Inventory) {
        self.refreshing = false;
        let focused = self.flattened_rows().get(self.focused_index).cloned();
        let package = self
            .focused_package()
            .and_then(|old| {
                inventory
                    .packages
                    .iter()
                    .find(|new| (new.kind, &new.name) == (old.kind, &old.name))
            })
            .map(homebrew::label);
        let mut fresh = self.rebuilt(inventory);
        let index = self.focused_index;
        let found = focused.zip(package).is_some_and(|(focused, label)| {
            fresh.focus_same_item(&focused, Some(&PathBuf::from(label)))
        });
        if !found {
            fresh.focus_item_at_or_before(index);
        }
        *self = fresh;
    }

    /// Keeps the rows when the list could not be read again after an upgrade.
    pub fn fail_refresh(&mut self, error: &str) {
        self.refreshing = false;
        self.toast = Some(Toast::error(format!("Could not update the list: {error}")));
    }

    fn focus_item_at_or_before(&mut self, index: usize) {
        let rows = self.flattened_rows();
        if rows.is_empty() {
            self.focused_index = 0;
            return;
        }
        let last = rows.len() - 1;
        let wanted = index.min(last);
        if let Some(found) = (0..=wanted)
            .rev()
            .chain(wanted + 1..=last)
            .find(|&i| rows[i].is_item())
        {
            self.focused_index = found;
        }
    }

    pub fn with_permanent(mut self, permanent: bool) -> Self {
        self.permanent = permanent;
        self
    }

    /// Sorts inside each section and keeps the cursor on its item.
    pub fn set_sort(&mut self, sort_order: SortOrder) {
        let focused = self.flattened_rows().get(self.focused_index).cloned();

        for group in &mut self.scan.groups {
            sort_order.sort_children(&mut group.items);
        }
        self.sort_order = Some(sort_order);

        if let Some(focused) = focused {
            self.focus_same_item(&focused, focused.full_path.as_ref());
        }
    }

    /// Moves the focus to the item in `focused`'s group and tree position whose path is `path`.
    fn focus_same_item(&mut self, focused: &FlattenedRow, path: Option<&PathBuf>) -> bool {
        let tree_path = |row: &FlattenedRow| row.tree.as_ref().map(|tree| tree.path.clone());
        let index = self.flattened_rows().iter().position(|row| {
            row.is_item()
                && row.group_name == focused.group_name
                && row.full_path.as_ref() == path
                && tree_path(row) == tree_path(focused)
        });
        if let Some(index) = index {
            self.focused_index = index;
        }
        index.is_some()
    }

    /// The groups with the items the search keeps.
    fn searched_sections(&self) -> Vec<Section> {
        let query = self.search_query.to_lowercase();
        self.scan
            .groups
            .iter()
            .enumerate()
            .map(|(g_idx, group)| Section {
                group_index: g_idx,
                entries: group
                    .items
                    .iter()
                    .enumerate()
                    .filter(|(_, item)| {
                        item.folders().any(|folder| {
                            folder
                                .path
                                .display()
                                .to_string()
                                .to_lowercase()
                                .contains(&query)
                        })
                    })
                    .map(|(i_idx, _)| (g_idx, i_idx, None))
                    .collect(),
            })
            .filter(|section| !section.entries.is_empty() || self.search_query.is_empty())
            .collect()
    }

    /// The Homebrew groups with the dependencies of the open rows under their formulae.
    fn homebrew_tree_sections(&self, inventory: &homebrew::Inventory) -> Vec<Section> {
        let mut places = HashMap::new();
        for (g_idx, group) in self.scan.groups.iter().enumerate() {
            for (i_idx, item) in group.items.iter().enumerate() {
                places.insert(
                    (homebrew::group_kind(&group.tool), item.path.clone()),
                    (g_idx, i_idx),
                );
            }
        }
        let order = self.sort_order.unwrap_or_default();
        homebrew::tree_sections(inventory, &self.expanded, &|rows| order.sort_children(rows))
            .into_iter()
            .filter_map(|(name, rows)| {
                let group_index = self.scan.groups.iter().position(|g| g.tool == name)?;
                let entries = rows
                    .into_iter()
                    .filter_map(|row| {
                        let key = (
                            row.package.kind,
                            PathBuf::from(homebrew::label(row.package)),
                        );
                        let (group, item) = *places.get(&key)?;
                        let tree = TreeRow {
                            path: row.path,
                            expandable: row.expandable,
                            expanded: row.expanded,
                        };
                        Some((group, item, Some(tree)))
                    })
                    .collect();
                Some(Section {
                    group_index,
                    entries,
                })
            })
            .collect()
    }

    pub fn flattened_rows(&self) -> Vec<FlattenedRow> {
        let mut rows = Vec::new();
        let sections = match &self.homebrew {
            Some(inventory) if self.search_query.is_empty() => {
                self.homebrew_tree_sections(inventory)
            }
            _ => self.searched_sections(),
        };

        for section in sections {
            let g_idx = section.group_index;
            let group = &self.scan.groups[g_idx];
            let top_level = || {
                section
                    .entries
                    .iter()
                    .filter(|(_, _, tree)| tree.as_ref().is_none_or(|tree| tree.depth() == 0))
                    .map(|&(g, i, _)| &self.scan.groups[g].items[i])
            };
            let group_bytes: u64 = top_level().map(|item| item.size_bytes).sum();
            let item_count = top_level().count();
            let heading_row = |kind| FlattenedRow {
                group_index: g_idx,
                item_index: None,
                kind,
                group_name: group.tool.clone(),
                size_bytes: 0,
                display_path: String::new(),
                is_selected: false,
                full_path: None,
                item_count: 0,
                created: None,
                more_folders: 0,
                tree: None,
            };

            if !rows.is_empty() {
                rows.push(heading_row(RowKind::Gap));
            }

            // Group header
            rows.push(FlattenedRow {
                group_index: g_idx,
                item_index: None,
                kind: RowKind::Section,
                group_name: group.tool.clone(),
                size_bytes: group_bytes,
                display_path: format!("{} items · {}", item_count, format_bytes(group_bytes)),
                is_selected: top_level().all(|item| item.selected) && item_count > 0,
                full_path: None,
                item_count,
                created: None,
                more_folders: 0,
                tree: None,
            });

            if self.sort_order.is_some() {
                let is_cache = |item: &ScanItem| item.docker == Some(DockerObject::BuildCache);
                let date = if self.scan.id == homebrew::SCAN_ID {
                    "INSTALLED"
                } else if self.scan.id == apps::LIST_ID
                    || self.scan.id == large_files::SCAN_ID
                    || group.items.iter().any(is_cache)
                {
                    "LAST USED"
                } else {
                    "CREATED"
                };
                rows.push(heading_row(RowKind::Columns { date }));
            }

            // Group items
            for (item_group, i_idx, tree) in section.entries {
                let item = &self.scan.groups[item_group].items[i_idx];
                rows.push(FlattenedRow {
                    group_index: item_group,
                    item_index: Some(i_idx),
                    kind: RowKind::Item,
                    group_name: self.scan.groups[item_group].tool.clone(),
                    size_bytes: item.size_bytes,
                    display_path: prettify_path(&item.path),
                    is_selected: item.selected,
                    full_path: Some(item.path.clone()),
                    item_count: 0,
                    created: item.modified,
                    more_folders: item.also.len(),
                    tree,
                });
            }

            rows.push(heading_row(RowKind::SectionEnd));
        }

        rows
    }

    /// A list that acts on its focused row: no check marks, no selecting several, no Reveal.
    pub fn is_single_row_list(&self) -> bool {
        self.scan.id == homebrew::SCAN_ID
    }

    fn focused_position(&self) -> Option<(usize, usize)> {
        let rows = self.flattened_rows();
        let row = rows.get(self.focused_index)?;
        Some((row.group_index, row.item_index?))
    }

    fn focused_item(&self) -> Option<&ScanItem> {
        let (group, item) = self.focused_position()?;
        self.scan.groups.get(group)?.items.get(item)
    }

    fn focused_tree(&self) -> Option<TreeRow> {
        self.flattened_rows().get(self.focused_index)?.tree.clone()
    }

    /// Enter on the Homebrew list: opens or closes the focused row.
    pub fn toggle_focused(&mut self) {
        let Some(tree) = self.focused_tree().filter(|tree| tree.expandable) else {
            return;
        };
        if !self.expanded.remove(&tree.path) {
            self.expanded.insert(tree.path);
        }
    }

    /// Right on the Homebrew list: opens the focused row.
    pub fn expand_focused(&mut self) {
        if let Some(tree) = self.focused_tree().filter(|tree| tree.expandable) {
            self.expanded.insert(tree.path);
        }
    }

    /// Left on the Homebrew list: closes the focused row, or moves to its parent.
    pub fn collapse_focused(&mut self) {
        let Some(tree) = self.focused_tree() else {
            return;
        };
        if tree.expandable && tree.expanded {
            self.expanded.remove(&tree.path);
        } else if tree.depth() > 0 {
            let parent = &tree.path[..tree.depth()];
            let rows = self.flattened_rows();
            if let Some(index) = rows[..self.focused_index]
                .iter()
                .rposition(|row| row.tree.as_ref().is_some_and(|t| t.path == parent))
            {
                self.focused_index = index;
                self.path_scroll = 0;
            }
        }
    }

    /// Why the focused row of the Homebrew list cannot be uninstalled, if it cannot.
    fn refusal_of_focused(&self) -> Option<&String> {
        let (group, item) = self.focused_position()?;
        let group = self.scan.groups.get(group)?;
        let key = (
            homebrew::group_kind(&group.tool),
            group.items.get(item)?.path.clone(),
        );
        self.refusals.get(&key)
    }

    /// The formula D uninstalls on the Homebrew list, if the focused row allows it.
    fn removable(&self) -> Option<&str> {
        let item = self.focused_item()?;
        if self.refusal_of_focused().is_some() {
            return None;
        }
        uninstall_name(item)
    }

    /// D on the Homebrew list: says why the focused row stays, or asks to remove only it.
    pub fn uninstall_focused(&mut self) {
        for item in self
            .scan
            .groups
            .iter_mut()
            .flat_map(|group| &mut group.items)
        {
            item.selected = false;
        }
        self.toast = None;
        let Some((group, item)) = self.focused_position() else {
            return;
        };
        if let Some(text) = self.refusal_of_focused() {
            self.toast = Some(Toast::error(text.clone()));
        } else if self.removable().is_some() {
            self.scan.groups[group].items[item].selected = true;
            self.open_confirmation();
        }
    }

    /// The package of the focused row of the Homebrew list. A formula and a cask can share a
    /// label, so the group says which kind it is.
    fn focused_package(&self) -> Option<&homebrew::Package> {
        let (group, item) = self.focused_position()?;
        let group = self.scan.groups.get(group)?;
        let label = group.items.get(item)?.path.to_string_lossy();
        let kind = homebrew::group_kind(&group.tool);
        self.homebrew
            .as_ref()?
            .packages
            .iter()
            .find(|package| package.kind == kind && homebrew::label(package) == label)
    }

    /// Whether the focused row of the Homebrew list has a newer version.
    fn has_update(&self) -> bool {
        self.focused_package()
            .is_some_and(|package| package.latest_version.is_some())
    }

    /// Says that an upgrade waits while the update check runs, so it never runs beside
    /// `brew update`, and while the list is read again after an upgrade.
    fn upgrade_waits(&mut self) -> bool {
        let text = if self.checking_updates {
            "Checking for updates. Try again when it ends."
        } else if self.refreshing {
            "Updating the list. Try again when it ends."
        } else {
            return false;
        };
        self.toast = Some(Toast::info(text));
        true
    }

    /// u on the Homebrew list: says why the focused row stays, or asks to upgrade only it.
    pub fn upgrade_focused(&mut self) {
        self.toast = None;
        if self.upgrade_waits() {
            return;
        }
        let Some(package) = self.focused_package() else {
            return;
        };
        if let Some(text) = homebrew::upgrade_refusal(&package.name, Some(package)) {
            self.toast = Some(Toast::error(text));
            return;
        }
        let packages = vec![package.clone()];
        self.ask_to_upgrade(packages);
    }

    /// Shift-U on the Homebrew list: asks to upgrade every package that is not refused.
    pub fn upgrade_all(&mut self) {
        self.toast = None;
        if self.upgrade_waits() {
            return;
        }
        let packages = self
            .homebrew
            .as_ref()
            .map(|inventory| homebrew::upgradable(&inventory.packages))
            .unwrap_or_default();
        if packages.is_empty() {
            self.toast = Some(Toast::info("Everything is up to date."));
            return;
        }
        self.ask_to_upgrade(packages);
    }

    fn ask_to_upgrade(&mut self, packages: Vec<homebrew::Package>) {
        let Some(brew) = self.homebrew.as_ref().and_then(|list| list.brew.clone()) else {
            return;
        };
        self.upgrade = Some(homebrew::UpgradePlan { brew, packages });
        self.overlay = ModalOverlay::Confirm(ConfirmButton::Cancel);
    }

    pub fn toggle_current(&mut self) {
        if self.is_single_row_list() {
            return;
        }
        let rows = self.flattened_rows();
        if let Some(row) = rows.get(self.focused_index) {
            if row.kind == RowKind::Section {
                let g_idx = row.group_index;
                let all_sel = self.scan.groups[g_idx]
                    .items
                    .iter()
                    .all(|item| item.selected);
                for item in &mut self.scan.groups[g_idx].items {
                    item.selected = !all_sel;
                }
            } else if let Some(i_idx) = row.item_index {
                let g_idx = row.group_index;
                self.scan.groups[g_idx].items[i_idx].selected =
                    !self.scan.groups[g_idx].items[i_idx].selected;
            }
        }
    }

    fn set_shown_items(&mut self, selected: bool) {
        if self.is_single_row_list() {
            return;
        }
        for row in self.flattened_rows() {
            if let Some(item) = row.item_index {
                self.scan.groups[row.group_index].items[item].selected = selected;
            }
        }
    }

    /// Selects what the search shows, so nothing hidden gets selected.
    pub fn select_all(&mut self) {
        self.set_shown_items(true);
    }

    pub fn select_none(&mut self) {
        self.set_shown_items(false);
    }

    pub fn next_selectable(&mut self) {
        let rows = self.flattened_rows();
        if rows.is_empty() {
            return;
        }

        let mut next = self.focused_index + 1;
        while next < rows.len() {
            if rows[next].is_item() {
                self.focused_index = next;
                self.path_scroll = 0;
                return;
            }
            next += 1;
        }
        self.move_to_end();
    }

    pub fn previous_selectable(&mut self) {
        let rows = self.flattened_rows();
        if rows.is_empty() {
            return;
        }

        let mut prev = self.focused_index.saturating_sub(1);
        loop {
            if rows[prev].is_item() {
                self.focused_index = prev;
                self.path_scroll = 0;
                return;
            }
            if prev == 0 {
                break;
            }
            prev -= 1;
        }
        self.move_to_start();
    }

    pub fn page_down(&mut self) {
        let rows = self.flattened_rows();
        if rows.is_empty() {
            return;
        }
        let target = (self.focused_index + 10).min(rows.len().saturating_sub(1));
        for (idx, row) in rows.iter().enumerate().skip(target) {
            if row.is_item() {
                self.focused_index = idx;
                self.path_scroll = 0;
                return;
            }
        }
        self.move_to_end();
    }

    pub fn page_up(&mut self) {
        let rows = self.flattened_rows();
        if rows.is_empty() {
            return;
        }
        let target = self.focused_index.saturating_sub(10);
        for idx in (0..=target).rev() {
            if rows[idx].is_item() {
                self.focused_index = idx;
                self.path_scroll = 0;
                return;
            }
        }
        self.move_to_start();
    }

    pub fn move_to_start(&mut self) {
        let rows = self.flattened_rows();
        for (idx, row) in rows.iter().enumerate() {
            if row.is_item() {
                self.focused_index = idx;
                self.path_scroll = 0;
                return;
            }
        }
        self.focused_index = 0;
        self.path_scroll = 0;
    }

    pub fn move_to_end(&mut self) {
        let rows = self.flattened_rows();
        for (idx, row) in rows.iter().enumerate().rev() {
            if row.is_item() {
                self.focused_index = idx;
                self.path_scroll = 0;
                return;
            }
        }
        self.focused_index = rows.len().saturating_sub(1);
        self.path_scroll = 0;
    }

    pub fn scroll_path_left(&mut self) {
        self.path_scroll = self.path_scroll.saturating_sub(4);
    }

    pub fn scroll_path_right(&mut self) {
        self.path_scroll = self.path_scroll.saturating_add(4);
    }

    pub fn current_focused_path(&self) -> Option<PathBuf> {
        if self.is_single_row_list() {
            return None;
        }
        let rows = self.flattened_rows();
        let row = rows.get(self.focused_index)?;
        let item = &self.scan.groups[row.group_index].items[row.item_index?];
        // The label of a brew row is no path.
        if item.brew.is_some() {
            return None;
        }
        row.full_path.clone()
    }

    pub fn open_confirmation(&mut self) {
        if !self.scan.selected_items().is_empty() {
            self.asks_password = (self.scan.id == uninstaller::PLAN_ID
                && uninstaller::needs_password(&self.scan, &self.routes))
                || (matches!(self.scan.id.as_str(), compress::SCAN_ID | slim::SCAN_ID)
                    && self
                        .scan
                        .selected_items()
                        .iter()
                        .any(|item| item.needs_admin));
            self.overlay = ModalOverlay::Confirm(ConfirmButton::Cancel);
        }
    }

    pub fn close_overlay(&mut self) {
        self.overlay = ModalOverlay::None;
        self.upgrade = None;
    }

    pub fn toggle_confirmation_button(&mut self) {
        if let ModalOverlay::Confirm(btn) = self.overlay {
            self.overlay = ModalOverlay::Confirm(match btn {
                ConfirmButton::Cancel => ConfirmButton::Remove,
                ConfirmButton::Remove => ConfirmButton::Cancel,
            });
        }
    }
}

/// The primary action of the Homebrew list for the focused row.
pub fn homebrew_primary<'a>(state: &SelectState, enter: &'a str) -> PrimaryAction<'a> {
    match state.focused_tree().filter(|tree| tree.expandable) {
        Some(tree) if tree.expanded => PrimaryAction::new("Collapse", enter),
        Some(_) => PrimaryAction::new("Expand", enter),
        None => PrimaryAction::disabled("Expand", enter),
    }
}

/// The disclosure mark of a tree row.
fn tree_mark(tree: &TreeRow) -> char {
    match (tree.expandable, tree.expanded) {
        (false, _) => ' ',
        (true, false) => '▸',
        (true, true) => '▾',
    }
}

pub fn render_select(frame: &mut Frame, state: &SelectState, theme: &Theme) {
    let total_shown_bytes: u64 = state.scan.total_all_bytes();
    let total_selected_bytes: u64 = state.scan.total_selected_bytes();
    let selected_count = state.scan.selected_items().len();
    let total_count: usize = state.scan.groups.iter().map(|g| g.items.len()).sum();

    let single_row = state.is_single_row_list();
    let right_status = if single_row {
        let mut spans = vec![
            Span::styled(plural(total_count, "package", "packages"), theme.headline()),
            Span::styled("  ·  ", theme.callout()),
            Span::styled(
                format_bytes(total_shown_bytes),
                theme.size(total_shown_bytes),
            ),
        ];
        let updates = state.homebrew.as_ref().map_or(0, |inventory| {
            inventory
                .packages
                .iter()
                .filter(|package| package.latest_version.is_some())
                .count()
        });
        if updates > 0 {
            spans.push(Span::styled("  ·  ", theme.callout()));
            spans.push(Span::styled(
                plural(updates, "update", "updates"),
                theme.accent_text(),
            ));
        }
        if state.checking_updates {
            spans.push(Span::styled("  ·  ", theme.callout()));
            spans.push(Span::styled("Checking for updates…", theme.callout()));
        }
        if state.refreshing {
            spans.push(Span::styled("  ·  ", theme.callout()));
            spans.push(Span::styled("Updating the list…", theme.callout()));
        }
        Line::from(spans)
    } else {
        Line::from(vec![
            Span::styled(format!("{selected_count}"), theme.headline()),
            Span::styled(format!(" of {total_count} selected  ·  "), theme.callout()),
            Span::styled(
                format_bytes(total_selected_bytes),
                theme.size(total_selected_bytes),
            ),
        ])
    };

    let enter = theme.g.key_enter;
    let id = state.scan.id.as_str();
    let action_key = state.action_key(enter);
    let primary_action = if state.is_searching {
        PrimaryAction::new("Apply", enter)
    } else if id == homebrew::SCAN_ID {
        homebrew_primary(state, enter)
    } else if id == uninstaller::PLAN_ID && selected_count > 0 {
        let items = plural(selected_count, "item", "items");
        PrimaryAction::new(format!("Uninstall {items}"), action_key)
    } else if id == uninstaller::PLAN_ID {
        PrimaryAction::disabled("Uninstall", action_key)
    } else if id == apps::LIST_ID && selected_count > 0 {
        let chosen = plural(selected_count, "app", "apps");
        PrimaryAction::new(format!("Review {chosen}"), action_key)
    } else if id == apps::LIST_ID && total_count > 0 {
        PrimaryAction::new("Review app", action_key)
    } else if id == slim::SCAN_ID && selected_count > 0 {
        let chosen = plural(selected_count, "app", "apps");
        PrimaryAction::new(format!("Slim {chosen}"), enter)
    } else if id == slim::SCAN_ID && total_count > 0 {
        PrimaryAction::new("Slim app", enter)
    } else if selected_count > 0 {
        PrimaryAction::new(format!("Review {selected_count} items"), action_key)
    } else {
        PrimaryAction::disabled("Review items", action_key)
    };

    let title = state.scan.title.as_str();
    let crumbs = if id == uninstaller::PLAN_ID {
        vec!["taert", "Uninstall apps", title]
    } else if id == slim::SCAN_ID {
        vec!["taert", "Optimize", "Slim apps"]
    } else if id == large_files::SCAN_ID {
        vec!["taert", "Inspect", "Large files"]
    } else if id == homebrew::CLEANUP_ID {
        vec!["taert", "Homebrew", "Cleanup"]
    } else if id == SCAN_ID
        || id == purger::SCAN_ID
        || id == apps::LIST_ID
        || id == homebrew::SCAN_ID
    {
        vec!["taert", title]
    } else {
        vec!["taert", "Clean caches", title]
    };

    let chrome = AppChrome::new(crumbs)
        .full_width()
        .status(right_status)
        .hints(toolbar_hints(state, total_count > 0))
        .toast(state.toast.as_ref())
        .behind_dialog(!matches!(state.overlay, ModalOverlay::None));
    let chrome = if single_row && total_count == 0 && !state.is_searching {
        chrome
    } else {
        chrome.primary(primary_action)
    };

    let content_rect = chrome.render(frame, theme);
    let note_is_the_reason = total_count == 0 && is_homebrew(id);
    let content_rect = if note_is_the_reason {
        content_rect
    } else {
        render_notes(frame, content_rect, &state.scan.notes, theme)
    };

    if total_count == 0 {
        render_nothing_to_show(frame, content_rect, state, theme);
        render_overlay(frame, state, theme);
        return;
    }

    let mut start_y = content_rect.y;
    let mut avail_h = content_rect.height;

    // Search bar if searching or query not empty
    if state.is_searching || !state.search_query.is_empty() {
        let results_count = state
            .flattened_rows()
            .iter()
            .filter(|r| r.is_item())
            .count();
        let search_line = Line::from(vec![
            Span::styled(format!("  {}  ", theme.g.search), theme.accent_text()),
            Span::styled(&state.search_query, theme.headline()),
            Span::styled("▏", theme.accent_text().add_modifier(Modifier::BOLD)),
            Span::raw("   "),
            Span::styled(plural(results_count, "result", "results"), theme.footnote()),
        ]);
        let search_rect = Rect {
            x: content_rect.x,
            y: start_y,
            width: content_rect.width,
            height: 1,
        };
        frame.render_widget(Paragraph::new(search_line), search_rect);
        start_y += 2; // bar + blank line
        avail_h = avail_h.saturating_sub(2);
    }

    let list_rect = Rect {
        x: content_rect.x,
        y: start_y,
        width: content_rect.width,
        height: avail_h,
    };

    let rows = state.flattened_rows();
    let list_area_height = list_rect.height as usize;
    let list_scroll_offset = if rows.len() > list_area_height && list_area_height > 0 {
        let half = list_area_height / 2;
        let max_offset = rows.len().saturating_sub(list_area_height);
        state.focused_index.saturating_sub(half).min(max_offset)
    } else {
        0
    };

    let width = list_rect.width as usize;
    let row_width = width.saturating_sub(FRAME_SIDES);
    let now = SystemTime::now();
    let brew = BrewColumns::of(state, theme);
    let mut list_lines = Vec::new();
    for (idx, row) in rows
        .iter()
        .enumerate()
        .skip(list_scroll_offset)
        .take(list_area_height)
    {
        let is_focused = idx == state.focused_index;
        let color = section_color(&state.scan.groups[row.group_index], theme);

        if row.kind == RowKind::Gap {
            list_lines.push(Line::default());
        } else if row.kind == RowKind::SectionEnd {
            list_lines.push(frame_bottom(width, color, theme));
        } else if let (RowKind::Columns { date }, Some(order)) = (row.kind, state.sort_order) {
            let titles = ColumnTitles {
                date,
                date_width: AGE_W,
                order,
                share: row_width >= BAR_MIN_WIDTH,
                chevron: false,
                columns: brew.as_ref().map_or_else(Vec::new, BrewColumns::titles),
                bar_cap: brew.is_some().then_some(BREW_BAR_CAP),
            };
            let titles = titles.render(row_width, theme);
            list_lines.push(framed(titles, row_width, color, theme));
        } else if row.kind == RowKind::Section {
            let noun = if row.item_count == 1 { "item" } else { "items" };
            let detail_spans = vec![
                Span::styled(format!("{} {noun} · ", row.item_count), theme.footnote()),
                Span::styled(format_bytes(row.size_bytes), theme.size(row.size_bytes)),
            ];
            list_lines.push(section_frame_top(
                &state.scan.groups[row.group_index],
                detail_spans,
                width,
                color,
                theme,
            ));
        } else {
            let bullet = if row.is_selected {
                theme.g.check_on
            } else {
                theme.g.check_off
            };
            let bullet_style = if row.is_selected {
                theme.accent_text().add_modifier(Modifier::BOLD)
            } else {
                theme.callout()
            };

            let app_name = (state.scan.id == apps::LIST_ID || state.scan.id == slim::SCAN_ID)
                .then(|| row.full_path.as_deref().and_then(Path::file_stem))
                .flatten();
            let title_spans = if let Some(name) = app_name {
                vec![Span::styled(
                    name.to_string_lossy().into_owned(),
                    theme.body(),
                )]
            } else if single_row {
                let mut spans = label_spans(&row.display_path, theme);
                if let Some(tree) = row.tree.as_ref().filter(|tree| tree.depth() > 0) {
                    let indent = "  ".repeat(tree.depth() - 1);
                    let mark = Span::styled(tree_mark(tree).to_string(), theme.callout());
                    spans.splice(0..0, [Span::raw(indent), mark, Span::raw(" ")]);
                }
                spans
            } else if let Some(path) = &row.full_path {
                let max_len = row_width.saturating_sub(20);
                let mut spans = row_path_spans(path, row.more_folders, max_len, theme);
                if is_focused && state.path_scroll > 0 {
                    let rest: String = prettify_path(path)
                        .chars()
                        .skip(state.path_scroll)
                        .collect();
                    if !rest.is_empty() {
                        spans = vec![Span::styled(rest, theme.body())];
                    }
                }
                spans
            } else {
                vec![Span::styled(&row.display_path, theme.body())]
            };

            let size_style = theme.size(row.size_bytes);
            let detail = Span::styled(
                format!("{:>SIZE_W$}", format_bytes(row.size_bytes)),
                size_style,
            );

            let frac = if total_shown_bytes > 0 {
                Some(row.size_bytes as f64 / total_shown_bytes as f64)
            } else {
                None
            };

            let mut list_row = ListRow::new(title_spans)
                .leading(
                    if let Some(tree) = row.tree.as_ref().filter(|tree| tree.depth() == 0) {
                        Span::styled(tree_mark(tree).to_string(), theme.callout())
                    } else if single_row {
                        Span::raw(" ")
                    } else {
                        Span::styled(bullet, bullet_style)
                    },
                )
                .detail(detail)
                .selected(is_focused);

            if state.sort_order.is_some() {
                let age = row
                    .created
                    .map_or("—".to_string(), |time| format_age(time, now));
                list_row = list_row.meta(Span::styled(format!("{age:<AGE_W$}"), theme.footnote()));
            } else if state.scan.id == uninstaller::PLAN_ID
                && let Some(i_idx) = row.item_index
                && let Some(item) = state
                    .scan
                    .groups
                    .get(row.group_index)
                    .and_then(|g| g.items.get(i_idx))
            {
                let dest = state.removal_mode(item).label();
                list_row = list_row.meta(Span::styled(dest, theme.footnote()));
            }

            if let Some(brew) = &brew {
                list_row = list_row
                    .columns(brew.cells(row, theme))
                    .bar_cap(BREW_BAR_CAP);
            }

            if let Some(f) = frac
                && row_width >= BAR_MIN_WIDTH
            {
                list_row = list_row.bar(f);
            }

            let line = list_row.render_line(row_width, theme);
            list_lines.push(framed(line, row_width, color, theme));
        }
    }

    frame.render_widget(Paragraph::new(list_lines), list_rect);

    render_overlay(frame, state, theme);
}

/// Draws the notes at the bottom and returns the area left above them.
pub(super) fn render_notes(frame: &mut Frame, area: Rect, notes: &[String], theme: &Theme) -> Rect {
    let lines: Vec<Line> = notes
        .iter()
        .map(|note| Line::from(Span::styled(note.clone(), theme.callout())))
        .collect();
    let text = Paragraph::new(lines).wrap(Wrap { trim: true });
    let height = text
        .line_count(area.width)
        .min(usize::from(area.height / 2)) as u16;
    if height == 0 {
        return area;
    }
    let notes_area = Rect {
        y: area.y + area.height - height,
        height,
        ..area
    };
    frame.render_widget(text, notes_area);
    Rect {
        height: area.height - height,
        ..area
    }
}

fn render_overlay(frame: &mut Frame, state: &SelectState, theme: &Theme) {
    match &state.overlay {
        ModalOverlay::None => {}
        ModalOverlay::Confirm(focus_btn) => {
            render_confirmation_dialog(frame, state, *focus_btn, theme);
        }
        ModalOverlay::Removing {
            progress,
            spinner_idx,
            cancel,
        } => {
            render_removing_dialog(
                frame,
                progress,
                *spinner_idx,
                cancel.is_some(),
                RunKind::of(state),
                theme,
            );
        }
        ModalOverlay::Outcome(report) => {
            render_outcome_dialog(frame, report, RunKind::of(state), theme);
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum RunKind {
    Remove,
    Uninstall,
    Slim,
    Compress,
    Upgrade,
}

impl RunKind {
    fn of(state: &SelectState) -> Self {
        let id = state.scan.id.as_str();
        if state.upgrade.is_some() {
            Self::Upgrade
        } else if is_plan(state) || id == homebrew::SCAN_ID {
            Self::Uninstall
        } else if id == slim::SCAN_ID {
            Self::Slim
        } else if id == compress::SCAN_ID {
            Self::Compress
        } else {
            Self::Remove
        }
    }

    fn verb(self) -> &'static str {
        match self {
            Self::Slim => "slim",
            Self::Compress => "compress",
            Self::Upgrade => "upgrade",
            Self::Remove | Self::Uninstall => "remove",
        }
    }
}

fn is_plan(state: &SelectState) -> bool {
    state.scan.id == uninstaller::PLAN_ID
}

fn section_color(group: &ToolGroup, theme: &Theme) -> Color {
    match group.items.first().and_then(|item| item.docker.as_ref()) {
        Some(DockerObject::Volume { .. }) => theme.p.orange,
        Some(DockerObject::Network { .. }) => theme.p.teal,
        Some(DockerObject::BuildCache) => theme.p.purple,
        _ => theme.p.accent,
    }
}

// Docker and app sections keep a note in brackets after their name, for plain output too.
fn section_frame_top(
    group: &ToolGroup,
    detail: Vec<Span<'static>>,
    width: usize,
    color: Color,
    theme: &Theme,
) -> Line<'static> {
    let name = group.tool.as_str();
    let has_note = group
        .items
        .iter()
        .any(|item| item.docker.is_some() || item.role.is_some());
    match name.split_once(" (") {
        Some((title, note)) if has_note => frame_top(
            title,
            note.trim_end_matches(')'),
            detail,
            width,
            color,
            theme,
        ),
        _ => frame_top(name, "", detail, width, color, theme),
    }
}

/// A label row: what it names first, then its details in a quieter style.
fn label_spans(label: &str, theme: &Theme) -> Vec<Span<'static>> {
    let Some((name, details)) = label.split_once(" · ") else {
        return vec![Span::styled(label.to_string(), theme.body())];
    };
    let mut spans = vec![Span::styled(name.to_string(), theme.body())];
    for detail in details.split(" · ") {
        spans.push(Span::styled(format!(" · {detail}"), theme.footnote()));
    }
    spans
}

/// What the UPDATE column says: the new version after an arrow, the arrow alone for a package
/// brew marks outdated without one, or nothing.
fn update_text(package: &homebrew::Package, theme: &Theme) -> String {
    let arrow = theme.g.update;
    match (&package.latest_version, package.outdated) {
        (Some(latest), _) => format!("{arrow} {latest}"),
        (None, true) => arrow.to_string(),
        (None, false) => String::new(),
    }
}

/// The ORIGIN and UPDATE columns of the Homebrew list, worked out once per frame.
struct BrewColumns<'a> {
    packages: HashMap<(homebrew::PackageKind, PathBuf), &'a homebrew::Package>,
    update_w: usize,
}

impl<'a> BrewColumns<'a> {
    /// The columns of `state`, when it is the Homebrew list.
    fn of(state: &'a SelectState, theme: &Theme) -> Option<Self> {
        if state.scan.id != homebrew::SCAN_ID {
            return None;
        }
        let packages: Vec<&homebrew::Package> = state
            .homebrew
            .iter()
            .flat_map(|inventory| &inventory.packages)
            .collect();
        let update_w = packages
            .iter()
            .map(|package| update_text(package, theme).width())
            .fold(UPDATE_TITLE.len(), usize::max)
            .min(UPDATE_MAX_W);
        let packages = packages
            .into_iter()
            .map(|package| {
                let label = PathBuf::from(homebrew::label(package));
                ((package.kind, label), package)
            })
            .collect();
        Some(Self { packages, update_w })
    }

    fn titles(&self) -> Vec<Column<'static>> {
        vec![
            Column::new(Span::raw("ORIGIN"), ORIGIN_W, BAR_MIN_WIDTH),
            Column::new(Span::raw(UPDATE_TITLE), self.update_w, 0),
        ]
    }

    fn cells(&self, row: &FlattenedRow, theme: &Theme) -> Vec<Column<'static>> {
        let kind = homebrew::group_kind(&row.group_name);
        let package = row
            .full_path
            .as_ref()
            .and_then(|label| self.packages.get(&(kind, label.clone())));
        let origin = package.map_or("", |package| homebrew::origin(package));
        let update = package.map_or_else(String::new, |package| update_text(package, theme));
        vec![
            Column::new(
                Span::styled(origin, theme.footnote()),
                ORIGIN_W,
                BAR_MIN_WIDTH,
            ),
            Column::new(Span::styled(update, theme.accent_text()), self.update_w, 0),
        ]
    }
}

fn toolbar_hints(state: &SelectState, has_items: bool) -> Vec<Hint<'static>> {
    if state.is_searching {
        return vec![Hint::new("⎋", "Cancel")];
    }

    let mut hints = Vec::new();
    if has_items && state.is_single_row_list() {
        hints.extend([
            Hint::new("/", "Search"),
            Hint::new("↑↓", "Move"),
            Hint::new("d", "Uninstall"),
        ]);
        if state.has_update() {
            hints.push(Hint::new("u", "Upgrade"));
        }
        if let Some(order) = state.sort_order {
            hints.push(Hint::new("⇧S/N/T", order.hint_label()));
        }
    } else if has_items {
        hints.extend([
            Hint::new("␣", "Select"),
            Hint::new("A", "All"),
            Hint::new("N", "None"),
        ]);
        if let Some(order) = state.sort_order {
            hints.push(Hint::new("⇧S/N/T", order.hint_label()));
        }
        hints.push(Hint::new("/", "Search"));
        let all_brew = state
            .scan
            .groups
            .iter()
            .flat_map(|group| &group.items)
            .all(|item| item.brew.is_some());
        if !all_brew {
            hints.push(Hint::new("O", "Reveal"));
        }
        hints.push(Hint::new("↑↓", "Move"));
    }
    hints.push(Hint::new("⎋", "Back"));
    hints.push(Hint::new("?", "More"));
    hints
}

fn is_homebrew(id: &str) -> bool {
    id == homebrew::SCAN_ID || id == homebrew::CLEANUP_ID
}

fn render_nothing_to_show(frame: &mut Frame, area: Rect, state: &SelectState, theme: &Theme) {
    if let Some(problem) = &state.problem {
        let title = format!("{} isn't available", state.scan.title);
        EmptyState::new(theme.g.warning, &title, problem)
            .glyph_color(theme.p.yellow)
            .render(frame, area, theme);
    } else if is_homebrew(&state.scan.id)
        && let Some(note) = state.scan.notes.first()
    {
        EmptyState::new(theme.g.warning, "Homebrew isn't available", note)
            .glyph_color(theme.p.yellow)
            .render(frame, area, theme);
    } else if state.scan.id == homebrew::SCAN_ID {
        EmptyState::new(
            theme.g.package,
            "No Homebrew packages",
            "Packages you install with Homebrew show up here.",
        )
        .render(frame, area, theme);
    } else if state.scan.id == homebrew::CLEANUP_ID {
        EmptyState::new(
            "✓",
            "Homebrew has nothing to clean",
            "No unused dependencies or old versions.",
        )
        .glyph_color(theme.p.green)
        .render(frame, area, theme);
    } else if state.scan.id == SCAN_ID {
        EmptyState::new("✓", "Docker has nothing unused", "Every object is in use.")
            .glyph_color(theme.p.green)
            .render(frame, area, theme);
    } else if state.scan.id == apps::LIST_ID {
        EmptyState::new(
            "✓",
            "No apps to uninstall",
            "Apps in your Applications folders show up here.",
        )
        .glyph_color(theme.p.green)
        .render(frame, area, theme);
    } else if is_plan(state) {
        EmptyState::new(
            "✓",
            "Nothing left to remove",
            "Everything you chose is gone.",
        )
        .glyph_color(theme.p.green)
        .render(frame, area, theme);
    } else {
        EmptyState::new("✓", "Nothing to clean", "Your caches are already tidy.")
            .glyph_color(theme.p.green)
            .render(frame, area, theme);
    }
}

/// The formula the confirm dialog of the Homebrew list is about.
fn homebrew_formula(state: &SelectState) -> Option<&str> {
    if state.scan.id != homebrew::SCAN_ID {
        return None;
    }
    uninstall_name(state.scan.selected_items().first()?)
}

fn uninstall_name(item: &ScanItem) -> Option<&str> {
    match item.brew.as_ref()? {
        homebrew::BrewTask::Uninstall { name, .. } => Some(name),
        homebrew::BrewTask::Cleanup { .. } => None,
    }
}

fn render_confirmation_dialog(
    frame: &mut Frame,
    state: &SelectState,
    focus_btn: ConfirmButton,
    theme: &Theme,
) {
    let count = state.scan.selected_items().len();
    let bundles = state
        .scan
        .selected_items()
        .iter()
        .filter(|item| item.role == Some(UninstallRole::Bundle))
        .count();
    let total = format_bytes(state.scan.total_selected_bytes());
    let (title, msg, action) = match RunKind::of(state) {
        RunKind::Upgrade => return render_upgrade_dialog(frame, state, focus_btn, theme),
        RunKind::Slim => (
            format!("Slim {}?", plural(count, "app", "apps")),
            format!(
                "Saves about {total} of Intel code. The old copy of each app goes to the Trash. Empty it to get the space back."
            ),
            "Slim",
        ),
        RunKind::Compress => (
            format!("Compress {}?", plural(count, "item", "items")),
            format!("Saves about {total} with APFS compression. The files stay where they are."),
            "Compress",
        ),
        RunKind::Remove | RunKind::Uninstall => removal_question(state, count, bundles, &total),
    };

    let mut message = vec![Line::from(msg)];
    let (path_width, _) = Modal::message_size(frame.area());
    let selected = state.scan.selected_items();
    for folder in selected.into_iter().flat_map(|item| &item.also) {
        let path = middle_truncate(&prettify_path(&folder.path), path_width);
        message.push(Line::from(Span::styled(
            format!("Also removes {path}"),
            theme.callout(),
        )));
    }
    let volumes = state
        .scan
        .selected_items()
        .iter()
        .filter(|item| matches!(item.docker, Some(DockerObject::Volume { .. })))
        .count();
    if volumes > 0 {
        message.push(Line::from(Span::styled(
            format!(
                "The data in {} is deleted for good.",
                plural(volumes, "volume", "volumes")
            ),
            theme.warning(),
        )));
    }

    if bundles > 0 {
        message.push(Line::from(Span::styled(
            "Open apps are quit first.",
            theme.callout(),
        )));
    }
    if state.asks_password {
        message.push(Line::from(Span::styled(
            super::terminal::PASSWORD_FIRST,
            theme.warning(),
        )));
    }
    for note in &state.notes {
        message.push(Line::from(Span::styled(note.clone(), theme.callout())));
    }

    let cancel_focused = matches!(focus_btn, ConfirmButton::Cancel);
    let remove_focused = matches!(focus_btn, ConfirmButton::Remove);

    Modal::new(&title)
        .message(message)
        .buttons(vec![
            Button::new("Cancel").focused(cancel_focused),
            Button::destructive(action).focused(remove_focused),
        ])
        .render(frame, frame.area(), theme);
}

/// The question before an upgrade: one line per package, as many as the dialog holds.
fn render_upgrade_dialog(
    frame: &mut Frame,
    state: &SelectState,
    focus_btn: ConfirmButton,
    theme: &Theme,
) {
    let Some(plan) = &state.upgrade else {
        return;
    };
    let title = match plan.packages.as_slice() {
        [only] => format!("Upgrade {}?", only.name),
        many => format!("Upgrade {}?", plural(many.len(), "package", "packages")),
    };
    let (width, height) = Modal::message_size(frame.area());
    // The blank line below the list and the note, which can wrap to two lines.
    let room = height.saturating_sub(3).max(1);
    let shown = if plan.packages.len() > room {
        room - 1
    } else {
        plan.packages.len()
    };
    let mut message: Vec<Line> = plan
        .packages
        .iter()
        .take(shown)
        .map(|package| Line::from(middle_truncate(&homebrew::upgrade_change(package), width)))
        .collect();
    if shown < plan.packages.len() {
        message.push(Line::from(format!(
            "and {} more",
            plan.packages.len() - shown
        )));
    }
    message.push(Line::from(""));
    message.push(Line::from(Span::styled(
        "Homebrew may also upgrade what these need or what needs them.",
        theme.callout(),
    )));

    Modal::new(&title)
        .message(message)
        .buttons(vec![
            Button::new("Cancel").focused(focus_btn == ConfirmButton::Cancel),
            Button::primary("Upgrade").focused(focus_btn == ConfirmButton::Remove),
        ])
        .render(frame, frame.area(), theme);
}

/// The title, the first line and the button of the question before a removal or an uninstall.
fn removal_question(
    state: &SelectState,
    count: usize,
    bundles: usize,
    total: &str,
) -> (String, String, &'static str) {
    if let Some(name) = homebrew_formula(state) {
        return (
            format!("Uninstall {name}?"),
            format!("Homebrew will remove {name} ({total}). This cannot be undone."),
            "Uninstall",
        );
    }
    let selected = state.scan.selected_items();
    let trashed_count = selected
        .iter()
        .filter(|item| state.removal_mode(item) == RemovalMode::Trash)
        .count();
    let perm_count = selected.len().saturating_sub(trashed_count);
    let msg = if trashed_count > 0 && perm_count == 0 {
        format!("{total} will be moved to the Trash. Empty it to get the space back.")
    } else if trashed_count > 0 && perm_count > 0 {
        format!(
            "Personal files and apps ({total}) will be moved to the Trash. System files will be permanently deleted."
        )
    } else {
        format!("{total} will be permanently deleted. This cannot be undone.")
    };
    if bundles > 0 {
        (
            format!("Uninstall {}?", plural(bundles, "app", "apps")),
            msg,
            "Uninstall",
        )
    } else {
        (
            format!("Remove {}?", plural(count, "item", "items")),
            msg,
            "Remove",
        )
    }
}

fn render_removing_dialog(
    frame: &mut Frame,
    progress: &RemovalProgress,
    spinner_idx: usize,
    cancellable: bool,
    kind: RunKind,
    theme: &Theme,
) {
    let count = progress.total_count;
    let title = match kind {
        RunKind::Slim => format!("Slimming {}…", plural(count, "app", "apps")),
        RunKind::Compress => format!("Compressing {}…", plural(count, "item", "items")),
        RunKind::Upgrade => format!("Upgrading {}…", plural(count, "package", "packages")),
        RunKind::Remove | RunKind::Uninstall => {
            format!("Removing {}…", plural(count, "item", "items"))
        }
    };

    let spinner_char = theme.spinner_frame(spinner_idx);

    let pretty_path = prettify_path(&progress.path);
    let path_line = Line::from(vec![
        Span::styled(format!("{spinner_char} "), theme.accent_text()),
        Span::styled(pretty_path, theme.body()),
    ]);

    let fraction = if progress.total_count > 0 {
        progress.current_index as f64 / progress.total_count as f64
    } else {
        0.0
    };

    let bar_spans = bar(fraction, 40, theme.p.accent, theme);

    let mut message = vec![
        path_line,
        Line::from(bar_spans),
        Line::from(vec![Span::styled(
            format!("{} of {}", progress.current_index, progress.total_count),
            theme.footnote(),
        )]),
    ];
    if cancellable {
        message.push(Line::from(vec![Span::styled(
            "Esc stops after the current file.",
            theme.footnote(),
        )]));
    }

    Modal::new(&title)
        .tint(theme.p.orange)
        .message(message)
        .buttons(vec![])
        .render(frame, frame.area(), theme);
}

fn push_totals(message: &mut Vec<Line<'static>>, report: &RemovalReport, theme: &Theme) {
    if report.trashed_bytes > 0 {
        message.push(Line::from(vec![
            Span::styled("Moved ", theme.body()),
            Span::styled(format_bytes(report.trashed_bytes), theme.success()),
            Span::styled(
                " to the Trash. Empty it to get the space back.",
                theme.body(),
            ),
        ]));
    }
    if report.freed_bytes > 0 {
        message.push(Line::from(vec![
            Span::styled("Removed ", theme.body()),
            Span::styled(format_bytes(report.freed_bytes), theme.success()),
            Span::styled(" permanently.", theme.body()),
        ]));
    }
    if report.trashed_bytes == 0 && report.freed_bytes == 0 {
        message.push(Line::from(vec![Span::styled(
            format!(
                "0 B freed across {}",
                plural(report.removed_count, "item", "items")
            ),
            theme.body(),
        )]));
    }
}

fn render_outcome_dialog(frame: &mut Frame, report: &RemovalReport, kind: RunKind, theme: &Theme) {
    let done = report.removed_count > 0;
    let title = match (kind, done) {
        (RunKind::Slim, true) => "Slim Complete",
        (RunKind::Slim, false) => "Nothing was slimmed",
        (RunKind::Compress, true) => "Compress Complete",
        (RunKind::Compress, false) => "Nothing was compressed",
        (RunKind::Upgrade, true) => "Upgrade Complete",
        (RunKind::Upgrade, false) => "Nothing was upgraded",
        (RunKind::Uninstall, true) => "Uninstall Complete",
        (RunKind::Remove, true) => "Clean Complete",
        (RunKind::Uninstall | RunKind::Remove, false) => "Nothing was removed",
    };

    let mut message = Vec::new();
    match kind {
        RunKind::Slim | RunKind::Compress => {
            let saved = if kind == RunKind::Slim {
                " of Intel code.".to_string()
            } else {
                format!(
                    " by compressing {}.",
                    plural(report.removed_count, "file", "files")
                )
            };
            if done {
                message.push(Line::from(vec![
                    Span::styled("Saved ", theme.body()),
                    Span::styled(format_bytes(report.freed_bytes), theme.success()),
                    Span::styled(saved, theme.body()),
                ]));
            }
        }
        RunKind::Upgrade => {
            if done {
                message.push(Line::from(format!(
                    "Upgraded {}.",
                    plural(report.removed_count, "package", "packages")
                )));
            }
        }
        RunKind::Remove | RunKind::Uninstall => push_totals(&mut message, report, theme),
    }

    if !report.failures.is_empty() {
        message.push(Line::from(""));
        message.push(Line::from(Span::styled(
            format!(
                "{} failed to {}:",
                plural(report.failures.len(), "item", "items"),
                kind.verb()
            ),
            theme.danger(),
        )));
        for fail in report.failures.iter().take(MAX_LISTED_FAILURES) {
            message.push(Line::from(Span::styled(
                format!("  ✕ {}", prettify_path(&fail.path)),
                theme.callout(),
            )));
            if matches!(kind, RunKind::Upgrade | RunKind::Slim | RunKind::Compress) {
                message.push(Line::from(Span::styled(
                    fail.error.clone(),
                    theme.callout(),
                )));
            }
        }
        if kind == RunKind::Uninstall {
            let reason = report.failures[0].error.clone();
            message.push(Line::from(Span::styled(reason, theme.callout())));
        }
    }

    if !report.notes.is_empty() {
        message.push(Line::from(""));
    }
    for note in report.notes.iter().take(MAX_LISTED_NOTES) {
        message.push(Line::from(Span::styled(note.clone(), theme.callout())));
    }

    Modal::new(title)
        .message(message)
        .buttons(vec![Button::primary("Done").focused(true)])
        .render(frame, frame.area(), theme);
}

#[cfg(test)]
pub(crate) mod testing {
    use ratatui::backend::TestBackend;
    use ratatui::{Frame, Terminal};

    /// The words `draw` puts on a `width` by `height` screen, so a line a dialog wraps reads
    /// as one.
    pub fn screen_words(width: u16, height: u16, draw: impl FnOnce(&mut Frame)) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(draw).unwrap();
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        rows.join(" ")
            .replace('│', " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::apps::testing::{fake_app, sandbox};
    use crate::core::cleaner::RemovalFailure;
    use crate::ui::env::UiEnv;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Style;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn label_spans_show_every_detail_in_the_footnote_style() {
        let theme = Theme::with_env(UiEnv::default());

        let spans = label_spans("bun 1.4.2 · used by 2 · pinned", &theme);

        let text: Vec<(&str, Style)> = spans
            .iter()
            .map(|s| (s.content.as_ref(), s.style))
            .collect();
        assert_eq!(
            text,
            [
                ("bun 1.4.2", theme.body()),
                (" · used by 2", theme.footnote()),
                (" · pinned", theme.footnote()),
            ]
        );
    }

    fn brew_state(packages: Vec<homebrew::Package>) -> SelectState {
        SelectState::homebrew_list(&homebrew::Inventory {
            packages,
            brew: Some(PathBuf::from("/opt/homebrew/bin/brew")),
            ..homebrew::Inventory::default()
        })
    }

    fn formula(name: &str, bytes: u64) -> homebrew::Package {
        homebrew::testing::package(homebrew::PackageKind::Formula, name, "1.4.2", bytes)
    }

    /// The text of each row of the screen, with the cells' styles.
    fn drawn(
        state: &SelectState,
        theme: &Theme,
        width: u16,
        height: u16,
    ) -> Vec<Vec<(String, Style)>> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| render_select(frame, state, theme))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| (buffer[(x, y)].symbol().to_string(), buffer[(x, y)].style()))
                    .collect()
            })
            .collect()
    }

    fn text_of(row: &[(String, Style)]) -> String {
        row.iter().map(|(symbol, _)| symbol.as_str()).collect()
    }

    fn row_of<'a>(rows: &'a [Vec<(String, Style)>], needle: &str) -> &'a [(String, Style)] {
        rows.iter()
            .find(|row| text_of(row).contains(needle))
            .unwrap_or_else(|| panic!("no row with {needle}"))
    }

    /// The style of the cell where `needle` starts in the row.
    fn style_at(row: &[(String, Style)], needle: &str) -> Style {
        let text = text_of(row);
        let at = text[..text.find(needle).unwrap()].chars().count();
        assert!(needle.starts_with(row[at].0.as_str()), "{text}");
        row[at].1
    }

    #[test]
    fn the_homebrew_list_shows_origin_and_the_new_version_in_columns() {
        let theme = Theme::with_env(UiEnv::default());
        let mut bun = formula("bun", 1000);
        bun.latest_version = Some("1.4.4_1".to_string());
        let mut ada = formula("ada-url", 900);
        ada.on_request = false;
        let mut outdated = formula("outdated", 800);
        outdated.outdated = true;
        let mut ghostty =
            homebrew::testing::package(homebrew::PackageKind::Cask, "ghostty", "1.3.1", 700);
        ghostty.apps = vec!["Ghostty.app".to_string()];
        let state = brew_state(vec![formula("go", 2000), bun, ada, outdated, ghostty]);

        let rows = drawn(&state, &theme, 120, 32);

        let titles = text_of(row_of(&rows, "ORIGIN"));
        assert!(titles.contains("UPDATE"), "{titles}");
        let bun = row_of(&rows, "bun 1.4.2");
        let text = text_of(bun);
        assert!(text.contains("on request"), "{text}");
        assert!(!text.contains(" · "), "{text}");
        assert!(text.contains("↑ 1.4.4_1"), "{text}");
        assert_eq!(style_at(bun, "↑ 1.4.4_1").fg, theme.accent_text().fg);
        assert!(text_of(row_of(&rows, "ada-url")).contains("dependency"));
        let up = text_of(row_of(&rows, "outdated 1.4.2"));
        assert!(up.contains("↑ "), "{up}");
        assert!(!up.contains("↑ 1"), "{up}");
        let cask = text_of(row_of(&rows, "ghostty 1.3.1 · Ghostty.app"));
        assert!(!cask.contains("on request") && !cask.contains("dependency"));
        assert!(!cask.contains('↑'), "{cask}");
        let plain = text_of(row_of(&rows, "go 1.4.2"));
        assert!(!plain.contains('↑'), "{plain}");
    }

    #[test]
    fn the_new_version_shows_an_ascii_arrow_in_an_ascii_theme() {
        let theme = Theme::with_env(UiEnv {
            ascii_glyphs: true,
            ..UiEnv::default()
        });
        let mut bun = formula("bun", 1000);
        bun.latest_version = Some("1.4.4_1".to_string());
        let state = brew_state(vec![formula("go", 2000), bun]);

        let rows = drawn(&state, &theme, 120, 32);

        let text = text_of(row_of(&rows, "bun 1.4.2"));
        assert!(text.contains("^ 1.4.4_1"), "{text}");
        assert!(!text.contains('↑'), "{text}");
    }

    #[test]
    fn below_100_columns_the_update_stays_and_the_origin_goes() {
        let theme = Theme::with_env(UiEnv::default());
        let mut bun = formula("bun", 1000);
        bun.latest_version = Some("1.4.4_1".to_string());
        let state = brew_state(vec![formula("go", 2000), bun]);

        let narrow = drawn(&state, &theme, 99, 32);
        let text = text_of(row_of(&narrow, "bun 1.4.2"));
        assert!(text.contains("↑ 1.4.4_1"), "{text}");
        assert!(!text.contains("on request"), "{text}");
        let titles = text_of(row_of(&narrow, "UPDATE"));
        assert!(!titles.contains("ORIGIN"), "{titles}");

        let wide = drawn(&state, &theme, 110, 32);
        assert!(text_of(row_of(&wide, "bun 1.4.2")).contains("on request"));
    }

    #[test]
    fn a_long_new_version_leaves_the_names_whole_at_80_columns() {
        let theme = Theme::with_env(UiEnv::default());
        let mut python = homebrew::testing::package(
            homebrew::PackageKind::Formula,
            "python@3.14",
            "3.14.8_1",
            2000,
        );
        python.used_by = vec!["azure-cli".to_string()];
        python.pinned = true;
        python.latest_version = Some("3.14.9".to_string());
        let mut editor = homebrew::testing::package(
            homebrew::PackageKind::Cask,
            "editor",
            "2025.2.3,252.26830.99",
            1000,
        );
        editor.latest_version = Some("2025.2.4,252.27397.103".to_string());
        let state = brew_state(vec![python, editor]);

        let rows = drawn(&state, &theme, 80, 24);

        let python = text_of(row_of(&rows, "python@3.14"));
        assert!(
            python.contains("python@3.14 3.14.8_1 · used by 1 · pinned"),
            "{python}"
        );
        let editor = text_of(row_of(&rows, "editor 2025"));
        let update = editor[editor.find('↑').unwrap()..]
            .split("  ")
            .next()
            .unwrap();
        assert!(update.chars().count() <= 14, "{update}");
    }

    #[test]
    fn the_origin_shows_at_the_width_where_the_share_bar_shows() {
        let theme = Theme::with_env(UiEnv::default());
        let state = brew_state(vec![formula("go", 2000), formula("bun", 1000)]);

        let titles = |width| text_of(row_of(&drawn(&state, &theme, width, 32), "UPDATE"));

        let narrow = titles(108);
        assert!(!narrow.contains("ORIGIN"), "{narrow}");
        assert!(!narrow.contains("SHARE"), "{narrow}");
        let wide = titles(109);
        assert!(wide.contains("ORIGIN"), "{wide}");
        assert!(wide.contains("SHARE"), "{wide}");
    }

    #[test]
    fn a_pin_found_by_the_check_renames_the_row_and_keeps_the_focus() {
        let cask = |name, bytes| {
            homebrew::testing::package(homebrew::PackageKind::Cask, name, "1.2.0", bytes)
        };
        let mut state = brew_state(vec![
            formula("go", 3000),
            cask("ghostty", 2000),
            cask("zed", 1000),
        ]);
        let focused = |state: &SelectState| {
            let rows = state.flattened_rows();
            rows[state.focused_index].display_path.clone()
        };
        state.focused_index = state
            .flattened_rows()
            .iter()
            .position(|row| row.display_path.starts_with("ghostty"))
            .unwrap();
        let latest = homebrew::parse_outdated(
            r#"{"formulae": [], "casks": [{"name": "ghostty", "current_version": "1.3", "pinned": true}]}"#,
        )
        .unwrap();

        state.finish_update_check(None, Ok(latest));

        assert_eq!(focused(&state), "ghostty 1.2.0 · pinned");
    }

    fn same_label_pair() -> SelectState {
        let mut formula_codex =
            homebrew::testing::package(homebrew::PackageKind::Formula, "codex", "0.166.0", 3000);
        formula_codex.latest_version = Some("0.170.0".to_string());
        let mut cask_codex =
            homebrew::testing::package(homebrew::PackageKind::Cask, "codex", "0.166.0", 2000);
        cask_codex.latest_version = Some("0.171.0".to_string());
        brew_state(vec![formula_codex, cask_codex, formula("zed", 1000)])
    }

    fn focus_kind(state: &mut SelectState, kind: homebrew::PackageKind) {
        state.focused_index = state
            .flattened_rows()
            .iter()
            .position(|row| {
                row.display_path.starts_with("codex")
                    && row.item_index.is_some()
                    && homebrew::group_kind(&state.scan.groups[row.group_index].tool) == kind
            })
            .unwrap();
    }

    #[test]
    fn a_formula_and_a_cask_with_the_same_label_keep_their_own_rows() {
        let theme = Theme::with_env(UiEnv::default());
        let mut state = same_label_pair();

        focus_kind(&mut state, homebrew::PackageKind::Formula);
        state.uninstall_focused();
        assert!(matches!(state.overlay, ModalOverlay::Confirm(_)));
        assert!(state.toast.is_none());
        state.close_overlay();

        focus_kind(&mut state, homebrew::PackageKind::Cask);
        state.uninstall_focused();
        assert!(!matches!(state.overlay, ModalOverlay::Confirm(_)));

        let rows = drawn(&state, &theme, 120, 32);
        let codex: Vec<String> = rows
            .iter()
            .map(|row| text_of(row))
            .filter(|text| text.contains("codex 0.166.0"))
            .collect();
        assert_eq!(codex.len(), 2, "{codex:?}");
        assert!(codex[0].contains("on request") && codex[0].contains("↑ 0.170.0"));
        assert!(!codex[1].contains("on request") && codex[1].contains("↑ 0.171.0"));

        state.upgrade_focused();
        let plan = state.upgrade.as_ref().unwrap();
        assert_eq!(plan.packages[0].kind, homebrew::PackageKind::Cask);
        assert_eq!(plan.packages[0].latest_version.as_deref(), Some("0.171.0"));
        state.close_overlay();
        focus_kind(&mut state, homebrew::PackageKind::Formula);
        state.upgrade_focused();
        let plan = state.upgrade.as_ref().unwrap();
        assert_eq!(plan.packages[0].kind, homebrew::PackageKind::Formula);
        assert_eq!(plan.packages[0].latest_version.as_deref(), Some("0.170.0"));
    }

    #[test]
    fn a_check_that_pins_the_cask_leaves_the_formula_with_the_same_label_in_the_list() {
        let mut state = same_label_pair();
        let latest = homebrew::parse_outdated(
            r#"{"formulae": [], "casks": [{"name": "codex", "current_version": "0.171.0", "pinned": true}]}"#,
        )
        .unwrap();

        state.finish_update_check(None, Ok(latest));

        let theme = Theme::with_env(UiEnv::default());
        let rows = drawn(&state, &theme, 120, 32);
        let codex = rows
            .iter()
            .map(|row| text_of(row))
            .filter(|text| text.contains("codex 0.166.0"))
            .count();
        assert_eq!(codex, 2);
        focus_kind(&mut state, homebrew::PackageKind::Formula);
        state.uninstall_focused();
        assert!(matches!(state.overlay, ModalOverlay::Confirm(_)));
    }

    #[test]
    fn a_pin_that_moves_rows_above_the_focus_keeps_the_focus_on_its_package() {
        let cask = |name, bytes| {
            homebrew::testing::package(homebrew::PackageKind::Cask, name, "1.2.0", bytes)
        };
        let mut small = cask("small", 1000);
        small.pinned = true;
        let mut state = brew_state(vec![cask("big", 3000), small]);
        state.search_query = "pinned".to_string();
        let focused = |state: &SelectState| {
            let rows = state.flattened_rows();
            rows[state.focused_index].display_path.clone()
        };
        state.focused_index = state
            .flattened_rows()
            .iter()
            .position(|row| row.display_path.starts_with("small"))
            .unwrap();
        let latest = homebrew::parse_outdated(
            r#"{"formulae": [], "casks": [{"name": "big", "current_version": "1.3", "pinned": true}]}"#,
        )
        .unwrap();

        state.finish_update_check(None, Ok(latest));

        assert_eq!(focused(&state), "small 1.2.0 · pinned");
    }

    #[test]
    fn the_share_bar_of_the_homebrew_list_has_at_most_12_cells() {
        let theme = Theme::with_env(UiEnv::default());
        let state = brew_state(vec![formula("go", 2000), formula("bun", 1000)]);

        for width in [120, 200] {
            let rows = drawn(&state, &theme, width, 32);
            let text = text_of(row_of(&rows, "bun 1.4.2"));
            let cells = text.matches(['━', '─']).count();
            assert!((1..=12).contains(&cells), "{width}: {cells}");
            assert!(text.contains('%'), "{text}");
        }
    }

    #[test]
    fn the_share_bar_of_large_files_stays_wider_than_12_cells() {
        let theme = Theme::with_env(UiEnv::default());
        let file = |path: &str, bytes| ScanItem::new(PathBuf::from(path), bytes, false, false);
        let state = SelectState::new(CategoryScan {
            id: large_files::SCAN_ID.to_string(),
            title: "Large Files".to_string(),
            notes: Vec::new(),
            groups: vec![ToolGroup {
                tool: "Downloads".to_string(),
                items: vec![
                    file("/Users/test/Downloads/big.iso", 2000),
                    file("/Users/test/Downloads/small.zip", 1000),
                ],
            }],
        });

        let rows = drawn(&state, &theme, 200, 32);

        let text = text_of(row_of(&rows, "small.zip"));
        let cells = text.matches(['━', '─']).count();
        assert!(cells > 12, "{cells}: {text}");
    }

    fn plan_with_app(app: &Path) -> CategoryScan {
        let mut item = ScanItem::new(app.to_path_buf(), 100, true, false);
        item.role = Some(UninstallRole::Bundle);
        CategoryScan {
            notes: Vec::new(),
            id: uninstaller::PLAN_ID.to_string(),
            title: "Review".to_string(),
            groups: vec![ToolGroup {
                tool: "Foo".to_string(),
                items: vec![item],
            }],
        }
    }

    fn screen(state: &SelectState) -> String {
        let theme = Theme::with_env(UiEnv::default());
        let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
        terminal
            .draw(|frame| render_select(frame, state, &theme))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| (x, y)))
            .map(|position| buffer[position].symbol())
            .collect()
    }

    #[test]
    fn the_upgrade_question_keeps_its_note_and_buttons_in_a_narrow_window() {
        let packages = (0..20)
            .map(|i| {
                let name = format!("microsoft-auto-update-{i:02}");
                let mut package = homebrew::testing::package(
                    homebrew::PackageKind::Cask,
                    &name,
                    "4.79.24120731",
                    1,
                );
                package.latest_version = Some("4.80.25010832".to_string());
                package
            })
            .collect();
        let mut state = SelectState::homebrew_list(&homebrew::Inventory {
            packages,
            brew: Some(PathBuf::from("/opt/homebrew/bin/brew")),
            ..homebrew::Inventory::default()
        });
        state.upgrade_all();
        let theme = Theme::with_env(UiEnv::default());

        let text = testing::screen_words(40, 20, |frame| render_select(frame, &state, &theme));

        assert!(text.contains("Upgrade 20 packages?"), "{text}");
        assert!(text.contains("and 14 more"), "{text}");
        assert!(
            text.contains("Homebrew may also upgrade what these need or what needs them."),
            "{text}"
        );
        assert!(text.contains("Cancel Upgrade"), "{text}");
    }

    #[test]
    fn closing_an_upgrade_outcome_with_no_brew_to_read_does_not_say_updating() {
        let mut bun = homebrew::testing::package(homebrew::PackageKind::Formula, "bun", "1", 1);
        bun.latest_version = Some("2".to_string());
        let mut state = SelectState::homebrew_list(&homebrew::Inventory {
            packages: vec![bun.clone()],
            ..homebrew::Inventory::default()
        });
        state.upgrade = Some(homebrew::UpgradePlan {
            brew: PathBuf::from("/opt/homebrew/bin/brew"),
            packages: vec![bun],
        });
        state.overlay = ModalOverlay::Outcome(RemovalReport::default());

        let request = state.close_upgrade_outcome();

        assert!(request.is_none());
        assert!(!state.refreshing);
        assert!(matches!(state.overlay, ModalOverlay::None));
    }

    #[test]
    fn a_slim_that_saved_nothing_says_saved_0_b() {
        let mut state = SelectState::new(CategoryScan {
            notes: Vec::new(),
            id: slim::SCAN_ID.to_string(),
            title: "Slim".to_string(),
            groups: Vec::new(),
        });
        state.overlay = ModalOverlay::Outcome(RemovalReport {
            removed_count: 1,
            ..Default::default()
        });

        let text = screen(&state);

        assert!(text.contains("Saved 0 B of Intel code."), "{text}");
        assert!(!text.contains("0 B freed across"), "{text}");
    }

    fn scan_of(id: &str, title: &str) -> CategoryScan {
        CategoryScan {
            notes: Vec::new(),
            id: id.to_string(),
            title: title.to_string(),
            groups: Vec::new(),
        }
    }

    fn failures(count: usize) -> Vec<RemovalFailure> {
        (1..=count)
            .map(|n| RemovalFailure {
                path: PathBuf::from(format!("/Applications/App{n}.app")),
                error: format!("Reason number {n} stayed unclear"),
            })
            .collect()
    }

    #[test]
    fn a_slim_outcome_says_why_the_item_failed() {
        let mut state = SelectState::new(scan_of(slim::SCAN_ID, "Slim"));
        let error = "Step 7 failed: macOS refused. Allow your terminal in System Settings > \
                     Privacy & Security > App Management, or move the app to the Trash in Finder";
        state.overlay = ModalOverlay::Outcome(RemovalReport {
            failures: vec![RemovalFailure {
                path: PathBuf::from("/Applications/Foo.app"),
                error: error.to_string(),
            }],
            ..Default::default()
        });

        let theme = Theme::with_env(UiEnv::default());
        let text = testing::screen_words(120, 32, |frame| render_select(frame, &state, &theme));

        assert!(text.contains("/Applications/Foo.app"), "{text}");
        assert!(text.contains("Step 7 failed: macOS refused."), "{text}");
        assert!(
            text.contains("move the app to the Trash in Finder"),
            "{text}"
        );
    }

    #[test]
    fn a_compress_outcome_lists_three_failures_with_their_errors() {
        let mut state = SelectState::new(scan_of(compress::SCAN_ID, "Compress"));
        state.overlay = ModalOverlay::Outcome(RemovalReport {
            failures: failures(4),
            ..Default::default()
        });

        let text = screen(&state);

        assert!(text.contains("4 items failed to compress:"), "{text}");
        for n in 1..=3 {
            assert!(text.contains(&format!("App{n}.app")), "{text}");
            assert!(
                text.contains(&format!("Reason number {n} stayed unclear")),
                "{text}"
            );
        }
        assert!(!text.contains("App4.app"), "{text}");
    }

    #[test]
    fn a_remove_outcome_shows_only_the_path_of_a_failure() {
        let mut state = SelectState::new(scan_of("purge", "Clean"));
        state.overlay = ModalOverlay::Outcome(RemovalReport {
            failures: failures(1),
            ..Default::default()
        });

        let text = screen(&state);

        assert!(text.contains("App1.app"), "{text}");
        assert!(!text.contains("Reason number 1"), "{text}");
    }

    #[test]
    fn rendering_the_uninstall_screen_does_not_touch_the_bundle() {
        let (_temp, layout) = sandbox();
        let app = layout.system("Applications/Foo.app");
        fake_app(&app, "com.acme.foo", "Foo");
        let state = SelectState::with_preserved_selection(plan_with_app(&app));
        let mtime = || {
            fs::metadata(app.join("Contents"))
                .unwrap()
                .modified()
                .unwrap()
        };
        let before = mtime();

        for _ in 0..100 {
            screen(&state);
        }

        assert_eq!(mtime(), before);
    }

    #[test]
    fn routes_are_worked_out_once() {
        let (_temp, layout) = sandbox();
        let apps = layout.system("Applications");
        let app = apps.join("Foo.app");
        fake_app(&app, "com.acme.foo", "Foo");
        let scan = plan_with_app(&app);
        let routes = scan
            .groups
            .iter()
            .flat_map(|group| &group.items)
            .map(|item| (item.path.clone(), uninstaller::route(item, false)))
            .collect();
        let state = SelectState::with_preserved_selection(scan).with_routes(routes);
        assert!(screen(&state).contains("(Trash)"));

        fs::set_permissions(&apps, fs::Permissions::from_mode(0o555)).unwrap();
        let after = screen(&state);
        fs::set_permissions(&apps, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(after.contains("(Trash)"), "{after}");
    }
}

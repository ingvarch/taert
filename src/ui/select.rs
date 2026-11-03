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

use super::analyze::SortOrder;
use super::components::button::Button;
use super::components::chrome::AppChrome;
use super::components::empty::EmptyState;
use super::components::frame::{FRAME_SIDES, frame_bottom, frame_top, framed};
use super::components::gauge::bar;
use super::components::hints::{Hint, PrimaryAction};
use super::components::list::{ColumnTitles, ListRow, SIZE_W, row_path_spans};
use super::components::modal::Modal;
use super::components::text::plural;
use super::components::toast::Toast;
use super::theme::Theme;
use crate::core::apps;
use crate::core::bytes::format_bytes;
use crate::core::cleaner::{RemovalMode, RemovalProgress, RemovalReport, item_removal_mode};
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
    /// Why the Homebrew list leaves a row alone, by the label of the row.
    pub refusals: HashMap<PathBuf, String>,
    /// What the Homebrew list was built from, so a removal can update it without reading brew.
    pub homebrew: Option<homebrew::Inventory>,
    /// The rows of the Homebrew tree that are open, by their path of names.
    pub expanded: HashSet<Vec<String>>,
    pub toast: Option<Toast>,
}

const BAR_MIN_WIDTH: usize = 100;
const MAX_LISTED_FAILURES: usize = 3;
const MAX_LISTED_NOTES: usize = 3;
// Fits "About an hour ago", the longest age that lasts more than a few seconds.
const AGE_W: usize = 17;

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
        state
    }

    /// Closes the outcome of a removal on the Homebrew list. The formulae that went leave the
    /// list and the ones they used are updated, with the search and the focus kept.
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
        let mut fresh = Self::homebrew_list(&inventory).with_permanent(self.permanent);
        fresh.search_query = std::mem::take(&mut self.search_query);
        fresh.expanded = std::mem::take(&mut self.expanded);
        if fresh.flattened_rows().is_empty() {
            // A search that matched only what went would show nothing.
            fresh.search_query.clear();
        }
        fresh.focus_item_at_or_before(self.focused_index);
        *self = fresh;
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

        let Some(focused) = focused else { return };
        let rows = self.flattened_rows();
        let same_item = |row: &FlattenedRow| {
            row.is_item()
                && row.group_index == focused.group_index
                && row.full_path == focused.full_path
        };
        if let Some(index) = rows.iter().position(same_item) {
            self.focused_index = index;
        }
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
                places.insert(item.path.clone(), (g_idx, i_idx));
            }
        }
        homebrew::tree_sections(inventory, &self.expanded)
            .into_iter()
            .filter_map(|(name, rows)| {
                let group_index = self.scan.groups.iter().position(|g| g.tool == name)?;
                let entries = rows
                    .into_iter()
                    .filter_map(|row| {
                        let (group, item) =
                            *places.get(Path::new(&homebrew::label(row.package)))?;
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
                let date = if self.scan.id == apps::LIST_ID
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

    /// The formula D uninstalls on the Homebrew list, if the focused row allows it.
    fn removable(&self) -> Option<&str> {
        let item = self.focused_item()?;
        if self.refusals.contains_key(&item.path) {
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
        if let Some(text) = self.refusals.get(&self.scan.groups[group].items[item].path) {
            self.toast = Some(Toast::error(text.clone()));
        } else if self.removable().is_some() {
            self.scan.groups[group].items[item].selected = true;
            self.open_confirmation();
        }
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
            self.asks_password = self.scan.id == uninstaller::PLAN_ID
                && uninstaller::needs_password(&self.scan, &self.routes);
            self.overlay = ModalOverlay::Confirm(ConfirmButton::Cancel);
        }
    }

    pub fn close_overlay(&mut self) {
        self.overlay = ModalOverlay::None;
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
        Line::from(vec![
            Span::styled(plural(total_count, "package", "packages"), theme.headline()),
            Span::styled("  ·  ", theme.callout()),
            Span::styled(
                format_bytes(total_shown_bytes),
                theme.size(total_shown_bytes),
            ),
        ])
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
    let primary_action = if state.is_searching {
        PrimaryAction::new("Apply", enter)
    } else if id == homebrew::SCAN_ID {
        homebrew_primary(state, enter)
    } else if id == uninstaller::PLAN_ID && selected_count > 0 {
        let items = plural(selected_count, "item", "items");
        PrimaryAction::new(format!("Uninstall {items}"), enter)
    } else if id == uninstaller::PLAN_ID {
        PrimaryAction::disabled("Uninstall", enter)
    } else if id == apps::LIST_ID && selected_count > 0 {
        let chosen = plural(selected_count, "app", "apps");
        PrimaryAction::new(format!("Review {chosen}"), enter)
    } else if id == apps::LIST_ID && total_count > 0 {
        PrimaryAction::new("Review app", enter)
    } else if id == slim::SCAN_ID && selected_count > 0 {
        let chosen = plural(selected_count, "app", "apps");
        PrimaryAction::new(format!("Slim {chosen}"), enter)
    } else if id == slim::SCAN_ID && total_count > 0 {
        PrimaryAction::new("Slim app", enter)
    } else if selected_count > 0 {
        PrimaryAction::new(format!("Review {selected_count} items"), enter)
    } else {
        PrimaryAction::disabled("Review items", enter)
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
fn render_notes(frame: &mut Frame, area: Rect, notes: &[String], theme: &Theme) -> Rect {
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
            render_removing_dialog(frame, progress, *spinner_idx, cancel.is_some(), theme);
        }
        ModalOverlay::Outcome(report) => {
            let uninstall = is_plan(state) || state.scan.id == homebrew::SCAN_ID;
            render_outcome_dialog(frame, report, uninstall, theme);
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
    match label.split_once(" · ") {
        Some((name, details)) => vec![
            Span::styled(name.to_string(), theme.body()),
            Span::styled(format!(" · {details}"), theme.footnote()),
        ],
        None => vec![Span::styled(label.to_string(), theme.body())],
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
            Hint::new("D", "Uninstall"),
        ]);
    } else if has_items {
        hints.extend([
            Hint::new("␣", "Select"),
            Hint::new("A", "All"),
            Hint::new("N", "None"),
        ]);
        if let Some(order) = state.sort_order {
            hints.push(Hint::new("⇧S/N/D", order.hint_label()));
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
    let noun = if count == 1 { "item" } else { "items" };
    let bundles = state
        .scan
        .selected_items()
        .iter()
        .filter(|item| item.role == Some(UninstallRole::Bundle))
        .count();
    let formula = homebrew_formula(state);
    let title = if let Some(name) = formula {
        format!("Uninstall {name}?")
    } else if state.scan.id == slim::SCAN_ID {
        format!("Slim {}?", plural(count, "app", "apps"))
    } else if bundles > 0 {
        format!("Uninstall {}?", plural(bundles, "app", "apps"))
    } else {
        format!("Remove {count} {noun}?")
    };

    let total_bytes = state.scan.total_selected_bytes();
    let selected = state.scan.selected_items();
    let trashed_count = selected
        .iter()
        .filter(|item| state.removal_mode(item) == RemovalMode::Trash)
        .count();
    let perm_count = selected.len().saturating_sub(trashed_count);

    let msg = if let Some(name) = formula {
        format!(
            "Homebrew will remove {name} ({}). This cannot be undone.",
            format_bytes(total_bytes)
        )
    } else if trashed_count > 0 && perm_count == 0 {
        format!(
            "{} will be moved to the Trash. Empty it to get the space back.",
            format_bytes(total_bytes)
        )
    } else if trashed_count > 0 && perm_count > 0 {
        format!(
            "Personal files and apps ({}) will be moved to the Trash. System files will be permanently deleted.",
            format_bytes(total_bytes)
        )
    } else {
        format!(
            "{} will be permanently deleted. This cannot be undone.",
            format_bytes(total_bytes)
        )
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
            "macOS will ask for your password.",
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
            Button::destructive(if state.scan.id == slim::SCAN_ID {
                "Slim"
            } else if bundles > 0 || formula.is_some() {
                "Uninstall"
            } else {
                "Remove"
            })
            .focused(remove_focused),
        ])
        .render(frame, frame.area(), theme);
}

fn render_removing_dialog(
    frame: &mut Frame,
    progress: &RemovalProgress,
    spinner_idx: usize,
    cancellable: bool,
    theme: &Theme,
) {
    let count = progress.total_count;
    let noun = if count == 1 { "item" } else { "items" };
    let title = format!("Removing {count} {noun}…");

    let spinner_char = if theme.env.reduce_motion {
        "*"
    } else {
        match spinner_idx % 4 {
            0 => "⠋",
            1 => "⠙",
            2 => "⠹",
            _ => "⠸",
        }
    };

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

fn render_outcome_dialog(
    frame: &mut Frame,
    report: &RemovalReport,
    uninstall: bool,
    theme: &Theme,
) {
    let title = if report.notes.iter().any(|n| n.contains("Trash is emptied")) {
        if report.removed_count > 0 {
            "Slim Complete"
        } else {
            "Nothing was slimmed"
        }
    } else {
        match (report.removed_count > 0, uninstall) {
            (true, true) => "Uninstall Complete",
            (true, false) => "Clean Complete",
            (false, _) => "Nothing was removed",
        }
    };

    let mut message = Vec::new();
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

    if !report.failures.is_empty() {
        message.push(Line::from(""));
        message.push(Line::from(Span::styled(
            format!(
                "{} failed to remove:",
                plural(report.failures.len(), "item", "items")
            ),
            theme.danger(),
        )));
        for fail in report.failures.iter().take(MAX_LISTED_FAILURES) {
            message.push(Line::from(Span::styled(
                format!("  ✕ {}", prettify_path(&fail.path)),
                theme.callout(),
            )));
        }
        if uninstall {
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
mod tests {
    use super::*;
    use crate::core::apps::testing::{fake_app, sandbox};
    use crate::ui::env::UiEnv;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

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

//! Plain text output, selection, and review for non-TUI environments.

use std::io::{self, BufRead, Write};

use crate::core::bytes::format_bytes;
use crate::core::cleaner::{
    Cleaner, RemovalMode, RemovalProgress, RemovalReport, item_removal_mode,
};
use crate::core::history::ItemOutcome;
use crate::core::host::MacHost;
use crate::core::scanner::CategoryScan;
use crate::core::stores::IgnoreStore;

pub struct PlainRunner;

impl PlainRunner {
    /// Print scan results in human-readable plain text format.
    pub fn print_scan(scan: &CategoryScan) {
        Self::print_scan_with_options(scan, false);
    }

    pub fn print_scan_with_options(scan: &CategoryScan, permanent: bool) {
        print!("{}", Self::scan_text_with_options(scan, permanent));
    }

    pub fn scan_text(scan: &CategoryScan) -> String {
        Self::scan_text_with_options(scan, false)
    }

    pub fn scan_text_with_options(scan: &CategoryScan, permanent: bool) -> String {
        let mut text = format!("\n=== {} ===\n", scan.title);
        if scan.groups.is_empty() {
            text.push_str("No items found.\n");
            text.push_str(&Self::notes_text(&scan.notes));
            return text;
        }

        let mut index = 1;
        for group in &scan.groups {
            text.push_str(&format!("\n[{}]\n", group.tool));
            for item in &group.items {
                let status_mark = if item.selected { "[x]" } else { "[ ]" };
                let dest = match item_removal_mode(item, &scan.id, permanent) {
                    RemovalMode::Trash => " (Trash)",
                    RemovalMode::Permanent => " (Permanent)",
                };
                text.push_str(&format!(
                    "  {:3}. {} {:>9}  {}{}\n",
                    index,
                    status_mark,
                    format_bytes(item.size_bytes),
                    item.path.display(),
                    dest
                ));
                for folder in &item.also {
                    let folder_dest = match item_removal_mode(folder, &scan.id, permanent) {
                        RemovalMode::Trash => " (Trash)",
                        RemovalMode::Permanent => " (Permanent)",
                    };
                    text.push_str(&format!(
                        "{:22}{}{}\n",
                        "",
                        folder.path.display(),
                        folder_dest
                    ));
                }
                index += 1;
            }
        }

        text.push_str(&format!(
            "\nTotal found: {}  |  Selected by default: {}\n",
            format_bytes(scan.total_all_bytes()),
            format_bytes(scan.total_selected_bytes())
        ));
        text.push_str(&Self::notes_text(&scan.notes));
        text
    }

    /// Allow interactive selection in terminal line mode (e.g. "1, 3-5, all, none")
    pub fn prompt_selection(scan: &mut CategoryScan) -> io::Result<()> {
        let total_items: usize = scan.groups.iter().map(|g| g.items.len()).sum();
        if total_items == 0 {
            return Ok(());
        }

        print!(
            "\nEnter item numbers to toggle (e.g. '1,3-5', 'all', 'none', or Enter to keep current): "
        );
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().lock().read_line(&mut input)?;
        let trimmed = input.trim();

        if trimmed.is_empty() {
            return Ok(());
        }

        if trimmed.eq_ignore_ascii_case("all") {
            for g in &mut scan.groups {
                for item in &mut g.items {
                    item.selected = true;
                }
            }
            return Ok(());
        }

        if trimmed.eq_ignore_ascii_case("none") {
            for g in &mut scan.groups {
                for item in &mut g.items {
                    item.selected = false;
                }
            }
            return Ok(());
        }

        // Parse individual numbers or ranges
        for part in trimmed.split(',') {
            let part = part.trim();
            if let Some((start_s, end_s)) = part.split_once('-') {
                if let (Ok(start), Ok(end)) = (
                    start_s.trim().parse::<usize>(),
                    end_s.trim().parse::<usize>(),
                ) {
                    for num in start..=end {
                        Self::toggle_item_by_index(scan, num);
                    }
                }
            } else if let Ok(num) = part.parse::<usize>() {
                Self::toggle_item_by_index(scan, num);
            }
        }

        Ok(())
    }

    fn toggle_item_by_index(scan: &mut CategoryScan, index: usize) {
        if index == 0 {
            return;
        }
        let mut curr = 1;
        for g in &mut scan.groups {
            for item in &mut g.items {
                if curr == index {
                    item.selected = !item.selected;
                    return;
                }
                curr += 1;
            }
        }
    }

    /// Confirm permanent deletion in plain text
    pub fn prompt_confirm(scan: &CategoryScan) -> io::Result<bool> {
        let selected_bytes = scan.total_selected_bytes();
        let selected_count = scan.selected_items().len();

        if selected_count == 0 {
            println!("\nNothing selected for deletion.");
            return Ok(false);
        }

        println!("\nSelected items for permanent deletion:");
        for item in scan.selected_items() {
            println!(
                "  - {:>9}  {}",
                format_bytes(item.size_bytes),
                item.path.display()
            );
            for folder in &item.also {
                println!("{:15}{}", "", folder.path.display());
            }
        }

        println!(
            "\nWARNING: Permanent deletion of {} item(s) ({}).",
            selected_count,
            format_bytes(selected_bytes)
        );
        print!("Type 'yes' to confirm permanent deletion: ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().lock().read_line(&mut input)?;

        Ok(input.trim() == "yes")
    }

    /// Execute cleaning and print results
    pub fn execute_clean(scan: &CategoryScan, ignore: &IgnoreStore) -> RemovalReport {
        Self::execute_clean_with_callbacks(scan, ignore, None::<fn(&ItemOutcome)>)
    }

    /// Execute cleaning with outcome callback and print results
    pub fn execute_clean_with_callbacks<O>(
        scan: &CategoryScan,
        ignore: &IgnoreStore,
        outcome_cb: Option<O>,
    ) -> RemovalReport
    where
        O: FnMut(&ItemOutcome),
    {
        let items: Vec<_> = scan.selected_items().into_iter().cloned().collect();
        let report = Cleaner::new(&MacHost, RemovalMode::Permanent).remove_items_with_callbacks(
            &items,
            ignore,
            None::<fn(RemovalProgress)>,
            outcome_cb,
        );
        Self::print_report("Clean", &report);
        report
    }

    /// Print what a removal did, then anything that went wrong or needs attention.
    pub fn print_report(action: &str, report: &RemovalReport) {
        println!("\n{action} complete:");
        if report.trashed_bytes > 0 {
            println!(
                "Moved {} to the Trash. Empty it to get the space back.",
                format_bytes(report.trashed_bytes)
            );
        }
        if report.freed_bytes > 0 {
            println!("Removed {} permanently.", format_bytes(report.freed_bytes));
        }
        if report.trashed_bytes == 0 && report.freed_bytes == 0 {
            if report.removed_count > 0 {
                println!("Removed {} items.", report.removed_count);
            } else {
                println!("Nothing was removed.");
            }
        }

        if !report.failures.is_empty() {
            eprintln!("\nEncountered {} error(s):", report.failures.len());
            for failure in &report.failures {
                eprintln!("  - {}: {}", failure.path.display(), failure.error);
            }
        }

        Self::print_notes(&report.notes);
    }

    pub fn print_notes(notes: &[String]) {
        print!("{}", Self::notes_text(notes));
    }

    fn notes_text(notes: &[String]) -> String {
        if notes.is_empty() {
            return String::new();
        }
        let lines: String = notes.iter().map(|note| format!("Note: {note}\n")).collect();
        format!("\n{lines}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::scanner::{ScanItem, ToolGroup};
    use std::path::PathBuf;

    fn scan_with_note() -> CategoryScan {
        CategoryScan {
            id: "junk".to_string(),
            title: "System Junk".to_string(),
            groups: Vec::new(),
            notes: vec!["Safari needs Full Disk Access.".to_string()],
        }
    }

    #[test]
    fn a_row_prints_every_folder_it_holds() {
        let mut row = ScanItem::new(PathBuf::from("/first"), 3000, true, false);
        row.also = vec![ScanItem::new(PathBuf::from("/second"), 1000, true, false)];
        let mut scan = scan_with_note();
        scan.groups.push(ToolGroup {
            tool: "Browser caches".to_string(),
            items: vec![row],
        });
        let text = PlainRunner::scan_text(&scan);
        assert!(text.contains("/first"), "{text}");
        assert!(text.contains("/second"), "{text}");
    }

    #[test]
    fn a_scan_with_no_rows_still_prints_its_notes() {
        let text = PlainRunner::scan_text(&scan_with_note());
        assert!(
            text.contains("Note: Safari needs Full Disk Access."),
            "{text}"
        );
    }
}

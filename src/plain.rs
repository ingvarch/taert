//! Plain text output, selection, and review for non-TUI environments.

use std::io::{self, BufRead, Write};

use crate::core::bytes::format_bytes;
use crate::core::cleaner::{Cleaner, RemovalReport};
use crate::core::scanner::CategoryScan;
use crate::core::stores::IgnoreStore;

pub struct PlainRunner;

impl PlainRunner {
    /// Print scan results in human-readable plain text format.
    pub fn print_scan(scan: &CategoryScan) {
        print!("{}", Self::scan_text(scan));
    }

    fn scan_text(scan: &CategoryScan) -> String {
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
                text.push_str(&format!(
                    "  {:3}. {} {:>9}  {}\n",
                    index,
                    status_mark,
                    format_bytes(item.size_bytes),
                    item.path.display()
                ));
                for folder in &item.also {
                    text.push_str(&format!("{:22}{}\n", "", folder.path.display()));
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
        let items: Vec<_> = scan.selected_items().into_iter().cloned().collect();
        let mut report = Cleaner::remove_items(&items, ignore);
        report.notes.extend(scan.notes.iter().cloned());
        Self::print_report("Clean", &report);
        report
    }

    /// Print what a removal did, then anything that went wrong or needs attention.
    pub fn print_report(action: &str, report: &RemovalReport) {
        println!(
            "\n{action} complete: removed {} items, freed {}.",
            report.removed_count,
            format_bytes(report.freed_bytes)
        );

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

    #[test]
    fn a_forced_clean_reports_the_notes_of_the_scan() {
        let report = PlainRunner::execute_clean(&scan_with_note(), &IgnoreStore::default());
        assert_eq!(report.notes, ["Safari needs Full Disk Access."]);
    }
}

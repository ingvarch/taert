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
        println!("\n=== {} ===", scan.title);
        if scan.groups.is_empty() {
            println!("No items found.");
            return;
        }

        let mut index = 1;
        for group in &scan.groups {
            println!("\n[{}]", group.tool);
            for item in &group.items {
                let status_mark = if item.selected { "[x]" } else { "[ ]" };
                println!(
                    "  {:3}. {} {:>9}  {}",
                    index,
                    status_mark,
                    format_bytes(item.size_bytes),
                    item.path.display()
                );
                index += 1;
            }
        }

        println!(
            "\nTotal found: {}  |  Selected by default: {}",
            format_bytes(scan.total_all_bytes()),
            format_bytes(scan.total_selected_bytes())
        );
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
        let report = Cleaner::remove_items(&items, ignore);

        println!(
            "\nClean complete: removed {} items, freed {}.",
            report.removed_count,
            format_bytes(report.freed_bytes)
        );

        if !report.failures.is_empty() {
            eprintln!("\nEncountered {} error(s):", report.failures.len());
            for failure in &report.failures {
                eprintln!("  - {}: {}", failure.path.display(), failure.error);
            }
        }

        report
    }
}

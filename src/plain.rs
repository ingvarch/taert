//! Plain text output, selection, and review for non-TUI environments.

use std::io::{self, BufRead, Write};

use crate::core::bytes::format_bytes;
use crate::core::cleaner::{
    RemovalMode, RemovalProgress, RemovalReport, item_removal_mode, remove_selected,
};
use crate::core::history::ItemOutcome;
use crate::core::host::Host;
use crate::core::scanner::{CategoryScan, ScanItem};
use crate::core::stores::IgnoreStore;

/// Where a command reads the answers to its prompts and writes what it prints.
pub struct Console<'a> {
    pub input: &'a mut dyn BufRead,
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
}

pub struct PlainRunner;

impl PlainRunner {
    pub fn scan_text(scan: &CategoryScan) -> String {
        Self::scan_text_with_options(scan, |item| item_removal_mode(item, &scan.id, false))
    }

    /// Lists the rows, each with the place `mode` says it goes to.
    pub fn scan_text_with_options(
        scan: &CategoryScan,
        mode: impl Fn(&ScanItem) -> RemovalMode,
    ) -> String {
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
                    "  {:3}. {} {:>9}  {} {}\n",
                    index,
                    status_mark,
                    format_bytes(item.size_bytes),
                    item.path.display(),
                    mode(item).label()
                ));
                for folder in &item.also {
                    text.push_str(&format!(
                        "{:22}{} {}\n",
                        "",
                        folder.path.display(),
                        mode(folder).label()
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
    pub fn prompt_selection(scan: &mut CategoryScan, console: &mut Console) -> io::Result<()> {
        let total_items: usize = scan.groups.iter().map(|g| g.items.len()).sum();
        if total_items == 0 {
            return Ok(());
        }

        write!(
            console.out,
            "\nEnter item numbers to toggle (e.g. '1,3-5', 'all', 'none', or Enter to keep current): "
        )?;
        console.out.flush()?;

        let mut input = String::new();
        console.input.read_line(&mut input)?;
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

    /// Lists the selected rows with where each goes, and asks for a "yes".
    pub fn prompt_confirm(
        scan: &CategoryScan,
        console: &mut Console,
        mode: impl Fn(&ScanItem) -> RemovalMode,
    ) -> io::Result<bool> {
        let selected = scan.selected_items();
        let out = &mut *console.out;

        if selected.is_empty() {
            writeln!(out, "\nNothing selected for deletion.")?;
            return Ok(false);
        }

        writeln!(out, "\nSelected items:")?;
        for item in &selected {
            writeln!(
                out,
                "  - {:>9}  {} {}",
                format_bytes(item.size_bytes),
                item.path.display(),
                mode(item).label()
            )?;
            for folder in &item.also {
                writeln!(
                    out,
                    "{:15}{} {}",
                    "",
                    folder.path.display(),
                    mode(folder).label()
                )?;
            }
        }

        let count_and_size = |wanted: RemovalMode| {
            let rows = selected.iter().filter(|item| mode(item) == wanted);
            let bytes: u64 = rows.clone().map(|item| item.size_bytes).sum();
            (rows.count(), format_bytes(bytes))
        };
        let (trashed, trashed_size) = count_and_size(RemovalMode::Trash);
        if trashed > 0 {
            writeln!(
                out,
                "\nMove {trashed} item(s) ({trashed_size}) to the Trash."
            )?;
        }
        let (gone, gone_size) = count_and_size(RemovalMode::Permanent);
        if gone > 0 {
            writeln!(
                out,
                "\nWARNING: Permanent deletion of {gone} item(s) ({gone_size})."
            )?;
        }
        write!(out, "Type 'yes' to confirm: ")?;
        out.flush()?;

        let mut input = String::new();
        console.input.read_line(&mut input)?;

        Ok(input.trim() == "yes")
    }

    /// Removes the selected rows of `scan`.
    pub fn execute_clean<O>(
        scan: &CategoryScan,
        ignore: &IgnoreStore,
        permanent: bool,
        host: &dyn Host,
        on_outcome: O,
    ) -> RemovalReport
    where
        O: FnMut(&ItemOutcome),
    {
        let items: Vec<_> = scan.selected_items().into_iter().cloned().collect();
        remove_selected(
            host,
            &scan.id,
            &items,
            permanent,
            ignore,
            None::<fn(RemovalProgress)>,
            Some(on_outcome),
        )
    }

    /// Writes what a removal did, then anything that went wrong or needs attention.
    pub fn write_report(
        action: &str,
        report: &RemovalReport,
        console: &mut Console,
    ) -> io::Result<()> {
        let out = &mut *console.out;
        writeln!(out, "\n{action} complete:")?;
        if report.trashed_bytes > 0 {
            writeln!(
                out,
                "Moved {} to the Trash. Empty it to get the space back.",
                format_bytes(report.trashed_bytes)
            )?;
        }
        if report.freed_bytes > 0 {
            writeln!(
                out,
                "Removed {} permanently.",
                format_bytes(report.freed_bytes)
            )?;
        }
        if report.trashed_bytes == 0 && report.freed_bytes == 0 {
            if report.removed_count > 0 {
                writeln!(out, "Removed {} items.", report.removed_count)?;
            } else {
                writeln!(out, "Nothing was removed.")?;
            }
        }

        if !report.failures.is_empty() {
            let err = &mut *console.err;
            writeln!(err, "\nEncountered {} error(s):", report.failures.len())?;
            for failure in &report.failures {
                writeln!(err, "  - {}: {}", failure.path.display(), failure.error)?;
            }
        }

        write!(console.out, "{}", Self::notes_text(&report.notes))
    }

    pub fn print_compress_report(
        report: &crate::core::compress::CompressReport,
        target: &std::path::Path,
    ) {
        println!("{}", Self::compress_report_text(report, target));
    }

    pub fn compress_report_text(
        report: &crate::core::compress::CompressReport,
        target: &std::path::Path,
    ) -> String {
        let mut out = String::new();
        let title = if report.undo {
            if report.dry_run {
                "=== APFS Decompression Estimate (Dry Run) ==="
            } else {
                "=== APFS Decompression (Undo) Report ==="
            }
        } else if report.dry_run {
            "=== APFS Compression Estimate (Dry Run) ==="
        } else {
            "=== APFS Compression Report ==="
        };

        out.push_str(&format!("\n{title}\n"));
        out.push_str(&format!("Target: {}\n", target.display()));
        out.push_str(&format!("Scanned files: {}\n", report.scanned_files));

        if report.undo {
            out.push_str(&format!("Unpacked files: {}\n", report.processed_files));
            out.push_str(&format!(
                "Restored size: {}\n",
                format_bytes(report.original_bytes)
            ));
        } else {
            out.push_str(&format!("Compressed files: {}\n", report.processed_files));
            out.push_str(&format!(
                "Original size: {} ({} blocks)\n",
                format_bytes(report.original_bytes),
                report.original_blocks
            ));
            out.push_str(&format!(
                "Compressed size: {} ({} blocks)\n",
                format_bytes(report.compressed_bytes),
                report.compressed_blocks
            ));
            let pct = if report.original_bytes > 0 {
                (report.saved_bytes as f64 / report.original_bytes as f64) * 100.0
            } else {
                0.0
            };
            out.push_str(&format!(
                "Space saved: {} ({} blocks, {:.1}%)\n",
                format_bytes(report.saved_bytes),
                report.saved_blocks,
                pct
            ));
        }

        if report.skipped.total() > 0 {
            out.push_str(&format!("\nSkipped: {} files\n", report.skipped.total()));
            if report.skipped.already_compressed > 0 {
                out.push_str(&format!(
                    "  • Already compressed: {}\n",
                    report.skipped.already_compressed
                ));
            }
            if report.skipped.symlink_or_hardlink > 0 {
                out.push_str(&format!(
                    "  • Hardlinked or symlink: {}\n",
                    report.skipped.symlink_or_hardlink
                ));
            }
            if report.skipped.cloned > 0 {
                out.push_str(&format!(
                    "  • Cloned (shares blocks): {}\n",
                    report.skipped.cloned
                ));
            }
            if report.skipped.cloud_or_dataless > 0 {
                out.push_str(&format!(
                    "  • Cloud or dataless: {}\n",
                    report.skipped.cloud_or_dataless
                ));
            }
            if report.skipped.has_xattr > 0 {
                out.push_str(&format!(
                    "  • Has ResourceFork or decmpfs: {}\n",
                    report.skipped.has_xattr
                ));
            }
            if report.skipped.not_user_owned > 0 {
                out.push_str(&format!(
                    "  • Not owned by user: {}\n",
                    report.skipped.not_user_owned
                ));
            }
            if report.skipped.incompressible > 0 {
                out.push_str(&format!(
                    "  • Incompressible (<4 KB gain): {}\n",
                    report.skipped.incompressible
                ));
            }
            if report.skipped.not_compressed > 0 {
                out.push_str(&format!(
                    "  • Not compressed: {}\n",
                    report.skipped.not_compressed
                ));
            }
            if report.skipped.locked > 0 {
                out.push_str(&format!(
                    "  • Locked by a flag or an ACL: {}\n",
                    report.skipped.locked
                ));
            }
            if report.skipped.open_for_writing > 0 {
                out.push_str(&format!(
                    "  • Open for writing: {}\n",
                    report.skipped.open_for_writing
                ));
            }
        }

        if !report.failures.is_empty() {
            out.push_str(&format!("\nFailures: {}\n", report.failures.len()));
            for failure in &report.failures {
                out.push_str(&format!(
                    "  • {}: {}\n",
                    failure.path.display(),
                    failure.error
                ));
            }
        }

        if report.dry_run {
            out.push_str("\nDry run completed. No files were modified.\n");
        }

        out
    }

    pub fn notes_text(notes: &[String]) -> String {
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
    use crate::core::cleaner::item_removal_mode;
    use crate::core::scanner::{ScanItem, ToolGroup};
    use std::path::PathBuf;

    #[test]
    fn the_compress_report_names_locked_and_busy_files() {
        let mut report = crate::core::compress::CompressReport::default();
        report.skipped.locked = 2;
        report.skipped.open_for_writing = 1;

        let text = PlainRunner::compress_report_text(&report, std::path::Path::new("/x"));

        assert!(text.contains("Locked by a flag or an ACL: 2"), "{text}");
        assert!(text.contains("Open for writing: 1"), "{text}");
    }

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

    fn confirmation(scan: &CategoryScan, mode: impl Fn(&ScanItem) -> RemovalMode) -> String {
        let mut input = io::Cursor::new(b"no\n".to_vec());
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut console = Console {
            input: &mut input,
            out: &mut out,
            err: &mut err,
        };
        PlainRunner::prompt_confirm(scan, &mut console, mode).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn scan_of(id: &str, rows: Vec<ScanItem>) -> CategoryScan {
        CategoryScan {
            id: id.to_string(),
            title: "Test".to_string(),
            groups: vec![ToolGroup {
                tool: "Test".to_string(),
                items: rows,
            }],
            notes: Vec::new(),
        }
    }

    #[test]
    fn the_plain_confirmation_names_the_trash() {
        let row = ScanItem::new(PathBuf::from("/big.bin"), 5000, true, false);
        let scan = scan_of(crate::core::large_files::SCAN_ID, vec![row]);

        let text = confirmation(&scan, |item| item_removal_mode(item, &scan.id, false));

        assert!(text.contains("(Trash)"), "{text}");
        assert!(text.contains("to the Trash"), "{text}");
        assert!(!text.contains("Permanent deletion"), "{text}");
        assert!(text.contains("Type 'yes' to confirm: "), "{text}");
    }

    #[test]
    fn the_plain_confirmation_names_both_places() {
        let kept = ScanItem::new(PathBuf::from("/trashed"), 2000, true, false);
        let gone = ScanItem::new(PathBuf::from("/gone"), 3000, true, false);
        let scan = scan_of("clean", vec![kept, gone]);

        let text = confirmation(&scan, |item| {
            if item.path.ends_with("trashed") {
                RemovalMode::Trash
            } else {
                RemovalMode::Permanent
            }
        });

        let (trashed, gone) = (format_bytes(2000), format_bytes(3000));
        assert!(
            text.contains(&format!("Move 1 item(s) ({trashed}) to the Trash.")),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "WARNING: Permanent deletion of 1 item(s) ({gone})."
            )),
            "{text}"
        );
    }

    #[test]
    fn permanent_reaches_the_rows_of_an_uninstall() {
        for (permanent, trashed) in [(false, 1), (true, 0)] {
            let temp = tempfile::tempdir().unwrap();
            let data = temp.path().join("data");
            std::fs::create_dir_all(&data).unwrap();
            std::fs::write(data.join("file"), b"x").unwrap();
            let mut row = ScanItem::new(data.clone(), 1, true, false);
            row.role = Some(crate::core::leftovers::UninstallRole::Leftover {
                evidence: crate::core::leftovers::Evidence::BundleId,
            });
            let scan = CategoryScan {
                id: crate::core::uninstaller::PLAN_ID.to_string(),
                title: "Review".to_string(),
                groups: vec![ToolGroup {
                    tool: "Foo".to_string(),
                    items: vec![row],
                }],
                notes: Vec::new(),
            };
            let host = crate::core::host::fake::FakeHost::default();
            host.simulate_trash();

            let report = PlainRunner::execute_clean(
                &scan,
                &IgnoreStore::default(),
                permanent,
                &host,
                |_| {},
            );

            assert!(report.failures.is_empty(), "{:?}", report.failures);
            assert_eq!(host.trashed().len(), trashed, "permanent={permanent}");
            assert!(!data.exists());
        }
    }
}

//! Interactive Review & Outcome Screen.

use ratatui::Frame;
use ratatui::layout::Alignment;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::components::chrome::AppChrome;
use super::components::hints::{Hint, PrimaryAction};
use super::components::list::{
    ListRow, path_spans, render_section_header, render_section_header_spans,
};
use super::theme::Theme;
use crate::core::bytes::format_bytes;
use crate::core::cleaner::RemovalReport;
use crate::core::path_utils::prettify_path;
use crate::core::scanner::CategoryScan;

pub struct ReviewState {
    pub scan: CategoryScan,
    pub scroll_offset: usize,
    pub confirmed: bool,
    pub report: Option<RemovalReport>,
}

impl ReviewState {
    pub fn new(scan: CategoryScan) -> Self {
        Self {
            scan,
            scroll_offset: 0,
            confirmed: false,
            report: None,
        }
    }

    pub fn scroll_down(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_add(1);
    }

    pub fn scroll_up(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(1);
    }

    pub fn page_down(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_add(10);
    }

    pub fn page_up(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(10);
    }

    pub fn move_to_start(&mut self) {
        self.scroll_offset = 0;
    }

    pub fn move_to_end(&mut self) {
        let count = self.scan.selected_items().len();
        self.scroll_offset = count.saturating_sub(1);
    }
}

pub fn render_review(frame: &mut Frame, state: &ReviewState, theme: &Theme) {
    if let Some(ref report) = state.report {
        render_outcome(frame, report, theme);
        return;
    }

    let selected_items = state.scan.selected_items();
    let total_bytes = state.scan.total_selected_bytes();
    let count = selected_items.len();

    let primary_action =
        PrimaryAction::destructive(format!("Remove {count} items"), theme.g.key_enter);

    let chrome = AppChrome::new(vec!["taert", "Clean caches", "Review"])
        .hints(vec![
            Hint::new("↑↓", "Scroll"),
            Hint::new("⎋", "Back"),
            Hint::new("?", "More"),
        ])
        .primary(primary_action);

    let content_rect = chrome.render(frame, theme);

    let mut lines = Vec::new();

    // Headline
    lines.push(Line::from(vec![
        Span::styled(format!("{count} items"), theme.warning()),
        Span::styled(" · ", theme.callout()),
        Span::styled(format_bytes(total_bytes), theme.size(total_bytes)),
        Span::styled(" will be permanently deleted", theme.headline()),
    ]));
    lines.push(Line::from(""));

    // Grouped items
    let mut flat_items: Vec<Line> = Vec::new();

    for group in &state.scan.groups {
        let sel_in_group: Vec<_> = group.items.iter().filter(|it| it.selected).collect();
        if sel_in_group.is_empty() {
            continue;
        }

        let group_bytes: u64 = sel_in_group.iter().map(|it| it.size_bytes).sum();
        flat_items.push(render_section_header_spans(
            &group.tool,
            vec![Span::styled(
                format_bytes(group_bytes),
                theme.size(group_bytes),
            )],
            content_rect.width as usize,
            theme,
        ));

        let max_len = (content_rect.width as usize).saturating_sub(20);
        for item in sel_in_group {
            let spans = path_spans(&item.path, max_len, theme);
            let size_style = theme.size(item.size_bytes);
            let detail = Span::styled(format!("{:>9}", format_bytes(item.size_bytes)), size_style);

            let row = ListRow::new(spans).detail(detail);
            flat_items.push(row.render_line(content_rect.width as usize, theme));
            for folder in &item.also {
                let row = ListRow::new(path_spans(&folder.path, max_len, theme));
                flat_items.push(row.render_line(content_rect.width as usize, theme));
            }
        }
        flat_items.push(Line::from(""));
    }

    let avail_h = content_rect.height.saturating_sub(2) as usize;
    let offset = state.scroll_offset.min(flat_items.len().saturating_sub(1));

    for line in flat_items.into_iter().skip(offset).take(avail_h) {
        lines.push(line);
    }

    frame.render_widget(Paragraph::new(lines), content_rect);
}

fn render_outcome(frame: &mut Frame, report: &RemovalReport, theme: &Theme) {
    let chrome = AppChrome::new(vec!["taert", "Clean caches", "Done"])
        .hints(vec![Hint::new("⎋", "Back")])
        .primary(PrimaryAction::new("Done", theme.g.key_enter));

    let content_rect = chrome.render(frame, theme);

    let mut lines = Vec::new();
    lines.push(Line::from(""));
    lines.push(Line::from(""));

    if report.freed_bytes > 0 {
        lines.push(
            Line::from(vec![Span::styled(theme.g.success, theme.success())])
                .alignment(Alignment::Center),
        );
        lines.push(Line::from(""));
        lines.push(
            Line::from(vec![Span::styled(
                format!("Freed {}", format_bytes(report.freed_bytes)),
                theme.headline(),
            )])
            .alignment(Alignment::Center),
        );
    } else {
        lines.push(
            Line::from(vec![Span::styled(theme.g.error, theme.danger())])
                .alignment(Alignment::Center),
        );
        lines.push(Line::from(""));
        lines.push(
            Line::from(vec![Span::styled("Nothing was removed", theme.headline())])
                .alignment(Alignment::Center),
        );
    }

    let fail_part = if report.failures.is_empty() {
        "".to_string()
    } else {
        format!(" · {} couldn't be removed", report.failures.len())
    };

    lines.push(
        Line::from(vec![Span::styled(
            format!("{} items removed{fail_part}", report.removed_count),
            theme.callout(),
        )])
        .alignment(Alignment::Center),
    );
    lines.push(Line::from(""));
    lines.push(Line::from(""));

    if !report.failures.is_empty() {
        lines.push(render_section_header(
            "COULDN'T REMOVE",
            None,
            content_rect.width as usize,
            theme,
        ));
        for fail in &report.failures {
            let row = ListRow::new(vec![Span::styled(
                prettify_path(&fail.path),
                theme.callout(),
            )])
            .leading(Span::styled(theme.g.error, theme.danger()))
            .detail(Span::styled(fail.error.clone(), theme.footnote()));

            lines.push(row.render_line(content_rect.width as usize, theme));
        }
    }

    frame.render_widget(Paragraph::new(lines), content_rect);
}

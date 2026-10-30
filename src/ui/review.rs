//! Interactive Review & Outcome Screen.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use super::size_level::SizeLevel;
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
    let area = frame.area();

    let window_title = if state.report.is_some() {
        " Clean Complete "
    } else {
        " Confirm Permanent Deletion "
    };

    let border_style = if state.report.is_some() {
        theme.border
    } else {
        theme.warning
    };

    let window_block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(border_style)
        .title(Line::from(vec![Span::styled(window_title, theme.title)]))
        .title_alignment(Alignment::Left);

    let inner_area = window_block.inner(area);
    frame.render_widget(window_block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // Subtitle / Status
            Constraint::Min(6),    // List or Summary
            Constraint::Length(1), // Divider
            Constraint::Length(1), // Footer keyhints
        ])
        .split(inner_area);

    if let Some(ref report) = state.report {
        // Summary View
        let header_line = Line::from(vec![Span::styled("Cleanup Summary", theme.subtitle)]);
        frame.render_widget(Paragraph::new(header_line), chunks[0]);

        let mut lines = vec![
            Line::from(vec![
                Span::styled("Freed space: ", theme.normal),
                Span::styled(format_bytes(report.freed_bytes), theme.success),
            ]),
            Line::from(vec![
                Span::styled("Removed items: ", theme.normal),
                Span::styled(report.removed_count.to_string(), theme.success),
            ]),
        ];

        if !report.failures.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("Errors encountered:", theme.error)));
            for failure in &report.failures {
                lines.push(Line::from(vec![
                    Span::styled("  ✕ ", theme.error),
                    Span::styled(prettify_path(&failure.path), theme.normal),
                    Span::styled(format!(" ({})", failure.error), theme.muted),
                ]));
            }
        }

        frame.render_widget(Paragraph::new(lines), chunks[1]);

        let divider = Block::default()
            .borders(Borders::BOTTOM)
            .border_style(theme.border);
        frame.render_widget(divider, chunks[2]);

        let footer_text = Line::from(vec![
            Span::styled("Enter/Esc/q ", theme.footer_key),
            Span::styled("return to workspace", theme.footer_desc),
        ]);
        frame.render_widget(Paragraph::new(footer_text), chunks[3]);
        return;
    }

    // List of deletion candidates
    let selected_items = state.scan.selected_items();
    let total_bytes = state.scan.total_selected_bytes();

    let subtitle = format!(
        "{} selected item(s) to remove ({})",
        selected_items.len(),
        format_bytes(total_bytes)
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(subtitle, theme.subtitle))),
        chunks[0],
    );

    let list_area_height = chunks[1].height as usize;
    let mut list_lines = Vec::new();

    for item in selected_items
        .iter()
        .skip(state.scroll_offset)
        .take(list_area_height)
    {
        let size_style = SizeLevel::of(item.size_bytes).style(false);
        list_lines.push(Line::from(vec![
            Span::styled("  ✕ ", theme.error),
            Span::styled(
                format!("{:>9}  ", format_bytes(item.size_bytes)),
                size_style,
            ),
            Span::styled(prettify_path(&item.path), theme.normal),
        ]));
    }

    frame.render_widget(Paragraph::new(list_lines), chunks[1]);

    let divider = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(theme.border);
    frame.render_widget(divider, chunks[2]);

    let footer_lines = Line::from(vec![
        Span::styled("Enter ", theme.footer_key),
        Span::styled("confirm permanent deletion  ", theme.footer_desc),
        Span::styled("Esc/q ", theme.footer_key),
        Span::styled("back", theme.footer_desc),
    ]);
    frame.render_widget(Paragraph::new(footer_lines), chunks[3]);
}

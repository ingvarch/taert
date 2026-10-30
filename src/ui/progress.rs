//! Scanning and Removal Progress view inside a framed window.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::core::bytes::format_bytes;
use crate::core::path_utils::middle_truncate;
use crate::core::purger::PurgeScanEvent;
use crate::core::scanner::ScanProgressEvent;

use super::theme::Theme;

#[derive(Debug, Clone)]
pub struct ProgressState {
    pub title: String,
    pub headline: String,
    pub folder: Option<String>,
    pub found_count: usize,
    pub found_bytes: u64,
    pub spinner_idx: usize,
    pub is_cancelling: bool,
}

impl ProgressState {
    pub fn new(headline: impl Into<String>) -> Self {
        Self {
            title: "taert".to_string(),
            headline: headline.into(),
            folder: None,
            found_count: 0,
            found_bytes: 0,
            spinner_idx: 0,
            is_cancelling: false,
        }
    }

    pub fn tick(&mut self) {
        self.spinner_idx = (self.spinner_idx + 1) % 4;
    }

    pub fn spinner_char(&self) -> &'static str {
        match self.spinner_idx {
            0 => "⠋",
            1 => "⠙",
            2 => "⠹",
            _ => "⠸",
        }
    }

    pub fn apply_purge(&mut self, event: PurgeScanEvent) {
        match event {
            PurgeScanEvent::Root(path) => {
                self.headline = format!("Scanning {path}");
                self.folder = None;
            }
            PurgeScanEvent::Walking(path) => {
                self.folder = Some(path);
            }
            PurgeScanEvent::Found { bytes } => {
                self.found_count += 1;
                self.found_bytes += bytes;
            }
        }
    }

    pub fn apply_category(&mut self, event: ScanProgressEvent) {
        match event {
            ScanProgressEvent::CategoryStarted {
                title,
                index,
                total,
            } => {
                self.headline = format!("Scanning {title}, {index} of {total}");
                self.folder = None;
            }
            ScanProgressEvent::ToolStarted(tool) => {
                self.folder = Some(tool);
            }
            ScanProgressEvent::ScanningPath(path) => {
                self.folder = Some(path);
            }
            ScanProgressEvent::ItemFound { bytes } => {
                self.found_count += 1;
                self.found_bytes += bytes;
            }
            ScanProgressEvent::ToolFinished { .. } => {}
        }
    }
}

pub fn render_progress(frame: &mut Frame, state: &ProgressState, theme: &Theme) {
    let area = frame.area();

    let window_block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(theme.border)
        .title(Line::from(vec![Span::styled(
            format!(" {} ", state.title),
            theme.title,
        )]))
        .title_alignment(Alignment::Left);

    let inner_area = window_block.inner(area);
    frame.render_widget(window_block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Rows 0, 1, 2
            Constraint::Min(0),    // Fill
            Constraint::Length(1), // Divider
            Constraint::Length(1), // Footer
        ])
        .split(inner_area);

    let row_width = chunks[0].width as usize;
    let headline_text = if state.is_cancelling {
        "Cancelling…".to_string()
    } else {
        state.headline.clone()
    };

    // Row 0: Spinner + headline (middle truncated)
    let spinner_str = format!("{} ", state.spinner_char());
    let headline_avail = row_width.saturating_sub(spinner_str.chars().count());
    let truncated_headline = middle_truncate(&headline_text, headline_avail);
    let row0 = Line::from(vec![
        Span::styled(spinner_str, theme.accent),
        Span::styled(truncated_headline, theme.normal),
    ]);

    // Row 1: Folder (middle truncated, dim/muted)
    let folder_str = state.folder.as_deref().unwrap_or("");
    let truncated_folder = middle_truncate(folder_str, row_width);
    let row1 = Line::from(Span::styled(truncated_folder, theme.muted));

    // Row 2: Found items · bytes
    let item_word = if state.found_count == 1 {
        "item"
    } else {
        "items"
    };
    let bytes_formatted = format_bytes(state.found_bytes);
    let row2 = Line::from(Span::styled(
        format!(
            "Found {} {} · {}",
            state.found_count, item_word, bytes_formatted
        ),
        theme.normal,
    ));

    frame.render_widget(Paragraph::new(vec![row0, row1, row2]), chunks[0]);

    let divider = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(theme.border);
    frame.render_widget(divider, chunks[2]);

    let footer_text = Line::from(vec![
        Span::styled("Esc/q ", theme.footer_key),
        Span::styled("cancel", theme.footer_desc),
    ]);
    frame.render_widget(Paragraph::new(footer_text), chunks[3]);
}

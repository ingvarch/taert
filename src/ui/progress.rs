//! Scanning and Removal Progress view.

use std::time::Instant;

use ratatui::Frame;

use super::components::chrome::AppChrome;
use super::components::hints::Hint;
use super::components::progress::ProgressView;
use super::theme::Theme;
use crate::core::bytes::format_bytes;
use crate::core::purger::PurgeScanEvent;
use crate::core::scanner::ScanProgressEvent;

#[derive(Debug, Clone)]
pub struct ProgressState {
    pub title: String,
    pub headline: String,
    pub folder: Option<String>,
    pub found_count: usize,
    pub found_bytes: u64,
    pub spinner_idx: usize,
    pub is_cancelling: bool,
    pub started_at: Instant,
    pub category_index: Option<usize>,
    pub category_total: Option<usize>,
    pub in_clean_caches: bool,
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
            started_at: Instant::now(),
            category_index: None,
            category_total: None,
            in_clean_caches: true,
        }
    }

    pub fn top_level(mut self) -> Self {
        self.in_clean_caches = false;
        self
    }

    pub fn tick(&mut self) {
        self.spinner_idx = (self.spinner_idx + 1) % 10;
    }

    pub fn spinner_char<'a>(&self, theme: &'a Theme) -> &'a str {
        if theme.env.reduce_motion {
            return "*";
        }
        let count = theme.g.spinner.len();
        if count == 0 {
            return "●";
        }
        theme.g.spinner[self.spinner_idx % count]
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
                self.category_index = Some(index);
                self.category_total = Some(total);
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
    let crumbs = if state.in_clean_caches {
        vec!["taert", "Clean caches", "Scanning"]
    } else {
        vec!["taert", "Scanning"]
    };
    let chrome = AppChrome::new(crumbs)
        .full_width()
        .hints(vec![Hint::new(theme.g.key_esc, "Cancel")]);
    let content_rect = chrome.render(frame, theme);

    let headline_text = if state.is_cancelling {
        "Cancelling…".to_string()
    } else {
        state.headline.clone()
    };

    let fraction = match (state.category_index, state.category_total) {
        (Some(idx), Some(total)) if total > 0 => Some(idx as f64 / total as f64),
        _ => None,
    };

    let item_word = if state.found_count == 1 {
        "item"
    } else {
        "items"
    };
    let counter_str = format!(
        "Found {} {} · {}",
        state.found_count,
        item_word,
        format_bytes(state.found_bytes)
    );

    let elapsed = state.started_at.elapsed().as_secs();

    let mut view = ProgressView::new(state.spinner_char(theme), &headline_text)
        .counter_info(&counter_str)
        .elapsed_seconds(elapsed);

    if let Some(frac) = fraction {
        view = view.fraction(Some(frac));
    }
    if let Some(p) = state.folder.as_deref() {
        view = view.current_path(p);
    }

    view.render(frame, content_rect, theme);
}

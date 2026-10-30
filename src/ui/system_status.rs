//! Interactive System & Memory Status screen.
//! Features hardware telemetry, memory pressure, and APFS storage breakdown.

use ratatui::Frame;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::components::chrome::AppChrome;
use super::components::gauge::{legend, segmented_bar};
use super::components::hints::Hint;
use super::components::list::render_section_header;
use super::theme::Theme;
use crate::core::bytes::{format_bytes, format_memory_bytes};
use crate::core::profiler::{FullSystemStatus, MemoryPressureLevel, SystemProfiler};

pub struct SystemStatusState {
    pub status: FullSystemStatus,
    pub scroll_offset: usize,
}

impl Default for SystemStatusState {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemStatusState {
    pub fn new() -> Self {
        Self {
            status: SystemProfiler::current_full_status(),
            scroll_offset: 0,
        }
    }

    pub fn scroll_up(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(1);
    }

    pub fn scroll_down(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_add(1);
    }

    pub fn page_up(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(5);
    }

    pub fn page_down(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_add(5);
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll_offset = 0;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_add(50);
    }
}

pub fn render_system_status(frame: &mut Frame, state: &SystemStatusState, theme: &Theme) {
    let chrome = AppChrome::new(vec!["taert", "System status"])
        .status(Line::from(vec![Span::styled(
            "Updated just now",
            theme.footnote(),
        )]))
        .hints(vec![
            Hint::new("↑↓", "Scroll"),
            Hint::new("⎋", "Back"),
            Hint::new("?", "More"),
        ]);

    let content_rect = chrome.render(frame, theme);
    let width = content_rect.width as usize;

    let mut lines = Vec::new();

    // 1. Hardware Summary
    let mach = &state.status.machine;
    let model = mach.model.as_deref().unwrap_or("Mac");
    let chip = mach.chip.as_deref().unwrap_or("Apple Silicon");
    let cores = mach.cores.as_deref().unwrap_or("");
    let mem_str = mach
        .memory_bytes
        .map(format_memory_bytes)
        .unwrap_or_default();
    let macos = &mach.macos_version;

    let uptime_days = mach.uptime_seconds / 86400;
    let uptime_hours = (mach.uptime_seconds % 86400) / 3600;
    let uptime_str = if uptime_days > 0 {
        format!("{uptime_days}d {uptime_hours}h")
    } else {
        format!("{uptime_hours}h")
    };

    lines.push(Line::from(vec![Span::styled(model, theme.headline())]));

    let mut hw_parts = vec![chip.to_string()];
    if !cores.is_empty() {
        hw_parts.push(cores.to_string());
    }
    if !mem_str.is_empty() {
        hw_parts.push(mem_str);
    }
    hw_parts.push(format!("macOS {macos}"));
    hw_parts.push(format!("up {uptime_str}"));

    lines.push(Line::from(vec![Span::styled(
        hw_parts.join(" · "),
        theme.callout(),
    )]));
    lines.push(Line::from(""));

    // 2. Memory Section
    let mem = &state.status.memory;
    let (pressure_text, _pressure_color) = match mem.pressure {
        Some(MemoryPressureLevel::Normal) | None => ("Normal", theme.p.green),
        Some(MemoryPressureLevel::Warning) => ("Warning", theme.p.yellow),
        Some(MemoryPressureLevel::Critical) => ("Critical", theme.p.red),
    };
    let pressure_detail = format!("Pressure  ● {pressure_text}");

    lines.push(render_section_header(
        "MEMORY",
        Some(&pressure_detail),
        width,
        theme,
    ));

    let bar_w = (width as u16).saturating_sub(22).max(10);
    let app_bytes = mem.active_bytes.saturating_add(mem.inactive_bytes);
    let total_mem = mem.total_bytes.max(1) as f64;

    let app_frac = app_bytes as f64 / total_mem;
    let wired_frac = mem.wired_bytes as f64 / total_mem;
    let comp_frac = mem.compressed_bytes as f64 / total_mem;

    let mem_bar_spans = segmented_bar(
        &[
            (app_frac, theme.p.accent),
            (wired_frac, theme.p.purple),
            (comp_frac, theme.p.teal),
        ],
        bar_w,
        theme,
    );

    let used_mem_bytes = mem.total_bytes.saturating_sub(mem.free_bytes);
    let mem_metric_str = format!(
        "   {} of {}",
        format_memory_bytes(used_mem_bytes),
        format_memory_bytes(mem.total_bytes)
    );

    let mut mem_bar_row = mem_bar_spans;
    mem_bar_row.push(Span::styled(mem_metric_str, theme.headline()));
    lines.push(Line::from(mem_bar_row));

    let mem_legend = legend(
        &[
            ("App", format_memory_bytes(app_bytes), theme.p.accent),
            (
                "Wired",
                format_memory_bytes(mem.wired_bytes),
                theme.p.purple,
            ),
            (
                "Compressed",
                format_memory_bytes(mem.compressed_bytes),
                theme.p.teal,
            ),
            (
                "Free",
                format_memory_bytes(mem.free_bytes),
                theme.p.quaternary,
            ),
        ],
        theme,
    );
    lines.push(mem_legend);
    lines.push(Line::from(""));

    // 3. Storage Section
    let st = &state.status.storage;
    let storage_detail = format!("{} available", format_bytes(st.available_bytes));
    lines.push(render_section_header(
        "STORAGE",
        Some(&storage_detail),
        width,
        theme,
    ));

    let total_st = st.total_bytes.max(1) as f64;
    let used_st_bytes = st.total_bytes.saturating_sub(st.free_bytes);
    let used_frac = used_st_bytes as f64 / total_st;
    let managed_frac = st.managed_bytes as f64 / total_st;

    let storage_bar_spans = segmented_bar(
        &[(used_frac, theme.p.accent), (managed_frac, theme.p.gray)],
        bar_w,
        theme,
    );

    let storage_metric_str = format!(
        "   {} of {}",
        format_bytes(used_st_bytes),
        format_bytes(st.total_bytes)
    );

    let mut storage_bar_row = storage_bar_spans;
    storage_bar_row.push(Span::styled(storage_metric_str, theme.headline()));
    lines.push(Line::from(storage_bar_row));

    let storage_legend = legend(
        &[
            ("Used", format_bytes(used_st_bytes), theme.p.accent),
            (
                "Managed by macOS",
                format_bytes(st.managed_bytes),
                theme.p.gray,
            ),
            ("Free", format_bytes(st.free_bytes), theme.p.quaternary),
        ],
        theme,
    );
    lines.push(storage_legend);
    lines.push(Line::from(""));

    // Footnote
    lines.push(Line::from(vec![Span::styled(
        "Managed space may include purgeable files and isn't counted as free.",
        theme.footnote(),
    )]));

    // Scrolling
    let visible_lines: Vec<Line> = lines
        .into_iter()
        .skip(state.scroll_offset)
        .take(content_rect.height as usize)
        .collect();

    frame.render_widget(Paragraph::new(visible_lines), content_rect);
}

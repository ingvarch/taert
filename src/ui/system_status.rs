//! Interactive System & Memory Status screen.
//! 3 separate rounded panels ("This Mac", "Memory", "Storage"), live gauges, factual APFS metrics,
//! footnote, and key hint footer.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

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
    let area = frame.area();
    if area.width < 10 || area.height < 5 {
        return;
    }

    let side_by_side = area.width >= 80;

    // 1. This Mac Panel Lines
    let mach = &state.status.machine;
    let uptime_days = mach.uptime_seconds / 86400;
    let uptime_hours = (mach.uptime_seconds % 86400) / 3600;
    let uptime_mins = (mach.uptime_seconds % 3600) / 60;
    let uptime_str = if uptime_days > 0 {
        format!("{uptime_days}d {uptime_hours}h {uptime_mins}m")
    } else {
        format!("{uptime_hours}h {uptime_mins}m")
    };

    let this_mac_lines = vec![
        metric_line(
            "Chip",
            mach.chip.as_deref().unwrap_or("Apple Silicon"),
            theme,
        ),
        metric_line("Cores", mach.cores.as_deref().unwrap_or(""), theme),
        metric_line(
            "Memory",
            mach.memory_bytes
                .map(format_memory_bytes)
                .unwrap_or_default(),
            theme,
        ),
        metric_line("macOS", &mach.macos_version, theme),
        metric_line("Model", mach.model.as_deref().unwrap_or("Mac"), theme),
        metric_line("Uptime", &uptime_str, theme),
    ];

    let this_mac_block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(theme.border)
        .title(
            Line::from(vec![Span::styled(" This Mac ", theme.title)]).alignment(Alignment::Left),
        );

    // 2. Memory Panel Lines & Gauge
    let mem = &state.status.memory;
    let mem_used_bytes = mem.total_bytes.saturating_sub(mem.free_bytes);
    let mem_fraction = if mem.total_bytes > 0 {
        mem_used_bytes as f64 / mem.total_bytes as f64
    } else {
        0.0
    };
    let mem_gauge_spans = gauge_spans(
        mem_fraction,
        20,
        &format!(
            "{} in use of {}",
            format_memory_bytes(mem_used_bytes),
            format_memory_bytes(mem.total_bytes)
        ),
        match mem.pressure {
            Some(MemoryPressureLevel::Normal) | None => theme.success,
            Some(MemoryPressureLevel::Warning) => theme.warning,
            Some(MemoryPressureLevel::Critical) => theme.error,
        },
        theme,
    );

    let (pressure_text, pressure_style) = match mem.pressure {
        Some(MemoryPressureLevel::Normal) | None => ("Normal", theme.success),
        Some(MemoryPressureLevel::Warning) => ("Warning", theme.warning),
        Some(MemoryPressureLevel::Critical) => ("Critical", theme.error),
    };

    let memory_lines = vec![
        Line::from(mem_gauge_spans),
        metric_line_colored("Pressure", pressure_text, pressure_style, theme),
        metric_line("Installed", format_memory_bytes(mem.total_bytes), theme),
        metric_line("Free now", format_memory_bytes(mem.free_bytes), theme),
        metric_line("Active", format_memory_bytes(mem.active_bytes), theme),
        metric_line("Inactive", format_memory_bytes(mem.inactive_bytes), theme),
        metric_line(
            "Compressed",
            format_memory_bytes(mem.compressed_bytes),
            theme,
        ),
        metric_line("Wired", format_memory_bytes(mem.wired_bytes), theme),
    ];

    let memory_block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(theme.border)
        .title(Line::from(vec![Span::styled(" Memory ", theme.title)]).alignment(Alignment::Left))
        .title(
            Line::from(vec![Span::styled(
                format!(" {pressure_text} "),
                pressure_style,
            )])
            .alignment(Alignment::Right),
        );

    // 3. Storage Panel Lines & Gauge
    let st = &state.status.storage;
    let storage_free_ratio = st.free_fraction();
    let free_pct = (storage_free_ratio * 100.0).round() as usize;
    let storage_color = if storage_free_ratio < 0.1 {
        theme.error
    } else if storage_free_ratio < 0.2 {
        theme.warning
    } else {
        theme.success
    };

    // Gauge displays USED fraction, matching macOS
    let storage_gauge_spans = gauge_spans(
        st.used_fraction(),
        20,
        &format!(
            "{} free of {}",
            format_bytes(st.available_bytes),
            format_bytes(st.total_bytes)
        ),
        storage_color,
        theme,
    );

    let mut storage_lines = vec![
        Line::from(storage_gauge_spans),
        metric_line_colored(
            "Free now",
            format_bytes(st.available_bytes),
            storage_color,
            theme,
        ),
        metric_line("Total", format_bytes(st.total_bytes), theme),
        metric_line("Managed by macOS", format_bytes(st.managed_bytes), theme),
    ];

    if st.managed_bytes > 0 {
        storage_lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "Managed capacity may include purgeable files and is not guaranteed",
                theme.muted,
            ),
        ]));
        storage_lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("free space.", theme.muted),
        ]));
    } else {
        storage_lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("Controlled and managed by macOS", theme.muted),
        ]));
    }

    let storage_block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(theme.border)
        .title(Line::from(vec![Span::styled(" Storage ", theme.title)]).alignment(Alignment::Left))
        .title(
            Line::from(vec![Span::styled(
                format!(" {free_pct}% free "),
                storage_color,
            )])
            .alignment(Alignment::Right),
        );

    // Layout
    if side_by_side {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(8),  // This Mac
                Constraint::Length(10), // Memory & Storage side-by-side
                Constraint::Length(1),  // Footnote
                Constraint::Min(0),     // Filler
                Constraint::Length(1),  // Footer
            ])
            .split(area);

        frame.render_widget(
            Paragraph::new(this_mac_lines).block(this_mac_block),
            rows[0],
        );

        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[1]);

        frame.render_widget(Paragraph::new(memory_lines).block(memory_block), cols[0]);
        frame.render_widget(Paragraph::new(storage_lines).block(storage_block), cols[1]);

        let footnote = Line::from(Span::styled(
            "  System status reflects live macOS metrics. Caches and memory are managed automatically by macOS.",
            theme.muted,
        ));
        frame.render_widget(Paragraph::new(footnote), rows[2]);

        let footer = Line::from(vec![
            Span::styled("  ", theme.normal),
            Span::styled("↑↓ ", theme.footer_key),
            Span::styled("scroll  ", theme.footer_desc),
            Span::styled("Esc ", theme.footer_key),
            Span::styled("back", theme.footer_desc),
        ]);
        frame.render_widget(Paragraph::new(footer), rows[4]);
    } else {
        // Stacked vertically for narrow screens
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(8),  // This Mac
                Constraint::Length(10), // Memory
                Constraint::Length(7),  // Storage
                Constraint::Length(1),  // Footnote
                Constraint::Min(0),     // Filler
                Constraint::Length(1),  // Footer
            ])
            .split(area);

        frame.render_widget(
            Paragraph::new(this_mac_lines).block(this_mac_block),
            rows[0],
        );
        frame.render_widget(Paragraph::new(memory_lines).block(memory_block), rows[1]);
        frame.render_widget(Paragraph::new(storage_lines).block(storage_block), rows[2]);

        let footnote = Line::from(Span::styled(
            "  System status reflects live macOS metrics.",
            theme.muted,
        ));
        frame.render_widget(Paragraph::new(footnote), rows[3]);

        let footer = Line::from(vec![
            Span::styled("  ", theme.normal),
            Span::styled("↑↓ ", theme.footer_key),
            Span::styled("scroll  ", theme.footer_desc),
            Span::styled("Esc ", theme.footer_key),
            Span::styled("back", theme.footer_desc),
        ]);
        frame.render_widget(Paragraph::new(footer), rows[5]);
    }
}

fn metric_line<'a>(label: &'static str, value: impl Into<String>, theme: &Theme) -> Line<'a> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(format!("{:<17}", label), theme.muted),
        Span::styled(value.into(), theme.normal),
    ])
}

fn metric_line_colored<'a>(
    label: &'static str,
    value: impl Into<String>,
    style: ratatui::style::Style,
    theme: &Theme,
) -> Line<'a> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(format!("{:<17}", label), theme.muted),
        Span::styled(value.into(), style),
    ])
}

fn gauge_spans<'a>(
    fraction: f64,
    width: usize,
    text: &str,
    filled_style: ratatui::style::Style,
    theme: &Theme,
) -> Vec<Span<'a>> {
    let filled_count = ((fraction * width as f64).round() as usize).min(width);
    let empty_count = width.saturating_sub(filled_count);
    vec![
        Span::raw("  "),
        Span::styled("█".repeat(filled_count), filled_style),
        Span::styled("░".repeat(empty_count), theme.muted),
        Span::styled(format!(" {text}"), theme.normal),
    ]
}

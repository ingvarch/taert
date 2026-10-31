//! Live System status screen: a machine summary and panels for CPU, memory, disk, power,
//! processes and network, refreshed by a background sampler.

use std::cell::Cell;
use std::sync::mpsc::Receiver;

use ratatui::Frame;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::components::chrome::AppChrome;
use super::components::columns::side_by_side;
use super::components::gauge::{legend_grid, segmented_bar, sparkline};
use super::components::hints::Hint;
use super::components::list::render_section_header_spans;
use super::components::text::fit_width;
use super::theme::Theme;
use crate::core::bytes::{format_bytes, format_memory_bytes};
use crate::core::profiler::{FullSystemStatus, MemoryPressureLevel, SystemProfiler};
use crate::core::telemetry::{
    LiveStatus, PowerSource, PowerStatus, Severity, disk_severity, health, spawn_sampler,
};

const HISTORY_LEN: usize = 240;
const TWO_COLUMNS_MIN_WIDTH: usize = 100;
const COLUMN_GAP: usize = 4;
const INDENT: usize = 2;
const MIN_GRAPH_WIDTH: usize = 6;
const MAX_NAME_WIDTH: usize = 16;
// Keeps idle traffic flat instead of letting a few bytes fill the graph.
const MIN_RATE_SCALE: f64 = 100_000.0;
const BUSY_PROCESS_PERCENT: f64 = 200.0;
const MEASURING: &str = "Measuring…";

#[derive(Debug, Default)]
pub struct History {
    pub cpu: Vec<f64>,
    pub read: Vec<f64>,
    pub write: Vec<f64>,
    pub down: Vec<f64>,
    pub up: Vec<f64>,
}

impl History {
    fn record(&mut self, live: &LiveStatus) {
        let disk = live.disk_io.as_ref();
        let usage = live.cpu.usage_percent.map(f64::from);

        record(&mut self.cpu, usage);
        record(&mut self.read, disk.map(|io| io.read_per_sec));
        record(&mut self.write, disk.map(|io| io.write_per_sec));
        record(&mut self.down, live.network.down_per_sec);
        record(&mut self.up, live.network.up_per_sec);
    }
}

fn record(series: &mut Vec<f64>, value: Option<f64>) {
    if let Some(value) = value {
        series.push(value);
        if series.len() > HISTORY_LEN {
            series.remove(0);
        }
    }
}

pub struct SystemStatusState {
    pub status: FullSystemStatus,
    pub history: History,
    pub scroll_offset: usize,
    max_scroll: Cell<usize>,
    updates: Option<Receiver<FullSystemStatus>>,
}

impl Default for SystemStatusState {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemStatusState {
    pub fn new() -> Self {
        Self {
            updates: Some(spawn_sampler()),
            ..Self::with_status(SystemProfiler::current_full_status())
        }
    }

    pub fn with_status(status: FullSystemStatus) -> Self {
        Self {
            status,
            history: History::default(),
            scroll_offset: 0,
            max_scroll: Cell::new(0),
            updates: None,
        }
    }

    pub fn apply(&mut self, status: FullSystemStatus) {
        self.history.record(&status.live);
        self.status = status;
    }

    pub fn tick(&mut self) {
        while let Some(status) = self.updates.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.apply(status);
        }
    }

    fn scroll_to(&mut self, offset: usize) {
        self.scroll_offset = offset.min(self.max_scroll.get());
    }

    pub fn scroll_up(&mut self) {
        self.scroll_to(self.scroll_offset.saturating_sub(1));
    }

    pub fn scroll_down(&mut self) {
        self.scroll_to(self.scroll_offset.saturating_add(1));
    }

    pub fn page_up(&mut self) {
        self.scroll_to(self.scroll_offset.saturating_sub(5));
    }

    pub fn page_down(&mut self) {
        self.scroll_to(self.scroll_offset.saturating_add(5));
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll_to(0);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_to(usize::MAX);
    }
}

pub fn render_system_status(frame: &mut Frame, state: &SystemStatusState, theme: &Theme) {
    let health = health(&state.status);
    let health_color = severity_color(health.severity, theme);

    let chrome = AppChrome::new(vec!["taert", "System status"])
        .status(Line::from(status_spans(
            health_color,
            health.message,
            theme,
        )))
        .hints(vec![
            Hint::new("↑↓", "Scroll"),
            Hint::new("⎋", "Back"),
            Hint::new("?", "More"),
        ]);

    let content_rect = chrome.render(frame, theme);
    let lines = content_lines(state, content_rect.width as usize, theme);

    let height = content_rect.height as usize;
    let max_scroll = lines.len().saturating_sub(height);
    state.max_scroll.set(max_scroll);

    let visible_lines: Vec<Line> = lines
        .into_iter()
        .skip(state.scroll_offset.min(max_scroll))
        .take(height)
        .collect();

    frame.render_widget(Paragraph::new(visible_lines), content_rect);
}

fn content_lines(state: &SystemStatusState, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let two_columns = width >= TWO_COLUMNS_MIN_WIDTH;
    let panel_w = if two_columns {
        (width - COLUMN_GAP) / 2
    } else {
        width
    };

    let live = &state.status.live;
    let mut panels = vec![
        cpu_panel(state, panel_w, theme),
        memory_panel(state, panel_w, theme),
        disk_panel(state, panel_w, theme),
    ];
    panels.extend(live.power.as_ref().map(|p| power_panel(p, panel_w, theme)));
    panels.push(processes_panel(live, panel_w, theme));
    panels.push(network_panel(state, panel_w, theme));

    let mut lines = summary_lines(state, width, theme);
    let mut panels = panels.into_iter();
    while let Some(panel) = panels.next() {
        lines.push(Line::default());
        if two_columns {
            let beside = panels.next().unwrap_or_default();
            lines.extend(side_by_side(panel, panel_w, COLUMN_GAP, beside));
        } else {
            lines.extend(panel);
        }
    }

    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        format!(
            "{}Managed space may include purgeable files and isn't counted as free.",
            indent()
        ),
        theme.footnote(),
    )));

    lines
}

fn summary_lines(state: &SystemStatusState, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let machine = &state.status.machine;
    let model = machine
        .model_name
        .as_deref()
        .or(machine.model.as_deref())
        .unwrap_or("Mac");
    let gpu_cores = state.status.live.gpu.as_ref().and_then(|g| g.core_count);

    let mut parts = Vec::new();
    parts.extend(machine.chip.clone());
    parts.extend(machine.cores.clone());
    parts.extend(gpu_cores.map(|cores| format!("{cores}-core GPU")));
    parts.extend(machine.memory_bytes.map(format_memory_bytes));
    parts.push(format!("macOS {}", machine.macos_version));
    parts.push(format!("up {}", format_uptime(machine.uptime_seconds)));

    let mut lines = vec![Line::from(Span::styled(
        format!("{}{model}", indent()),
        theme.headline(),
    ))];
    lines.extend(
        wrap_parts(&parts, width.saturating_sub(INDENT))
            .into_iter()
            .map(|text| Line::from(Span::styled(format!("{}{text}", indent()), theme.callout()))),
    );
    lines
}

fn wrap_parts(parts: &[String], width: usize) -> Vec<String> {
    const SEPARATOR: &str = " · ";
    let mut rows: Vec<String> = Vec::new();

    for part in parts {
        match rows.last_mut() {
            Some(row) if row.len() + SEPARATOR.len() + part.len() <= width => {
                row.push_str(SEPARATOR);
                row.push_str(part);
            }
            _ => rows.push(part.clone()),
        }
    }

    rows
}

fn format_uptime(seconds: u64) -> String {
    let days = seconds / 86400;
    let hours = (seconds % 86400) / 3600;
    if days > 0 {
        format!("{days}d {hours}h")
    } else {
        format!("{hours}h")
    }
}

fn format_minutes(minutes: i64) -> String {
    if minutes >= 60 {
        format!("{}h {}m", minutes / 60, minutes % 60)
    } else {
        format!("{minutes}m")
    }
}

fn indent() -> String {
    " ".repeat(INDENT)
}

fn percent(value: f32) -> String {
    format!("{value:.1}%")
}

fn rate(bytes_per_sec: f64) -> String {
    format!("{}/s", format_bytes(bytes_per_sec as u64))
}

fn capacity(used: u64, total: u64) -> String {
    format!("{} of {}", format_bytes(used), format_bytes(total))
}

fn rate_scale(series: &[&[f64]]) -> f64 {
    let peak = series.iter().flat_map(|s| s.iter().copied());
    peak.fold(MIN_RATE_SCALE, f64::max)
}

fn load_color(fraction: f64, theme: &Theme) -> Color {
    if fraction >= 0.9 {
        theme.p.red
    } else if fraction >= 0.7 {
        theme.p.yellow
    } else {
        theme.p.accent
    }
}

fn severity_color(severity: Severity, theme: &Theme) -> Color {
    match severity {
        Severity::Good => theme.p.green,
        Severity::Warning => theme.p.yellow,
        Severity::Critical => theme.p.red,
    }
}

fn disk_color(free_fraction: f64, theme: &Theme) -> Color {
    match disk_severity(free_fraction) {
        Severity::Good => theme.p.accent,
        severity => severity_color(severity, theme),
    }
}

fn status_spans(color: Color, text: impl Into<String>, theme: &Theme) -> Vec<Span<'static>> {
    vec![
        Span::styled(format!("{} ", theme.g.bullet), Style::default().fg(color)),
        Span::styled(text.into(), theme.callout()),
    ]
}

fn panel_header(
    title: &str,
    detail: Vec<Span<'static>>,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    render_section_header_spans(title, detail, width, theme)
}

fn detail_text(text: impl Into<String>, theme: &Theme) -> Vec<Span<'static>> {
    vec![Span::styled(text.into(), theme.callout())]
}

fn info_line(parts: &[String], theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        format!("{}{}", indent(), parts.join(" · ")),
        theme.callout(),
    ))
}

/// Rows of a panel: a label, a graph that takes the spare width, and a value on the right.
struct Rows<'t> {
    width: usize,
    label_w: usize,
    value_w: usize,
    theme: &'t Theme,
}

impl Rows<'_> {
    fn graph_width(&self) -> usize {
        let fixed = INDENT + self.label_w + 1 + 1 + self.value_w;
        self.width.saturating_sub(fixed).max(MIN_GRAPH_WIDTH)
    }

    fn row(&self, label: &str, graph: Vec<Span<'static>>, value: &str) -> Line<'static> {
        let label = fit_width(label, self.label_w);
        let mut spans = vec![Span::styled(
            format!("{}{label} ", indent()),
            self.theme.callout(),
        )];
        spans.extend(graph);
        spans.push(Span::styled(
            format!(" {value:>width$}", width = self.value_w),
            self.theme.body(),
        ));
        Line::from(spans)
    }

    fn segments(&self, label: &str, segments: &[(f64, Color)], value: &str) -> Line<'static> {
        let bar = segmented_bar(segments, self.graph_width() as u16, self.theme);
        self.row(label, bar, value)
    }

    fn bar(&self, label: &str, fraction: f64, color: Color, value: &str) -> Line<'static> {
        self.segments(label, &[(fraction, color)], value)
    }

    fn trend(
        &self,
        label: &str,
        series: &[f64],
        max: f64,
        color: Color,
        value: &str,
    ) -> Line<'static> {
        let graph = sparkline(series, self.graph_width(), max, color, self.theme);
        self.row(label, vec![graph], value)
    }

    fn waiting(&self, label: &str) -> Line<'static> {
        Line::from(vec![
            Span::styled(
                format!("{}{} ", indent(), fit_width(label, self.label_w)),
                self.theme.callout(),
            ),
            Span::styled(MEASURING, self.theme.footnote()),
        ])
    }
}

fn cpu_panel(state: &SystemStatusState, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let live = &state.status.live;
    let cpu = &live.cpu;

    let mut detail = Vec::new();
    detail.extend(cpu.temperature_c.map(|c| format!("{c:.0}°C")));
    detail.extend(
        cpu.load_average
            .map(|[one, five, fifteen]| format!("Load {one:.2} {five:.2} {fifteen:.2}")),
    );

    let rows = Rows {
        width,
        label_w: 7,
        value_w: 6,
        theme,
    };
    let mut lines = vec![panel_header(
        "CPU",
        detail_text(detail.join(" · "), theme),
        width,
        theme,
    )];

    lines.push(match cpu.usage_percent {
        Some(usage) => {
            let fraction = usage as f64 / 100.0;
            rows.bar(
                "Usage",
                fraction,
                load_color(fraction, theme),
                &percent(usage),
            )
        }
        None => rows.waiting("Usage"),
    });

    if let Some(usage) = live.gpu.as_ref().and_then(|g| g.usage_percent) {
        let fraction = usage as f64 / 100.0;
        lines.push(rows.bar(
            "GPU",
            fraction,
            load_color(fraction, theme),
            &percent(usage),
        ));
    }

    if !cpu.core_percents.is_empty() {
        lines.push(rows.row(
            "Cores",
            core_strip(cpu.core_percents.as_slice(), cpu.efficiency_cores, theme),
            "",
        ));
    }

    if !state.history.cpu.is_empty() {
        lines.push(rows.trend("History", &state.history.cpu, 100.0, theme.p.accent, ""));
    }

    lines
}

fn core_strip(percents: &[f32], efficiency: usize, theme: &Theme) -> Vec<Span<'static>> {
    let values: Vec<f64> = percents.iter().copied().map(f64::from).collect();
    let (efficient, performance) = values.split_at(efficiency.min(values.len()));

    let mut spans = Vec::new();
    if !efficient.is_empty() {
        spans.push(sparkline(
            efficient,
            efficient.len(),
            100.0,
            theme.p.teal,
            theme,
        ));
        spans.push(Span::raw(" "));
    }
    spans.push(sparkline(
        performance,
        performance.len(),
        100.0,
        theme.p.accent,
        theme,
    ));
    spans
}

fn memory_panel(state: &SystemStatusState, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mem = &state.status.memory;
    let (pressure, color) = match mem.pressure {
        Some(MemoryPressureLevel::Normal) | None => ("Normal", theme.p.green),
        Some(MemoryPressureLevel::Warning) => ("Warning", theme.p.yellow),
        Some(MemoryPressureLevel::Critical) => ("Critical", theme.p.red),
    };

    let mut detail = detail_text("Pressure  ", theme);
    detail.extend(status_spans(color, pressure, theme));
    let mut lines = vec![panel_header("Memory", detail, width, theme)];

    let total = mem.total_bytes.max(1) as f64;
    let share = |bytes: u64| bytes as f64 / total;
    let value = format!(
        "{} of {}",
        format_memory_bytes(mem.used_bytes()),
        format_memory_bytes(mem.total_bytes)
    );
    let rows = Rows {
        width,
        label_w: 4,
        value_w: value.len(),
        theme,
    };
    lines.push(rows.segments(
        "Used",
        &[
            (share(mem.app_bytes), theme.p.accent),
            (share(mem.wired_bytes), theme.p.purple),
            (share(mem.compressed_bytes), theme.p.teal),
            (share(mem.cached_bytes), theme.p.gray),
        ],
        &value,
    ));

    lines.extend(legend_grid(
        &[
            ("App", format_memory_bytes(mem.app_bytes), theme.p.accent),
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
                "Cached",
                format_memory_bytes(mem.cached_bytes),
                theme.p.gray,
            ),
            (
                "Free",
                format_memory_bytes(mem.free_bytes),
                theme.p.quaternary,
            ),
            (
                "Swap",
                format_memory_bytes(mem.swap_used_bytes),
                theme.p.quaternary,
            ),
        ],
        INDENT,
        width,
        theme,
    ));

    lines
}

fn disk_panel(state: &SystemStatusState, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let storage = &state.status.storage;
    let live = &state.status.live;
    let history = &state.history;

    let boot_name = live.boot_volume.as_deref().unwrap_or("System");
    let boot_used = storage.total_bytes.saturating_sub(storage.free_bytes);
    let boot_value = capacity(boot_used, storage.total_bytes);
    let external: Vec<(&str, u64, String)> = live
        .volumes
        .iter()
        .map(|volume| {
            let used = volume.total_bytes.saturating_sub(volume.available_bytes);
            (
                volume.name.as_str(),
                used,
                capacity(used, volume.total_bytes),
            )
        })
        .collect();

    let names = external.iter().map(|(name, _, _)| name.len());
    let values = external.iter().map(|(_, _, value)| value.len());
    let rows = Rows {
        width,
        label_w: names
            .chain([boot_name.len()])
            .max()
            .unwrap_or(0)
            .clamp(5, MAX_NAME_WIDTH),
        value_w: values.chain([boot_value.len()]).max().unwrap_or(0),
        theme,
    };

    let mut lines = vec![panel_header(
        "Storage",
        detail_text(
            format!("{} available", format_bytes(storage.available_bytes)),
            theme,
        ),
        width,
        theme,
    )];

    let total = storage.total_bytes.max(1) as f64;
    let used_fraction = boot_used as f64 / total;
    lines.push(rows.segments(
        boot_name,
        &[
            (used_fraction, disk_color(storage.free_fraction(), theme)),
            (storage.managed_bytes as f64 / total, theme.p.gray),
        ],
        &boot_value,
    ));
    for ((name, used, value), volume) in external.iter().zip(&live.volumes) {
        let fraction = *used as f64 / volume.total_bytes.max(1) as f64;
        lines.push(rows.bar(name, fraction, disk_color(1.0 - fraction, theme), value));
    }

    lines.extend(legend_grid(
        &[
            ("Used", format_bytes(boot_used), theme.p.accent),
            ("Managed", format_bytes(storage.managed_bytes), theme.p.gray),
            ("Free", format_bytes(storage.free_bytes), theme.p.quaternary),
        ],
        INDENT,
        width,
        theme,
    ));

    match &live.disk_io {
        Some(io) => {
            let scale = rate_scale(&[&history.read, &history.write]);
            lines.push(rows.trend(
                "Read",
                &history.read,
                scale,
                theme.p.teal,
                &rate(io.read_per_sec),
            ));
            lines.push(rows.trend(
                "Write",
                &history.write,
                scale,
                theme.p.purple,
                &rate(io.write_per_sec),
            ));
        }
        None => lines.push(rows.waiting("Speed")),
    }

    lines
}

fn power_panel(power: &PowerStatus, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let minutes = power.minutes_left.map(format_minutes);
    let (source, color) = match (power.source, minutes) {
        (PowerSource::Battery, Some(left)) => (format!("On battery, {left} left"), theme.p.yellow),
        (PowerSource::Battery, None) => ("On battery".to_string(), theme.p.yellow),
        (PowerSource::Charging, Some(left)) => (format!("Charging, {left} to full"), theme.p.green),
        (PowerSource::Charging, None) => ("Charging".to_string(), theme.p.green),
        (PowerSource::PluggedIn, _) => ("Plugged in".to_string(), theme.p.green),
    };

    let rows = Rows {
        width,
        label_w: 6,
        value_w: 4,
        theme,
    };
    let mut lines = vec![panel_header(
        "Power",
        status_spans(color, source, theme),
        width,
        theme,
    )];

    let charge = power.charge_percent;
    let charge_color = match charge {
        0..=10 => theme.p.red,
        11..=20 => theme.p.yellow,
        _ => theme.p.green,
    };
    lines.push(rows.bar(
        "Charge",
        charge as f64 / 100.0,
        charge_color,
        &format!("{charge}%"),
    ));

    if let Some(health) = power.health_percent {
        let color = if health >= 80 {
            theme.p.green
        } else {
            theme.p.yellow
        };
        lines.push(rows.bar(
            "Health",
            health as f64 / 100.0,
            color,
            &format!("{health}%"),
        ));
    }

    let mut facts = Vec::new();
    facts.extend(power.cycles.map(|cycles| format!("{cycles} cycles")));
    facts.extend(power.temperature_c.map(|c| format!("{c:.0}°C")));
    facts.extend(
        power
            .adapter_watts
            .map(|watts| format!("{watts} W adapter")),
    );
    facts.extend(power.draw_watts.map(|watts| format!("{watts:.1} W draw")));
    if !facts.is_empty() {
        lines.push(info_line(&facts, theme));
    }

    lines
}

fn processes_panel(live: &LiveStatus, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let rows = Rows {
        width,
        label_w: MAX_NAME_WIDTH,
        value_w: 17,
        theme,
    };
    let mut lines = vec![panel_header(
        "Processes",
        detail_text("Top by CPU", theme),
        width,
        theme,
    )];

    if live.processes.is_empty() {
        lines.push(rows.waiting("Top"));
    }

    // Bars compare the processes with each other, and a full core is the least they span.
    let peak = live.processes.iter().map(|p| p.cpu_percent as f64);
    let scale = peak.fold(100.0, f64::max);
    for process in &live.processes {
        let cpu = process.cpu_percent as f64;
        let value = format!(
            "{:>7} {:>9}",
            percent(process.cpu_percent),
            format_memory_bytes(process.memory_bytes)
        );
        let color = load_color(cpu / BUSY_PROCESS_PERCENT, theme);
        lines.push(rows.bar(&process.name, cpu / scale, color, &value));
    }

    lines
}

fn network_panel(state: &SystemStatusState, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let network = &state.status.live.network;
    let history = &state.history;

    let local = network.local.as_ref();
    let detail = local.map_or("Offline".to_string(), |l| {
        format!("{} {}", l.interface, l.address)
    });
    let rows = Rows {
        width,
        label_w: 6,
        value_w: 10,
        theme,
    };
    let mut lines = vec![panel_header(
        "Network",
        detail_text(detail, theme),
        width,
        theme,
    )];

    match (network.down_per_sec, network.up_per_sec) {
        (Some(down), Some(up)) => {
            let scale = rate_scale(&[&history.down, &history.up]);
            lines.push(rows.trend("Down", &history.down, scale, theme.p.accent, &rate(down)));
            lines.push(rows.trend("Up", &history.up, scale, theme.p.teal, &rate(up)));
        }
        _ => lines.push(rows.waiting("Speed")),
    }

    if let Some(tunnel) = &network.tunnel {
        lines.push(info_line(
            &[format!("Tunnel {} {}", tunnel.interface, tunnel.address)],
            theme,
        ));
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::core::profiler::{DetailedMemoryStatus, DetailedStorageStatus, MachineInfo};
    use crate::core::telemetry::{
        CpuStatus, DiskIo, GpuStatus, LiveStatus, NetworkStatus, PowerSource, PowerStatus,
        ProcessStatus,
    };
    use crate::ui::env::UiEnv;

    const GB: u64 = 1_000_000_000;

    fn measuring_status() -> FullSystemStatus {
        FullSystemStatus {
            machine: MachineInfo {
                chip: Some("Apple M4 Pro".to_string()),
                cores: Some("14 cores".to_string()),
                memory_bytes: Some(48 * GB),
                macos_version: "27.0.1".to_string(),
                model: Some("Mac16,7".to_string()),
                model_name: Some("MacBook Pro".to_string()),
                uptime_seconds: 3_600,
            },
            memory: DetailedMemoryStatus {
                total_bytes: 48 * GB,
                free_bytes: 10 * GB,
                app_bytes: 20 * GB,
                wired_bytes: 5 * GB,
                compressed_bytes: 5 * GB,
                cached_bytes: 8 * GB,
                swap_used_bytes: 0,
                pressure: None,
            },
            storage: DetailedStorageStatus {
                mount_point: "/".to_string(),
                total_bytes: 500 * GB,
                available_bytes: 100 * GB,
                free_bytes: 105 * GB,
                managed_bytes: 5 * GB,
            },
            live: LiveStatus::default(),
        }
    }

    fn measured_status() -> FullSystemStatus {
        FullSystemStatus {
            live: LiveStatus {
                cpu: CpuStatus {
                    usage_percent: Some(40.0),
                    core_percents: vec![10.0, 90.0],
                    efficiency_cores: 1,
                    load_average: Some([1.0, 2.0, 3.0]),
                    temperature_c: Some(60.0),
                },
                gpu: Some(GpuStatus {
                    usage_percent: Some(20.0),
                    core_count: Some(20),
                }),
                boot_volume: Some("Macintosh HD".to_string()),
                volumes: Vec::new(),
                disk_io: Some(DiskIo {
                    read_per_sec: 1_000.0,
                    write_per_sec: 2_000.0,
                }),
                network: NetworkStatus {
                    down_per_sec: Some(1_000.0),
                    up_per_sec: Some(500.0),
                    local: None,
                    tunnel: None,
                },
                power: Some(PowerStatus {
                    charge_percent: 80,
                    health_percent: Some(100),
                    cycles: Some(124),
                    source: PowerSource::PluggedIn,
                    adapter_watts: Some(96),
                    draw_watts: None,
                    minutes_left: None,
                    temperature_c: Some(31.0),
                }),
                processes: vec![
                    ProcessStatus {
                        name: "a-process-with-a-very-long-name".to_string(),
                        cpu_percent: 250.0,
                        memory_bytes: GB,
                    },
                    ProcessStatus {
                        name: "node".to_string(),
                        cpu_percent: 50.0,
                        memory_bytes: GB,
                    },
                ],
            },
            ..measuring_status()
        }
    }

    fn render(state: &SystemStatusState, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let theme = Theme::with_env(UiEnv::default());
        terminal
            .draw(|f| render_system_status(f, state, &theme))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect()
    }

    fn find(rows: &[String], text: &str) -> (usize, usize) {
        rows.iter()
            .enumerate()
            .find_map(|(y, row)| row.find(text).map(|i| (row[..i].chars().count(), y)))
            .unwrap_or_else(|| panic!("{text} is not on the screen:\n{}", rows.join("\n")))
    }

    #[test]
    fn wide_windows_show_the_panels_in_two_columns() {
        let rows = render(&SystemStatusState::with_status(measured_status()), 120, 40);

        let (cpu_x, cpu_y) = find(&rows, "CPU");
        let (memory_x, memory_y) = find(&rows, "MEMORY");
        assert_eq!(cpu_y, memory_y);
        assert!(cpu_x < memory_x);
        for panel in ["STORAGE", "POWER", "PROCESSES", "NETWORK"] {
            find(&rows, panel);
        }
    }

    #[test]
    fn narrow_windows_stack_the_panels() {
        let rows = render(&SystemStatusState::with_status(measured_status()), 80, 60);

        let (cpu_x, cpu_y) = find(&rows, "CPU");
        let (memory_x, memory_y) = find(&rows, "MEMORY");
        assert_eq!(cpu_x, memory_x);
        assert!(cpu_y < memory_y);
    }

    #[test]
    fn a_mac_without_a_battery_has_no_power_panel() {
        let mut status = measured_status();
        status.live.power = None;

        let rows = render(&SystemStatusState::with_status(status), 120, 40);

        assert!(!rows.join("\n").contains("POWER"));
        find(&rows, "NETWORK");
    }

    #[test]
    fn values_wait_for_the_first_measurement() {
        let rows = render(&SystemStatusState::with_status(measuring_status()), 120, 40);

        find(&rows, "Measuring");
    }

    #[test]
    fn the_title_bar_reports_health() {
        let rows = render(&SystemStatusState::with_status(measured_status()), 120, 40);

        find(&rows, "All clear");
    }

    #[test]
    fn the_marketing_name_replaces_the_model_identifier() {
        let rows = render(&SystemStatusState::with_status(measured_status()), 120, 40);

        find(&rows, "MacBook Pro");
        find(&rows, "20-core GPU");
        assert!(!rows.join("\n").contains("Mac16,7"));
    }

    #[test]
    fn a_long_process_name_keeps_the_columns_straight() {
        let rows = render(&SystemStatusState::with_status(measured_status()), 120, 40);

        assert_eq!(find(&rows, "250.0%").0, find(&rows, " 50.0%").0);
        find(&rows, "…");
    }

    #[test]
    fn scrolling_stops_at_the_last_line() {
        let mut state = SystemStatusState::with_status(measured_status());
        render(&state, 80, 24);

        state.scroll_to_bottom();
        let bottom = state.scroll_offset;
        assert!(bottom > 0);

        state.scroll_down();
        assert_eq!(state.scroll_offset, bottom);

        state.page_down();
        assert_eq!(state.scroll_offset, bottom);

        state.scroll_up();
        assert_eq!(state.scroll_offset, bottom - 1);
    }

    #[test]
    fn a_disk_stays_calm_until_space_runs_low() {
        let theme = Theme::with_env(UiEnv::default());

        assert_eq!(disk_color(0.24, &theme), theme.p.accent);
        assert_eq!(disk_color(0.08, &theme), theme.p.yellow);
        assert_eq!(disk_color(0.03, &theme), theme.p.red);
    }

    #[test]
    fn history_keeps_a_bounded_window_of_measurements() {
        let mut state = SystemStatusState::with_status(measuring_status());
        state.apply(measuring_status());
        assert!(state.history.cpu.is_empty());

        for _ in 0..HISTORY_LEN + 10 {
            state.apply(measured_status());
        }

        assert_eq!(state.history.cpu.len(), HISTORY_LEN);
        assert_eq!(state.history.down.len(), HISTORY_LEN);
    }

    #[test]
    fn tick_applies_every_waiting_update() {
        let (tx, rx) = mpsc::channel();
        let mut state = SystemStatusState::with_status(measuring_status());
        state.updates = Some(rx);

        tx.send(measured_status()).unwrap();
        tx.send(measured_status()).unwrap();
        state.tick();

        assert_eq!(state.history.cpu.len(), 2);
        assert_eq!(state.status.live.cpu.usage_percent, Some(40.0));
    }
}

//! Live System status screen: a machine summary and panels for CPU, memory, disk, power,
//! processes and network, refreshed by a background sampler.

use std::cell::Cell;
use std::sync::mpsc::Receiver;

use ratatui::Frame;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use super::components::chrome::AppChrome;
use super::components::columns::side_by_side;
use super::components::frame::{FRAME_ROWS, FRAME_SIDES, card};
use super::components::gauge::{chart, legend_grid, segmented_bar, sparkline};
use super::components::hints::Hint;
use super::components::text::fit_width;
use super::theme::Theme;
use crate::core::bytes::{format_bytes, format_memory_bytes};
use crate::core::profiler::{FullSystemStatus, MemoryPressureLevel, SystemProfiler};
use crate::core::telemetry::{
    LiveStatus, PowerSource, PowerStatus, ProcessStatus, Severity, disk_severity, health,
    spawn_sampler,
};

const HISTORY_LEN: usize = 240;
const TWO_COLUMNS_MIN_WIDTH: usize = 100;
const THREE_COLUMNS_MIN_WIDTH: usize = 170;
const COLUMN_GAP: usize = 4;
const MIN_GRAPH_WIDTH: usize = 6;
const MAX_NAME_WIDTH: usize = 16;
// A frame around the fewest rows a process list shows.
const MIN_LIST_HEIGHT: usize = 5 + FRAME_ROWS;
// Two such frames and the blank line between them.
const TWO_LISTS_MIN_ROWS: usize = 2 * MIN_LIST_HEIGHT + 1;
const MAX_CHART_ROWS: usize = 24;
// CPU and memory in the upper panels share a height, as do network and storage below,
// so three charts stack in a column. A single column stacks all six.
const PAIRED_CHARTS: usize = 3;
const STACKED_CHARTS: usize = 6;
// Keeps idle traffic flat instead of letting a few bytes fill the graph.
const MIN_RATE_SCALE: f64 = 100_000.0;
const BUSY_PROCESS_PERCENT: f64 = 200.0;
const MEASURING: &str = "Measuring…";
const UNKNOWN: &str = "—";

#[derive(Debug, Default)]
pub struct History {
    pub cpu: Vec<f64>,
    pub memory: Vec<f64>,
    pub read: Vec<f64>,
    pub write: Vec<f64>,
    pub down: Vec<f64>,
    pub up: Vec<f64>,
}

impl History {
    fn record(&mut self, status: &FullSystemStatus) {
        let live = &status.live;
        let memory = &status.memory;
        let disk = live.disk_io.as_ref();
        let usage = live.cpu.usage_percent.map(f64::from);
        let used = memory.used_bytes() as f64 / memory.total_bytes.max(1) as f64;

        record(&mut self.cpu, usage);
        record(&mut self.memory, Some(used * 100.0));
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

    pub fn is_live(&self) -> bool {
        self.updates.is_some()
    }

    /// Starts or stops the sampler. It exits on its own once its receiver is gone.
    pub fn set_live(&mut self, live: bool) {
        if live != self.is_live() {
            self.updates = live.then(spawn_sampler);
        }
    }

    pub fn apply(&mut self, status: FullSystemStatus) {
        self.history.record(&status);
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

    pub fn can_scroll(&self) -> bool {
        self.max_scroll.get() > 0
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
        .full_width()
        .status(Line::from(status_spans(
            health_color,
            health.message,
            theme,
        )));

    let content_rect = chrome.content_rect(frame.area());
    let height = content_rect.height as usize;
    let lines = content_lines(state, content_rect.width as usize, height, theme);

    let max_scroll = lines.len().saturating_sub(height);
    state.max_scroll.set(max_scroll);

    let mut hints = Vec::new();
    if max_scroll > 0 {
        hints.push(Hint::new("↑↓", "Scroll"));
    }
    hints.push(Hint::new("⎋", "Back"));
    hints.push(Hint::new("?", "More"));
    chrome.hints(hints).render(frame, theme);

    let visible_lines: Vec<Line> = lines
        .into_iter()
        .skip(state.scroll_offset.min(max_scroll))
        .take(height)
        .collect();

    frame.render_widget(Paragraph::new(visible_lines), content_rect);
}

/// A titled panel. Panels side by side give `top` the same height to line up their charts.
struct Card {
    title: &'static str,
    detail: Vec<Span<'static>>,
    color: Color,
    top: Vec<Line<'static>>,
    charts: Vec<Line<'static>>,
}

impl Card {
    fn lines(self, inner_w: usize, top_rows: usize, theme: &Theme) -> Vec<Line<'static>> {
        let Self {
            title,
            detail,
            color,
            mut top,
            charts,
        } = self;

        top.resize(top_rows.max(top.len()), Line::default());
        top.extend(charts);
        card(title, detail, top, inner_w + FRAME_SIDES, 0, color, theme)
    }
}

// A chart taller than one row needs a caption of its own.
fn chart_height(rows: usize) -> usize {
    if rows >= 3 {
        (rows - 1).min(MAX_CHART_ROWS)
    } else {
        1
    }
}

/// Rows for the one chart of the upper panels and each of the two in the lower panels.
fn chart_rows(spare: usize) -> (usize, usize) {
    let lower = chart_height(spare / PAIRED_CHARTS);
    if lower == 1 {
        return (1, 1);
    }
    let upper = spare.saturating_sub(2 * (lower + 1));
    (chart_height(upper), lower)
}

fn stacked(cards: Vec<Vec<Line<'static>>>) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for card in cards {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.extend(card);
    }
    lines
}

struct Dashboard<'a> {
    state: &'a SystemStatusState,
    inner_w: usize,
    panel_w: usize,
    theme: &'a Theme,
}

impl Dashboard<'_> {
    fn cpu(&self, charts: usize) -> Card {
        cpu_card(self.state, self.inner_w, charts, self.theme)
    }

    fn memory(&self, charts: usize) -> Card {
        memory_card(self.state, self.inner_w, charts, self.theme)
    }

    fn storage(&self, charts: usize) -> Card {
        storage_card(self.state, self.inner_w, charts, self.theme)
    }

    fn network(&self, charts: usize) -> Card {
        network_card(self.state, self.inner_w, charts, self.theme)
    }

    fn power(&self) -> Option<Vec<Line<'static>>> {
        let power = self.state.status.live.power.as_ref()?;
        let card = power_card(power, self.inner_w, self.theme);
        Some(card.lines(self.inner_w, 0, self.theme))
    }

    fn top_by_cpu(&self, height: usize) -> Vec<Line<'static>> {
        let rows = height.saturating_sub(FRAME_ROWS);
        let live = &self.state.status.live;
        top_by_cpu_card(live, self.inner_w, rows, self.theme).lines(self.inner_w, rows, self.theme)
    }

    fn top_by_memory(&self, height: usize) -> Vec<Line<'static>> {
        let rows = height.saturating_sub(FRAME_ROWS);
        let live = &self.state.status.live;
        top_by_memory_card(live, self.inner_w, rows, self.theme).lines(
            self.inner_w,
            rows,
            self.theme,
        )
    }

    /// CPU over Network and Memory over Storage, with every chart facing its neighbor.
    fn paired(&self, height: usize) -> (Vec<Line<'static>>, Vec<Line<'static>>) {
        let top_rows = |a: Card, b: Card| a.top.len().max(b.top.len());
        let upper_top = top_rows(self.cpu(1), self.memory(1));
        let lower_top = top_rows(self.network(1), self.storage(1));
        let fixed = upper_top + lower_top + 2 * FRAME_ROWS + 1;
        let (upper, lower) = chart_rows(height.saturating_sub(fixed));

        let (w, theme) = (self.inner_w, self.theme);
        let left = stacked(vec![
            self.cpu(upper).lines(w, upper_top, theme),
            self.network(lower).lines(w, lower_top, theme),
        ]);
        let right = stacked(vec![
            self.memory(upper).lines(w, upper_top, theme),
            self.storage(lower).lines(w, lower_top, theme),
        ]);
        (left, right)
    }

    fn beside(&self, left: Vec<Line<'static>>, right: Vec<Line<'static>>) -> Vec<Line<'static>> {
        side_by_side(left, self.panel_w, COLUMN_GAP, right)
    }

    // One list takes the room until both can show a full set of rows.
    fn process_lists(&self, height: usize) -> Vec<Line<'static>> {
        if height < TWO_LISTS_MIN_ROWS {
            return self.top_by_cpu(height);
        }
        let upper = height.saturating_sub(1) / 2;
        let lower = height.saturating_sub(upper + 1);
        stacked(vec![self.top_by_cpu(upper), self.top_by_memory(lower)])
    }

    fn three_columns(&self, height: usize) -> Vec<Line<'static>> {
        let (left, right) = self.paired(height);
        let total = left.len();

        let side = match self.power() {
            Some(power) => {
                let rest = total.saturating_sub(power.len() + 1).max(MIN_LIST_HEIGHT);
                stacked(vec![power, self.process_lists(rest)])
            }
            None => self.process_lists(total.max(MIN_LIST_HEIGHT)),
        };

        let panels = self.beside(left, right);
        let left_w = 2 * self.panel_w + COLUMN_GAP;
        side_by_side(panels, left_w, COLUMN_GAP, side)
    }

    fn two_columns(&self, height: usize) -> Vec<Line<'static>> {
        let reserved = (height / 3).max(MIN_LIST_HEIGHT);
        let (left, right) = self.paired(height.saturating_sub(reserved + 1));
        let rest = height.saturating_sub(left.len() + 1).max(MIN_LIST_HEIGHT);

        let beside_list = match self.power() {
            Some(power) => {
                let spare = rest.saturating_sub(power.len() + 1);
                if spare >= MIN_LIST_HEIGHT {
                    stacked(vec![power, self.top_by_memory(spare)])
                } else {
                    let card = self.state.status.live.power.as_ref().map(|power| {
                        power_card(power, self.inner_w, self.theme).lines(
                            self.inner_w,
                            rest.saturating_sub(FRAME_ROWS),
                            self.theme,
                        )
                    });
                    card.unwrap_or_default()
                }
            }
            None => self.top_by_memory(rest),
        };

        stacked(vec![
            self.beside(left, right),
            self.beside(beside_list, self.top_by_cpu(rest)),
        ])
    }

    fn one_column(&self, height: usize) -> Vec<Line<'static>> {
        let (w, theme) = (self.inner_w, self.theme);
        let cards = |charts: usize| {
            let mut cards = vec![
                self.cpu(charts).lines(w, 0, theme),
                self.memory(charts).lines(w, 0, theme),
                self.storage(charts).lines(w, 0, theme),
            ];
            cards.extend(self.power());
            cards.push(self.top_by_cpu(MIN_LIST_HEIGHT));
            cards.push(self.top_by_memory(MIN_LIST_HEIGHT));
            cards.push(self.network(charts).lines(w, 0, theme));
            stacked(cards)
        };

        let spare = height.saturating_sub(cards(1).len());
        cards(chart_height(spare / STACKED_CHARTS + 1))
    }
}

fn dashboard<'a>(
    state: &'a SystemStatusState,
    width: usize,
    columns: usize,
    theme: &'a Theme,
) -> Dashboard<'a> {
    let panel_w = width.saturating_sub((columns - 1) * COLUMN_GAP) / columns;
    Dashboard {
        state,
        inner_w: panel_w.saturating_sub(FRAME_SIDES),
        panel_w,
        theme,
    }
}

/// CPU and memory for the main menu.
pub fn load_summary(
    state: &SystemStatusState,
    width: usize,
    height: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let columns = if width >= TWO_COLUMNS_MIN_WIDTH { 2 } else { 1 };
    let cards = dashboard(state, width, columns, theme);
    let w = cards.inner_w;
    let top = cards.cpu(1).top.len().max(cards.memory(1).top.len());

    if columns == 2 {
        let charts = chart_height(height.saturating_sub(top + FRAME_ROWS));
        let left = cards.cpu(charts).lines(w, top, theme);
        let right = cards.memory(charts).lines(w, top, theme);
        cards.beside(left, right)
    } else {
        let spare = height.saturating_sub(2 * (top + FRAME_ROWS) + 1);
        let charts = chart_height(spare / 2);
        stacked(vec![
            cards.cpu(charts).lines(w, top, theme),
            cards.memory(charts).lines(w, top, theme),
        ])
    }
}

/// Disk capacity for the main menu. Speeds need the sampler, which runs on demand.
pub fn storage_summary(
    state: &SystemStatusState,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let inner_w = width.saturating_sub(FRAME_SIDES);
    capacity_card(state, inner_w, theme)
        .0
        .lines(inner_w, 0, theme)
}

fn content_lines(
    state: &SystemStatusState,
    width: usize,
    height: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut lines = summary_lines(state, width, theme);
    // The blank lines around the panels and the footnote.
    let available = height.saturating_sub(lines.len() + 3);

    let panels = if width >= THREE_COLUMNS_MIN_WIDTH {
        dashboard(state, width, 3, theme).three_columns(available)
    } else if width >= TWO_COLUMNS_MIN_WIDTH {
        dashboard(state, width, 2, theme).two_columns(available)
    } else {
        dashboard(state, width, 1, theme).one_column(available)
    };

    lines.push(Line::default());
    lines.extend(panels);
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "Managed space may include purgeable files and isn't counted as free.",
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
        model.to_string(),
        theme.headline(),
    ))];
    lines.extend(
        wrap_parts(&parts, width)
            .into_iter()
            .map(|text| Line::from(Span::styled(text, theme.callout()))),
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

fn detail_text(text: impl Into<String>, theme: &Theme) -> Vec<Span<'static>> {
    vec![Span::styled(text.into(), theme.callout())]
}

fn info_line(parts: &[String], theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(parts.join(" · "), theme.callout()))
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
        let fixed = self.label_w + 1 + 1 + self.value_w;
        self.width.saturating_sub(fixed).max(MIN_GRAPH_WIDTH)
    }

    fn row(&self, label: &str, graph: Vec<Span<'static>>, value: &str) -> Line<'static> {
        let label = fit_width(label, self.label_w);
        let mut spans = vec![Span::styled(format!("{label} "), self.theme.callout())];
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

    fn caption(&self, label: &str, value: &str) -> Line<'static> {
        let pad = self.width.saturating_sub(label.len() + value.width());
        Line::from(vec![
            Span::styled(label.to_string(), self.theme.callout()),
            Span::raw(" ".repeat(pad)),
            Span::styled(value.to_string(), self.theme.body()),
        ])
    }

    // One row beside its label, or a chart of its own under a caption.
    fn series(
        &self,
        label: &str,
        series: &[f64],
        max: f64,
        color: Color,
        value: &str,
        chart_rows: usize,
    ) -> Vec<Line<'static>> {
        if chart_rows < 2 {
            return vec![self.trend(label, series, max, color, value)];
        }

        let area = Style::default().bg(self.theme.p.fill);
        let mut lines = vec![self.caption(label, value)];
        lines.extend(
            chart(series, self.width, chart_rows, max, color, self.theme)
                .into_iter()
                .map(|row| Line::from(row.patch_style(area))),
        );
        lines
    }

    fn waiting(&self, label: &str) -> Line<'static> {
        Line::from(vec![
            Span::styled(
                format!("{} ", fit_width(label, self.label_w)),
                self.theme.callout(),
            ),
            Span::styled(MEASURING, self.theme.footnote()),
        ])
    }
}

fn cpu_card(state: &SystemStatusState, width: usize, chart_rows: usize, theme: &Theme) -> Card {
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

    let usage_bar = |label, usage: f32| {
        let fraction = usage as f64 / 100.0;
        rows.bar(
            label,
            fraction,
            load_color(fraction, theme),
            &percent(usage),
        )
    };
    let usage = match cpu.usage_percent {
        Some(usage) => usage_bar("Usage", usage),
        None => rows.waiting("Usage"),
    };
    let gpu = match live.gpu.as_ref().and_then(|g| g.usage_percent) {
        Some(usage) => usage_bar("GPU", usage),
        None => rows.caption("GPU", UNKNOWN),
    };
    let cores = if cpu.core_percents.is_empty() {
        rows.caption("Cores", UNKNOWN)
    } else {
        rows.row(
            "Cores",
            core_strip(cpu.core_percents.as_slice(), cpu.efficiency_cores, theme),
            "",
        )
    };

    Card {
        title: "CPU",
        detail: detail_text(detail.join(" · "), theme),
        color: theme.p.accent,
        top: vec![usage, gpu, cores],
        charts: rows.series(
            "History",
            &state.history.cpu,
            100.0,
            theme.p.accent,
            "",
            chart_rows,
        ),
    }
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

fn memory_card(state: &SystemStatusState, width: usize, chart_rows: usize, theme: &Theme) -> Card {
    let mem = &state.status.memory;
    let (pressure, color) = match mem.pressure {
        Some(MemoryPressureLevel::Normal) | None => ("Normal", theme.p.green),
        Some(MemoryPressureLevel::Warning) => ("Warning", theme.p.yellow),
        Some(MemoryPressureLevel::Critical) => ("Critical", theme.p.red),
    };

    let mut detail = detail_text("Pressure  ", theme);
    detail.extend(status_spans(color, pressure, theme));

    let total = mem.total_bytes.max(1) as f64;
    let share = |bytes: u64| bytes as f64 / total;
    let value = format!(
        "{} of {}",
        format_memory_bytes(mem.used_bytes()),
        format_memory_bytes(mem.total_bytes)
    );
    let rows = Rows {
        width,
        label_w: 7,
        value_w: value.len(),
        theme,
    };

    let mut top = vec![rows.segments(
        "Used",
        &[
            (share(mem.app_bytes), theme.p.accent),
            (share(mem.wired_bytes), theme.p.purple),
            (share(mem.compressed_bytes), theme.p.teal),
            (share(mem.cached_bytes), theme.p.gray),
        ],
        &value,
    )];
    top.extend(legend_grid(
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
        0,
        width,
        theme,
    ));

    Card {
        title: "Memory",
        detail,
        color: theme.p.purple,
        top,
        charts: rows.series(
            "History",
            &state.history.memory,
            100.0,
            theme.p.gray,
            "",
            chart_rows,
        ),
    }
}

fn capacity_card<'t>(
    state: &SystemStatusState,
    width: usize,
    theme: &'t Theme,
) -> (Card, Rows<'t>) {
    let storage = &state.status.storage;
    let live = &state.status.live;

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

    let total = storage.total_bytes.max(1) as f64;
    let used_fraction = boot_used as f64 / total;
    let mut top = vec![rows.segments(
        boot_name,
        &[
            (used_fraction, disk_color(storage.free_fraction(), theme)),
            (storage.managed_bytes as f64 / total, theme.p.gray),
        ],
        &boot_value,
    )];
    for ((name, used, value), volume) in external.iter().zip(&live.volumes) {
        let fraction = *used as f64 / volume.total_bytes.max(1) as f64;
        top.push(rows.bar(name, fraction, disk_color(1.0 - fraction, theme), value));
    }

    top.extend(legend_grid(
        &[
            ("Used", format_bytes(boot_used), theme.p.accent),
            ("Managed", format_bytes(storage.managed_bytes), theme.p.gray),
            ("Free", format_bytes(storage.free_bytes), theme.p.quaternary),
        ],
        0,
        width,
        theme,
    ));

    let card = Card {
        title: "Storage",
        detail: detail_text(
            format!("{} available", format_bytes(storage.available_bytes)),
            theme,
        ),
        color: theme.p.orange,
        top,
        charts: Vec::new(),
    };
    (card, rows)
}

fn storage_card(state: &SystemStatusState, width: usize, chart_rows: usize, theme: &Theme) -> Card {
    let history = &state.history;
    let (mut card, rows) = capacity_card(state, width, theme);

    let io = state.status.live.disk_io.as_ref();
    let read = io.map_or(UNKNOWN.to_string(), |io| rate(io.read_per_sec));
    let write = io.map_or(UNKNOWN.to_string(), |io| rate(io.write_per_sec));
    let scale = rate_scale(&[&history.read, &history.write]);

    card.charts = rows.series(
        "Read",
        &history.read,
        scale,
        theme.p.teal,
        &read,
        chart_rows,
    );
    card.charts.extend(rows.series(
        "Write",
        &history.write,
        scale,
        theme.p.purple,
        &write,
        chart_rows,
    ));
    card
}

fn power_card(power: &PowerStatus, width: usize, theme: &Theme) -> Card {
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

    let charge = power.charge_percent;
    let charge_color = match charge {
        0..=10 => theme.p.red,
        11..=20 => theme.p.yellow,
        _ => theme.p.green,
    };
    let mut top = vec![rows.bar(
        "Charge",
        charge as f64 / 100.0,
        charge_color,
        &format!("{charge}%"),
    )];

    if let Some(health) = power.health_percent {
        let color = if health >= 80 {
            theme.p.green
        } else {
            theme.p.yellow
        };
        top.push(rows.bar(
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
        top.push(info_line(&facts, theme));
    }

    Card {
        title: "Power",
        detail: status_spans(color, source, theme),
        color: theme.p.green,
        top,
        charts: Vec::new(),
    }
}

fn process_rows(width: usize, theme: &Theme) -> Rows<'_> {
    Rows {
        width,
        label_w: MAX_NAME_WIDTH,
        value_w: 17,
        theme,
    }
}

fn process_row(rows: &Rows, process: &ProcessStatus, fraction: f64, color: Color) -> Line<'static> {
    let value = format!(
        "{:>7} {:>9}",
        percent(process.cpu_percent),
        format_memory_bytes(process.memory_bytes)
    );
    rows.bar(&process.name, fraction, color, &value)
}

fn top_by_cpu_card(live: &LiveStatus, width: usize, list_rows: usize, theme: &Theme) -> Card {
    let rows = process_rows(width, theme);
    let busy: Vec<&ProcessStatus> = live
        .processes
        .iter()
        .filter(|process| process.cpu_percent > 0.0)
        .collect();

    // Bars compare the processes with each other, and a full core is the least they span.
    let peak = busy.iter().map(|p| p.cpu_percent as f64);
    let scale = peak.fold(100.0, f64::max);
    let mut top: Vec<Line<'static>> = busy
        .iter()
        .take(list_rows)
        .map(|process| {
            let cpu = process.cpu_percent as f64;
            let color = load_color(cpu / BUSY_PROCESS_PERCENT, theme);
            process_row(&rows, process, cpu / scale, color)
        })
        .collect();
    if top.is_empty() {
        top.push(rows.waiting("Top"));
    }

    Card {
        title: "Top by CPU",
        detail: Vec::new(),
        color: theme.p.accent,
        top,
        charts: Vec::new(),
    }
}

fn top_by_memory_card(live: &LiveStatus, width: usize, list_rows: usize, theme: &Theme) -> Card {
    let rows = process_rows(width, theme);
    let processes = &live.memory_processes;

    let peak = processes.iter().map(|p| p.memory_bytes).max().unwrap_or(0);
    let scale = peak.max(1) as f64;
    let mut top: Vec<Line<'static>> = processes
        .iter()
        .take(list_rows)
        .map(|process| {
            let fraction = process.memory_bytes as f64 / scale;
            process_row(&rows, process, fraction, theme.p.purple)
        })
        .collect();
    if top.is_empty() {
        top.push(rows.waiting("Top"));
    }

    Card {
        title: "Top by memory",
        detail: Vec::new(),
        color: theme.p.purple,
        top,
        charts: Vec::new(),
    }
}

fn network_card(state: &SystemStatusState, width: usize, chart_rows: usize, theme: &Theme) -> Card {
    let network = &state.status.live.network;
    let history = &state.history;

    let rows = Rows {
        width,
        label_w: 6,
        value_w: 10,
        theme,
    };

    let local = network.local.as_ref();
    let status = if local.is_some() {
        status_spans(theme.p.green, "Connected", theme)
    } else {
        status_spans(theme.p.quaternary, "Offline", theme)
    };
    let tunnel = network
        .tunnel
        .as_ref()
        .map_or("None".to_string(), |tunnel| {
            format!("{} {}", tunnel.interface, tunnel.address)
        });
    let top = vec![
        rows.caption("Interface", local.map_or(UNKNOWN, |l| l.interface.as_str())),
        rows.caption("Address", local.map_or(UNKNOWN, |l| l.address.as_str())),
        rows.caption("Tunnel", &tunnel),
    ];

    let speed = |value: Option<f64>| value.map_or(UNKNOWN.to_string(), rate);
    let scale = rate_scale(&[&history.down, &history.up]);
    let mut charts = rows.series(
        "Down",
        &history.down,
        scale,
        theme.p.accent,
        &speed(network.down_per_sec),
        chart_rows,
    );
    charts.extend(rows.series(
        "Up",
        &history.up,
        scale,
        theme.p.teal,
        &speed(network.up_per_sec),
        chart_rows,
    ));

    Card {
        title: "Network",
        detail: status,
        color: theme.p.teal,
        top,
        charts,
    }
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
        VolumeStatus,
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
                memory_processes: Vec::new(),
            },
            ..measuring_status()
        }
    }

    fn process(name: String, cpu_percent: f32) -> ProcessStatus {
        ProcessStatus {
            name,
            cpu_percent,
            memory_bytes: GB,
        }
    }

    // A long list of processes and a history that charts can draw.
    fn busy_state() -> SystemStatusState {
        let mut status = measured_status();
        status.live.processes = (0..40).map(|i| process(format!("cpu-{i}"), 10.0)).collect();
        status.live.memory_processes = (0..60).map(|i| process(format!("ram-{i}"), 0.0)).collect();

        let mut state = SystemStatusState::with_status(measuring_status());
        for _ in 0..HISTORY_LEN {
            state.apply(status.clone());
        }
        state
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

    fn all(rows: &[String], text: &str) -> Vec<(usize, usize)> {
        rows.iter()
            .enumerate()
            .flat_map(|(y, row)| {
                row.match_indices(text)
                    .map(move |(i, _)| (row[..i].chars().count(), y))
            })
            .collect()
    }

    fn cell(rows: &[String], x: usize, y: usize) -> Option<char> {
        rows[y].chars().nth(x)
    }

    const LAYOUTS: [(u16, u16); 3] = [(205, 85), (205, 60), (120, 40)];

    #[test]
    fn wide_windows_show_the_panels_in_two_columns() {
        let rows = render(&SystemStatusState::with_status(measured_status()), 120, 40);

        let (cpu_x, cpu_y) = find(&rows, "CPU");
        let (memory_x, memory_y) = find(&rows, "Memory");
        assert_eq!(cpu_y, memory_y);
        assert!(cpu_x < memory_x);
        for panel in ["Storage", "Power", "Top by CPU", "Network"] {
            find(&rows, panel);
        }
    }

    #[test]
    fn very_wide_windows_show_the_panels_in_three_columns() {
        let rows = render(&busy_state(), 205, 60);

        let (cpu_x, cpu_y) = find(&rows, "CPU");
        let (memory_x, memory_y) = find(&rows, "Memory");
        let (power_x, power_y) = find(&rows, "Power");
        assert_eq!((cpu_y, cpu_y), (memory_y, power_y));
        assert!(cpu_x < memory_x && memory_x < power_x);
    }

    #[test]
    fn panels_keep_their_column_all_the_way_down() {
        let rows = render(&busy_state(), 205, 60);

        assert_eq!(find(&rows, "╭─ CPU").0, find(&rows, "╭─ Network").0);
        assert_eq!(find(&rows, "╭─ Memory").0, find(&rows, "╭─ Storage").0);
        assert_eq!(find(&rows, "╭─ Power").0, find(&rows, "╭─ Top by CPU").0);
    }

    #[test]
    fn every_panel_sits_in_a_frame() {
        let state = busy_state();

        for (width, height) in [(205, 85), (120, 40), (80, 90)] {
            let rows = render(&state, width, height);
            for title in ["CPU", "Memory", "Storage", "Network", "Power", "Top by CPU"] {
                find(&rows, &format!("╭─ {title}"));
            }
            let (tops, bottoms) = (all(&rows, "╭").len(), all(&rows, "╰").len());
            assert_eq!(tops, bottoms, "{width}x{height}");
        }
    }

    #[test]
    fn panels_are_tinted_by_what_they_show() {
        let theme = Theme::with_env(UiEnv::default());
        let colors = [
            ("CPU", theme.p.accent),
            ("Memory", theme.p.purple),
            ("Storage", theme.p.orange),
            ("Network", theme.p.teal),
            ("Power", theme.p.green),
        ];
        let mut terminal = Terminal::new(TestBackend::new(205, 60)).unwrap();
        let state = busy_state();
        terminal
            .draw(|f| render_system_status(f, &state, &theme))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let rows = render(&state, 205, 60);

        for (title, color) in colors {
            let (x, y) = find(&rows, &format!("─ {title}"));
            assert_eq!(buffer[(x as u16 + 2, y as u16)].fg, color, "{title}");
        }
    }

    #[test]
    fn cpu_and_memory_history_start_on_the_same_row() {
        let state = busy_state();

        for (width, height) in LAYOUTS {
            let rows = render(&state, width, height);
            let history = all(&rows, "History");
            assert_eq!(history.len(), 2, "{width}x{height}");
            assert_eq!(history[0].1, history[1].1, "{width}x{height}");
        }
    }

    #[test]
    fn network_and_storage_face_each_other() {
        let state = busy_state();

        for (width, height) in LAYOUTS {
            let rows = render(&state, width, height);
            let at = |text| find(&rows, text).1;
            assert_eq!(at("╭─ Network"), at("╭─ Storage"), "{width}x{height}");
            assert_eq!(at("Down"), at("Read"), "{width}x{height}");
            assert_eq!(at("Up  "), at("Write"), "{width}x{height}");
        }
    }

    #[test]
    fn extra_volumes_keep_network_and_storage_facing_each_other() {
        let mut state = busy_state();
        state.status.live.volumes = (0..3)
            .map(|i| VolumeStatus {
                name: format!("Backup {i}"),
                total_bytes: 2 * GB,
                available_bytes: GB,
            })
            .collect();

        let rows = render(&state, 205, 60);

        let at = |text| find(&rows, text).1;
        assert_eq!(at("Down"), at("Read"));
        assert_eq!(at("Up  "), at("Write"));
    }

    #[test]
    fn charts_side_by_side_have_the_same_height() {
        let rows = render(&busy_state(), 205, 85);
        let at = |text| find(&rows, text).1;

        assert_eq!(at("Up  ") - at("Down"), at("Write") - at("Read"));
        assert!(at("Up  ") - at("Down") > 5, "{}", rows.join("\n"));
    }

    #[test]
    fn all_columns_end_on_the_same_row() {
        let state = busy_state();

        for (width, height) in LAYOUTS {
            let rows = render(&state, width, height);
            let bottoms = all(&rows, "╰");
            let last = bottoms.iter().map(|(_, y)| *y).max().unwrap();
            let columns: Vec<_> = bottoms.iter().filter(|(_, y)| *y == last).collect();
            let wanted = if width >= 175 { 3 } else { 2 };
            assert_eq!(
                columns.len(),
                wanted,
                "{width}x{height}\n{}",
                rows.join("\n")
            );
        }
    }

    #[test]
    fn the_cores_stay_on_one_row_above_the_history() {
        let rows = render(&busy_state(), 205, 85);

        let (_, cores) = find(&rows, "Cores");
        let (_, history) = find(&rows, "History");
        assert!(history > cores, "{}", rows.join("\n"));
        assert!(history - cores <= 2, "{}", rows.join("\n"));
        assert!(rows[cores].contains('▇'), "{}", rows[cores]);
    }

    fn with_processes(busy: usize) -> SystemStatusState {
        let mut state = busy_state();
        state.status.live.processes = (0..busy)
            .map(|i| process(format!("cpu-{i}"), 10.0))
            .collect();
        state
    }

    #[test]
    fn a_process_list_keeps_its_size_however_few_processes_run() {
        for (width, height) in LAYOUTS {
            let top_of_memory = |busy| {
                let rows = render(&with_processes(busy), width, height);
                find(&rows, "Top by memory")
            };
            assert_eq!(top_of_memory(40), top_of_memory(2), "{width}x{height}");
            assert_eq!(top_of_memory(40), top_of_memory(0), "{width}x{height}");
        }
    }

    #[test]
    fn the_two_process_lists_share_the_column_evenly() {
        let rows = render(&busy_state(), 205, 85);

        let shown = |text: &str| all(&rows, text).len();
        let difference = shown("cpu-").abs_diff(shown("ram-"));
        assert!(difference <= 1, "{}", rows.join("\n"));
    }

    #[test]
    fn the_layout_stays_put_when_the_measurements_arrive() {
        let mut waiting = SystemStatusState::with_status(measuring_status());
        waiting.status.live.power = measured_status().live.power;

        for (width, height) in LAYOUTS {
            let first = render(&waiting, width, height);
            let later = render(&busy_state(), width, height);
            for title in ["╭─ CPU", "╭─ Network", "╭─ Storage", "╭─ Top by memory"]
            {
                assert_eq!(find(&first, title), find(&later, title), "{title}");
            }
        }
    }

    #[test]
    fn charts_grow_with_the_height_of_the_window() {
        let chart_rows = |height| {
            let rows = render(&busy_state(), 205, height);
            rows.iter().filter(|row| row.contains('█')).count()
        };

        assert!(chart_rows(85) > chart_rows(45), "{}", chart_rows(85));
    }

    #[test]
    fn short_windows_keep_each_graph_on_one_row() {
        let rows = render(&busy_state(), 120, 24);

        let (_, y) = find(&rows, "History");
        assert!(rows[y].contains('▄'), "{}", rows[y]);
    }

    #[test]
    fn the_dashboard_fits_the_window_without_scrolling() {
        let state = busy_state();

        for (width, height) in [(205, 85), (205, 60), (205, 45), (140, 40), (120, 40)] {
            render(&state, width, height);
            assert_eq!(state.max_scroll.get(), 0, "{width}x{height}");
        }
    }

    #[test]
    fn a_mac_without_a_battery_fills_a_large_window_too() {
        let mut state = busy_state();
        state.status.live.power = None;

        let rows = render(&state, 205, 85);

        assert_eq!(state.max_scroll.get(), 0);
        find(&rows, "Top by memory");
    }

    #[test]
    fn a_tiny_window_still_draws() {
        let rows = render(&busy_state(), 30, 10);

        find(&rows, "System status");
    }

    #[test]
    fn a_tall_window_lists_more_processes() {
        let screen = render(&busy_state(), 205, 85).join("\n");

        assert!(screen.matches("cpu-").count() > 20, "{screen}");
    }

    #[test]
    fn a_tall_window_also_lists_top_by_memory() {
        let tall = render(&busy_state(), 205, 85).join("\n");
        let short = render(&busy_state(), 120, 24).join("\n");

        assert!(tall.contains("Top by memory"), "{tall}");
        assert!(tall.contains("ram-0"), "{tall}");
        assert!(!short.contains("Top by memory"), "{short}");
    }

    #[test]
    fn idle_processes_stay_out_of_top_by_cpu() {
        let mut status = measured_status();
        status.live.processes.push(process("idle".to_string(), 0.0));

        let screen = render(&SystemStatusState::with_status(status), 120, 40).join("\n");

        assert!(!screen.contains("idle"), "{screen}");
    }

    #[test]
    fn a_chart_stays_inside_its_frame() {
        let rows = render(&busy_state(), 205, 60);

        let memory_x = find(&rows, "╭─ Memory").0;
        let right_border = memory_x - COLUMN_GAP - 1;
        let (_, history_y) = find(&rows, "History");
        for y in history_y..history_y + 4 {
            assert_eq!(cell(&rows, right_border, y), Some('│'), "{}", rows[y]);
        }
    }

    #[test]
    fn narrow_windows_stack_the_panels() {
        let rows = render(&SystemStatusState::with_status(measured_status()), 80, 60);

        let (cpu_x, cpu_y) = find(&rows, "CPU");
        let (memory_x, memory_y) = find(&rows, "Memory");
        assert_eq!(cpu_x, memory_x);
        assert!(cpu_y < memory_y);
    }

    #[test]
    fn a_mac_without_a_battery_has_no_power_panel() {
        let mut status = measured_status();
        status.live.power = None;

        let rows = render(&SystemStatusState::with_status(status), 120, 40);

        assert!(!rows.join("\n").contains("Power"));
        find(&rows, "Network");
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
    fn the_scroll_hint_shows_only_when_there_is_something_to_scroll() {
        let toolbar =
            |state: &SystemStatusState, width, height| render(state, width, height).pop().unwrap();

        assert!(!toolbar(&busy_state(), 120, 40).contains("Scroll"));

        let small_window = SystemStatusState::with_status(measured_status());
        assert!(toolbar(&small_window, 80, 24).contains("Scroll"));
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
    fn history_tracks_the_used_share_of_memory() {
        let mut state = SystemStatusState::with_status(measuring_status());

        state.apply(measured_status());

        assert_eq!(state.history.memory, [62.5]);
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

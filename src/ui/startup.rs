//! Startup Items Screen.
//! Review and switch off login and background items.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use super::components::button::Button;
use super::components::chrome::AppChrome;
use super::components::empty::EmptyState;
use super::components::frame::{FRAME_ROWS, FRAME_SIDES, card};
use super::components::hints::Hint;
use super::components::list::{Column, ListRow};
use super::components::modal::{Modal, dim_backdrop};
use super::theme::Theme;
use crate::core::bytes::format_bytes;
use crate::core::path_utils::prettify_path;
use crate::core::startup::{Btm, StartupItem, StartupKind, StartupScan, StartupState};

const SPLIT_MIN_WIDTH: u16 = 100;
const MAX_NOTE_ROWS: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupConfirm {
    Toggle { index: usize, enable: bool },
    Stop { index: usize },
    RemoveBroken { index: usize },
}

pub struct StartupScreenState {
    pub items: Vec<StartupItem>,
    pub selected_index: usize,
    pub include_all: bool,
    pub confirm: Option<StartupConfirm>,
    pub confirm_button: usize,
    pub message: Option<(String, bool)>,
    pub notes: Vec<String>,
    /// The Background Task Management records read for this screen, reused after each change.
    pub btm: Btm,
}

impl StartupScreenState {
    pub fn new(items: Vec<StartupItem>, include_all: bool) -> Self {
        Self {
            items,
            selected_index: 0,
            include_all,
            confirm: None,
            confirm_button: 0,
            message: None,
            notes: Vec::new(),
            btm: Btm::Skipped,
        }
    }

    pub fn apply_scan(&mut self, scan: StartupScan) {
        self.items = scan.items;
        self.notes = scan.notes;
        self.selected_index = self.selected_index.min(self.items.len().saturating_sub(1));
    }

    pub fn finish_action(&mut self, result: Result<String, String>) {
        self.message = Some(match result {
            Ok(msg) => (msg, false),
            Err(err) => (err, true),
        });
    }

    pub fn selected_item(&self) -> Option<&StartupItem> {
        self.items.get(self.selected_index)
    }

    pub fn next(&mut self) {
        if self.selected_index + 1 < self.items.len() {
            self.selected_index += 1;
        }
    }

    pub fn previous(&mut self) {
        if self.selected_index > 0 {
            self.selected_index -= 1;
        }
    }

    pub fn page_down(&mut self) {
        let count = self.items.len();
        if count > 0 {
            self.selected_index = (self.selected_index + 10).min(count - 1);
        }
    }

    pub fn page_up(&mut self) {
        self.selected_index = self.selected_index.saturating_sub(10);
    }

    pub fn move_to_start(&mut self) {
        self.selected_index = 0;
    }

    pub fn move_to_end(&mut self) {
        if !self.items.is_empty() {
            self.selected_index = self.items.len() - 1;
        }
    }

    pub fn toggle_confirm_button(&mut self) {
        self.confirm_button = 1 - self.confirm_button;
    }

    pub fn cancel_confirm(&mut self) {
        self.confirm = None;
    }

    pub fn request_toggle(&mut self) {
        if self.confirm.is_some() || self.items.is_empty() {
            return;
        }
        let Some(item) = self.items.get(self.selected_index) else {
            return;
        };

        let enable = match item.state {
            StartupState::Broken => return,
            StartupState::Disabled => true,
            StartupState::Running | StartupState::Enabled => false,
        };

        if let Some(reason) = item.read_only_reason() {
            self.message = Some((reason.to_string(), true));
            return;
        }

        self.confirm = Some(if item.is_disabled_but_running() {
            StartupConfirm::Stop {
                index: self.selected_index,
            }
        } else {
            StartupConfirm::Toggle {
                index: self.selected_index,
                enable,
            }
        });
        self.confirm_button = 0;
    }

    /// Asks to remove the plist of the selected item when it is broken. Other items stay.
    pub fn request_remove(&mut self) {
        if self.confirm.is_some() {
            return;
        }
        let Some(item) = self.selected_item() else {
            return;
        };
        if item.state != StartupState::Broken {
            return;
        }

        if let Some(reason) = item.read_only_reason() {
            self.message = Some((reason.to_string(), true));
        } else if item.kind == StartupKind::UserAgent && item.plist_path.is_some() {
            self.confirm = Some(StartupConfirm::RemoveBroken {
                index: self.selected_index,
            });
            self.confirm_button = 0;
        } else {
            self.message = Some((
                "System-level broken plists cannot be removed without root privileges".to_string(),
                true,
            ));
        }
    }
}

pub fn render_startup(frame: &mut Frame, state: &StartupScreenState, theme: &Theme) {
    let area = frame.area();

    let crumbs = vec!["taert", "Optimize", "Startup items"];
    let status_text = format!("{} items", state.items.len());

    let hints = if state.confirm.is_some() {
        vec![
            Hint::new(theme.g.key_enter, "Confirm"),
            Hint::new(theme.g.key_tab, "Switch button"),
            Hint::new(theme.g.key_esc, "Cancel"),
        ]
    } else {
        let mut hints = Vec::new();
        if state
            .selected_item()
            .is_some_and(|item| item.state == StartupState::Broken)
        {
            hints.push(Hint::new("d", "Remove broken item"));
        } else {
            hints.push(Hint::new(theme.g.key_space, "Toggle / Action"));
        }
        hints.push(Hint::new(theme.g.key_up_down, "Navigate"));
        hints.push(Hint::new(theme.g.key_esc, "Back"));
        hints
    };

    let chrome = AppChrome::new(crumbs)
        .full_width()
        .status(Line::from(status_text))
        .hints(hints)
        .behind_dialog(state.confirm.is_some());

    let inner = chrome.render(frame, theme);
    let notes_height = state.notes.len().min(MAX_NOTE_ROWS) as u16;
    let [inner, notes_area] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(notes_height.min(inner.height.saturating_sub(1))),
    ])
    .areas(inner);
    render_notes(frame, notes_area, &state.notes, theme);

    if state.items.is_empty() {
        EmptyState::new(
            theme.g.icon_startup,
            "No startup items",
            "No launch agents, daemons, or login items found.",
        )
        .render(frame, inner, theme);
        return;
    }

    let is_split = inner.width >= SPLIT_MIN_WIDTH;
    if is_split {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(58),
                Constraint::Length(2),
                Constraint::Min(40),
            ])
            .split(inner);

        render_item_list(frame, chunks[0], state, theme);
        render_item_details(frame, chunks[2], state, theme);
    } else {
        render_item_list(frame, inner, state, theme);
    }

    // Confirmation Modal
    if let Some(ref confirm) = state.confirm {
        render_confirm_modal(frame, area, state, confirm, theme);
    }
}

fn render_notes(frame: &mut Frame, area: Rect, notes: &[String], theme: &Theme) {
    let width = area.width as usize;
    let lines: Vec<Line> = notes
        .iter()
        .take(area.height as usize)
        .map(|note| {
            let text = format!("Note: {note}");
            let text = if text.chars().count() > width {
                let cut: String = text.chars().take(width.saturating_sub(1)).collect();
                format!("{cut}…")
            } else {
                text
            };
            Line::from(Span::styled(text, theme.warning()))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_item_list(frame: &mut Frame, area: Rect, state: &StartupScreenState, theme: &Theme) {
    let frame_w = area.width as usize;
    let row_width = frame_w.saturating_sub(FRAME_SIDES);
    let frame_rows = (area.height as usize).saturating_sub(FRAME_ROWS);
    let color = theme.p.orange;

    let columns = StartupColumns::of(&state.items);
    let item_rows = frame_rows.saturating_sub(1);
    let mut lines = vec![titles_line(&columns, row_width, theme)];

    let scroll_offset = if state.items.len() > item_rows && item_rows > 0 {
        let half = item_rows / 2;
        let max_offset = state.items.len().saturating_sub(item_rows);
        state.selected_index.saturating_sub(half).min(max_offset)
    } else {
        0
    };

    lines.extend(
        state
            .items
            .iter()
            .enumerate()
            .skip(scroll_offset)
            .take(item_rows)
            .map(|(idx, item)| {
                item_line(
                    item,
                    &columns,
                    idx == state.selected_index,
                    row_width,
                    theme,
                )
            }),
    );

    let card_title = format!("Startup items ({})", state.items.len());
    let card_widget = card(
        &card_title,
        Vec::new(),
        lines,
        frame_w,
        frame_rows,
        color,
        theme,
    );
    frame.render_widget(Paragraph::new(card_widget), area);
}

/// The least width of the name, in cells, before a column hides.
const NAME_MIN_W: usize = 20;
/// Selection bar, symbol, and the gap before the columns.
const ROW_MARGIN_W: usize = 5;
const COLUMN_GAP_W: usize = 2;
const TYPE_TITLE: &str = "TYPE";
const STATE_TITLE: &str = "STATE";
const CPU_TITLE: &str = "CPU";
const MEMORY_TITLE: &str = "MEMORY";

fn state_label(item: &StartupItem) -> &'static str {
    match item.state {
        StartupState::Disabled if item.is_disabled_but_running() => "disabled · still running",
        state => state.as_str(),
    }
}

fn state_style(item: &StartupItem, theme: &Theme) -> Style {
    match item.state {
        StartupState::Running => theme.success(),
        StartupState::Enabled => theme.accent_text(),
        StartupState::Disabled if item.is_disabled_but_running() => theme.warning(),
        StartupState::Disabled => theme.footnote(),
        StartupState::Broken => theme.danger(),
    }
}

fn cpu_text(item: &StartupItem) -> String {
    item.cpu_percent
        .map_or_else(String::new, |cpu| format!("{cpu:.1}%"))
}

fn memory_text(item: &StartupItem) -> String {
    item.memory_bytes.map_or_else(String::new, format_bytes)
}

/// The TYPE, STATE, CPU, and MEMORY columns, one width for every row.
struct StartupColumns {
    type_w: usize,
    state_w: usize,
    cpu_w: usize,
    memory_w: usize,
}

impl StartupColumns {
    fn of(items: &[StartupItem]) -> Self {
        let widest = |title: &str, cell: &dyn Fn(&StartupItem) -> usize| {
            items.iter().map(cell).fold(title.width(), usize::max)
        };
        Self {
            type_w: widest(TYPE_TITLE, &|item| item.kind.as_str().width()),
            state_w: widest(STATE_TITLE, &|item| state_label(item).width()),
            cpu_w: widest(CPU_TITLE, &|item| cpu_text(item).width()),
            memory_w: widest(MEMORY_TITLE, &|item| memory_text(item).width()),
        }
    }

    fn numbers_min_row_width(&self) -> usize {
        ROW_MARGIN_W
            + NAME_MIN_W
            + [self.state_w, self.cpu_w, self.memory_w]
                .iter()
                .map(|w| w + COLUMN_GAP_W)
                .sum::<usize>()
    }

    fn type_min_row_width(&self) -> usize {
        self.numbers_min_row_width() + self.type_w + COLUMN_GAP_W
    }

    fn columns(&self, cells: [Span<'static>; 4]) -> Vec<Column<'static>> {
        let [kind, state, cpu, memory] = cells;
        let numbers = self.numbers_min_row_width();
        vec![
            Column::new(kind, self.type_w, self.type_min_row_width()),
            Column::new(state, self.state_w, 0),
            Column::new(right_aligned(cpu, self.cpu_w), self.cpu_w, numbers),
            Column::new(right_aligned(memory, self.memory_w), self.memory_w, numbers),
        ]
    }

    fn titles(&self, theme: &Theme) -> Vec<Column<'static>> {
        let title = |text| Span::styled(text, theme.footnote());
        self.columns([
            title(TYPE_TITLE),
            title(STATE_TITLE),
            title(CPU_TITLE),
            title(MEMORY_TITLE),
        ])
    }

    fn cells(&self, item: &StartupItem, theme: &Theme) -> Vec<Column<'static>> {
        self.columns([
            Span::styled(item.kind.as_str(), theme.footnote()),
            Span::styled(state_label(item), state_style(item, theme)),
            Span::styled(cpu_text(item), theme.body()),
            Span::styled(memory_text(item), theme.body()),
        ])
    }
}

fn right_aligned(span: Span<'static>, width: usize) -> Span<'static> {
    let text = format!("{:>width$}", span.content);
    Span::styled(text, span.style)
}

fn titles_line(columns: &StartupColumns, width: usize, theme: &Theme) -> Line<'static> {
    ListRow::new(vec![Span::styled("NAME", theme.footnote())])
        .leading(Span::raw(" "))
        .columns(columns.titles(theme))
        .render_line(width, theme)
}

fn item_line(
    item: &StartupItem,
    columns: &StartupColumns,
    selected: bool,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let symbol = match item.state {
        StartupState::Running => theme.g.bullet,
        StartupState::Enabled => theme.g.check_off,
        StartupState::Disabled => theme.g.file,
        StartupState::Broken => theme.g.error,
    };

    let mut title = vec![Span::styled(item.name.clone(), theme.body())];
    if let Some(app) = &item.app_name
        && app != &item.name
    {
        title.push(Span::raw(" "));
        title.push(Span::styled(format!("({app})"), theme.footnote()));
    }

    ListRow::new(title)
        .leading(Span::styled(symbol, state_style(item, theme)))
        .columns(columns.cells(item, theme))
        .selected(selected)
        .render_line(width, theme)
}

fn render_item_details(frame: &mut Frame, area: Rect, state: &StartupScreenState, theme: &Theme) {
    let frame_w = area.width as usize;
    let frame_rows = (area.height as usize).saturating_sub(FRAME_ROWS);
    let color = theme.p.teal;

    let Some(item) = state.selected_item() else {
        return;
    };

    let mut lines: Vec<Line<'static>> = Vec::new();

    // Name & Label
    lines.push(Line::from(vec![
        Span::styled("Name:    ", theme.callout().add_modifier(Modifier::BOLD)),
        Span::styled(item.name.clone(), theme.body()),
    ]));
    lines.push(Line::from(vec![
        Span::styled("Label:   ", theme.callout().add_modifier(Modifier::BOLD)),
        Span::styled(item.label.clone(), theme.footnote()),
    ]));

    // Owning App
    if let Some(ref app) = item.app_name {
        lines.push(Line::from(vec![
            Span::styled("App:     ", theme.callout().add_modifier(Modifier::BOLD)),
            Span::styled(app.clone(), theme.body()),
        ]));
    }
    if let Some(ref bundle) = item.app_bundle {
        lines.push(Line::from(vec![
            Span::styled("Bundle:  ", theme.callout().add_modifier(Modifier::BOLD)),
            Span::styled(prettify_path(bundle), theme.footnote()),
        ]));
    }

    // Kind & State
    lines.push(Line::from(vec![
        Span::styled("Kind:    ", theme.callout().add_modifier(Modifier::BOLD)),
        Span::styled(item.kind.as_str().to_string(), theme.body()),
    ]));

    let (state_text, state_style): (&str, _) = match item.state {
        StartupState::Running => ("Running", theme.success()),
        StartupState::Enabled => ("Enabled", theme.callout()),
        StartupState::Disabled if item.is_disabled_but_running() => {
            ("Disabled, still running", theme.warning())
        }
        StartupState::Disabled => ("Disabled", theme.footnote()),
        StartupState::Broken => ("Broken (missing executable)", theme.danger()),
    };
    lines.push(Line::from(vec![
        Span::styled("State:   ", theme.callout().add_modifier(Modifier::BOLD)),
        Span::styled(state_text.to_string(), state_style),
    ]));

    // Program Executable
    if let Some(ref prog) = item.program {
        lines.push(Line::from(vec![
            Span::styled("Program: ", theme.callout().add_modifier(Modifier::BOLD)),
            Span::styled(prettify_path(prog), theme.footnote()),
        ]));
    }

    // Plist path
    if let Some(ref plist) = item.plist_path {
        lines.push(Line::from(vec![
            Span::styled("Plist:   ", theme.callout().add_modifier(Modifier::BOLD)),
            Span::styled(prettify_path(plist), theme.footnote()),
        ]));
    }

    // Live Metrics
    if let Some(pid) = item.pid {
        let cpu_str = item
            .cpu_percent
            .map(|c| format!("{c:.1}%"))
            .unwrap_or_else(|| "-".to_string());
        let mem_str = item
            .memory_bytes
            .map(format_bytes)
            .unwrap_or_else(|| "-".to_string());
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("PID:     ", theme.callout().add_modifier(Modifier::BOLD)),
            Span::styled(format!("{pid}"), theme.body()),
            Span::styled("  CPU: ", theme.callout().add_modifier(Modifier::BOLD)),
            Span::styled(cpu_str, theme.body()),
            Span::styled("  Memory: ", theme.callout().add_modifier(Modifier::BOLD)),
            Span::styled(mem_str, theme.body()),
        ]));
    }

    // Protection note
    if item.read_only_reason().is_some() {
        lines.push(Line::from(""));
        let (note_text, note_style) = if item.is_apple {
            ("Apple system service · Read-only", theme.footnote())
        } else if item.is_security {
            ("Security vendor service · Protected", theme.warning())
        } else if item.is_ignored {
            ("Protected path · Excluded by rules", theme.footnote())
        } else if item.kind == StartupKind::Background {
            ("Background task entry · Read-only", theme.footnote())
        } else {
            ("Registered by the app · Read-only", theme.footnote())
        };
        lines.push(Line::from(Span::styled(note_text, note_style)));
    }

    // Message notification if any
    if let Some((ref msg, is_err)) = state.message {
        lines.push(Line::from(""));
        let msg_style = if is_err {
            theme.danger()
        } else {
            theme.success()
        };
        lines.push(Line::from(Span::styled(
            format!("Notice: {msg}"),
            msg_style,
        )));
    }

    // Pad lines to frame_rows
    while lines.len() < frame_rows {
        lines.push(Line::from(""));
    }

    let card_widget = card(
        "Service detail",
        Vec::new(),
        lines,
        frame_w,
        frame_rows,
        color,
        theme,
    );
    frame.render_widget(Paragraph::new(card_widget), area);
}

fn render_confirm_modal(
    frame: &mut Frame,
    area: Rect,
    state: &StartupScreenState,
    confirm: &StartupConfirm,
    theme: &Theme,
) {
    dim_backdrop(frame.buffer_mut(), area);

    match confirm {
        StartupConfirm::Toggle { index, enable } => {
            let Some(item) = state.items.get(*index) else {
                return;
            };

            let (title, action_name, tint) = if *enable {
                ("Enable Startup Item", "Enable", theme.p.teal)
            } else {
                ("Disable Startup Item", "Disable", theme.p.orange)
            };

            let app_desc = item.app_name.as_deref().unwrap_or(item.name.as_str());

            let mut message = vec![
                Line::from(vec![
                    Span::styled(
                        if *enable { "Enable " } else { "Disable " },
                        theme.body().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("\"{}\"", item.name), theme.accent_text()),
                    Span::styled("?", theme.body().add_modifier(Modifier::BOLD)),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("Owning app: ", theme.callout()),
                    Span::styled(app_desc, theme.body()),
                ]),
                Line::from(vec![
                    Span::styled("Service:    ", theme.callout()),
                    Span::styled(item.label.clone(), theme.footnote()),
                ]),
                Line::from(""),
            ];

            if *enable {
                message.push(Line::from(Span::styled(
                    "This will enable the service to start automatically at login or boot.",
                    theme.callout(),
                )));
            } else {
                message.push(Line::from(Span::styled(
                    "This will stop any active process and prevent it from starting automatically.",
                    theme.callout(),
                )));
            }

            let buttons = vec![
                Button::primary(action_name).focused(state.confirm_button == 0),
                Button::new("Cancel").focused(state.confirm_button == 1),
            ];

            Modal::new(title)
                .tint(tint)
                .message(message)
                .buttons(buttons)
                .hints(Line::from(vec![
                    Span::styled(theme.g.key_enter, theme.accent_text()),
                    Span::styled(" Confirm  ", theme.callout()),
                    Span::styled(theme.g.key_esc, theme.accent_text()),
                    Span::styled(" Cancel", theme.callout()),
                ]))
                .render(frame, area, theme);
        }

        StartupConfirm::Stop { index } => {
            let Some(item) = state.items.get(*index) else {
                return;
            };

            let message = vec![
                Line::from(vec![
                    Span::styled("Stop ", theme.body().add_modifier(Modifier::BOLD)),
                    Span::styled(format!("\"{}\"", item.name), theme.accent_text()),
                    Span::styled("?", theme.body().add_modifier(Modifier::BOLD)),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("Service:    ", theme.callout()),
                    Span::styled(item.label.clone(), theme.footnote()),
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    "It is switched off but still running. This stops the running process.",
                    theme.callout(),
                )),
            ];

            let buttons = vec![
                Button::primary("Stop").focused(state.confirm_button == 0),
                Button::new("Cancel").focused(state.confirm_button == 1),
            ];

            Modal::new("Stop Startup Item")
                .tint(theme.p.orange)
                .message(message)
                .buttons(buttons)
                .hints(Line::from(vec![
                    Span::styled(theme.g.key_enter, theme.accent_text()),
                    Span::styled(" Confirm  ", theme.callout()),
                    Span::styled(theme.g.key_esc, theme.accent_text()),
                    Span::styled(" Cancel", theme.callout()),
                ]))
                .render(frame, area, theme);
        }

        StartupConfirm::RemoveBroken { index } => {
            let Some(item) = state.items.get(*index) else {
                return;
            };

            let plist_path_str = item
                .plist_path
                .as_ref()
                .map(|p| prettify_path(p))
                .unwrap_or_default();

            let message = vec![
                Line::from(vec![
                    Span::styled(
                        "Remove broken plist for ",
                        theme.body().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("\"{}\"", item.name), theme.accent_text()),
                    Span::styled("?", theme.body().add_modifier(Modifier::BOLD)),
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    "The program executable does not exist on disk.",
                    theme.callout(),
                )),
                Line::from(vec![
                    Span::styled("Plist: ", theme.callout()),
                    Span::styled(plist_path_str, theme.footnote()),
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    "This will permanently delete the plist file from ~/Library/LaunchAgents.",
                    theme.warning(),
                )),
            ];

            let buttons = vec![
                Button::destructive("Remove").focused(state.confirm_button == 0),
                Button::new("Cancel").focused(state.confirm_button == 1),
            ];

            Modal::new("Remove Broken Startup Item")
                .tint(theme.p.red)
                .message(message)
                .buttons(buttons)
                .hints(Line::from(vec![
                    Span::styled(theme.g.key_enter, theme.accent_text()),
                    Span::styled(" Confirm  ", theme.callout()),
                    Span::styled(theme.g.key_esc, theme.accent_text()),
                    Span::styled(" Cancel", theme.callout()),
                ]))
                .render(frame, area, theme);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::env::UiEnv;

    fn item(kind: StartupKind, state: StartupState, pid: Option<u32>) -> StartupItem {
        StartupItem {
            label: "com.example.item".to_string(),
            name: "Item".to_string(),
            app_name: None,
            app_bundle: None,
            kind,
            state,
            program: None,
            plist_path: Some("/Library/LaunchDaemons/com.example.item.plist".into()),
            pid,
            cpu_percent: None,
            memory_bytes: None,
            is_apple: false,
            is_security: false,
            is_ignored: false,
        }
    }

    fn named(name: &str, kind: StartupKind, state: StartupState) -> StartupItem {
        let mut it = item(kind, state, None);
        it.name = name.to_string();
        it
    }

    fn running(name: &str) -> StartupItem {
        let mut it = named(name, StartupKind::UserAgent, StartupState::Running);
        it.pid = Some(7);
        it.cpu_percent = Some(1.04);
        it.memory_bytes = Some(48_660_000);
        it
    }

    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn cell_of(line: &Line, needle: &str) -> usize {
        let t = text(line);
        t[..t.find(needle).unwrap()].chars().count()
    }

    fn style_of(line: &Line, needle: &str) -> ratatui::style::Style {
        line.spans
            .iter()
            .find(|s| s.content.trim() == needle)
            .unwrap_or_else(|| panic!("no span {needle:?} in {line:?}"))
            .style
    }

    fn line_of(it: &StartupItem, width: usize) -> Line<'static> {
        let theme = Theme::with_env(UiEnv::default());
        let columns = StartupColumns::of(std::slice::from_ref(it));
        item_line(it, &columns, false, width, &theme)
    }

    #[test]
    fn each_state_draws_its_text_and_color_in_the_state_column() {
        let theme = Theme::with_env(UiEnv::default());
        let mut stuck = named("Stuck", StartupKind::Daemon, StartupState::Disabled);
        stuck.pid = Some(3);
        let cases = [
            (running("A"), "running", theme.success()),
            (
                named("B", StartupKind::UserAgent, StartupState::Enabled),
                "enabled",
                theme.accent_text(),
            ),
            (
                named("C", StartupKind::UserAgent, StartupState::Disabled),
                "disabled",
                theme.footnote(),
            ),
            (stuck, "disabled · still running", theme.warning()),
            (
                named("D", StartupKind::UserAgent, StartupState::Broken),
                "broken",
                theme.danger(),
            ),
        ];
        for (it, label, style) in cases {
            let line = line_of(&it, 120);
            assert!(text(&line).contains(label), "{label}");
            assert_eq!(style_of(&line, label), style, "{label}");
            assert_eq!(line.spans[1].style, style, "symbol of {label}");
        }
    }

    #[test]
    fn an_enabled_row_is_accent_not_callout() {
        let theme = Theme::with_env(UiEnv::default());
        let it = named("B", StartupKind::UserAgent, StartupState::Enabled);

        let line = line_of(&it, 120);

        assert_eq!(style_of(&line, "enabled"), theme.accent_text());
        assert_eq!(line.spans[1].style, theme.accent_text());
        assert_ne!(theme.accent_text(), theme.callout());
    }

    #[test]
    fn a_running_row_shows_cpu_and_memory_in_their_own_columns() {
        let theme = Theme::with_env(UiEnv::default());
        let line = line_of(&running("A"), 120);

        assert_eq!(style_of(&line, "1.0%"), theme.body());
        assert_eq!(style_of(&line, "48.7 MB"), theme.body());
        assert_eq!(style_of(&line, "user agent"), theme.footnote());
        assert!(!text(&line).contains("running ("), "{}", text(&line));
    }

    #[test]
    fn a_running_row_without_numbers_has_empty_cpu_and_memory() {
        let mut it = running("A");
        it.cpu_percent = None;
        it.memory_bytes = None;

        let line = line_of(&it, 120);

        assert_eq!(cpu_text(&it), "");
        assert_eq!(memory_text(&it), "");
        assert!(!text(&line).contains('%'));
        assert!(!text(&line).contains(" MB"));
        assert_eq!(line.width(), 120);
    }

    #[test]
    fn type_and_state_start_in_the_same_cell_on_every_row() {
        let theme = Theme::with_env(UiEnv::default());
        let a = running("Docker");
        let b = named(
            "A much longer name",
            StartupKind::UserAgent,
            StartupState::Enabled,
        );
        let columns = StartupColumns::of(&[a.clone(), b.clone()]);

        let first = item_line(&a, &columns, false, 120, &theme);
        let second = item_line(&b, &columns, false, 120, &theme);
        let titles = titles_line(&columns, 120, &theme);

        assert_eq!(
            cell_of(&first, "user agent"),
            cell_of(&second, "user agent")
        );
        assert_eq!(cell_of(&first, "running"), cell_of(&titles, "STATE"));
        assert_eq!(cell_of(&second, "enabled"), cell_of(&titles, "STATE"));
        assert_eq!(cell_of(&first, "user agent"), cell_of(&titles, "TYPE"));
        let end = |l: &Line, t: &str| cell_of(l, t) + t.chars().count();
        assert_eq!(end(&first, "1.0%"), end(&titles, "CPU"));
        assert_eq!(end(&first, "48.7 MB"), end(&titles, "MEMORY"));
        assert_eq!(first.width(), second.width());
    }

    #[test]
    fn narrow_rows_hide_type_first_then_cpu_and_memory() {
        let theme = Theme::with_env(UiEnv::default());
        let mut long = running(&"N".repeat(40));
        long.kind = StartupKind::SystemAgent;
        let columns = StartupColumns::of(std::slice::from_ref(&long));
        let shown = |width: usize| {
            let row = text(&item_line(&long, &columns, false, width, &theme));
            let head = text(&titles_line(&columns, width, &theme));
            let row = (
                row.contains("system agent"),
                row.contains("1.0%") && row.contains("MB"),
                row.contains("running"),
            );
            let head = (
                head.contains("TYPE"),
                head.contains("CPU") && head.contains("MEMORY"),
                head.contains("STATE") && head.contains("NAME"),
            );
            assert_eq!(row, head, "{width}");
            row
        };

        assert_eq!(shown(200), (true, true, true));
        let widest = (20..200).find(|w| shown(*w).0).unwrap();
        assert_eq!(shown(widest - 1), (false, true, true));
        let with_numbers = (20..widest).find(|w| shown(*w).1).unwrap();
        assert_eq!(shown(with_numbers - 1), (false, false, true));
        assert_eq!(shown(20), (false, false, true));
        for width in [widest, with_numbers] {
            let line = item_line(&long, &columns, false, width, &theme);
            let name_cells = text(&line)
                .chars()
                .filter(|c| matches!(c, 'N' | '…'))
                .count();
            assert_eq!(name_cells, 20, "{width}: {}", text(&line));
        }
    }

    #[test]
    fn rows_and_titles_fill_their_width() {
        let theme = Theme::with_env(UiEnv::default());
        let it = running("A");
        let columns = StartupColumns::of(std::slice::from_ref(&it));
        for width in [20, 40, 60, 80, 120, 200] {
            assert_eq!(item_line(&it, &columns, true, width, &theme).width(), width);
            assert_eq!(titles_line(&columns, width, &theme).width(), width);
        }
    }

    #[test]
    fn the_focused_enabled_row_keeps_the_accent_on_the_selection_color() {
        let theme = Theme::with_env(UiEnv::default());
        let it = named("B", StartupKind::UserAgent, StartupState::Enabled);
        let columns = StartupColumns::of(std::slice::from_ref(&it));

        let line = item_line(&it, &columns, true, 120, &theme);

        let style = style_of(&line, "enabled");
        assert_eq!(style.fg, theme.accent_text().fg);
        assert_eq!(style.bg, Some(theme.p.selection));
    }

    #[test]
    fn a_long_list_keeps_the_titles_the_focused_row_and_the_bottom_border_in_view() {
        let items: Vec<_> = (0..30)
            .map(|i| {
                named(
                    &format!("Item{i:02}"),
                    StartupKind::UserAgent,
                    StartupState::Enabled,
                )
            })
            .collect();
        let mut state = StartupScreenState::new(items, false);
        state.selected_index = 10;
        let theme = Theme::with_env(UiEnv::default());
        let area = Rect::new(0, 0, 80, 12);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();

        terminal
            .draw(|f| render_item_list(f, area, &state, &theme))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..12)
            .map(|y| (0..80).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        assert!(rows[1].contains("NAME"), "{rows:#?}");
        assert!(rows.iter().any(|r| r.contains("Item10")), "{rows:#?}");
        let last = rows.last().unwrap();
        assert!(last.contains('╰'), "{rows:#?}");
    }

    #[test]
    fn a_disabled_item_that_still_runs_offers_stop() {
        let running = item(StartupKind::Daemon, StartupState::Disabled, Some(9));
        let mut state = StartupScreenState::new(vec![running], false);

        state.request_toggle();

        assert_eq!(state.confirm, Some(StartupConfirm::Stop { index: 0 }));
    }

    #[test]
    fn the_detail_card_says_an_app_registered_the_service() {
        let mut registered = item(StartupKind::Daemon, StartupState::Enabled, None);
        registered.plist_path = None;
        let state = StartupScreenState::new(vec![registered], false);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();

        terminal
            .draw(|f| render_startup(f, &state, &Theme::new()))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let text: String = buffer.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("Registered by the app · Read-only"), "{text}");
    }

    #[test]
    fn a_background_entry_cannot_be_toggled() {
        let entry = item(StartupKind::Background, StartupState::Enabled, None);
        let mut state = StartupScreenState::new(vec![entry], false);

        state.request_toggle();

        assert_eq!(state.confirm, None);
        assert!(state.message.is_some_and(|(_, is_error)| is_error));
    }
}

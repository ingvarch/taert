//! Startup Items Screen.
//! Review and switch off login and background items.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::components::button::Button;
use super::components::chrome::AppChrome;
use super::components::empty::EmptyState;
use super::components::frame::{FRAME_ROWS, FRAME_SIDES, card};
use super::components::hints::Hint;
use super::components::list::ListRow;
use super::components::modal::{Modal, dim_backdrop};
use super::theme::Theme;
use crate::core::bytes::format_bytes;
use crate::core::path_utils::prettify_path;
use crate::core::startup::{StartupItem, StartupKind, StartupState};

const SPLIT_MIN_WIDTH: u16 = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupConfirm {
    Toggle { index: usize, enable: bool },
    RemoveBroken { index: usize },
}

pub struct StartupScreenState {
    pub items: Vec<StartupItem>,
    pub selected_index: usize,
    pub include_all: bool,
    pub confirm: Option<StartupConfirm>,
    pub confirm_button: usize,
    pub message: Option<(String, bool)>,
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
        }
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

        if item.is_apple {
            self.message = Some(("Apple system services cannot be modified".to_string(), true));
            return;
        }
        if item.is_security {
            self.message = Some(("Security agent services are read-only".to_string(), true));
            return;
        }
        if item.is_ignored {
            self.message = Some(("Protected startup items are read-only".to_string(), true));
            return;
        }

        match item.state {
            StartupState::Running | StartupState::Enabled => {
                self.confirm = Some(StartupConfirm::Toggle {
                    index: self.selected_index,
                    enable: false,
                });
                self.confirm_button = 0;
            }
            StartupState::Disabled => {
                self.confirm = Some(StartupConfirm::Toggle {
                    index: self.selected_index,
                    enable: true,
                });
                self.confirm_button = 0;
            }
            StartupState::Broken => {
                if item.kind == StartupKind::UserAgent && item.plist_path.is_some() {
                    self.confirm = Some(StartupConfirm::RemoveBroken {
                        index: self.selected_index,
                    });
                    self.confirm_button = 0;
                } else {
                    self.message = Some((
                        "System-level broken plists cannot be removed without root privileges"
                            .to_string(),
                        true,
                    ));
                }
            }
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
        vec![
            Hint::new(theme.g.key_space, "Toggle / Action"),
            Hint::new(theme.g.key_up_down, "Navigate"),
            Hint::new(theme.g.key_esc, "Back"),
        ]
    };

    let chrome = AppChrome::new(crumbs)
        .full_width()
        .status(Line::from(status_text))
        .hints(hints)
        .behind_dialog(state.confirm.is_some());

    let inner = chrome.render(frame, theme);

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

fn render_item_list(frame: &mut Frame, area: Rect, state: &StartupScreenState, theme: &Theme) {
    let frame_w = area.width as usize;
    let row_width = frame_w.saturating_sub(FRAME_SIDES);
    let frame_rows = (area.height as usize).saturating_sub(FRAME_ROWS);
    let color = theme.p.orange;

    let mut lines = Vec::new();

    let scroll_offset = if state.items.len() > frame_rows && frame_rows > 0 {
        let half = frame_rows / 2;
        let max_offset = state.items.len().saturating_sub(frame_rows);
        state.selected_index.saturating_sub(half).min(max_offset)
    } else {
        0
    };

    for (idx, item) in state
        .items
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(frame_rows)
    {
        let is_selected = idx == state.selected_index;

        let (leading_symbol, leading_style) = match item.state {
            StartupState::Running => (theme.g.bullet, theme.success()),
            StartupState::Enabled => (theme.g.check_off, theme.callout()),
            StartupState::Disabled => (theme.g.file, theme.footnote()),
            StartupState::Broken => (theme.g.error, theme.danger()),
        };

        let mut title_spans = vec![Span::styled(item.name.clone(), theme.body())];
        if let Some(ref app) = item.app_name
            && app != &item.name
        {
            title_spans.push(Span::raw(" "));
            title_spans.push(Span::styled(format!("({app})"), theme.footnote()));
        }

        let detail_str = match item.state {
            StartupState::Running => {
                if let (Some(cpu), Some(mem)) = (item.cpu_percent, item.memory_bytes) {
                    format!("running ({cpu:.1}% · {})", format_bytes(mem))
                } else {
                    "running".to_string()
                }
            }
            StartupState::Enabled => "enabled".to_string(),
            StartupState::Disabled => "disabled".to_string(),
            StartupState::Broken => "broken".to_string(),
        };

        let detail_style = match item.state {
            StartupState::Running => theme.success(),
            StartupState::Enabled => theme.callout(),
            StartupState::Disabled => theme.footnote(),
            StartupState::Broken => theme.danger(),
        };

        let meta_str = item.kind.as_str();

        let row = ListRow::new(title_spans)
            .leading(Span::styled(leading_symbol, leading_style))
            .meta(Span::styled(meta_str, theme.footnote()))
            .detail(Span::styled(detail_str, detail_style))
            .selected(is_selected);

        lines.push(row.render_line(row_width, theme));
    }

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
    if item.is_apple || item.is_security || item.is_ignored {
        lines.push(Line::from(""));
        let (note_text, note_style) = if item.is_apple {
            ("Apple system service · Read-only", theme.footnote())
        } else if item.is_security {
            ("Security vendor service · Protected", theme.warning())
        } else {
            ("Protected path · Excluded by rules", theme.footnote())
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

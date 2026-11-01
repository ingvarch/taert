//! Doctor Screen.
//! Explains what is slowing the Mac down and offers actionable suggestions.

use std::sync::mpsc::Receiver;
use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::components::button::Button;
use super::components::chrome::AppChrome;
use super::components::frame::{FRAME_ROWS, FRAME_SIDES, card};
use super::components::hints::Hint;
use super::components::list::ListRow;
use super::components::modal::{Modal, dim_backdrop};
use super::theme::Theme;
use crate::core::doctor::{DoctorReport, Finding, FindingAction, FindingSeverity};
use crate::core::startup::StartupItem;

const SPLIT_MIN_WIDTH: u16 = 100;

#[derive(Debug, Clone)]
pub enum DoctorPhase {
    Checking {
        started: Instant,
        target_seconds: u64,
    },
    Done(DoctorReport),
}

#[derive(Debug, Clone, PartialEq)]
pub enum DoctorActionConfirm {
    QuitApp {
        name: String,
        bundle_id: Option<String>,
        pid: Option<u32>,
    },
    KillProcess {
        pid: u32,
        name: String,
    },
    DisableStartupItem {
        item: StartupItem,
    },
    ExploreStorage,
}

pub struct DoctorScreenState {
    pub seconds: u64,
    pub phase: DoctorPhase,
    pub selected_index: usize,
    pub confirm: Option<DoctorActionConfirm>,
    pub confirm_button: usize,
    pub message: Option<(String, bool)>,
    pub receiver: Option<Receiver<DoctorReport>>,
}

impl DoctorScreenState {
    pub fn new_checking(seconds: u64, receiver: Receiver<DoctorReport>) -> Self {
        Self {
            seconds,
            phase: DoctorPhase::Checking {
                started: Instant::now(),
                target_seconds: seconds,
            },
            selected_index: 0,
            confirm: None,
            confirm_button: 0,
            message: None,
            receiver: Some(receiver),
        }
    }

    pub fn new_done(report: DoctorReport) -> Self {
        let seconds = report.seconds;
        Self {
            seconds,
            phase: DoctorPhase::Done(report),
            selected_index: 0,
            confirm: None,
            confirm_button: 0,
            message: None,
            receiver: None,
        }
    }

    pub fn findings(&self) -> &[Finding] {
        match &self.phase {
            DoctorPhase::Done(report) => &report.findings,
            DoctorPhase::Checking { .. } => &[],
        }
    }

    pub fn selected_finding(&self) -> Option<&Finding> {
        self.findings().get(self.selected_index)
    }

    pub fn next(&mut self) {
        let count = self.findings().len();
        if count > 0 && self.selected_index + 1 < count {
            self.selected_index += 1;
        }
    }

    pub fn previous(&mut self) {
        if self.selected_index > 0 {
            self.selected_index -= 1;
        }
    }

    pub fn page_down(&mut self) {
        let count = self.findings().len();
        if count > 0 {
            self.selected_index = (self.selected_index + 5).min(count - 1);
        }
    }

    pub fn page_up(&mut self) {
        self.selected_index = self.selected_index.saturating_sub(5);
    }

    pub fn move_to_start(&mut self) {
        self.selected_index = 0;
    }

    pub fn move_to_end(&mut self) {
        let count = self.findings().len();
        if count > 0 {
            self.selected_index = count - 1;
        }
    }

    pub fn toggle_confirm_button(&mut self) {
        self.confirm_button = 1 - self.confirm_button;
    }

    pub fn cancel_confirm(&mut self) {
        self.confirm = None;
    }

    pub fn request_action(&mut self) {
        if self.confirm.is_some() {
            return;
        }
        let Some(target) = self
            .selected_finding()
            .and_then(|f| f.action_target.clone())
        else {
            return;
        };

        self.confirm_button = 0;
        self.confirm = match target {
            FindingAction::AskAppToQuit {
                name,
                bundle_id,
                pid,
            } => Some(DoctorActionConfirm::QuitApp {
                name,
                bundle_id,
                pid,
            }),
            FindingAction::KillProcess { pid, name } => {
                Some(DoctorActionConfirm::KillProcess { pid, name })
            }
            FindingAction::DisableStartupItem { item } => {
                Some(DoctorActionConfirm::DisableStartupItem { item })
            }
            FindingAction::ExploreStorage => Some(DoctorActionConfirm::ExploreStorage),
        };
    }

    pub fn remove_selected_finding(&mut self) {
        if let DoctorPhase::Done(ref mut report) = self.phase
            && self.selected_index < report.findings.len()
        {
            report.findings.remove(self.selected_index);
            if self.selected_index >= report.findings.len() && !report.findings.is_empty() {
                self.selected_index = report.findings.len() - 1;
            }
        }
    }

    pub fn check_receiver(&mut self) {
        if let Some(ref rx) = self.receiver
            && let Ok(report) = rx.try_recv()
        {
            self.phase = DoctorPhase::Done(report);
            self.receiver = None;
        }
    }
}

pub fn render_doctor(frame: &mut Frame, state: &DoctorScreenState, theme: &Theme) {
    let area = frame.area();
    let crumbs = vec!["taert", "Optimize", "Doctor"];

    let hints = if state.confirm.is_some() {
        vec![
            Hint::new(theme.g.key_enter, "Confirm"),
            Hint::new(theme.g.key_tab, "Switch button"),
            Hint::new(theme.g.key_esc, "Cancel"),
        ]
    } else {
        match &state.phase {
            DoctorPhase::Checking { .. } => {
                vec![Hint::new(theme.g.key_esc, "Back")]
            }
            DoctorPhase::Done(report) => {
                if report.findings.is_empty() {
                    vec![
                        Hint::new("r", "Recheck"),
                        Hint::new(theme.g.key_esc, "Back"),
                    ]
                } else {
                    vec![
                        Hint::new(theme.g.key_enter, "Action"),
                        Hint::new(theme.g.key_up_down, "Navigate"),
                        Hint::new("r", "Recheck"),
                        Hint::new(theme.g.key_esc, "Back"),
                    ]
                }
            }
        }
    };

    let status_text = match &state.phase {
        DoctorPhase::Checking { .. } => "Checking...".to_string(),
        DoctorPhase::Done(report) => {
            if report.findings.is_empty() {
                "All clear".to_string()
            } else {
                format!("{} findings", report.findings.len())
            }
        }
    };

    let chrome = AppChrome::new(crumbs)
        .full_width()
        .status(Line::from(status_text))
        .hints(hints)
        .behind_dialog(state.confirm.is_some());

    let inner = chrome.render(frame, theme);

    match &state.phase {
        DoctorPhase::Checking {
            started,
            target_seconds,
        } => {
            render_checking_card(frame, inner, *started, *target_seconds, theme);
        }
        DoctorPhase::Done(report) => {
            if report.findings.is_empty() {
                render_all_clear(frame, inner, report.seconds, theme);
            } else {
                let is_split = inner.width >= SPLIT_MIN_WIDTH;
                if is_split {
                    let chunks = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([
                            Constraint::Percentage(55),
                            Constraint::Length(2),
                            Constraint::Min(40),
                        ])
                        .split(inner);

                    render_findings_list(frame, chunks[0], state, report, theme);
                    render_finding_details(frame, chunks[2], state, theme);
                } else {
                    render_findings_list(frame, inner, state, report, theme);
                }
            }
        }
    }

    if let Some(ref confirm) = state.confirm {
        render_confirm_modal(frame, area, confirm, state.confirm_button, theme);
    }
}

fn render_checking_card(
    frame: &mut Frame,
    area: Rect,
    started: Instant,
    target_seconds: u64,
    theme: &Theme,
) {
    let width = area.width as usize;
    let elapsed = started.elapsed().as_secs().min(target_seconds);

    let rows = vec![
        Line::from(vec![
            Span::styled(format!("{} ", theme.g.icon_doctor), theme.callout()),
            Span::styled(
                format!("Watching system activity for {target_seconds} seconds..."),
                theme.body(),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                format!("{elapsed}s / {target_seconds}s"),
                theme.accent_text(),
            ),
            Span::styled(" elapsed", theme.footnote()),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "Analyzing CPU, background energy, memory pressure, disk space, sleep blockers, and network...",
            theme.footnote(),
        )),
    ];

    let color = theme.p.orange;
    let card_lines = card("Doctor", Vec::new(), rows, width, 0, color, theme);
    frame.render_widget(Paragraph::new(card_lines), area);
}

fn render_all_clear(frame: &mut Frame, area: Rect, seconds: u64, theme: &Theme) {
    let width = area.width as usize;
    let color = theme.p.teal;

    let rows = vec![
        Line::from(vec![
            Span::styled(format!("{} ", theme.g.success), theme.success()),
            Span::styled(
                "Nothing stands out.",
                theme.body().add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            format!(
                "Checked for {seconds} seconds. CPU, memory, disk, heat, and network look normal."
            ),
            theme.callout(),
        )),
        Line::from(""),
        Line::from(Span::styled("Checks verified:", theme.footnote())),
        Line::from(Span::styled(
            "  • Sustained CPU usage (top)",
            theme.footnote(),
        )),
        Line::from(Span::styled(
            "  • Background energy and wakeups",
            theme.footnote(),
        )),
        Line::from(Span::styled(
            "  • Memory pressure and swap activity",
            theme.footnote(),
        )),
        Line::from(Span::styled(
            "  • APFS disk space headroom",
            theme.footnote(),
        )),
        Line::from(Span::styled(
            "  • Power assertions (sleep blockers)",
            theme.footnote(),
        )),
        Line::from(Span::styled(
            "  • Wake reasons and wake requests (last 24h)",
            theme.footnote(),
        )),
        Line::from(Span::styled(
            "  • Thermal throttling and performance warnings",
            theme.footnote(),
        )),
        Line::from(Span::styled(
            "  • Sustained network bandwidth",
            theme.footnote(),
        )),
        Line::from(Span::styled(
            "  • Launchd agents and login items",
            theme.footnote(),
        )),
    ];

    let card_lines = card("Diagnosis", Vec::new(), rows, width, 0, color, theme);
    frame.render_widget(Paragraph::new(card_lines), area);
}

fn render_findings_list(
    frame: &mut Frame,
    area: Rect,
    state: &DoctorScreenState,
    report: &DoctorReport,
    theme: &Theme,
) {
    let frame_w = area.width as usize;
    let row_width = frame_w.saturating_sub(FRAME_SIDES);
    let frame_rows = (area.height as usize).saturating_sub(FRAME_ROWS);
    let color = theme.p.orange;

    let scroll_offset = if report.findings.len() > frame_rows && frame_rows > 0 {
        let half = frame_rows / 2;
        let max_offset = report.findings.len().saturating_sub(frame_rows);
        state.selected_index.saturating_sub(half).min(max_offset)
    } else {
        0
    };

    let mut rows = Vec::new();
    for (idx, finding) in report
        .findings
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(frame_rows)
    {
        let is_selected = idx == state.selected_index;
        let (leading_sym, leading_style) = match finding.severity {
            FindingSeverity::Critical | FindingSeverity::Warning => {
                (theme.g.warning, theme.danger())
            }
            FindingSeverity::Notice => (theme.g.bullet, theme.callout()),
            FindingSeverity::Info => (theme.g.file, theme.footnote()),
        };

        let title_spans = vec![Span::styled(finding.title.clone(), theme.body())];

        let line = ListRow::new(title_spans)
            .leading(Span::styled(leading_sym.to_string(), leading_style))
            .chevron(true)
            .selected(is_selected)
            .render_line(row_width, theme);

        rows.push(line);
    }

    let card_lines = card("Findings", Vec::new(), rows, frame_w, 0, color, theme);
    frame.render_widget(Paragraph::new(card_lines), area);
}

fn render_finding_details(frame: &mut Frame, area: Rect, state: &DoctorScreenState, theme: &Theme) {
    let frame_w = area.width as usize;
    let color = theme.p.accent;

    let Some(finding) = state.selected_finding() else {
        return;
    };

    let (badge_text, badge_style) = match finding.severity {
        FindingSeverity::Critical => ("Critical", theme.danger()),
        FindingSeverity::Warning => ("Warning", theme.danger()),
        FindingSeverity::Notice => ("Notice", theme.callout()),
        FindingSeverity::Info => ("Info", theme.footnote()),
    };

    let mut rows = vec![
        Line::from(vec![
            Span::styled(badge_text, badge_style.add_modifier(Modifier::BOLD)),
            Span::raw(" · "),
            Span::styled(finding.id.clone(), theme.footnote()),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            finding.title.clone(),
            theme.body().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled("Evidence:", theme.callout())),
        Line::from(Span::styled(
            format!("  {}", finding.evidence),
            theme.body(),
        )),
        Line::from(""),
        Line::from(Span::styled("Suggested Action:", theme.callout())),
        Line::from(Span::styled(
            format!("  {}", finding.action),
            theme.success(),
        )),
    ];

    if finding.action_target.is_some() {
        rows.push(Line::from(""));
        rows.push(Line::from(Span::styled(
            "Press Enter to take action.",
            theme.accent_text(),
        )));
    }

    let card_lines = card("Details", Vec::new(), rows, frame_w, 0, color, theme);
    frame.render_widget(Paragraph::new(card_lines), area);
}

fn render_confirm_modal(
    frame: &mut Frame,
    area: Rect,
    confirm: &DoctorActionConfirm,
    selected_button: usize,
    theme: &Theme,
) {
    dim_backdrop(frame.buffer_mut(), area);

    let (title, message, action_label, tint) = match confirm {
        DoctorActionConfirm::QuitApp { name, .. } => (
            "Ask App to Quit",
            format!("Politely ask '{name}' to quit via macOS system events?"),
            "Ask to Quit",
            theme.p.orange,
        ),
        DoctorActionConfirm::KillProcess { name, pid } => (
            "Force Quit Process",
            format!("Send termination signal (SIGTERM) to '{name}' (PID {pid})?"),
            "Force Quit",
            theme.p.orange,
        ),
        DoctorActionConfirm::DisableStartupItem { item } => (
            "Switch Off Startup Item",
            format!(
                "Disable '{}' so it no longer runs in the background?",
                item.name
            ),
            "Switch Off",
            theme.p.orange,
        ),
        DoctorActionConfirm::ExploreStorage => (
            "Open Explore Storage",
            "Switch to interactive Explore Storage to find what takes up disk space?".to_string(),
            "Explore",
            theme.p.teal,
        ),
    };

    let buttons = vec![
        Button::primary(action_label).focused(selected_button == 0),
        Button::new("Cancel").focused(selected_button == 1),
    ];

    Modal::new(title)
        .tint(tint)
        .message(vec![
            Line::from(Span::styled(message, theme.body())),
            Line::from(""),
        ])
        .buttons(buttons)
        .hints(Line::from(vec![
            Span::styled(theme.g.key_enter, theme.accent_text()),
            Span::styled(" Confirm  ", theme.callout()),
            Span::styled(theme.g.key_esc, theme.accent_text()),
            Span::styled(" Cancel", theme.callout()),
        ]))
        .render(frame, area, theme);
}

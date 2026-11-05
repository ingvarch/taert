//! Doctor Screen.
//! Explains what is slowing the Mac down and offers actionable suggestions.

use std::sync::mpsc::Receiver;
use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::components::button::Button;
use super::components::chrome::AppChrome;
use super::components::frame::{FRAME_ROWS, FRAME_SIDES, card};
use super::components::hints::Hint;
use super::components::list::ListRow;
use super::components::modal::{Modal, dim_backdrop};
use super::startup::password_line;
use super::theme::Theme;
use crate::core::doctor::{DoctorReport, Finding, FindingAction, FindingSeverity};
use crate::core::startup::StartupItem;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const SPLIT_MIN_WIDTH: u16 = 100;
const NARROW_LIST_ROWS: usize = 5;

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
    KillProcess { pid: u32, name: String },
    DisableStartupItem { item: StartupItem },
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

        self.confirm_button = match target {
            FindingAction::ExploreStorage => 0,
            _ => 1,
        };
        self.confirm = match target {
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

    let status = match (&state.message, &state.phase) {
        (Some((message, true)), _) => Span::styled(message.clone(), theme.danger()),
        (Some((message, false)), _) => Span::styled(message.clone(), theme.success()),
        (None, DoctorPhase::Checking { .. }) => Span::raw("Checking..."),
        (None, DoctorPhase::Done(report)) => {
            if report.findings.is_empty() {
                Span::raw("All clear")
            } else {
                Span::raw(format!("{} findings", report.findings.len()))
            }
        }
    };

    let chrome = AppChrome::new(crumbs)
        .full_width()
        .status(Line::from(status))
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
                render_all_clear(frame, inner, report, theme);
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
                    let list_height =
                        (report.findings.len() + FRAME_ROWS).min(NARROW_LIST_ROWS) as u16;
                    let chunks = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([Constraint::Length(list_height), Constraint::Min(0)])
                        .split(inner);

                    render_findings_list(frame, chunks[0], state, report, theme);
                    render_narrow_details(frame, chunks[1], state, theme);
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

fn render_all_clear(frame: &mut Frame, area: Rect, report: &DoctorReport, theme: &Theme) {
    let width = area.width as usize;
    let color = theme.p.teal;

    let mut rows = vec![
        Line::from(vec![
            Span::styled(format!("{} ", theme.g.success), theme.success()),
            Span::styled(
                "Nothing stands out.",
                theme.body().add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            format!("Checked for {} seconds.", report.seconds),
            theme.callout(),
        )),
        Line::from(""),
        Line::from(Span::styled("Checks:", theme.footnote())),
    ];
    for check in &report.checks {
        rows.push(match (&check.status[..], &check.reason) {
            ("skipped", reason) => Line::from(Span::styled(
                format!(
                    "  • {} (skipped: {})",
                    check.label(),
                    reason.as_deref().unwrap_or_default()
                ),
                theme.warning(),
            )),
            _ => Line::from(Span::styled(
                format!("  • {}", check.label()),
                theme.footnote(),
            )),
        });
    }

    let card_lines = card("Diagnosis", Vec::new(), rows, width, 0, color, theme);
    frame.render_widget(Paragraph::new(card_lines), area);
}

/// Splits `text` into lines of at most `width` columns, breaking between words.
/// A word wider than `width` is cut into chunks of `width` columns.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        if let Some(line) = lines.last_mut()
            && line.width() + 1 + word.width() <= width
        {
            line.push(' ');
            line.push_str(word);
            continue;
        }
        let mut chunk = String::new();
        for ch in word.chars() {
            if !chunk.is_empty() && chunk.width() + ch.width().unwrap_or(0) > width {
                lines.push(std::mem::take(&mut chunk));
            }
            chunk.push(ch);
        }
        lines.push(chunk);
    }
    lines
}

fn severity_style(severity: FindingSeverity, theme: &Theme) -> Style {
    match severity {
        FindingSeverity::Critical => theme.danger(),
        FindingSeverity::Warning => theme.warning(),
        FindingSeverity::Notice => theme.callout(),
        FindingSeverity::Info => theme.footnote(),
    }
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
        let leading_sym = match finding.severity {
            FindingSeverity::Critical | FindingSeverity::Warning => theme.g.warning,
            FindingSeverity::Notice => theme.g.bullet,
            FindingSeverity::Info => theme.g.file,
        };
        let leading_style = severity_style(finding.severity, theme);

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

    let badge_text = match finding.severity {
        FindingSeverity::Critical => "Critical",
        FindingSeverity::Warning => "Warning",
        FindingSeverity::Notice => "Notice",
        FindingSeverity::Info => "Info",
    };
    let badge_style = severity_style(finding.severity, theme);
    let text_width = frame_w.saturating_sub(FRAME_SIDES + 2);
    let indented = |text: &str, style: Style| -> Vec<Line<'static>> {
        wrap_words(text, text_width)
            .into_iter()
            .map(|line| Line::from(Span::styled(format!("  {line}"), style)))
            .collect()
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
    ];
    rows.extend(indented(&finding.evidence, theme.body()));
    rows.push(Line::from(""));
    rows.push(Line::from(Span::styled(
        "Suggested Action:",
        theme.callout(),
    )));
    rows.extend(indented(&finding.action, theme.success()));

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

fn render_narrow_details(frame: &mut Frame, area: Rect, state: &DoctorScreenState, theme: &Theme) {
    let frame_w = area.width as usize;
    let Some(finding) = state.selected_finding() else {
        return;
    };
    let text_width = frame_w.saturating_sub(FRAME_SIDES);

    let mut rows: Vec<Line<'static>> = wrap_words(&finding.title, text_width)
        .into_iter()
        .map(|line| {
            Line::from(Span::styled(
                line,
                theme.body().add_modifier(Modifier::BOLD),
            ))
        })
        .collect();
    for (label, text, style) in [
        ("Evidence", &finding.evidence, theme.body()),
        ("Action", &finding.action, theme.success()),
    ] {
        rows.extend(
            wrap_words(&format!("{label}: {text}"), text_width)
                .into_iter()
                .map(|line| Line::from(Span::styled(line, style))),
        );
    }

    let card_lines = card(
        "Details",
        Vec::new(),
        rows,
        frame_w,
        0,
        theme.p.accent,
        theme,
    );
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

    let mut password = None;
    let (title, message, action_label, tint) = match confirm {
        DoctorActionConfirm::KillProcess { name, pid } => (
            "Stop Process",
            format!(
                "Send SIGTERM to '{name}' (PID {pid})? The program can ignore it or take a moment to close."
            ),
            "Send SIGTERM",
            theme.p.orange,
        ),
        DoctorActionConfirm::DisableStartupItem { item } => {
            password = password_line(item.kind, theme);
            (
                "Switch Off Startup Item",
                format!(
                    "Disable '{}' so it no longer runs in the background?",
                    item.name
                ),
                "Switch Off",
                theme.p.orange,
            )
        }
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

    let mut lines = vec![Line::from(Span::styled(message, theme.body()))];
    if let Some(line) = password {
        lines.push(Line::from(""));
        lines.push(line);
    }
    lines.push(Line::from(""));

    Modal::new(title)
        .tint(tint)
        .message(lines)
        .buttons(buttons)
        .hints(Line::from(vec![
            Span::styled(theme.g.key_enter, theme.accent_text()),
            Span::styled(" Confirm  ", theme.callout()),
            Span::styled(theme.g.key_esc, theme.accent_text()),
            Span::styled(" Cancel", theme.callout()),
        ]))
        .render(frame, area, theme);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::startup::{StartupKind, StartupState};

    fn disable_dialog(kind: StartupKind) -> String {
        let item = StartupItem {
            label: "com.test.item".into(),
            name: "Test".into(),
            app_name: None,
            app_bundle: None,
            kind,
            state: StartupState::Running,
            program: None,
            plist_path: None,
            pid: Some(9),
            cpu_percent: None,
            memory_bytes: None,
            is_apple: false,
            is_security: false,
            is_ignored: false,
        };
        let report = DoctorReport {
            seconds: 1,
            findings: Vec::new(),
            checks: Vec::new(),
        };
        let mut state = DoctorScreenState::new_done(report);
        state.confirm = Some(DoctorActionConfirm::DisableStartupItem { item });
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
        terminal
            .draw(|f| render_doctor(f, &state, &Theme::new()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        buffer.content().iter().map(|c| c.symbol()).collect()
    }

    #[test]
    fn the_daemon_disable_dialog_says_a_password_comes_first() {
        let text = disable_dialog(StartupKind::Daemon);

        assert!(
            text.contains("taert asks for your administrator password first."),
            "{text}"
        );
    }

    #[test]
    fn the_user_agent_disable_dialog_does_not_mention_a_password() {
        let text = disable_dialog(StartupKind::UserAgent);

        assert!(!text.contains("administrator password"), "{text}");
    }

    #[test]
    fn wrap_words_splits_a_word_wider_than_the_width() {
        assert_eq!(wrap_words("abcdefgh", 3), ["abc", "def", "gh"]);
        assert_eq!(wrap_words("ab cdefg", 3), ["ab", "cde", "fg"]);
    }
}

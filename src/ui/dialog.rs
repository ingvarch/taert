//! Unified, reusable Dialog component.
//! Provides consistent rounded borders, content-based sizing, padding, button focus styles,
//! and key hint footers.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use super::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogButtonRole {
    Normal,
    Destructive,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogButton {
    pub title: String,
    pub role: DialogButtonRole,
}

impl DialogButton {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            role: DialogButtonRole::Normal,
        }
    }

    pub fn destructive(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            role: DialogButtonRole::Destructive,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MessageAlignment {
    #[default]
    Center,
    Leading,
}

#[derive(Debug, Clone)]
pub struct DialogHint {
    pub key: String,
    pub action: String,
}

impl DialogHint {
    pub fn new(key: impl Into<String>, action: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            action: action.into(),
        }
    }

    pub fn choice_hints() -> Vec<Self> {
        vec![Self::new("Enter", "select"), Self::new("Esc", "back")]
    }
}

pub struct Dialog<'a> {
    pub title: String,
    pub title_style: Option<Style>,
    pub border_style: Option<Style>,
    pub message: Vec<Line<'a>>,
    pub buttons: Vec<DialogButton>,
    pub focused_button: Option<usize>,
    pub hints: Vec<DialogHint>,
    pub message_alignment: MessageAlignment,
}

impl<'a> Dialog<'a> {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            title_style: None,
            border_style: None,
            message: Vec::new(),
            buttons: Vec::new(),
            focused_button: None,
            hints: Vec::new(),
            message_alignment: MessageAlignment::Center,
        }
    }

    pub fn with_message(mut self, message: Vec<Line<'a>>) -> Self {
        self.message = message;
        self
    }

    pub fn with_buttons(mut self, buttons: Vec<DialogButton>, focused: Option<usize>) -> Self {
        self.buttons = buttons;
        self.focused_button = focused;
        self
    }

    pub fn with_hints(mut self, hints: Vec<DialogHint>) -> Self {
        self.hints = hints;
        self
    }

    pub fn with_title_style(mut self, style: Style) -> Self {
        self.title_style = Some(style);
        self
    }

    pub fn with_border_style(mut self, style: Style) -> Self {
        self.border_style = Some(style);
        self
    }

    pub fn with_alignment(mut self, alignment: MessageAlignment) -> Self {
        self.message_alignment = alignment;
        self
    }

    /// Renders the dialog centered within the given area.
    pub fn render(self, frame: &mut Frame, area: Rect, theme: &Theme) {
        if area.width < 10 || area.height < 5 {
            return;
        }

        // Max content width: capped at 50 for dialog readability.
        let max_content = (area.width.saturating_sub(10) as usize).clamp(1, 50);

        // Word wrap message lines
        let mut wrapped_message: Vec<Line<'a>> = Vec::new();
        for line in &self.message {
            let line_str: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            if line_str.is_empty() {
                wrapped_message.push(Line::from(""));
                continue;
            }

            let words: Vec<&str> = line_str.split_whitespace().collect();
            if words.is_empty() {
                wrapped_message.push(Line::from(""));
                continue;
            }

            let mut cur_line = String::new();
            for word in words {
                if cur_line.is_empty() {
                    cur_line.push_str(word);
                } else if cur_line.len() + 1 + word.len() <= max_content {
                    cur_line.push(' ');
                    cur_line.push_str(word);
                } else {
                    let style = line.spans.first().map(|s| s.style).unwrap_or_default();
                    wrapped_message.push(Line::from(vec![Span::styled(cur_line, style)]));
                    cur_line = word.to_string();
                }
            }
            if !cur_line.is_empty() {
                let style = line.spans.first().map(|s| s.style).unwrap_or_default();
                wrapped_message.push(Line::from(vec![Span::styled(cur_line, style)]));
            }
        }

        // Calculate natural content width
        let title_len = self.title.chars().count() + 2; // titleMargin
        let msg_width = wrapped_message
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.chars().count())
                    .sum::<usize>()
            })
            .max()
            .unwrap_or(0);

        // Buttons width
        let btn_width = if self.buttons.is_empty() {
            0
        } else {
            let total_chars: usize = self
                .buttons
                .iter()
                .map(|b| b.title.chars().count() + 6) // "[ Title ]" or "› Title ‹"
                .sum();
            total_chars + (self.buttons.len().saturating_sub(1) * 2) // 2 spaces between buttons
        };

        // Hints width
        let hint_width = if self.hints.is_empty() {
            0
        } else {
            let chars: usize = self
                .hints
                .iter()
                .map(|h| h.key.chars().count() + 1 + h.action.chars().count())
                .sum();
            chars + (self.hints.len().saturating_sub(1) * 2)
        };

        let natural_content_width = title_len
            .max(msg_width)
            .max(btn_width)
            .max(hint_width)
            .min(max_content)
            .max(24);

        // Total width: contentWidth + 4 (border + title padding) + 2 * sidePadding (1) = contentWidth + 6
        let dialog_width = ((natural_content_width + 6) as u16).min(area.width.saturating_sub(2));

        // Layout rows
        let msg_rows = wrapped_message.len();
        let has_tail = !self.buttons.is_empty() || !self.hints.is_empty();
        let has_groups = msg_rows > 0 || has_tail;

        let blank_above_first = if has_groups { 1 } else { 0 };
        let blank_below_message = if msg_rows > 0 && has_tail { 1 } else { 0 };
        let button_rows = if self.buttons.is_empty() { 0 } else { 1 };
        let blank_below_buttons = if button_rows > 0 && !self.hints.is_empty() {
            1
        } else {
            0
        };
        let hint_rows = if self.hints.is_empty() { 0 } else { 1 };
        let blank_below_last = if has_groups { 1 } else { 0 };

        let total_inner_rows = blank_above_first
            + msg_rows
            + blank_below_message
            + button_rows
            + blank_below_buttons
            + hint_rows
            + blank_below_last;

        let dialog_height = ((total_inner_rows + 2) as u16).min(area.height.saturating_sub(2));

        let rect = Rect {
            x: area.x + (area.width.saturating_sub(dialog_width)) / 2,
            y: area.y + (area.height.saturating_sub(dialog_height)) / 2,
            width: dialog_width,
            height: dialog_height,
        };

        frame.render_widget(Clear, rect);

        let border_style = self.border_style.unwrap_or(theme.border);
        let title_style = self.title_style.unwrap_or(theme.title);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_set(border::ROUNDED)
            .border_style(border_style)
            .title(Line::from(vec![
                Span::raw(" "),
                Span::styled(&self.title, title_style),
                Span::raw(" "),
            ]))
            .title_alignment(Alignment::Left);

        let inner = block.inner(rect);
        frame.render_widget(block, rect);

        if inner.height == 0 || inner.width == 0 {
            return;
        }

        // Build inner lines
        let mut inner_lines: Vec<Line> = Vec::new();

        if blank_above_first > 0 {
            inner_lines.push(Line::from(""));
        }

        for msg_line in wrapped_message {
            let alignment = match self.message_alignment {
                MessageAlignment::Center => Alignment::Center,
                MessageAlignment::Leading => Alignment::Left,
            };
            inner_lines.push(msg_line.alignment(alignment));
        }

        if blank_below_message > 0 {
            inner_lines.push(Line::from(""));
        }

        if button_rows > 0 {
            let mut btn_spans = Vec::new();
            for (idx, btn) in self.buttons.iter().enumerate() {
                if idx > 0 {
                    btn_spans.push(Span::raw("  "));
                }
                let is_focused = self.focused_button == Some(idx);
                let (btn_text, btn_style) = if is_focused {
                    let mut s = if btn.role == DialogButtonRole::Destructive {
                        theme.error
                    } else {
                        theme.title
                    };
                    s = s.add_modifier(Modifier::REVERSED);
                    (format!("› {} ‹", btn.title), s)
                } else {
                    let s = if btn.role == DialogButtonRole::Destructive {
                        theme.error
                    } else {
                        theme.muted
                    };
                    (format!("[ {} ]", btn.title), s)
                };
                btn_spans.push(Span::styled(btn_text, btn_style));
            }
            inner_lines.push(Line::from(btn_spans).alignment(Alignment::Center));
        }

        if blank_below_buttons > 0 {
            inner_lines.push(Line::from(""));
        }

        if hint_rows > 0 {
            let mut hint_spans = Vec::new();
            for (idx, hint) in self.hints.iter().enumerate() {
                if idx > 0 {
                    hint_spans.push(Span::raw("  "));
                }
                hint_spans.push(Span::styled(&hint.key, theme.footer_key));
                hint_spans.push(Span::raw(" "));
                hint_spans.push(Span::styled(&hint.action, theme.footer_desc));
            }
            inner_lines.push(Line::from(hint_spans).alignment(Alignment::Center));
        }

        if blank_below_last > 0 {
            inner_lines.push(Line::from(""));
        }

        frame.render_widget(Paragraph::new(inner_lines), inner);
    }
}

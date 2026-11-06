//! A dialog that picks one value from a short list.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use super::frame::{FRAME_ROWS, FRAME_SIDES};
use super::list::ListRow;
use super::modal::{dim_backdrop, drop_shadow};
use crate::ui::theme::Theme;

// Two borders, title, gap, gap, hints.
const CHROME_H: usize = FRAME_ROWS + 4;
// Rows kept free around the frame, as `Modal` does.
const MARGIN: usize = 2;
// Selection bar and leading mark before the choice, one cell after it.
const ROW_PAD: usize = 5;

#[derive(Debug, Clone)]
pub struct PickerState {
    pub title: String,
    pub choices: Vec<String>,
    pub current: Option<usize>,
    pub highlighted: usize,
}

impl PickerState {
    pub fn new(title: String, choices: Vec<String>, current: Option<usize>) -> Self {
        Self {
            title,
            choices,
            current,
            highlighted: current.unwrap_or(0),
        }
    }

    pub fn next(&mut self) {
        self.highlighted = (self.highlighted + 1).min(self.choices.len().saturating_sub(1));
    }

    pub fn previous(&mut self) {
        self.highlighted = self.highlighted.saturating_sub(1);
    }

    pub fn first(&mut self) {
        self.highlighted = 0;
    }

    pub fn last(&mut self) {
        self.highlighted = self.choices.len().saturating_sub(1);
    }
}

pub fn render_picker(frame: &mut Frame, area: Rect, state: &PickerState, theme: &Theme) {
    if (area.height as usize) < CHROME_H + MARGIN + 1 {
        return;
    }
    dim_backdrop(frame.buffer_mut(), area);

    let hints = vec![
        Span::styled(theme.g.key_enter, theme.callout()),
        Span::styled(" Choose   ", theme.footnote()),
        Span::styled(theme.g.key_esc, theme.callout()),
        Span::styled(" Cancel", theme.footnote()),
    ];
    let hints_w: usize = hints.iter().map(|s| s.content.width()).sum();

    let longest = state.choices.iter().map(|c| c.width()).max().unwrap_or(0);
    let inner_w = (longest + ROW_PAD).max(hints_w).max(state.title.width());
    let width = ((inner_w + FRAME_SIDES) as u16).min(area.width);
    let visible = state
        .choices
        .len()
        .clamp(1, area.height as usize - CHROME_H - MARGIN);
    let height = (visible + CHROME_H) as u16;
    let rect = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };

    drop_shadow(frame.buffer_mut(), rect, area, theme);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(theme.separator());
    frame.render_widget(block, rect);

    let inner = Rect {
        x: rect.x + 2,
        y: rect.y + 1,
        width: rect.width.saturating_sub(FRAME_SIDES as u16),
        height: rect.height.saturating_sub(FRAME_ROWS as u16),
    };
    let row_w = inner.width as usize;
    let offset = state
        .highlighted
        .saturating_sub(visible / 2)
        .min(state.choices.len().saturating_sub(visible));

    let mut lines = vec![
        Line::from(Span::styled(state.title.as_str(), theme.headline())),
        Line::from(""),
    ];
    for (i, choice) in state.choices.iter().enumerate().skip(offset).take(visible) {
        let leading = if state.current == Some(i) {
            Span::styled(theme.g.success, theme.success())
        } else {
            Span::raw(" ")
        };
        let row = ListRow::new(vec![Span::styled(choice.as_str(), theme.body())])
            .leading(leading)
            .selected(i == state.highlighted);
        lines.push(row.render_line(row_w, theme));
    }
    lines.push(Line::from(""));
    let pad = row_w.saturating_sub(hints_w);
    let mut hint_spans = vec![Span::raw(" ".repeat(pad))];
    hint_spans.extend(hints);
    lines.push(Line::from(hint_spans));

    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::Modifier;

    fn render(w: u16, h: u16, state: &PickerState) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        let theme = Theme::new();
        terminal
            .draw(|f| render_picker(f, f.area(), state, &theme))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn screen(w: u16, h: u16, state: &PickerState) -> Vec<String> {
        let buffer = render(w, h, state);
        (0..h)
            .map(|y| (0..w).map(|x| buffer[(x, y)].symbol()).collect())
            .collect()
    }

    fn nine_choices() -> PickerState {
        let choices = (1..=9).map(|n| format!("Choice {n}")).collect();
        PickerState::new("Older than".into(), choices, None)
    }

    fn frame_width(rows: &[String]) -> usize {
        let top = rows.iter().find(|r| r.contains('╭')).unwrap();
        let start = top.chars().position(|c| c == '╭').unwrap();
        let end = top.chars().position(|c| c == '╮').unwrap();
        end - start + 1
    }

    fn picker(current: Option<usize>) -> PickerState {
        let choices = ["a", "b", "c"].map(String::from).to_vec();
        PickerState::new("Title".into(), choices, current)
    }

    #[test]
    fn it_highlights_the_current_choice_or_the_first() {
        assert_eq!(picker(Some(2)).highlighted, 2);
        assert_eq!(picker(None).highlighted, 0);
    }

    #[test]
    fn next_and_previous_stop_at_the_ends() {
        let mut state = picker(Some(1));
        state.next();
        state.next();
        assert_eq!(state.highlighted, 2);
        state.previous();
        state.previous();
        state.previous();
        assert_eq!(state.highlighted, 0);
    }

    #[test]
    fn first_and_last_jump_to_the_ends() {
        let mut state = picker(Some(1));
        state.last();
        assert_eq!(state.highlighted, 2);
        state.first();
        assert_eq!(state.highlighted, 0);
    }

    #[test]
    fn the_frame_fits_the_longest_choice_and_stays_inside_the_area() {
        let choices = ["Every 2 weeks on Monday", "Never"].map(String::from);
        let state = PickerState::new("Run".into(), choices.to_vec(), None);
        let wide = screen(60, 12, &state);
        assert_eq!(frame_width(&wide), 32);
        assert!(wide.iter().any(|r| r.contains("Every 2 weeks on Monday")));
        assert_eq!(frame_width(&screen(20, 12, &state)), 20);
    }

    #[test]
    fn the_highlighted_choice_stays_in_view() {
        let bar = Theme::new().g.selection_bar;
        let mut state = nine_choices();
        for i in 0..9 {
            state.highlighted = i;
            let rows = screen(40, 12, &state);
            let selected = rows.iter().find(|r| r.contains(bar)).unwrap();
            assert!(selected.contains(&format!("Choice {}", i + 1)), "{i}");
        }
    }

    #[test]
    fn moving_up_from_the_last_choice_does_not_scroll() {
        let mut state = nine_choices();
        state.last();
        state.previous();
        let rows = screen(40, 12, &state);
        for n in 6..=9 {
            let choice = format!("Choice {n}");
            assert!(rows.iter().any(|r| r.contains(&choice)), "{choice}");
        }
    }

    #[test]
    fn it_draws_the_shadow_of_a_modal() {
        let theme = Theme::with_env(crate::ui::env::UiEnv::default());
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal
            .draw(|f| render_picker(f, f.area(), &picker(None), &theme))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let corner = |symbol: &str| {
            (0..12u16)
                .flat_map(|y| (0..40u16).map(move |x| (x, y)))
                .find(|&(x, y)| buffer[(x, y)].symbol() == symbol)
                .unwrap()
        };
        let (left, top) = corner("╭");
        let (right, _) = corner("╮");
        let (_, bottom) = corner("╰");
        assert_eq!(buffer[(right + 1, top + 1)].bg, theme.p.fill);
        assert_eq!(buffer[(left + 1, bottom + 1)].bg, theme.p.fill);
    }

    #[test]
    fn it_dims_the_backdrop() {
        let buffer = render(40, 12, &picker(None));
        assert!(buffer[(0, 0)].modifier.contains(Modifier::DIM));
    }
}

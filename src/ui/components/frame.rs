//! Rounded frames drawn line by line, so they scroll with the lines they hold.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::text::fit_width;
use crate::ui::theme::Theme;

/// A border and a space on each side.
pub const FRAME_SIDES: usize = 4;
/// The top and bottom borders.
pub const FRAME_ROWS: usize = 2;

pub fn frame_style(color: Color) -> Style {
    Style::default().fg(color).add_modifier(Modifier::DIM)
}

/// The top border, with the title on the left and `detail` on the right. `note` follows
/// the title and goes first when the frame is too narrow.
pub fn frame_top(
    title: &str,
    note: &str,
    detail: Vec<Span<'static>>,
    width: usize,
    color: Color,
    theme: &Theme,
) -> Line<'static> {
    let detail_w: usize = detail.iter().map(|span| span.content.width()).sum();
    // Detail adds itself and the spaces around it, otherwise the rule runs to the corner.
    let detail_room = if detail.is_empty() { 0 } else { detail_w + 2 };
    // "╭─ ", the space after the title, a piece of rule, and "─╮".
    let chrome = 3 + 1 + 1 + 2;
    let title_room = width.saturating_sub(chrome + detail_room);
    let title = if title.width() > title_room {
        fit_width(title, title_room).trim_end().to_string()
    } else {
        title.to_string()
    };
    let fixed = chrome - 1 + title.width() + detail_room;
    let note = if fixed + 2 + note.width() < width {
        note
    } else {
        ""
    };
    let note_w = if note.is_empty() { 0 } else { 2 + note.width() };
    let rule = width.saturating_sub(fixed + note_w);

    let border = frame_style(color);
    let line = theme.g.frame_horizontal;
    let mut spans = vec![
        Span::styled(format!("{}{line} ", theme.g.frame_top_left), border),
        Span::styled(
            title,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ];
    if !note.is_empty() {
        spans.push(Span::styled(
            format!("  {note}"),
            Style::default().fg(color),
        ));
    }
    spans.push(Span::styled(format!(" {}", line.repeat(rule)), border));
    if !detail.is_empty() {
        spans.push(Span::raw(" "));
        spans.extend(detail);
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled(
        format!("{line}{}", theme.g.frame_top_right),
        border,
    ));
    Line::from(spans)
}

pub fn frame_bottom(width: usize, color: Color, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        format!(
            "{}{}{}",
            theme.g.frame_bottom_left,
            theme.g.frame_horizontal.repeat(width.saturating_sub(2)),
            theme.g.frame_bottom_right
        ),
        frame_style(color),
    ))
}

/// Puts the side borders around a line of `width` columns.
pub fn framed(line: Line<'static>, width: usize, color: Color, theme: &Theme) -> Line<'static> {
    let border = frame_style(color);
    let pad = " ".repeat(width.saturating_sub(line.width()));
    let mut spans = vec![Span::styled(format!("{} ", theme.g.frame_vertical), border)];
    spans.extend(line.spans);
    spans.push(Span::raw(pad));
    spans.push(Span::styled(format!(" {}", theme.g.frame_vertical), border));
    Line::from(spans)
}

/// A whole frame around `body`, `rows` tall inside so it holds its size however little it shows.
pub fn card(
    title: &str,
    detail: Vec<Span<'static>>,
    mut body: Vec<Line<'static>>,
    width: usize,
    rows: usize,
    color: Color,
    theme: &Theme,
) -> Vec<Line<'static>> {
    body.resize(rows.max(body.len()), Line::default());

    let inner = width.saturating_sub(FRAME_SIDES);
    let mut lines = vec![frame_top(title, "", detail, width, color, theme)];
    lines.extend(
        body.into_iter()
            .map(|line| framed(line, inner, color, theme)),
    );
    lines.push(frame_bottom(width, color, theme));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::env::UiEnv;

    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn top(note: &str, detail: &'static str, width: usize) -> Line<'static> {
        let theme = Theme::with_env(UiEnv::default());
        let detail = if detail.is_empty() {
            Vec::new()
        } else {
            vec![Span::raw(detail)]
        };
        frame_top("Title", note, detail, width, Color::Reset, &theme)
    }

    #[test]
    fn the_top_border_fills_its_width() {
        assert_eq!(top("", "1 item", 40).width(), 40);
        assert_eq!(top("note", "1 item", 40).width(), 40);
        assert_eq!(top("", "", 40).width(), 40);
    }

    #[test]
    fn a_top_border_without_detail_runs_to_the_corner() {
        assert_eq!(text(&top("", "", 16)), "╭─ Title ──────╮");
    }

    #[test]
    fn a_note_goes_first_when_the_frame_is_narrow() {
        let narrow = text(&top("a long note", "1 item", 28));

        assert!(!narrow.contains("note"), "{narrow}");
        assert!(narrow.contains("1 item"), "{narrow}");
    }

    #[test]
    fn side_borders_pad_a_short_line() {
        let theme = Theme::with_env(UiEnv::default());

        let line = framed(Line::from("hi"), 6, Color::Reset, &theme);

        assert_eq!(text(&line), "│ hi     │");
    }

    #[test]
    fn a_card_wraps_its_rows_and_pads_to_a_fixed_height() {
        let theme = Theme::with_env(UiEnv::default());
        let body = vec![Line::from("one")];

        let lines = card("Title", Vec::new(), body, 16, 3, Color::Reset, &theme);

        assert_eq!(lines.len(), 5);
        assert!(lines.iter().all(|line| line.width() == 16));
        assert_eq!(text(&lines[1]), "│ one          │");
        assert_eq!(text(&lines[3]), "│              │");
    }

    #[test]
    fn a_long_title_is_cut_to_fit_the_frame() {
        let theme = Theme::with_env(UiEnv::default());
        let title = "a title much longer than the frame";

        let line = frame_top(title, "", Vec::new(), 20, Color::Reset, &theme);

        assert_eq!(line.width(), 20);
        assert!(text(&line).contains('…'), "{}", text(&line));
    }

    #[test]
    fn the_bottom_border_fills_its_width() {
        let theme = Theme::with_env(UiEnv::default());

        assert_eq!(text(&frame_bottom(6, Color::Reset, &theme)), "╰────╯");
    }
}

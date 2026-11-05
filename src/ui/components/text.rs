//! Text and formatting helpers for HIG style.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub fn plural(n: usize, singular: &str, plural_form: &str) -> String {
    if n == 1 {
        format!("{n} {singular}")
    } else {
        format!("{n} {plural_form}")
    }
}

/// Cuts `text` to `width` columns with an ellipsis and pads it to fill them.
pub fn fit_width(text: &str, width: usize) -> String {
    if text.width() <= width {
        return format!("{text}{}", " ".repeat(width - text.width()));
    }

    let mut fitted = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let ch_w = ch.width().unwrap_or(0);
        if used + ch_w + 1 > width {
            break;
        }
        fitted.push(ch);
        used += ch_w;
    }
    fitted.push('…');
    fitted.push_str(&" ".repeat(width.saturating_sub(used + 1)));
    fitted
}

/// Breaks `line` into lines no wider than `width` columns, before the word that does not fit.
/// Lines after the first start with `indent` spaces, when the line has room for them.
pub fn wrap_line(line: Line<'static>, width: usize, indent: usize) -> Vec<Line<'static>> {
    if line.width() <= width {
        return vec![line];
    }

    let chars: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|span| span.content.chars().map(|ch| (ch, span.style)))
        .collect();
    let indent = if indent < width { indent } else { 0 };
    let blank = Style::default();
    let new_row = || vec![(' ', blank); indent];

    let mut rows: Vec<Vec<(char, Style)>> = Vec::new();
    let mut cur: Vec<(char, Style)> = Vec::new();
    let mut cur_w = 0;
    let mut base = 0;
    let mut pending: Vec<(char, Style)> = Vec::new();

    for token in chars.chunk_by(|a, b| (a.0 == ' ') == (b.0 == ' ')) {
        if token[0].0 == ' ' {
            pending = token.to_vec();
            continue;
        }
        let word_w: usize = token.iter().map(|(ch, _)| ch.width().unwrap_or(0)).sum();
        let space_w = pending.len();
        let has_content = cur_w > base;

        if cur_w + space_w + word_w <= width {
            if has_content {
                cur.append(&mut pending);
                cur_w += space_w;
            }
            cur.extend_from_slice(token);
            cur_w += word_w;
        } else if has_content && word_w <= width - indent {
            rows.push(std::mem::replace(&mut cur, new_row()));
            cur_w = indent;
            base = indent;
            cur.extend_from_slice(token);
            cur_w += word_w;
        } else {
            if has_content && cur_w + space_w < width {
                cur.append(&mut pending);
                cur_w += space_w;
            }
            for &(ch, style) in token {
                let ch_w = ch.width().unwrap_or(0);
                if cur_w + ch_w > width && cur_w > base {
                    rows.push(std::mem::replace(&mut cur, new_row()));
                    cur_w = indent;
                    base = indent;
                }
                if cur_w + ch_w > width && cur_w == base && base > 0 {
                    cur.clear();
                    cur_w = 0;
                    base = 0;
                }
                cur.push((ch, style));
                cur_w += ch_w;
            }
        }
        pending.clear();
    }
    rows.push(cur);

    rows.into_iter().map(spans_of).collect()
}

fn spans_of(chars: Vec<(char, Style)>) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_style = Style::default();
    for (ch, style) in chars {
        if !run.is_empty() && style != run_style {
            spans.push(Span::styled(std::mem::take(&mut run), run_style));
        }
        run_style = style;
        run.push(ch);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, run_style));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    #[test]
    fn fit_width_pads_short_text() {
        assert_eq!(fit_width("node", 6), "node  ");
    }

    #[test]
    fn fit_width_cuts_long_text_with_an_ellipsis() {
        assert_eq!(fit_width("WindowServer", 7), "Window…");
    }

    #[test]
    fn fit_width_counts_columns_not_characters() {
        assert_eq!(fit_width("日本語", 5), "日本…");
    }

    #[test]
    fn test_plural() {
        assert_eq!(plural(1, "folder", "folders"), "1 folder");
        assert_eq!(plural(0, "folder", "folders"), "0 folders");
        assert_eq!(plural(5, "path", "paths"), "5 paths");
    }

    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn texts(lines: &[Line]) -> Vec<String> {
        lines.iter().map(text).collect()
    }

    #[test]
    fn wrap_line_keeps_a_line_that_fits() {
        let line = Line::from(vec![
            Span::styled("Name: ", Style::new().fg(Color::Red)),
            Span::raw("node"),
        ]);

        assert_eq!(wrap_line(line.clone(), 10, 6), vec![line]);
    }

    #[test]
    fn wrap_line_breaks_between_words_and_indents_the_next_line() {
        let line = Line::from("alpha beta gamma");

        let lines = wrap_line(line, 11, 2);

        assert_eq!(texts(&lines), ["alpha beta", "  gamma"]);
    }

    #[test]
    fn wrap_line_keeps_styles_on_both_parts_of_a_split_span() {
        let red = Style::new().fg(Color::Red);
        let line = Line::from(vec![Span::raw("ab "), Span::styled("cd ef", red)]);

        let lines = wrap_line(line, 5, 0);

        assert_eq!(texts(&lines), ["ab cd", "ef"]);
        assert_eq!(lines[0].spans.last().unwrap().style, red);
        assert_eq!(lines[1].spans.last().unwrap().style, red);
    }

    #[test]
    fn wrap_line_cuts_a_word_that_does_not_fit_on_a_line() {
        let lines = wrap_line(Line::from("abcdefghij"), 4, 0);

        assert_eq!(texts(&lines), ["abcd", "efgh", "ij"]);
    }

    #[test]
    fn wrap_line_starts_a_long_word_next_to_the_label() {
        let lines = wrap_line(Line::from("Plist:   /a/very/long/path"), 14, 9);

        assert_eq!(
            texts(&lines),
            [
                "Plist:   /a/ve",
                "         ry/lo",
                "         ng/pa",
                "         th"
            ]
        );
    }

    #[test]
    fn wrap_line_counts_wide_characters_by_columns() {
        let lines = wrap_line(Line::from("日本語日本語"), 5, 0);

        assert!(lines.iter().all(|l| l.width() <= 5), "{lines:?}");
        assert_eq!(text(&lines[0]), "日本");
        assert_eq!(texts(&lines).concat(), "日本語日本語");
    }

    #[test]
    fn wrap_line_drops_the_indent_when_a_wide_character_does_not_fit_after_it() {
        let lines = wrap_line(Line::from("日本語"), 3, 2);

        assert!(lines.iter().all(|l| l.width() <= 3), "{lines:?}");
        assert_eq!(texts(&lines), ["日", "本", "語"]);
    }

    #[test]
    fn wrap_line_ignores_an_indent_that_leaves_no_room() {
        let lines = wrap_line(Line::from("aa bb cc"), 4, 4);

        assert_eq!(texts(&lines), ["aa", "bb", "cc"]);
    }
}

//! Places two stacks of lines next to each other.

use ratatui::text::{Line, Span};

pub fn side_by_side<'a>(
    left: Vec<Line<'a>>,
    left_w: usize,
    gap: usize,
    right: Vec<Line<'a>>,
) -> Vec<Line<'a>> {
    let height = left.len().max(right.len());
    let mut left = left.into_iter();
    let mut right = right.into_iter();

    (0..height)
        .map(|_| {
            let mut line = left.next().unwrap_or_default();
            let pad = (left_w + gap).saturating_sub(line.width());
            line.spans.push(Span::raw(" ".repeat(pad)));
            line.spans.extend(right.next().unwrap_or_default().spans);
            line
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn the_right_column_starts_at_the_same_place() {
        let left = vec![Line::from("ab"), Line::from("abcd")];
        let right = vec![Line::from("x"), Line::from("y")];

        let lines = side_by_side(left, 4, 2, right);

        assert_eq!(text(&lines[0]), "ab    x");
        assert_eq!(text(&lines[1]), "abcd  y");
    }

    #[test]
    fn the_taller_column_sets_the_height() {
        let lines = side_by_side(
            vec![Line::from("a")],
            2,
            1,
            vec![Line::from("x"), Line::from("y")],
        );

        assert_eq!(lines.len(), 2);
        assert_eq!(text(&lines[1]), "   y");
    }

    #[test]
    fn an_overlong_left_line_does_not_panic() {
        let lines = side_by_side(vec![Line::from("abcdef")], 2, 1, vec![Line::from("x")]);

        assert_eq!(text(&lines[0]), "abcdefx");
    }
}

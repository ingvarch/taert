//! Text and formatting helpers for HIG style.

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

#[cfg(test)]
mod tests {
    use super::*;

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
}

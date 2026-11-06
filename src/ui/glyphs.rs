//! Glyphs and symbols for Unicode and ASCII terminal rendering.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Glyphs {
    pub selection_bar: &'static str,
    pub chevron: &'static str,
    pub breadcrumb_sep: &'static str,
    pub check_on: &'static str,
    pub check_off: &'static str,
    pub check_mixed: &'static str,
    pub success: &'static str,
    pub error: &'static str,
    pub warning: &'static str,
    pub folder: &'static str,
    pub file: &'static str,
    pub package: &'static str,
    pub search: &'static str,
    pub bullet: &'static str,
    pub frame_top_left: &'static str,
    pub frame_top_right: &'static str,
    pub frame_bottom_left: &'static str,
    pub frame_bottom_right: &'static str,
    pub frame_horizontal: &'static str,
    pub frame_vertical: &'static str,
    pub sort_descending: &'static str,
    pub sort_ascending: &'static str,
    pub update: &'static str,
    pub popup: &'static str,
    pub bar_fill: &'static str,
    pub bar_track: &'static str,
    pub gauge_blocks: &'static [&'static str],
    pub spark_blocks: &'static [&'static str],
    pub spinner: &'static [&'static str],
    pub key_up_down: &'static str,
    pub key_left_right: &'static str,
    pub key_enter: &'static str,
    pub key_esc: &'static str,
    pub key_space: &'static str,
    pub key_tab: &'static str,
    pub key_backspace: &'static str,
    pub icon_clean: &'static str,
    pub icon_projects: &'static str,
    pub icon_docker: &'static str,
    pub icon_uninstall: &'static str,
    pub icon_storage: &'static str,
    pub icon_status: &'static str,
    pub icon_doctor: &'static str,
    pub icon_startup: &'static str,
    pub icon_prefs: &'static str,
    pub icon_schedule: &'static str,
    pub icon_protected: &'static str,
}

impl Default for Glyphs {
    fn default() -> Self {
        Self::unicode()
    }
}

impl Glyphs {
    pub fn unicode() -> Self {
        Self {
            selection_bar: "▌",
            chevron: "›",
            breadcrumb_sep: "›",
            check_on: "●",
            check_off: "○",
            check_mixed: "◐",
            success: "✓",
            error: "✕",
            warning: "!",
            folder: "▸",
            file: "·",
            package: "◆",
            search: "⌕",
            bullet: "●",
            frame_top_left: "╭",
            frame_top_right: "╮",
            frame_bottom_left: "╰",
            frame_bottom_right: "╯",
            frame_horizontal: "─",
            frame_vertical: "│",
            sort_descending: "↓",
            sort_ascending: "↑",
            update: "↑",
            popup: "⌄",
            bar_fill: "━",
            bar_track: "─",
            gauge_blocks: &["▏", "▎", "▍", "▌", "▋", "▊", "▉", "█"],
            spark_blocks: &["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"],
            spinner: &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
            key_up_down: "↑↓",
            key_left_right: "←→",
            key_enter: "↵",
            key_esc: "⎋",
            key_space: "␣",
            key_tab: "⇥",
            key_backspace: "⌫",
            icon_clean: "✧",
            icon_projects: "◇",
            icon_docker: "▦",
            icon_uninstall: "⌦",
            icon_storage: "◔",
            icon_status: "▤",
            icon_doctor: "✚",
            icon_startup: "▲",
            icon_prefs: "⌥",
            icon_schedule: "◷",
            icon_protected: "◈",
        }
    }

    pub fn ascii() -> Self {
        Self {
            selection_bar: ">",
            chevron: ">",
            breadcrumb_sep: "/",
            check_on: "x",
            check_off: "o",
            check_mixed: "-",
            success: "+",
            error: "x",
            warning: "!",
            folder: ">",
            file: ".",
            package: "#",
            search: "/",
            bullet: "*",
            frame_top_left: "+",
            frame_top_right: "+",
            frame_bottom_left: "+",
            frame_bottom_right: "+",
            frame_horizontal: "-",
            frame_vertical: "|",
            sort_descending: "v",
            sort_ascending: "^",
            update: "^",
            popup: "v",
            bar_fill: "=",
            bar_track: "-",
            gauge_blocks: &["#"],
            spark_blocks: &["_", ".", "-", "=", "#"],
            spinner: &["-", "\\", "|", "/"],
            key_up_down: "Up/Dn",
            key_left_right: "L/R",
            key_enter: "Enter",
            key_esc: "Esc",
            key_space: "Space",
            key_tab: "Tab",
            key_backspace: "Bksp",
            icon_clean: "C",
            icon_projects: "P",
            icon_docker: "D",
            icon_uninstall: "U",
            icon_storage: "E",
            icon_status: "S",
            icon_doctor: "+",
            icon_startup: "T",
            icon_prefs: "O",
            icon_schedule: "R",
            icon_protected: "X",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn the_schedule_icon_takes_one_cell_and_has_an_ascii_letter() {
        assert_eq!(Glyphs::unicode().icon_schedule.width(), 1);
        let ascii = Glyphs::ascii().icon_schedule;
        assert_eq!(ascii.len(), 1);
        assert!(ascii.chars().all(|c| c.is_ascii_alphabetic()));
    }
}

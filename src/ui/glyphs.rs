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
    pub bar_fill: &'static str,
    pub bar_track: &'static str,
    pub gauge_blocks: &'static [&'static str],
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
    pub icon_storage: &'static str,
    pub icon_status: &'static str,
    pub icon_prefs: &'static str,
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
            bar_fill: "━",
            bar_track: "─",
            gauge_blocks: &["▏", "▎", "▍", "▌", "▋", "▊", "▉", "█"],
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
            icon_storage: "◔",
            icon_status: "▤",
            icon_prefs: "⌥",
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
            bar_fill: "=",
            bar_track: "-",
            gauge_blocks: &["#"],
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
            icon_storage: "E",
            icon_status: "S",
            icon_prefs: "O",
            icon_protected: "X",
        }
    }
}

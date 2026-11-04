//! Keys shared by several screens.

use crossterm::event::KeyCode;

/// The keys that start a removal: `d`, `D`, Backspace (labelled "delete" on a Mac), and Delete.
pub fn is_removal_key(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::Char('d') | KeyCode::Char('D') | KeyCode::Backspace | KeyCode::Delete
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn four_keys_start_a_removal() {
        for key in [
            KeyCode::Char('d'),
            KeyCode::Char('D'),
            KeyCode::Backspace,
            KeyCode::Delete,
        ] {
            assert!(is_removal_key(key), "{key:?}");
        }
        for key in [
            KeyCode::Enter,
            KeyCode::Char('r'),
            KeyCode::Char('T'),
            KeyCode::Char(' '),
        ] {
            assert!(!is_removal_key(key), "{key:?}");
        }
    }
}

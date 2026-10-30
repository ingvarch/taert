//! Path prettification and Home directory tilde contraction.

use std::path::{Path, PathBuf};

use crate::core::safety::Safety;

/// Contract absolute paths starting with user's home directory to "~/..."
pub fn prettify_path(path: &Path) -> String {
    let home = Safety::home_dir();
    if let Ok(rel) = path.strip_prefix(&home) {
        if rel.as_os_str().is_empty() {
            "~".to_string()
        } else {
            format!("~/{}", rel.display())
        }
    } else {
        path.display().to_string()
    }
}

/// Expands leading `~` or `~/...` to the user's home directory.
pub fn expand_tilde(path: &str) -> PathBuf {
    let trimmed = path.trim();
    if trimmed == "~" {
        Safety::home_dir()
    } else if let Some(rest) = trimmed.strip_prefix("~/") {
        Safety::home_dir().join(rest)
    } else if let Some(rest) = trimmed.strip_prefix('~') {
        let clean_rest = rest.trim_start_matches('/');
        Safety::home_dir().join(clean_rest)
    } else {
        PathBuf::from(trimmed)
    }
}

/// Standardizes a path by expanding tilde and making it canonical/absolute if possible.
pub fn standardize_path(path: &str) -> PathBuf {
    let expanded = expand_tilde(path);
    expanded.canonicalize().unwrap_or(expanded)
}

/// Truncate a string in the middle with an ellipsis ('…') if it exceeds `max_len` characters.
pub fn middle_truncate(s: &str, max_len: usize) -> String {
    if max_len == 0 {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_len {
        return s.to_string();
    }
    if max_len <= 1 {
        return "…".to_string();
    }
    let left_len = (max_len - 1) / 2;
    let right_len = max_len - 1 - left_len;
    let left: String = chars[..left_len].iter().collect();
    let right: String = chars[chars.len() - right_len..].iter().collect();
    format!("{left}…{right}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prettify_path() {
        let home = Safety::home_dir();
        let target = home.join("Projects/demo");
        assert_eq!(prettify_path(&target), "~/Projects/demo");
    }

    #[test]
    fn test_expand_tilde() {
        let home = Safety::home_dir();
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("~/Repos"), home.join("Repos"));
        assert_eq!(expand_tilde("/tmp/abc"), PathBuf::from("/tmp/abc"));
    }

    #[test]
    fn test_middle_truncate() {
        assert_eq!(middle_truncate("", 10), "");
        assert_eq!(middle_truncate("hello", 10), "hello");
        assert_eq!(middle_truncate("hello", 5), "hello");
        assert_eq!(middle_truncate("1234567890", 5), "12…90");
        assert_eq!(middle_truncate("1234567890", 1), "…");
        assert_eq!(middle_truncate("1234567890", 0), "");
    }
}

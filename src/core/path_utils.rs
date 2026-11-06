//! Path prettification, tilde expansion and contraction, path completion, and the 30-day change check.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::core::safety::Safety;

pub(crate) const STALE_AFTER: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Did anything in the tree change after `cutoff`? What cannot be read counts as changed.
pub(crate) fn changed_since(path: &Path, cutoff: SystemTime) -> bool {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return true;
    };
    if meta.modified().is_ok_and(|time| time > cutoff) {
        return true;
    }
    if !meta.is_dir() {
        return false;
    }
    match fs::read_dir(path) {
        Ok(entries) => entries
            .flatten()
            .any(|entry| changed_since(&entry.path(), cutoff)),
        Err(_) => true,
    }
}

/// Contract absolute paths starting with user's home directory to "~/..."
pub fn prettify_path(path: &Path) -> String {
    prettify_path_in(path, &Safety::home_dir())
}

/// Like `prettify_path`, with `home` in place of the real home folder.
pub fn prettify_path_in(path: &Path, home: &Path) -> String {
    if let Ok(rel) = path.strip_prefix(home) {
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
    expand_tilde_in(path, &Safety::home_dir())
}

/// Like `expand_tilde`, with `home` in place of the real home folder.
pub fn expand_tilde_in(path: &str, home: &Path) -> PathBuf {
    let trimmed = path.trim();
    if trimmed == "~" {
        home.to_path_buf()
    } else if let Some(rest) = trimmed.strip_prefix("~/") {
        home.join(rest)
    } else if let Some(rest) = trimmed.strip_prefix('~') {
        home.join(rest.trim_start_matches('/'))
    } else {
        PathBuf::from(trimmed)
    }
}

/// Standardizes a path by expanding tilde with `home` and making it canonical/absolute if possible.
pub fn standardize_path(path: &str, home: &Path) -> PathBuf {
    let expanded = expand_tilde_in(path, home);
    expanded.canonicalize().unwrap_or(expanded)
}

/// What Tab does to a typed path: the new text, and the names to list when several fit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub text: String,
    pub matches: Vec<String>,
}

/// Completes the last part of `input` from the names in its folder. `~` stands for `home`
/// and stays `~` in the result.
pub fn complete_path(input: &str, home: &Path, folders_only: bool) -> Completion {
    let unchanged = Completion {
        text: input.to_string(),
        matches: Vec::new(),
    };
    if input == "~" {
        return Completion {
            text: "~/".to_string(),
            ..unchanged
        };
    }
    let Some(split) = input.rfind('/') else {
        return unchanged;
    };
    let (folder, typed) = input.split_at(split + 1);
    let Ok(entries) = fs::read_dir(expand_tilde_in(folder, home)) else {
        return unchanged;
    };
    let show_hidden = typed.starts_with('.');

    let mut found: Vec<(String, bool)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with(typed) || (name.starts_with('.') && !show_hidden) {
                return None;
            }
            let is_dir = entry.path().is_dir();
            (is_dir || !folders_only).then_some((name, is_dir))
        })
        .collect();
    found.sort();

    match found.as_slice() {
        [] => unchanged,
        [(name, is_dir)] => Completion {
            text: format!("{folder}{name}{}", if *is_dir { "/" } else { "" }),
            matches: Vec::new(),
        },
        _ => {
            let names: Vec<String> = found.into_iter().map(|(name, _)| name).collect();
            Completion {
                text: format!("{folder}{}", common_start(&names)),
                matches: names,
            }
        }
    }
}

fn common_start(names: &[String]) -> String {
    let mut start: &str = &names[0];
    for name in &names[1..] {
        let shared = start
            .chars()
            .zip(name.chars())
            .take_while(|(a, b)| a == b)
            .map(|(c, _)| c.len_utf8())
            .sum();
        start = &start[..shared];
    }
    start.to_string()
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
    fn paths_in_the_home_folder_show_with_a_tilde() {
        let home = Path::new("/Users/me");
        assert_eq!(prettify_path_in(home, home), "~");
        assert_eq!(
            prettify_path_in(Path::new("/Users/me/.config/x"), home),
            "~/.config/x"
        );
        assert_eq!(
            prettify_path_in(Path::new("/opt/homebrew/bin/taert"), home),
            "/opt/homebrew/bin/taert"
        );
    }

    #[test]
    fn test_expand_tilde() {
        let home = Safety::home_dir();
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("~/Repos"), home.join("Repos"));
        assert_eq!(expand_tilde("/tmp/abc"), PathBuf::from("/tmp/abc"));
    }

    #[test]
    fn expand_tilde_in_uses_the_given_home() {
        let home = Path::new("/sandbox/home");
        assert_eq!(expand_tilde_in("~", home), home);
        assert_eq!(expand_tilde_in("~/Code", home), home.join("Code"));
        assert_eq!(expand_tilde_in("/tmp/x", home), PathBuf::from("/tmp/x"));
    }

    fn tree() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().canonicalize().unwrap();
        fs::create_dir_all(home.join("Code/alpha")).unwrap();
        fs::create_dir_all(home.join("Code/alps")).unwrap();
        fs::create_dir_all(home.join("Code/.hidden")).unwrap();
        fs::create_dir_all(home.join("Code/zeta")).unwrap();
        fs::write(home.join("Code/notes.txt"), "").unwrap();
        (temp, home)
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn one_match_completes_the_name_and_adds_a_slash_after_a_folder() {
        let (_temp, home) = tree();

        let done = complete_path("~/Code/ze", &home, false);

        assert_eq!(done.text, "~/Code/zeta/");
        assert!(done.matches.is_empty());
        let file = complete_path("~/Code/no", &home, false);
        assert_eq!(file.text, "~/Code/notes.txt");
    }

    #[test]
    fn several_matches_complete_the_common_start_and_list_the_names() {
        let (_temp, home) = tree();

        let done = complete_path("~/Code/al", &home, false);

        assert_eq!(done.text, "~/Code/alp");
        assert_eq!(done.matches, names(&["alpha", "alps"]));
    }

    #[test]
    fn no_match_leaves_the_input_alone() {
        let (_temp, home) = tree();

        let done = complete_path("~/Code/qq", &home, false);

        assert_eq!(done.text, "~/Code/qq");
        assert!(done.matches.is_empty());
    }

    #[test]
    fn a_hidden_name_shows_only_after_a_dot() {
        let (_temp, home) = tree();

        let plain = complete_path("~/Code/", &home, false);
        assert_eq!(
            plain.matches,
            names(&["alpha", "alps", "notes.txt", "zeta"])
        );

        let dotted = complete_path("~/Code/.", &home, false);
        assert_eq!(dotted.text, "~/Code/.hidden/");
    }

    #[test]
    fn folders_only_leaves_files_out_and_keeps_links_to_folders() {
        let (_temp, home) = tree();
        std::os::unix::fs::symlink(home.join("Code/zeta"), home.join("Code/zlink")).unwrap();
        std::os::unix::fs::symlink(home.join("Code/notes.txt"), home.join("Code/zfile")).unwrap();

        let done = complete_path("~/Code/z", &home, true);

        assert_eq!(done.text, "~/Code/z");
        assert_eq!(done.matches, names(&["zeta", "zlink"]));
        assert_eq!(complete_path("~/Code/no", &home, true).text, "~/Code/no");
    }

    #[test]
    fn a_tilde_alone_becomes_the_home_folder() {
        let (_temp, home) = tree();

        assert_eq!(complete_path("~", &home, true).text, "~/");
        assert_eq!(complete_path("~/Co", &home, true).text, "~/Code/");
    }

    #[test]
    fn an_absolute_path_completes_without_the_home() {
        let (_temp, home) = tree();
        let input = format!("{}/Co", home.display());

        let done = complete_path(&input, Path::new("/nowhere"), true);

        assert_eq!(done.text, format!("{}/Code/", home.display()));
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

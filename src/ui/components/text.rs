//! Text and formatting helpers for HIG style.

pub fn plural(n: usize, singular: &str, plural_form: &str) -> String {
    if n == 1 {
        format!("{n} {singular}")
    } else {
        format!("{n} {plural_form}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plural() {
        assert_eq!(plural(1, "folder", "folders"), "1 folder");
        assert_eq!(plural(0, "folder", "folders"), "0 folders");
        assert_eq!(plural(5, "path", "paths"), "5 paths");
    }
}

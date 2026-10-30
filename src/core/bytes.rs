//! Byte formatting utilities using decimal units (Finder/macOS style, 1 KB = 1000 B).

const KB: u64 = 1_000;
const MB: u64 = 1_000_000;
const GB: u64 = 1_000_000_000;
const TB: u64 = 1_000_000_000_000;

/// Format a byte count in human-readable decimal units (macOS standard for storage).
pub fn format_bytes(bytes: u64) -> String {
    if bytes < KB {
        format!("{bytes} B")
    } else if bytes < MB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else if bytes < GB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes < TB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else {
        format!("{:.2} TB", bytes as f64 / TB as f64)
    }
}

/// Format RAM memory byte counts in binary units (1024 base), matching macOS.
pub fn format_memory_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const TIB: f64 = 1024.0 * 1024.0 * 1024.0 * 1024.0;

    let (val, unit) = if (bytes as f64) < KIB {
        return format!("{bytes} B");
    } else if (bytes as f64) < MIB {
        (bytes as f64 / KIB, "KB")
    } else if (bytes as f64) < GIB {
        (bytes as f64 / MIB, "MB")
    } else if (bytes as f64) < TIB {
        (bytes as f64 / GIB, "GB")
    } else {
        (bytes as f64 / TIB, "TB")
    };

    let mut formatted = format!("{val:.1}");
    if formatted.ends_with(".0") {
        formatted.truncate(formatted.len() - 2);
    }
    format!("{formatted} {unit}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(1_000), "1.0 KB");
        assert_eq!(format_bytes(1_500), "1.5 KB");
        assert_eq!(format_bytes(25_200_000), "25.2 MB");
        assert_eq!(format_bytes(1_200_000_000), "1.2 GB");
        assert_eq!(format_bytes(2_500_000_000_000), "2.50 TB");
    }

    #[test]
    fn test_format_memory_bytes() {
        assert_eq!(format_memory_bytes(0), "0 B");
        assert_eq!(format_memory_bytes(51_539_607_552), "48 GB");
        assert_eq!(format_memory_bytes(17_179_869_184), "16 GB");
        assert_eq!(format_memory_bytes(522_190_848), "498 MB");
    }
}

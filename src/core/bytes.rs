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

/// Parse a human-readable size string (e.g. "500M", "100MB", "1G", "1.5GB", "500KiB", "1024") into bytes.
/// Standard decimal units (KB = 1,000, MB = 1,000,000, GB = 1,000,000,000, TB = 1,000,000,000,000)
/// match macOS storage standards. Binary units (KiB, MiB, GiB, TiB) use 1024 base.
pub fn parse_size(input: &str) -> Result<u64, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("size cannot be empty".to_string());
    }

    let num_end = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (num_str, unit_str) = s.split_at(num_end);
    let unit = unit_str.trim().to_ascii_lowercase();

    let multiplier: f64 = match unit.as_str() {
        "" | "b" | "bytes" | "byte" => 1.0,
        "k" | "kb" => KB as f64,
        "kib" => 1024.0,
        "m" | "mb" => MB as f64,
        "mib" => 1024.0 * 1024.0,
        "g" | "gb" => GB as f64,
        "gib" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" => TB as f64,
        "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        other => return Err(format!("unknown size unit: '{other}'")),
    };

    let number: f64 = num_str
        .parse()
        .map_err(|_| format!("invalid number in size: '{num_str}'"))?;

    if number < 0.0 {
        return Err("size cannot be negative".to_string());
    }

    Ok((number * multiplier).round() as u64)
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

    #[test]
    fn test_parse_size() {
        assert_eq!(parse_size("0").unwrap(), 0);
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert_eq!(parse_size("500 B").unwrap(), 500);
        assert_eq!(parse_size("100K").unwrap(), 100_000);
        assert_eq!(parse_size("100KB").unwrap(), 100_000);
        assert_eq!(parse_size("100KiB").unwrap(), 102_400);
        assert_eq!(parse_size("500M").unwrap(), 500_000_000);
        assert_eq!(parse_size("500MB").unwrap(), 500_000_000);
        assert_eq!(parse_size("1G").unwrap(), 1_000_000_000);
        assert_eq!(parse_size("1.5G").unwrap(), 1_500_000_000);
        assert_eq!(parse_size("2TB").unwrap(), 2_000_000_000_000);
        assert!(parse_size("").is_err());
        assert!(parse_size("abc").is_err());
        assert!(parse_size("100xyz").is_err());
    }
}

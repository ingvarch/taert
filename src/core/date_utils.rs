//! Date and time formatting utilities.

use std::time::SystemTime;

/// Formats a `SystemTime` into "YYYY-MM-DD HH:MM" using the local timezone.
pub fn format_system_time(time: SystemTime) -> String {
    if let Ok(dur) = time.duration_since(std::time::UNIX_EPOCH) {
        let secs = dur.as_secs() as libc::time_t;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        unsafe {
            libc::localtime_r(&secs, &mut tm);
        }
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min
        )
    } else {
        "—".to_string()
    }
}

/// Formats an optional `SystemTime` into "YYYY-MM-DD HH:MM" or "—".
pub fn format_system_time_opt(time: Option<SystemTime>) -> String {
    match time {
        Some(t) => format_system_time(t),
        None => "—".to_string(),
    }
}

/// Formats a `SystemTime` into "YYYY-MM-DD" using the local timezone.
pub fn format_date_only(time: SystemTime) -> String {
    if let Ok(dur) = time.duration_since(std::time::UNIX_EPOCH) {
        let secs = dur.as_secs() as libc::time_t;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        unsafe {
            libc::localtime_r(&secs, &mut tm);
        }
        format!(
            "{:04}-{:02}-{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday
        )
    } else {
        "—".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_format_system_time_opt_none() {
        assert_eq!(format_system_time_opt(None), "—");
    }

    #[test]
    fn test_format_system_time_epoch() {
        let epoch = SystemTime::UNIX_EPOCH;
        let formatted = format_system_time(epoch);
        // Should produce a valid 16-character "YYYY-MM-DD HH:MM"
        assert_eq!(formatted.len(), 16);
        assert!(formatted.contains(':'));
        assert!(formatted.contains('-'));
    }

    #[test]
    fn test_format_date_only() {
        let time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let formatted = format_date_only(time);
        assert_eq!(formatted.len(), 10);
        assert_eq!(formatted.matches('-').count(), 2);
    }
}

//! Date and time formatting utilities.

use std::ops::Range;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

/// How long ago `time` was, in the words `docker images` uses.
pub fn format_age(time: SystemTime, now: SystemTime) -> String {
    let elapsed = now.duration_since(time).unwrap_or_default();
    let seconds = elapsed.as_secs();
    let minutes = seconds / 60;
    // Docker rounds hours to the nearest one and everything longer down.
    let hours = (seconds + 1_800) / 3_600;

    let age = if seconds < 1 {
        "Less than a second".to_string()
    } else if seconds == 1 {
        "1 second".to_string()
    } else if seconds < 60 {
        format!("{seconds} seconds")
    } else if minutes == 1 {
        "About a minute".to_string()
    } else if minutes < 60 {
        format!("{minutes} minutes")
    } else if hours == 1 {
        "About an hour".to_string()
    } else if hours < 48 {
        format!("{hours} hours")
    } else if hours < 24 * 7 * 2 {
        format!("{} days", hours / 24)
    } else if hours < 24 * 30 * 2 {
        format!("{} weeks", hours / 24 / 7)
    } else if hours < 24 * 365 * 2 {
        format!("{} months", hours / 24 / 30)
    } else {
        format!("{} years", seconds / 3_600 / 24 / 365)
    };

    format!("{age} ago")
}

/// Reads an RFC 3339 time. Fractions of a second are dropped, and a time before
/// 1970 is `None`, because Docker uses the zero date for "unknown".
pub fn parse_rfc3339(text: &str) -> Option<SystemTime> {
    let number = |range: Range<usize>| text.get(range)?.parse::<i32>().ok();

    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = number(0..4)? - 1900;
    tm.tm_mon = number(5..7)? - 1;
    tm.tm_mday = number(8..10)?;
    tm.tm_hour = number(11..13)?;
    tm.tm_min = number(14..16)?;
    tm.tm_sec = number(17..19)?;

    let zone = text
        .get(19..)?
        .trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    let offset_seconds = match zone {
        "Z" => 0,
        _ => {
            let hours = zone.get(1..3)?.parse::<i64>().ok()?;
            let minutes = zone.get(4..6)?.parse::<i64>().ok()?;
            let offset = hours * 3_600 + minutes * 60;
            match zone.get(..1)? {
                "+" => offset,
                "-" => -offset,
                _ => return None,
            }
        }
    };

    let seconds = unsafe { libc::timegm(&mut tm) } as i64 - offset_seconds;
    u64::try_from(seconds)
        .ok()
        .map(|seconds| UNIX_EPOCH + Duration::from_secs(seconds))
}

/// Formats a `SystemTime` into an RFC 3339 UTC string: "YYYY-MM-DDTHH:MM:SSZ".
pub fn format_rfc3339(time: SystemTime) -> String {
    if let Ok(dur) = time.duration_since(UNIX_EPOCH) {
        let secs = dur.as_secs() as libc::time_t;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        unsafe {
            libc::gmtime_r(&secs, &mut tm);
        }
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    } else {
        "1970-01-01T00:00:00Z".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;

    fn age(seconds: u64) -> String {
        let now = UNIX_EPOCH + Duration::from_secs(4_000_000_000);
        format_age(now - Duration::from_secs(seconds), now)
    }

    #[test]
    fn age_reads_like_docker_images() {
        let cases = [
            (0, "Less than a second ago"),
            (1, "1 second ago"),
            (59, "59 seconds ago"),
            (MINUTE, "About a minute ago"),
            (2 * MINUTE - 1, "About a minute ago"),
            (2 * MINUTE, "2 minutes ago"),
            (HOUR - 1, "59 minutes ago"),
            (HOUR, "About an hour ago"),
            (HOUR + 29 * MINUTE, "About an hour ago"),
            (HOUR + 30 * MINUTE, "2 hours ago"),
            (47 * HOUR, "47 hours ago"),
            (2 * DAY, "2 days ago"),
            (13 * DAY, "13 days ago"),
            (14 * DAY, "2 weeks ago"),
            (59 * DAY, "8 weeks ago"),
            (60 * DAY, "2 months ago"),
            (729 * DAY, "24 months ago"),
            (730 * DAY, "2 years ago"),
        ];

        for (seconds, expected) in cases {
            assert_eq!(age(seconds), expected, "{seconds} seconds");
        }
    }

    #[test]
    fn a_time_in_the_future_is_not_an_age() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000);

        let age = format_age(now + Duration::from_secs(50), now);

        assert_eq!(age, "Less than a second ago");
    }

    #[test]
    fn rfc3339_reads_the_shapes_docker_sends() {
        let seconds = |text| {
            let time: SystemTime = parse_rfc3339(text).unwrap();
            time.duration_since(UNIX_EPOCH).unwrap().as_secs()
        };

        assert_eq!(seconds("2026-05-11T10:24:36Z"), 1_778_495_076);
        assert_eq!(seconds("2026-05-11T10:24:36.261354675Z"), 1_778_495_076);
        assert_eq!(seconds("2026-05-11T12:24:36+02:00"), 1_778_495_076);
        assert_eq!(seconds("2026-05-11T05:24:36.5-05:00"), 1_778_495_076);
    }

    #[test]
    fn rfc3339_turns_down_what_it_cannot_read() {
        for text in ["", "yesterday", "2026-05-11", "2026-05-11T10:24:36"] {
            assert_eq!(parse_rfc3339(text), None, "{text}");
        }
    }

    #[test]
    fn the_zero_date_counts_as_unknown() {
        assert_eq!(parse_rfc3339("0001-01-01T00:00:00Z"), None);
    }

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

    #[test]
    fn test_format_rfc3339() {
        let time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_778_495_076);
        let formatted = format_rfc3339(time);
        assert_eq!(formatted, "2026-05-11T10:24:36Z");
        assert_eq!(parse_rfc3339(&formatted), Some(time));
    }
}

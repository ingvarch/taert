use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use taert::core::date_utils::format_rfc3339;
use taert::core::history::{HistoryEvent, OutcomeKind};

/// An item of a test run: path, bytes, outcome, error.
pub type TestItem<'a> = (&'a str, u64, OutcomeKind, Option<&'a str>);

/// A local time as the RFC 3339 text a run stores, so the date shown does not depend on the time zone.
pub fn local_time(year: i32, month: i32, day: i32, hour: i32, minute: i32) -> String {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = year - 1900;
    tm.tm_mon = month - 1;
    tm.tm_mday = day;
    tm.tm_hour = hour;
    tm.tm_min = minute;
    tm.tm_isdst = -1;
    let secs = unsafe { libc::mktime(&mut tm) };
    format_rfc3339(UNIX_EPOCH + Duration::from_secs(secs as u64))
}

/// Appends a finished run to the history file at `path`.
pub fn write_run(path: &Path, run: &str, at: &str, command: &str, mode: &str, items: &[TestItem]) {
    let mut events = vec![HistoryEvent::Start {
        run: run.to_string(),
        at: at.to_string(),
        command: command.to_string(),
        mode: mode.to_string(),
        version: "0.9.0".to_string(),
        total: Some(items.len()),
    }];
    for (item_path, bytes, outcome, error) in items {
        events.push(HistoryEvent::Item {
            run: run.to_string(),
            path: item_path.to_string(),
            bytes: *bytes,
            outcome: *outcome,
            error: error.map(String::from),
        });
    }
    events.push(HistoryEvent::End {
        run: run.to_string(),
        at: at.to_string(),
    });

    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    for event in events {
        writeln!(file, "{}", serde_json::to_string(&event).unwrap()).unwrap();
    }
}

/// Three runs, oldest first, the way the file keeps them.
pub fn write_three_runs(path: &Path) {
    write_run(
        path,
        "20261030-090000-aaaa",
        &local_time(2026, 10, 30, 9, 0),
        "clean dev",
        "interactive",
        &[
            (
                "/Users/test/.cargo/registry/cache",
                15_000_000,
                OutcomeKind::Trashed,
                None,
            ),
            (
                "/Users/test/.npm/_cacache",
                80_000_000,
                OutcomeKind::Trashed,
                None,
            ),
        ],
    );
    write_run(
        path,
        "20261031-182000-bbbb",
        &local_time(2026, 10, 31, 18, 20),
        "docker",
        "force",
        &[(
            "docker image prune",
            2_300_000_000,
            OutcomeKind::Removed,
            None,
        )],
    );
    write_run(
        path,
        "20261101-134000-cccc",
        &local_time(2026, 11, 1, 13, 40),
        "clean junk",
        "schedule",
        &[
            (
                "/Users/test/Library/Logs/old.log",
                4_000_000,
                OutcomeKind::Removed,
                None,
            ),
            (
                "/Users/test/Library/Caches/locked",
                9_000_000,
                OutcomeKind::Failed,
                Some("Permission denied (os error 13)"),
            ),
        ],
    );
}

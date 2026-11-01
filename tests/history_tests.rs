use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

fn taert(home: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin("taert").unwrap();
    cmd.env("HOME", home.path().canonicalize().unwrap());
    cmd
}

#[test]
fn history_empty_prints_no_history() {
    let home = TempDir::new().unwrap();
    taert(&home)
        .arg("history")
        .assert()
        .success()
        .stdout(predicate::str::contains("No history found."));
}

#[test]
fn history_empty_json_prints_empty_array() {
    let home = TempDir::new().unwrap();
    taert(&home)
        .args(["history", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::diff("[]\n"));
}

#[test]
fn run_writes_start_items_and_end_and_shows_in_history() {
    let home = TempDir::new().unwrap();
    let home_path = home.path().canonicalize().unwrap();

    // Create a dummy cache to clean in Dev
    let cache_dir = home_path.join("Library/Caches/Homebrew");
    fs::create_dir_all(&cache_dir).unwrap();
    fs::write(cache_dir.join("test.tar.gz"), b"brew-test").unwrap();

    // Run clean dev with --force
    taert(&home)
        .args(["clean", "dev", "--force"])
        .assert()
        .success();

    let history_file = home_path.join(".config/taert/history.jsonl");
    assert!(history_file.exists(), "history.jsonl should be created");

    // Verify mode is 0600
    let meta = fs::metadata(&history_file).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);

    let content = fs::read_to_string(&history_file).unwrap();
    assert!(content.contains("\"event\":\"start\""));
    assert!(content.contains("\"command\":\"clean dev\""));
    assert!(content.contains("\"mode\":\"force\""));
    assert!(content.contains("\"event\":\"item\""));
    assert!(content.contains("\"event\":\"end\""));

    // Verify `taert history` lists the run
    let output = taert(&home)
        .arg("history")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).unwrap();
    assert!(stdout.contains("clean dev"));
    assert!(stdout.contains("force"));
    assert!(stdout.contains("removed"));

    // Verify `taert history --json` returns list with items
    taert(&home)
        .args(["history", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"command\": \"clean dev\""))
        .stdout(predicate::str::contains("\"mode\": \"force\""))
        .stdout(predicate::str::contains("\"items\":"))
        .stdout(predicate::str::contains("\"outcome\": \"removed\""));

    // Extract run id from jsonl to test `taert history show <run>`
    let first_line = content.lines().next().unwrap();
    let start_event: serde_json::Value = serde_json::from_str(first_line).unwrap();
    let run_id = start_event["run"].as_str().unwrap();

    // Verify `taert history show <run>`
    taert(&home)
        .args(["history", "show", run_id])
        .assert()
        .success()
        .stdout(predicate::str::contains(run_id))
        .stdout(predicate::str::contains("clean dev"))
        .stdout(predicate::str::contains("completed"))
        .stdout(predicate::str::contains("removed"));
}

#[test]
fn dry_run_writes_nothing() {
    let home = TempDir::new().unwrap();
    let home_path = home.path().canonicalize().unwrap();

    let cache_dir = home_path.join("Library/Caches/Homebrew");
    fs::create_dir_all(&cache_dir).unwrap();
    fs::write(cache_dir.join("test.tar.gz"), b"brew-test").unwrap();

    taert(&home)
        .args(["clean", "dev", "--dry-run"])
        .assert()
        .success();

    let history_file = home_path.join(".config/taert/history.jsonl");
    assert!(
        !history_file.exists(),
        "--dry-run should not create history file"
    );
}

#[test]
fn no_history_env_writes_nothing() {
    let home = TempDir::new().unwrap();
    let home_path = home.path().canonicalize().unwrap();

    let cache_dir = home_path.join("Library/Caches/Homebrew");
    fs::create_dir_all(&cache_dir).unwrap();
    fs::write(cache_dir.join("test.tar.gz"), b"brew-test").unwrap();

    let mut cmd = taert(&home);
    cmd.env("TAERT_NO_HISTORY", "1");
    cmd.args(["clean", "dev", "--force"]).assert().success();

    let history_file = home_path.join(".config/taert/history.jsonl");
    assert!(
        !history_file.exists(),
        "TAERT_NO_HISTORY=1 should not write history"
    );
}

#[test]
fn broken_line_is_skipped_and_never_fails_history() {
    let home = TempDir::new().unwrap();
    let home_path = home.path().canonicalize().unwrap();

    let config_dir = home_path.join(".config/taert");
    fs::create_dir_all(&config_dir).unwrap();
    let history_file = config_dir.join("history.jsonl");

    let content = "\
{\"event\":\"start\",\"run\":\"20261101-100000-0001\",\"at\":\"2026-11-01T10:00:00Z\",\"command\":\"clean dev\",\"mode\":\"force\",\"version\":\"0.6.0\"}
{BROKEN CORRUPTED JSON}
{\"event\":\"item\",\"run\":\"20261101-100000-0001\",\"path\":\"/path/to/cache\",\"bytes\":1024,\"outcome\":\"removed\"}
GARBAGE LINE HERE
{\"event\":\"end\",\"run\":\"20261101-100000-0001\",\"at\":\"2026-11-01T10:00:02Z\"}
";
    fs::write(&history_file, content).unwrap();

    taert(&home)
        .arg("history")
        .assert()
        .success()
        .stdout(predicate::str::contains("clean dev"))
        .stdout(predicate::str::contains("force"))
        .stdout(predicate::str::contains("removed 1, 1.0 KB"));
}

#[test]
fn run_without_end_shows_as_interrupted() {
    let home = TempDir::new().unwrap();
    let home_path = home.path().canonicalize().unwrap();

    let config_dir = home_path.join(".config/taert");
    fs::create_dir_all(&config_dir).unwrap();
    let history_file = config_dir.join("history.jsonl");

    let content = "\
{\"event\":\"start\",\"run\":\"20261027-220300-1234\",\"at\":\"2026-10-27T22:03:00Z\",\"command\":\"purge ~/Projects\",\"mode\":\"interactive\",\"version\":\"0.6.0\",\"total\":9}
{\"event\":\"item\",\"run\":\"20261027-220300-1234\",\"path\":\"/Projects/app1/target\",\"bytes\":1000,\"outcome\":\"removed\"}
{\"event\":\"item\",\"run\":\"20261027-220300-1234\",\"path\":\"/Projects/app2/target\",\"bytes\":2000,\"outcome\":\"removed\"}
{\"event\":\"item\",\"run\":\"20261027-220300-1234\",\"path\":\"/Projects/app3/target\",\"bytes\":3000,\"outcome\":\"removed\"}
";
    fs::write(&history_file, content).unwrap();

    taert(&home)
        .arg("history")
        .assert()
        .success()
        .stdout(predicate::str::contains("purge ~/Projects"))
        .stdout(predicate::str::contains("interactive"))
        .stdout(predicate::str::contains("interrupted after 3 of 9"));

    taert(&home)
        .args(["history", "show", "20261027"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Status:  interrupted"))
        .stdout(predicate::str::contains("/Projects/app1/target"));
}

#[test]
fn history_limit_respects_count() {
    let home = TempDir::new().unwrap();
    let home_path = home.path().canonicalize().unwrap();

    let config_dir = home_path.join(".config/taert");
    fs::create_dir_all(&config_dir).unwrap();
    let history_file = config_dir.join("history.jsonl");

    let mut content = String::new();
    for i in 1..=5 {
        content.push_str(&format!(
            "{{\"event\":\"start\",\"run\":\"run{i}\",\"at\":\"2026-11-01T10:0{i}:00Z\",\"command\":\"clean dev\",\"mode\":\"force\",\"version\":\"0.6.0\"}}\n\
             {{\"event\":\"end\",\"run\":\"run{i}\",\"at\":\"2026-11-01T10:0{i}:05Z\"}}\n"
        ));
    }
    fs::write(&history_file, content).unwrap();

    let output = taert(&home)
        .args(["history", "--limit", "2"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).unwrap();
    let lines: Vec<_> = stdout.trim().lines().collect();
    assert_eq!(lines.len(), 2, "Should show exactly 2 runs");
}

#[test]
fn taert_clean_junk_never_lists_history_log() {
    let home = TempDir::new().unwrap();
    let home_path = home.path().canonicalize().unwrap();

    let config_dir = home_path.join(".config/taert");
    fs::create_dir_all(&config_dir).unwrap();
    let history_file = config_dir.join("history.jsonl");
    fs::write(&history_file, "dummy content").unwrap();

    // Create a real junk item to make sure junk scanning works
    let user_logs = home_path.join("Library/Logs");
    fs::create_dir_all(&user_logs).unwrap();
    fs::write(user_logs.join("junk.log"), b"log").unwrap();

    let output = taert(&home)
        .args(["clean", "junk", "--plain"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).unwrap();

    assert!(
        !stdout.contains("history.jsonl"),
        "clean junk must never list history.jsonl"
    );
    assert!(
        !stdout.contains(".config"),
        "clean junk must never list ~/.config"
    );
}

#[test]
fn unwritable_log_warns_and_cleanup_finishes() {
    let home = TempDir::new().unwrap();
    let home_path = home.path().canonicalize().unwrap();

    let config_dir = home_path.join(".config/taert");
    fs::create_dir_all(&config_dir).unwrap();
    let history_file = config_dir.join("history.jsonl");
    fs::write(&history_file, "read-only").unwrap();

    // Make the file read-only (0444)
    fs::set_permissions(&history_file, fs::Permissions::from_mode(0o444)).unwrap();
    // Also make directory read-only so it cannot be replaced or opened
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o555)).unwrap();

    // Create a dummy cache to clean in Dev
    let cache_dir = home_path.join("Library/Caches/Homebrew");
    fs::create_dir_all(&cache_dir).unwrap();
    fs::write(cache_dir.join("test.tar.gz"), b"brew-test").unwrap();

    let assert = taert(&home)
        .args(["clean", "dev", "--force"])
        .assert()
        .success();

    assert.stderr(predicate::str::contains(
        "Warning: could not write to history log",
    ));

    // Restore permissions so TempDir can clean up
    let _ = fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o755));
    let _ = fs::set_permissions(&history_file, fs::Permissions::from_mode(0o644));
}

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

fn taert(home: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin("taert").unwrap();
    cmd.env("HOME", home.path().canonicalize().unwrap());
    cmd
}

#[test]
fn test_cli_help() {
    let home = tempfile::tempdir().unwrap();
    let mut cmd = taert(&home);
    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Fast, transparent macOS disk cleanup",
        ));
}

#[test]
fn test_cli_config() {
    let home = tempfile::tempdir().unwrap();
    let mut cmd = taert(&home);
    cmd.arg("config")
        .assert()
        .success()
        .stdout(predicate::str::contains("taert Configuration"))
        .stdout(predicate::str::contains("Appearance:"))
        .stdout(predicate::str::contains("Default artifact age threshold"));
}

#[test]
fn test_cli_config_invalid_appearance() {
    let home = tempfile::tempdir().unwrap();
    let mut cmd = taert(&home);
    cmd.args(["config", "--appearance", "neon"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Invalid appearance"));
}

#[test]
fn test_cli_ignore_list() {
    let home = tempfile::tempdir().unwrap();
    let mut cmd = taert(&home);
    cmd.args(["ignore", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Protected paths"));
}

#[test]
fn test_cli_purge_dry_run() {
    let home = tempfile::tempdir().unwrap();
    let mut cmd = taert(&home);
    cmd.args(["purge", ".", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Dry run completed"));
}

#[test]
fn test_cli_docker_reports_an_unreachable_daemon() {
    let home = tempfile::tempdir().unwrap();
    let mut cmd = taert(&home);
    cmd.env("DOCKER_HOST", "tcp://127.0.0.1:1")
        .args(["docker", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Can't reach Docker"));
}

#[test]
fn test_cli_large_force_fails() {
    let home = tempfile::tempdir().unwrap();
    taert(&home)
        .args(["large", "--force"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "personal files must never be removed unreviewed",
        ));
}

#[test]
fn test_cli_large_json() {
    let home = tempfile::tempdir().unwrap();
    taert(&home)
        .args(["large", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"id\": \"large\""));
}

#[test]
fn test_cli_large_dry_run() {
    let home = tempfile::tempdir().unwrap();
    taert(&home)
        .args(["large", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Dry run completed."));
}

#[test]
fn test_cli_schedule_refuses_purge_and_docker() {
    let home = tempfile::tempdir().unwrap();
    taert(&home)
        .args(["schedule", "set", "--categories", "purge"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be scheduled"));

    taert(&home)
        .args(["schedule", "set", "--categories", "docker"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be scheduled"));
}

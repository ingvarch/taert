use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn test_cli_help() {
    let mut cmd = Command::cargo_bin("taert").unwrap();
    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Fast, transparent macOS disk cleanup",
        ));
}

#[test]
fn test_cli_optimize_json() {
    let mut cmd = Command::cargo_bin("taert").unwrap();
    cmd.args(["optimize", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"memory\""))
        .stdout(predicate::str::contains("\"total_bytes\""));
}

#[test]
fn test_cli_config() {
    let mut cmd = Command::cargo_bin("taert").unwrap();
    cmd.arg("config")
        .assert()
        .success()
        .stdout(predicate::str::contains("taert Configuration"))
        .stdout(predicate::str::contains("Appearance:"))
        .stdout(predicate::str::contains("Default artifact age threshold"));
}

#[test]
fn test_cli_config_invalid_appearance() {
    let mut cmd = Command::cargo_bin("taert").unwrap();
    cmd.args(["config", "--appearance", "neon"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Invalid appearance"));
}

#[test]
fn test_cli_ignore_list() {
    let mut cmd = Command::cargo_bin("taert").unwrap();
    cmd.args(["ignore", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Protected paths"));
}

#[test]
fn test_cli_purge_dry_run() {
    let mut cmd = Command::cargo_bin("taert").unwrap();
    cmd.args(["purge", ".", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Dry run completed"));
}

#[test]
fn test_cli_docker_reports_an_unreachable_daemon() {
    let mut cmd = Command::cargo_bin("taert").unwrap();
    cmd.env("DOCKER_HOST", "tcp://127.0.0.1:1")
        .args(["docker", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Can't reach Docker"));
}

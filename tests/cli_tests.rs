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

mod uninstall {
    use assert_cmd::Command;
    use predicates::prelude::*;
    use std::fs;
    use std::path::Path;
    use tempfile::{TempDir, tempdir};

    fn write_app(bundle: &Path, id: &str) {
        let contents = bundle.join("Contents/MacOS");
        fs::create_dir_all(&contents).unwrap();
        fs::write(contents.join("run"), vec![0_u8; 4096]).unwrap();
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>{id}</string>
<key>CFBundleExecutable</key><string>run</string>
</dict></plist>"#
        );
        fs::write(bundle.join("Contents/Info.plist"), plist).unwrap();
    }

    fn home_with_app() -> TempDir {
        let home = tempdir().unwrap();
        write_app(
            &home.path().join("Applications/Taert Fixture.app"),
            "is.taert.fixture",
        );
        let cache = home.path().join("Library/Caches/is.taert.fixture");
        fs::create_dir_all(&cache).unwrap();
        fs::write(cache.join("data"), vec![0_u8; 4096]).unwrap();
        home
    }

    fn taert(home: &TempDir) -> Command {
        let mut cmd = Command::cargo_bin("taert").unwrap();
        cmd.env("HOME", home.path().canonicalize().unwrap());
        cmd
    }

    #[test]
    fn dry_run_shows_the_plan_and_removes_nothing() {
        let home = home_with_app();

        taert(&home)
            .args(["uninstall", "Taert Fixture", "--dry-run"])
            .assert()
            .success()
            .stdout(predicate::str::contains("Taert Fixture.app"))
            .stdout(predicate::str::contains("is.taert.fixture"))
            .stdout(predicate::str::contains("Dry run completed"));

        assert!(home.path().join("Applications/Taert Fixture.app").exists());
        assert!(home.path().join("Library/Caches/is.taert.fixture").exists());
    }

    #[test]
    fn json_describes_the_plan_and_removes_nothing() {
        let home = home_with_app();

        let output = taert(&home)
            .args(["uninstall", "is.taert.fixture", "--json"])
            .output()
            .unwrap();

        let plan: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(plan["apps"][0]["app"]["bundle_id"], "is.taert.fixture");
        assert_eq!(plan["scan"]["groups"][0]["items"][0]["role"], "bundle");
        assert!(home.path().join("Applications/Taert Fixture.app").exists());
    }

    #[test]
    fn dry_run_beats_force() {
        let home = home_with_app();

        taert(&home)
            .args(["uninstall", "Taert Fixture", "--dry-run", "--force"])
            .assert()
            .success()
            .stdout(predicate::str::contains("Dry run completed"));

        assert!(home.path().join("Applications/Taert Fixture.app").exists());
        assert!(home.path().join("Library/Caches/is.taert.fixture").exists());
    }

    #[test]
    fn force_needs_the_whole_name() {
        let home = home_with_app();

        taert(&home)
            .args(["uninstall", "taert fix", "--force"])
            .assert()
            .failure()
            .stderr(predicate::str::contains("No installed app matches"));

        assert!(home.path().join("Applications/Taert Fixture.app").exists());
    }

    #[test]
    fn force_without_a_name_refuses() {
        let home = home_with_app();

        taert(&home)
            .args(["uninstall", "--force"])
            .assert()
            .failure()
            .stderr(predicate::str::contains("Name the apps"));

        assert!(home.path().join("Applications/Taert Fixture.app").exists());
    }

    #[test]
    fn an_unknown_name_fails() {
        let home = home_with_app();

        taert(&home)
            .args(["uninstall", "No Such App", "--dry-run"])
            .assert()
            .failure()
            .stderr(predicate::str::contains("No installed app matches"));
    }

    #[test]
    fn plain_mode_asks_before_it_removes() {
        let home = home_with_app();

        taert(&home)
            .args(["uninstall", "Taert Fixture", "--plain"])
            .write_stdin("\nno\n")
            .assert()
            .success()
            .stdout(predicate::str::contains("Operation cancelled"));

        assert!(home.path().join("Applications/Taert Fixture.app").exists());
    }
}

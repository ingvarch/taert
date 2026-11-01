//! Runs a real uninstall against a throwaway app in a temporary home folder.
//!
//! Ignored by default, because it talks to the running Mac: it reads the whole
//! /Applications folder and may ask for permission to use System Events.
//! `cargo test --test uninstall_live -- --ignored`

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
    for folder in [
        "Library/Caches/is.taert.fixture",
        "Library/Application Support/Taert Fixture",
        "Library/Containers/is.taert.fixture.share",
    ] {
        let folder = home.path().join(folder);
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("data"), vec![0_u8; 4096]).unwrap();
    }
    let prefs = home.path().join("Library/Preferences");
    fs::create_dir_all(&prefs).unwrap();
    fs::write(prefs.join("is.taert.fixture.plist"), b"x").unwrap();
    home
}

fn taert(home: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin("taert").unwrap();
    cmd.env("HOME", home.path().canonicalize().unwrap());
    cmd
}

#[track_caller]
fn assert_all_gone(home: &TempDir) {
    for path in [
        "Applications/Taert Fixture.app",
        "Library/Caches/is.taert.fixture",
        "Library/Application Support/Taert Fixture",
        "Library/Containers/is.taert.fixture.share",
        "Library/Preferences/is.taert.fixture.plist",
    ] {
        assert!(!home.path().join(path).exists(), "{path} stayed");
    }
}

#[test]
#[ignore = "talks to the running Mac"]
fn force_removes_the_app_and_everything_it_left() {
    let home = home_with_app();

    taert(&home)
        .args(["uninstall", "Taert Fixture", "--force"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Uninstall complete"));

    assert_all_gone(&home);
}

#[test]
#[ignore = "talks to the running Mac"]
fn plain_mode_removes_after_yes() {
    let home = home_with_app();

    taert(&home)
        .args(["uninstall", "Taert Fixture", "--plain"])
        .write_stdin("\nyes\n")
        .assert()
        .success();

    assert_all_gone(&home);
}

#[test]
#[ignore = "talks to the running Mac"]
fn the_list_shows_the_app() {
    let home = home_with_app();

    let output = taert(&home).args(["uninstall", "--json"]).output().unwrap();

    let list: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(list.to_string().contains("Taert Fixture.app"));
}

#[test]
#[ignore = "talks to the running Mac"]
fn plain_mode_without_a_choice_removes_nothing() {
    let home = home_with_app();

    taert(&home)
        .args(["uninstall", "--plain"])
        .write_stdin("\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("Nothing selected"));

    assert!(home.path().join("Applications/Taert Fixture.app").exists());
}

#[test]
#[ignore = "talks to the running Mac"]
fn mac_host_trash_moves_file_to_trash() {
    use taert::core::host::{Host, MacHost};
    let dir = tempdir().unwrap();
    let file = dir.path().join("trash_test.txt");
    fs::write(&file, "test trash content").unwrap();
    assert!(file.exists());

    let host = MacHost;
    let res = host.trash(&file);
    assert!(res.is_ok(), "host.trash failed: {:?}", res);
    assert!(!file.exists(), "file still exists after trash");
}

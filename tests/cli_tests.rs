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

// The removal in these tests stays in a temporary home: `clean junk` has no target outside it.
mod clean {
    use super::taert;
    use predicates::prelude::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, SystemTime};
    use tempfile::{TempDir, tempdir};

    const SAFARI: &str = "Library/Caches/com.apple.Safari";
    const SAFARI_CONTAINER: &str = "Library/Containers/com.apple.Safari/Data/Library/Caches";
    const CHROME: &str = "Library/Caches/Google/Chrome";
    // A real Chrome or Safari keeps its own row out of `--force`, whatever taert sees of
    // the fixture. Edge is the browser least likely to run where these tests run.
    const EDGE: &str = "Library/Caches/Microsoft Edge";
    const EDGE_ID: &str = "com.microsoft.edgemac";
    const FULL_DISK_ACCESS: &str = "Full Disk Access";

    fn fill(home: &TempDir, relative: &str) -> PathBuf {
        let folder = home.path().canonicalize().unwrap().join(relative);
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("data"), [1_u8; 8192]).unwrap();
        folder
    }

    fn age(folder: &Path, days: u32) {
        let then = SystemTime::now() - Duration::from_secs(24 * 60 * 60) * days;
        for path in [folder.join("data"), folder.to_path_buf()] {
            fs::File::open(path).unwrap().set_modified(then).unwrap();
        }
    }

    /// A folder this terminal cannot read. It opens up again so the home can be deleted.
    struct Refused(PathBuf);

    impl Refused {
        fn in_home(home: &TempDir, relative: &str) -> Self {
            let folder = fill(home, relative);
            fs::set_permissions(&folder, fs::Permissions::from_mode(0o000)).unwrap();
            Self(folder)
        }
    }

    impl Drop for Refused {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        }
    }

    /// A copy of `sleep` inside an app bundle, so `ps` lists that app as running.
    struct RunningApp(Child);

    impl RunningApp {
        fn start(bundle: &Path, id: &str) -> Self {
            let program = bundle.join("Contents/MacOS/run");
            fs::create_dir_all(program.parent().unwrap()).unwrap();
            fs::copy("/bin/sleep", &program).unwrap();
            let plist = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>{id}</string>
<key>CFBundleExecutable</key><string>run</string>
</dict></plist>"#
            );
            fs::write(bundle.join("Contents/Info.plist"), plist).unwrap();
            let child = Command::new(&program)
                .arg("120")
                .stdout(Stdio::null())
                .spawn()
                .unwrap();
            Self(child)
        }
    }

    impl Drop for RunningApp {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn dry_run_lists_both_groups_and_removes_nothing() {
        let home = tempdir().unwrap();
        let app = fill(&home, "Library/Caches/com.acme.app");
        let chrome = fill(&home, CHROME);

        taert(&home)
            .args(["clean", "junk", "--dry-run"])
            .assert()
            .success()
            .stdout(predicate::str::contains("[App caches]"))
            .stdout(predicate::str::contains("[Browser caches]"))
            .stdout(predicate::str::contains("Dry run completed"));

        assert!(app.exists());
        assert!(chrome.exists());
    }

    #[test]
    fn one_category_keeps_the_note_of_a_refused_read() {
        let home = tempdir().unwrap();
        let _refused = Refused::in_home(&home, SAFARI_CONTAINER);

        taert(&home)
            .args(["clean", "junk", "--dry-run"])
            .assert()
            .success()
            .stdout(predicate::str::contains(FULL_DISK_ACCESS));
    }

    #[test]
    fn every_category_keeps_the_note_of_a_refused_read() {
        let home = tempdir().unwrap();
        let _refused = Refused::in_home(&home, SAFARI_CONTAINER);

        taert(&home)
            .args(["clean", "--dry-run"])
            .assert()
            .success()
            .stdout(predicate::str::contains(FULL_DISK_ACCESS));
    }

    #[test]
    fn json_carries_the_notes_and_every_folder_of_a_row() {
        let home = tempdir().unwrap();
        let outer = fill(&home, SAFARI);
        let container = fill(&home, SAFARI_CONTAINER);
        let _refused = Refused::in_home(&home, "Library/Caches/Firefox");

        let output = taert(&home)
            .args(["clean", "junk", "--json"])
            .output()
            .unwrap();

        let scan: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(
            scan["notes"][0]
                .as_str()
                .unwrap()
                .contains(FULL_DISK_ACCESS)
        );
        let browsers = scan["groups"]
            .as_array()
            .unwrap()
            .iter()
            .find(|group| group["tool"] == "Browser caches")
            .unwrap();
        let row = &browsers["items"][0];
        assert_eq!(row["path"].as_str(), outer.to_str());
        assert_eq!(row["also"][0]["path"].as_str(), container.to_str());
    }

    fn applications(home: &TempDir) -> PathBuf {
        home.path().canonicalize().unwrap().join("Applications")
    }

    #[test]
    fn force_leaves_a_running_browsers_cache_and_removes_a_stale_app_cache() {
        let home = tempdir().unwrap();
        let edge = fill(&home, EDGE);
        let stale = fill(&home, "Library/Caches/com.acme.stale");
        age(&stale, 90);
        let _edge = RunningApp::start(&applications(&home).join("Renamed.app"), EDGE_ID);

        taert(&home)
            .args(["clean", "junk", "--force"])
            .assert()
            .success()
            .stdout(predicate::str::contains("Clean complete"));

        assert!(edge.join("data").exists());
        assert!(!stale.exists());
    }

    #[test]
    fn force_leaves_the_cache_of_a_running_app() {
        let home = tempdir().unwrap();
        let cache = fill(&home, "Library/Caches/com.acme.running");
        age(&cache, 90);
        let _app = RunningApp::start(&applications(&home).join("Acme.app"), "com.acme.running");

        taert(&home)
            .args(["clean", "junk", "--force"])
            .assert()
            .success();

        assert!(cache.join("data").exists());
    }

    #[test]
    fn force_reports_the_note_of_a_refused_read() {
        let home = tempdir().unwrap();
        let _refused = Refused::in_home(&home, SAFARI_CONTAINER);

        taert(&home)
            .args(["clean", "junk", "--force"])
            .assert()
            .success()
            .stdout(predicate::str::contains("Clean complete"))
            .stdout(predicate::str::contains(FULL_DISK_ACCESS).count(1));
    }

    #[test]
    fn plain_mode_prints_a_note_once_through_a_whole_clean() {
        let home = tempdir().unwrap();
        let _refused = Refused::in_home(&home, SAFARI_CONTAINER);
        let cache = fill(&home, "Library/Caches/com.acme.app");

        taert(&home)
            .args(["clean", "junk", "--plain"])
            .write_stdin("all\nyes\n")
            .assert()
            .success()
            .stdout(predicate::str::contains("Clean complete"))
            .stdout(predicate::str::contains(FULL_DISK_ACCESS).count(1));

        assert!(!cache.exists());
    }

    #[test]
    fn plain_mode_names_both_folders_of_a_row_before_it_asks() {
        let home = tempdir().unwrap();
        let outer = fill(&home, SAFARI);
        let container = fill(&home, SAFARI_CONTAINER);

        taert(&home)
            .args(["clean", "junk", "--plain"])
            .write_stdin("all\nno\n")
            .assert()
            .success()
            .stdout(predicate::str::contains(container.display().to_string()).count(2))
            .stdout(predicate::str::contains("Operation cancelled"));

        assert!(outer.exists());
        assert!(container.exists());
    }
}

mod uninstall {
    use super::taert;
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

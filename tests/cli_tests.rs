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
fn the_history_help_says_what_it_does() {
    let home = tempfile::tempdir().unwrap();
    taert(&home)
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Show what taert removed or changed",
        ))
        .stdout(predicate::str::contains("Keep a log of removals").not());
}

#[test]
fn the_help_lists_the_brew_command() {
    let home = tempfile::tempdir().unwrap();
    taert(&home)
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "List Homebrew packages and the space they take",
        ));
}

#[test]
fn brew_help_lists_uninstall() {
    let home = tempfile::tempdir().unwrap();
    taert(&home)
        .args(["brew", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("uninstall"))
        .stdout(predicate::str::contains(
            "Remove one formula through Homebrew",
        ));
}

#[test]
fn clean_help_mentions_orphans() {
    let home = tempfile::tempdir().unwrap();
    taert(&home)
        .args(["clean", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("orphans"));
}

#[test]
fn startup_disable_and_enable_help_name_the_path_of_a_login_item() {
    let home = tempfile::tempdir().unwrap();
    for action in ["disable", "enable"] {
        taert(&home)
            .args(["startup", action, "--help"])
            .assert()
            .success()
            .stdout(predicate::str::contains("path of a login item"));
    }
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
fn older_than_above_100_years_is_refused() {
    let home = tempfile::tempdir().unwrap();
    taert(&home)
        .args(["large", "--older-than", "200000000000000"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("panicked").not());
}

#[test]
fn relative_root_skips_credentials_and_library() {
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path().canonicalize().unwrap();

    let ssh_dir = home_path.join(".ssh");
    let lib_dir = home_path.join("Library/Containers/app");
    let doc_dir = home_path.join("Documents");
    std::fs::create_dir_all(&ssh_dir).unwrap();
    std::fs::create_dir_all(&lib_dir).unwrap();
    std::fs::create_dir_all(&doc_dir).unwrap();

    let data = vec![0u8; 8192];
    std::fs::write(ssh_dir.join("id_big"), &data).unwrap();
    std::fs::write(lib_dir.join("big.bin"), &data).unwrap();
    std::fs::write(doc_dir.join("ok.bin"), &data).unwrap();

    taert(&home)
        .current_dir(&home_path)
        .args(["large", ".", "--json", "--min-size", "1K"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ok.bin"))
        .stdout(predicate::str::contains(".ssh").not())
        .stdout(predicate::str::contains("Containers").not());
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

#[test]
fn compress_refuses_a_folder_where_apps_keep_live_data() {
    let home = tempfile::tempdir().unwrap();
    let data = home
        .path()
        .canonicalize()
        .unwrap()
        .join("Library/Application Support/Foo");
    std::fs::create_dir_all(&data).unwrap();

    taert(&home)
        .args(["compress", "--dry-run"])
        .arg(&data)
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "where apps keep data that changes while they run",
        ));
}

#[test]
fn completion_fish_prints_a_fish_script() {
    let home = tempfile::tempdir().unwrap();
    taert(&home)
        .args(["completion", "fish"])
        .assert()
        .success()
        .stdout(predicate::str::contains("complete -c taert"))
        .stdout(predicate::str::contains("-a \"clean\""))
        .stdout(predicate::str::contains("-l dry-run"));
}

#[test]
fn completion_zsh_prints_a_zsh_script() {
    let home = tempfile::tempdir().unwrap();
    taert(&home)
        .args(["completion", "zsh"])
        .assert()
        .success()
        .stdout(predicate::str::contains("#compdef taert"));
}

#[test]
fn completion_bash_prints_a_bash_script() {
    let home = tempfile::tempdir().unwrap();
    taert(&home)
        .args(["completion", "bash"])
        .assert()
        .success()
        .stdout(predicate::str::contains("_taert"));
}

#[test]
fn completion_of_an_unknown_shell_fails() {
    let home = tempfile::tempdir().unwrap();
    taert(&home)
        .args(["completion", "nosuchshell"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("invalid value"));
}

#[test]
fn completion_without_a_shell_fails() {
    let home = tempfile::tempdir().unwrap();
    taert(&home)
        .args(["completion"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("<SHELL>"));
}

#[test]
fn completion_values_come_from_clap() {
    let home = tempfile::tempdir().unwrap();
    // The fish generator does not complete positional values, so zsh and bash prove it.
    taert(&home)
        .args(["completion", "zsh"])
        .assert()
        .success()
        .stdout(predicate::str::contains("(dev junk ai trash orphans)"));
    taert(&home)
        .args(["completion", "bash"])
        .assert()
        .success()
        .stdout(predicate::str::contains("dev junk ai trash orphans"));
}

#[test]
fn completion_leaves_the_home_folder_empty() {
    let home = tempfile::tempdir().unwrap();
    taert(&home).args(["completion", "zsh"]).assert().success();

    assert_eq!(
        std::fs::read_dir(home.path()).unwrap().count(),
        0,
        "home folder should have no entries after completion"
    );
}

#[test]
fn status_json_prints_one_valid_document() {
    let home = tempfile::tempdir().unwrap();
    let assert = taert(&home).args(["status", "--json"]).assert().success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let val: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(val["schema_version"], 1);
    let collected_at = val["collected_at"]
        .as_str()
        .expect("collected_at is a string");
    assert!(collected_at.ends_with('Z'));
    assert!(val.get("health").is_some());
    assert!(val.get("machine").is_some());
    assert!(val.get("memory").is_some());
    assert!(val.get("storage").is_some());
    assert!(val.get("live").is_some());
    let macos_version = val["machine"]["macos_version"]
        .as_str()
        .expect("macos_version string");
    assert!(!macos_version.is_empty());
}

#[test]
fn status_json_reads_the_cpu_twice() {
    let home = tempfile::tempdir().unwrap();
    let assert = taert(&home).args(["status", "--json"]).assert().success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let val: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert!(val["live"]["cpu"]["usage_percent"].is_number());
}

#[test]
fn status_json_has_no_escape_codes() {
    let home = tempfile::tempdir().unwrap();
    let assert = taert(&home).args(["status", "--json"]).assert().success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(!stdout.contains('\u{1b}'));
}

#[test]
fn status_without_flags_is_plain_when_piped() {
    let home = tempfile::tempdir().unwrap();
    let assert = taert(&home).args(["status"]).assert().success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(stdout.starts_with("Health:"));
    assert!(stdout.contains("CPU:"));
    assert!(!stdout.contains('\u{1b}'));
    assert!(serde_json::from_str::<serde_json::Value>(&stdout).is_err());
}

#[test]
fn status_json_wins_over_plain() {
    let home = tempfile::tempdir().unwrap();
    let assert = taert(&home)
        .args(["status", "--json", "--plain"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let val = serde_json::from_str::<serde_json::Value>(&stdout);
    assert!(val.is_ok());
}

#[test]
fn status_appears_in_help() {
    let home = tempfile::tempdir().unwrap();
    taert(&home)
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("status"));
}

#[test]
fn ignore_add_with_dry_run_saves_nothing() {
    let home = tempfile::tempdir().unwrap();
    let keep = home.path().canonicalize().unwrap().join("keep");
    taert(&home)
        .args(["ignore", "add"])
        .arg(&keep)
        .arg("--dry-run")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "Error: --dry-run does not work with taert ignore",
        ));
    assert!(
        !home
            .path()
            .join("Library/Application Support/taert/ignore.toml")
            .exists()
    );
}

#[test]
fn the_admin_helper_answers_a_user_process_with_a_json_error() {
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    taert(&home)
        .args(["admin-helper", "compress", "--home"])
        .arg(&home_path)
        .arg(home_path.join("Applications/Foo.app"))
        .assert()
        .success()
        .stdout("{\"error\":\"admin-helper must run as root\"}\n");
}

#[test]
fn the_admin_helper_answers_a_user_process_that_slims_with_a_json_error() {
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    taert(&home)
        .args([
            "admin-helper",
            "slim",
            "--owner",
            "501",
            "--group",
            "20",
            "--home",
        ])
        .arg(&home_path)
        .arg(home_path.join("Applications/Foo.app"))
        .assert()
        .success()
        .stdout("{\"error\":\"admin-helper must run as root\",\"step\":1}\n");
}

/// Runs the admin helper with `args` and returns the text of its one `{"error"}` answer.
fn admin_helper_error(args: &[&str]) -> String {
    let home = tempfile::tempdir().unwrap();
    let output = taert(&home)
        .arg("admin-helper")
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let answer: serde_json::Value = serde_json::from_slice(&output).expect("one JSON object");
    let object = answer.as_object().expect("a JSON object");
    assert_eq!(object.len(), 1, "{answer}");
    object["error"].as_str().expect("an error text").to_string()
}

#[test]
fn the_admin_helper_answers_missing_arguments_with_a_json_error() {
    let error = admin_helper_error(&["compress", "/Applications/Foo.app"]);
    assert!(error.contains("--home"), "{error}");
}

#[test]
fn the_admin_helper_answers_help_with_a_json_error() {
    let error = admin_helper_error(&["--help"]);
    assert!(error.contains("compress"), "{error}");
}

#[test]
fn config_with_dry_run_saves_nothing() {
    let home = tempfile::tempdir().unwrap();
    taert(&home)
        .args(["config", "--appearance", "dark", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "Error: --dry-run does not work with taert config",
        ));
    assert!(
        !home
            .path()
            .join("Library/Application Support/taert/config.toml")
            .exists()
    );
}

#[test]
fn purge_force_leaves_a_folder_found_by_a_custom_name() {
    let home = tempfile::tempdir().unwrap();
    let config_dir = home.path().join("Library/Application Support/taert");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        "extra_artifacts = [\"out\"]\n",
    )
    .unwrap();
    let project = home.path().join("code/app");
    for name in ["out", "node_modules"] {
        let dir = project.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("file"), vec![1u8; 2048]).unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 86400);
        std::fs::File::open(&dir)
            .unwrap()
            .set_modified(old)
            .unwrap();
    }

    let listing = taert(&home)
        .args(["purge", "--json"])
        .arg(home.path().join("code"))
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let listing: serde_json::Value = serde_json::from_slice(&listing).expect("valid JSON");
    let selected_of = |name: &str| -> bool {
        let suffix = format!("app/{name}");
        let item = listing["groups"]
            .as_array()
            .expect("groups")
            .iter()
            .flat_map(|group| group["items"].as_array().expect("items"))
            .find(|item| {
                item["path"]
                    .as_str()
                    .is_some_and(|path| std::path::Path::new(path).ends_with(&suffix))
            })
            .unwrap_or_else(|| panic!("{name} not listed: {listing}"));
        item["selected"].as_bool().expect("a selected flag")
    };
    assert!(!selected_of("out"), "{listing}");
    assert!(selected_of("node_modules"), "{listing}");

    taert(&home)
        .args(["purge", "--force", "--permanent"])
        .arg(home.path().join("code"))
        .assert()
        .success();

    assert!(project.join("out/file").exists());
    assert!(!project.join("node_modules").exists());
}

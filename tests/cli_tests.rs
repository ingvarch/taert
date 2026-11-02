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
        .stdout(predicate::str::contains("(dev junk ai trash)"));
    taert(&home)
        .args(["completion", "bash"])
        .assert()
        .success()
        .stdout(predicate::str::contains("dev junk ai trash"));
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

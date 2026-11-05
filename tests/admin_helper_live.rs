//! Runs the admin helper as real root on a throwaway root-owned bundle in /Applications.
//!
//! Ignored by default, because it needs sudo (it asks for your password in the terminal)
//! and writes in /Applications. Only the owner runs it by hand, never an agent.
//! `cargo test --test admin_helper_live -- --ignored --nocapture`

use std::fs;
use std::os::darwin::fs::MetadataExt;
use std::path::Path;
use std::process::{Command, Output};
use taert::core::compress::UF_COMPRESSED;
use tempfile::tempdir;

const BUNDLE: &str = "/Applications/Taert Helper Fixture.app";
const FILE: &str = "/Applications/Taert Helper Fixture.app/Contents/Resources/data.txt";

fn sudo(args: &[&str]) -> Output {
    let output = Command::new("/usr/bin/sudo").args(args).output().unwrap();
    assert!(output.status.success(), "sudo {args:?}: {output:?}");
    output
}

fn helper(home: &Path, undo: bool) -> serde_json::Value {
    let mut args = vec![env!("CARGO_BIN_EXE_taert"), "admin-helper", "compress"];
    if undo {
        args.push("--undo");
    }
    let home = home.to_str().unwrap();
    args.extend(["--home", home, BUNDLE]);
    let output = sudo(&args);
    serde_json::from_slice(&output.stdout).unwrap()
}

struct Fixture;

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = Command::new("/usr/bin/sudo")
            .args(["/bin/rm", "-rf", BUNDLE])
            .status();
    }
}

#[test]
#[ignore = "needs sudo and writes in /Applications"]
fn the_helper_compresses_and_unpacks_a_root_owned_bundle() {
    let source = tempdir().unwrap();
    let resources = source
        .path()
        .join("Taert Helper Fixture.app/Contents/Resources");
    fs::create_dir_all(&resources).unwrap();
    let data = "Hello APFS transparent decmpfs compression!\n".repeat(8_000);
    fs::write(resources.join("data.txt"), &data).unwrap();
    let source_bundle = source.path().join("Taert Helper Fixture.app");

    let _fixture = Fixture;
    sudo(&["/bin/rm", "-rf", BUNDLE]);
    sudo(&["/bin/cp", "-R", source_bundle.to_str().unwrap(), BUNDLE]);
    sudo(&["/usr/sbin/chown", "-R", "root:wheel", BUNDLE]);
    sudo(&["/bin/chmod", "-R", "go-w", BUNDLE]);

    let home = tempdir().unwrap();
    let answer = helper(home.path(), false);
    assert_eq!(answer["processed_files"], 1, "{answer}");
    assert_eq!(answer["failures"].as_array().unwrap().len(), 0, "{answer}");
    let meta = fs::symlink_metadata(FILE).unwrap();
    assert_eq!((meta.st_uid(), meta.st_gid()), (0, 0));
    assert_ne!(meta.st_flags() & UF_COMPRESSED, 0);

    let answer = helper(home.path(), true);
    assert_eq!(answer["processed_files"], 1, "{answer}");
    assert_eq!(answer["undo"], true, "{answer}");
    let meta = fs::symlink_metadata(FILE).unwrap();
    assert_eq!((meta.st_uid(), meta.st_gid()), (0, 0));
    assert_eq!(meta.st_flags() & UF_COMPRESSED, 0);
    assert_eq!(fs::read_to_string(FILE).unwrap(), data);
}

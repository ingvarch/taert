//! Runs the admin helper as real root on throwaway root-owned bundles in /Applications.
//!
//! Ignored by default, because it needs sudo (it asks for your password in the terminal)
//! and writes in /Applications and /Library/Application Support/taert. Only the owner runs it
//! by hand, never an agent.
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

const SLIM_BUNDLE: &str = "/Applications/Taert Slim Fixture.app";
const HOLDINGS: &str = "/Library/Application Support/taert/slim";

/// The holding folders root has now. A slim that ends leaves none behind.
fn holdings() -> usize {
    fs::read_dir(HOLDINGS).map_or(0, |entries| entries.count())
}

struct SlimFixture;

impl Drop for SlimFixture {
    fn drop(&mut self) {
        let _ = Command::new("/usr/bin/sudo")
            .args(["/bin/rm", "-rf", SLIM_BUNDLE])
            .status();
    }
}

fn build_universal_app(bundle: &Path) {
    let macos = bundle.join("Contents/MacOS");
    fs::create_dir_all(&macos).unwrap();
    let source = bundle.join("main.c");
    fs::write(&source, "int main(void) { return 0; }\n").unwrap();
    let built = Command::new("/usr/bin/cc")
        .args(["-arch", "arm64", "-arch", "x86_64"])
        .arg(&source)
        .arg("-o")
        .arg(macos.join("fixture"))
        .status()
        .unwrap();
    assert!(built.success());
    fs::remove_file(source).unwrap();
    let plist = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\
        <key>CFBundleIdentifier</key><string>is.taert.test.slimfixture</string>\
        <key>CFBundleExecutable</key><string>fixture</string></dict></plist>";
    fs::write(bundle.join("Contents/Info.plist"), plist).unwrap();
    let signed = Command::new("/usr/bin/codesign")
        .args(["-s", "-", "--force", "--deep"])
        .arg(bundle)
        .status()
        .unwrap();
    assert!(signed.success());
}

fn helper_job(args: &[&str]) -> serde_json::Value {
    let mut command = vec![env!("CARGO_BIN_EXE_taert"), "admin-helper"];
    command.extend(args);
    serde_json::from_slice(&sudo(&command).stdout).unwrap()
}

/// An ad hoc signed app has no Developer ID, so the helper must refuse it before it writes.
#[test]
#[ignore = "needs sudo and writes in /Applications"]
fn the_helper_refuses_an_ad_hoc_signed_app_before_it_writes() {
    let source = tempdir().unwrap();
    let source_bundle = source.path().join("Taert Slim Fixture.app");
    build_universal_app(&source_bundle);

    let _fixture = SlimFixture;
    sudo(&["/bin/rm", "-rf", SLIM_BUNDLE]);
    sudo(&[
        "/bin/cp",
        "-R",
        source_bundle.to_str().unwrap(),
        SLIM_BUNDLE,
    ]);
    sudo(&["/usr/sbin/chown", "-R", "root:wheel", SLIM_BUNDLE]);
    sudo(&["/bin/chmod", "-R", "go-w", SLIM_BUNDLE]);
    let held = holdings();

    let home = tempdir().unwrap();
    let (uid, gid) = unsafe { (libc::getuid().to_string(), libc::getgid().to_string()) };
    let answer = helper_job(&[
        "slim",
        "--home",
        home.path().to_str().unwrap(),
        "--owner",
        &uid,
        "--group",
        &gid,
        SLIM_BUNDLE,
    ]);

    assert_eq!(answer["step"], 1, "{answer}");
    assert_eq!(
        answer["error"], "Not signed with a Developer ID",
        "{answer}"
    );
    assert_eq!(holdings(), held);
    assert!(!Path::new("/Applications/Taert Slim Fixture.taert-old.app").exists());
}

/// Removes the root-owned copy of the fixture and the old copy the helper left.
struct FixtureCopy(Vec<String>);

impl Drop for FixtureCopy {
    fn drop(&mut self) {
        let _ = Command::new("/usr/bin/sudo")
            .args(["/bin/rm", "-rf"])
            .args(&self.0)
            .status();
    }
}

fn archs(binary: &Path) -> String {
    let output = Command::new("/usr/bin/lipo")
        .arg("-archs")
        .arg(binary)
        .output()
        .unwrap();
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

/// Slims a root-owned copy of a universal app that has a Developer ID signature and passes
/// Gatekeeper. Only the owner can make such an app: set `TAERT_SLIM_FIXTURE` to the path of a
/// signed, notarized, universal `.app` with a name that is not in /Applications yet. The copy
/// and the old copy in /Applications are removed at the end; nothing goes to the Trash.
#[test]
#[ignore = "needs sudo, writes in /Applications, and a Developer ID signed universal app"]
fn the_helper_slims_a_root_owned_app_and_moves_the_old_copy_to_applications_as_the_users() {
    let source = std::env::var("TAERT_SLIM_FIXTURE")
        .expect("set TAERT_SLIM_FIXTURE to a Developer ID signed universal .app");
    let source = Path::new(&source);
    let name = source.file_name().unwrap().to_str().unwrap();
    let stem = source.file_stem().unwrap().to_str().unwrap();
    let bundle = format!("/Applications/{name}");
    let old_app = format!("/Applications/{stem}.taert-old.app");
    assert!(
        !Path::new(&bundle).exists() && !Path::new(&old_app).exists(),
        "{bundle} or {old_app} is already there; use a fixture with another name"
    );

    let _copy = FixtureCopy(vec![bundle.clone(), old_app.clone()]);
    sudo(&["/usr/bin/ditto", source.to_str().unwrap(), &bundle]);
    sudo(&["/usr/sbin/chown", "-R", "root:wheel", &bundle]);
    sudo(&["/bin/chmod", "-R", "go-w", &bundle]);
    let macos = Path::new(&bundle).join("Contents/MacOS");
    let binary = fs::read_dir(&macos)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(
        archs(&binary).contains("x86_64"),
        "the fixture must be universal"
    );
    let held = holdings();

    let home = tempdir().unwrap();
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let answer = helper_job(&[
        "slim",
        "--home",
        home.path().to_str().unwrap(),
        "--owner",
        &uid.to_string(),
        "--group",
        &gid.to_string(),
        &bundle,
    ]);

    assert!(answer["saved"].as_u64().unwrap() > 0, "{answer}");
    assert_eq!(answer["old_app"], old_app.as_str(), "{answer}");
    assert!(answer.get("handover_error").is_none(), "{answer}");
    assert_eq!(archs(&binary), "arm64");
    assert_eq!(fs::symlink_metadata(&bundle).unwrap().st_uid(), 0);
    let old = fs::symlink_metadata(&old_app).unwrap();
    assert_eq!((old.st_uid(), old.st_gid()), (uid, gid));
    assert_eq!(holdings(), held);
    assert!(
        archs(
            &Path::new(&old_app)
                .join("Contents/MacOS")
                .join(binary.file_name().unwrap())
        )
        .contains("x86_64")
    );
}

//! Live test against a universal app bundle built, ad-hoc signed, and slimmed on the running Mac.
//!
//! Marked `#[ignore]` by default because it compiles binaries and calls macOS code signing tools:
//! `cargo test --test slim_live -- --ignored`
//!
//! It never starts a program from inside the app bundle, which would make macOS show a "damaged"
//! alert, and it keeps the replaced original in a temporary folder instead of the real Trash.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tempfile::tempdir;

use taert::core::apps::InstalledApp;
use taert::core::host::{Host, MacHost, Output};
use taert::core::slim;

/// Runs the real signing tools and moves "trashed" items into a temporary folder.
struct TempTrashHost {
    trash: PathBuf,
}

impl Host for TempTrashHost {
    fn run(&self, program: &str, args: &[&str], timeout: Duration) -> Output {
        MacHost.run(program, args, timeout)
    }

    fn run_admin(&self, script: &str, prompt: &str) -> Result<(), String> {
        MacHost.run_admin(script, prompt)
    }

    fn run_admin_output(
        &self,
        script: &str,
        prompt: &str,
        timeout: Duration,
    ) -> Result<String, String> {
        MacHost.run_admin_output(script, prompt, timeout)
    }

    fn raw_signal(&self, pid: u32, signal: i32) -> Result<(), String> {
        MacHost.raw_signal(pid, signal)
    }

    fn pause(&self, duration: Duration) {
        MacHost.pause(duration);
    }

    fn trash(&self, path: &Path) -> Result<(), String> {
        fs::create_dir_all(&self.trash).map_err(|e| e.to_string())?;
        let name = path.file_name().ok_or("path has no name")?;
        fs::rename(path, self.trash.join(name)).map_err(|e| e.to_string())
    }

    fn uid(&self) -> u32 {
        MacHost.uid()
    }

    fn pid(&self) -> u32 {
        MacHost.pid()
    }
}

/// Runs a copy of the executable outside the bundle.
fn run_outside_the_bundle(exe: &Path, scratch: &Path) -> std::process::Output {
    let copy = scratch.join("liveapp-copy");
    fs::copy(exe, &copy).unwrap();
    Command::new(&copy)
        .output()
        .expect("Execution outside the bundle failed")
}

fn build_universal_app(bundle: &Path, id: &str) {
    let macos = bundle.join("Contents/MacOS");
    fs::create_dir_all(&macos).unwrap();

    let c_file = bundle.join("main.c");
    fs::write(
        &c_file,
        r#"#include <stdio.h>
int main(void) {
    printf("SLIM_LIVE_TEST_SUCCESS\n");
    return 0;
}
"#,
    )
    .unwrap();

    let exe_path = macos.join("liveapp");
    let cc_status = Command::new("cc")
        .args([
            "-arch",
            "arm64",
            "-arch",
            "x86_64",
            c_file.to_str().unwrap(),
            "-o",
            exe_path.to_str().unwrap(),
        ])
        .status()
        .expect("cc compilation failed");
    assert!(cc_status.success(), "Failed to build universal binary");

    let _ = fs::remove_file(c_file);

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>{id}</string>
<key>CFBundleExecutable</key><string>liveapp</string>
</dict></plist>"#
    );
    fs::write(bundle.join("Contents/Info.plist"), plist).unwrap();

    // Sign the app ad hoc
    let sign_status = Command::new("codesign")
        .args(["-s", "-", "--force", "--deep", bundle.to_str().unwrap()])
        .status()
        .expect("codesign failed");
    assert!(sign_status.success(), "Failed to ad-hoc sign universal app");
}

#[test]
#[ignore]
fn test_live_build_and_slim_universal_app() {
    let temp = tempdir().unwrap();
    let app_path = temp.path().join("LiveUniversalApp.app");
    build_universal_app(&app_path, "is.taert.test.liveapp");

    let exe_path = app_path.join("Contents/MacOS/liveapp");
    assert!(exe_path.exists());

    // Verify it runs before slimming
    let scratch = temp.path().join("scratch");
    fs::create_dir_all(&scratch).unwrap();
    let out_before = run_outside_the_bundle(&exe_path, &scratch);
    assert!(out_before.status.success());
    assert!(String::from_utf8_lossy(&out_before.stdout).contains("SLIM_LIVE_TEST_SUCCESS"));

    // Verify it is a universal binary with 2 architectures before slimming
    let header_before = slim::parse_fat_header_from_file(&exe_path)
        .unwrap()
        .expect("Expected fat header before slimming");
    assert_eq!(header_before.slices.len(), 2);
    assert!(
        header_before
            .slices
            .iter()
            .any(|s| s.cputype == slim::CPU_TYPE_X86_64)
    );
    assert!(
        header_before
            .slices
            .iter()
            .any(|s| s.cputype == slim::CPU_TYPE_ARM64)
    );

    let app = InstalledApp {
        name: "LiveUniversalApp".to_string(),
        path: app_path.clone(),
        bundle_id: Some("is.taert.test.liveapp".to_string()),
        executable: Some("liveapp".to_string()),
        size_bytes: fs::metadata(&exe_path).unwrap().len(),
        last_used: None,
    };

    let (binaries, intel_size) = slim::find_universal_binaries(&app_path);
    assert_eq!(binaries.len(), 1);
    assert!(intel_size > 0);

    // Slim the app skipping Developer ID check (ad hoc signed)
    let host = TempTrashHost {
        trash: temp.path().join("trash"),
    };
    let saved =
        slim::slim_app(&app, &binaries, &host, true).expect("Slimming live universal app failed");
    assert_eq!(saved, intel_size);

    // Verify it still exists at the original path
    assert!(app_path.exists());
    assert!(exe_path.exists());

    // Verify that the rewritten binary has only the ARM slice
    let header_after = slim::parse_fat_header_from_file(&exe_path)
        .unwrap()
        .expect("Expected fat header after slimming");
    assert_eq!(header_after.slices.len(), 1);
    assert_eq!(header_after.slices[0].cputype, slim::CPU_TYPE_ARM64);

    // Verify the slimmed app executes successfully and prints output
    let out_after = run_outside_the_bundle(&exe_path, &scratch);
    assert!(out_after.status.success());
    assert!(String::from_utf8_lossy(&out_after.stdout).contains("SLIM_LIVE_TEST_SUCCESS"));

    // Verify codesign verification on the slimmed app bundle passes
    let verify_status = Command::new("codesign")
        .args(["--verify", "--deep", "--strict", app_path.to_str().unwrap()])
        .status()
        .expect("codesign verification command failed");
    assert!(
        verify_status.success(),
        "Codesign verification of slimmed app failed"
    );
}

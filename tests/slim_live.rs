//! Live test against a universal app bundle built, ad-hoc signed, and slimmed on the running Mac.
//!
//! Marked `#[ignore]` by default because it compiles binaries and calls macOS code signing tools:
//! `cargo test --test slim_live -- --ignored`

use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::tempdir;

use taert::core::apps::InstalledApp;
use taert::core::host::MacHost;
use taert::core::slim;

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
    let out_before = Command::new(&exe_path)
        .output()
        .expect("Execution before slimming failed");
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
    let saved = slim::slim_app(&app, &binaries, &MacHost, true)
        .expect("Slimming live universal app failed");
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
    let out_after = Command::new(&exe_path)
        .output()
        .expect("Execution after slimming failed");
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

//! App slimming engine to remove Intel code from universal macOS app bundles.
//!
//! On Apple Silicon Macs, universal apps contain Intel (x86_64) slices that are never executed
//! unless the app is explicitly configured to run under Rosetta. This module identifies qualifying
//! universal apps, strips the Intel architecture slices from Mach-O fat binaries while preserving
//! ARM code and code signatures, and safely moves the original bundle to the Trash.

use std::collections::HashSet;
use std::ffi::CString;
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::Duration;

use plist::Value;
use serde::{Deserialize, Serialize};

use super::apps::{self, InstalledApp};
use super::host::Host;
use super::layout::Layout;
use super::safety::{FileIdentity, Safety};
use super::scanner::{CategoryScan, ScanItem, ScanProgressEvent, ToolGroup};
use super::stores::IgnoreStore;

pub const SCAN_ID: &str = "slim";

pub const FAT_MAGIC: u32 = 0xCAFEBABE;
pub const FAT_MAGIC_64: u32 = 0xCAFEBABF;
pub const CPU_TYPE_I386: u32 = 7;
pub const CPU_TYPE_X86_64: u32 = 0x01000007;
pub const CPU_TYPE_ARM64: u32 = 0x0100000c;

pub const APP_MANAGEMENT_HINT: &str = "macOS refused. Allow your terminal in System Settings > Privacy & Security > App Management, or move the app to the Trash in Finder";

const RENAME_SWAP: libc::c_uint = 0x0000_0002;

unsafe extern "C" {
    fn renamex_np(
        oldpath: *const libc::c_char,
        newpath: *const libc::c_char,
        flags: libc::c_uint,
    ) -> libc::c_int;
    fn removexattr(
        path: *const libc::c_char,
        name: *const libc::c_char,
        options: libc::c_int,
    ) -> libc::c_int;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FatSlice {
    pub cputype: u32,
    pub cpusubtype: u32,
    pub offset: u64,
    pub size: u64,
    pub align: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FatHeader {
    pub is_64: bool,
    pub slices: Vec<FatSlice>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SlimError {
    #[error("not a fat binary")]
    NotFatBinary,
    #[error("truncated fat header")]
    TruncatedHeader,
    #[error("Java class file detected, not a Mach-O fat binary")]
    JavaClassFile,
    #[error("no ARM architecture slice found")]
    NoArmSlice,
    #[error("no Intel architecture slice found")]
    NoIntelSlice,
    #[error("invalid slice bounds")]
    InvalidSliceBounds,
    #[error("I/O error: {0}")]
    Io(String),
}

/// Parses the fat header from an in-memory byte buffer.
/// Returns `Ok(None)` if the magic does not match or if the file is a Java class file (nfat_arch >= 8).
pub fn parse_fat_header(data: &[u8]) -> Result<Option<FatHeader>, SlimError> {
    if data.len() < 8 {
        return Ok(None);
    }
    let magic = u32::from_be_bytes(data[0..4].try_into().unwrap());
    let is_64 = match magic {
        FAT_MAGIC => false,
        FAT_MAGIC_64 => true,
        _ => return Ok(None),
    };

    let nfat_arch = u32::from_be_bytes(data[4..8].try_into().unwrap());
    // Java class files start with 0xCAFEBABE and have minor/major versions as the next 4 bytes.
    // Legitimate Mach-O universal binaries contain a small number of architectures (< 8).
    if nfat_arch == 0 || nfat_arch >= 8 {
        return Ok(None);
    }

    let record_size = if is_64 { 32 } else { 20 };
    let required_len = 8 + (nfat_arch as usize) * record_size;
    if data.len() < required_len {
        return Err(SlimError::TruncatedHeader);
    }

    let mut slices = Vec::with_capacity(nfat_arch as usize);
    let mut offset = 8;
    for _ in 0..nfat_arch {
        let cputype = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap());
        let cpusubtype = u32::from_be_bytes(data[offset + 4..offset + 8].try_into().unwrap());
        let (slice_offset, size, align) = if is_64 {
            let slice_offset =
                u64::from_be_bytes(data[offset + 8..offset + 16].try_into().unwrap());
            let size = u64::from_be_bytes(data[offset + 16..offset + 24].try_into().unwrap());
            let align = u32::from_be_bytes(data[offset + 24..offset + 28].try_into().unwrap());
            offset += 32;
            (slice_offset, size, align)
        } else {
            let slice_offset =
                u32::from_be_bytes(data[offset + 8..offset + 12].try_into().unwrap()) as u64;
            let size =
                u32::from_be_bytes(data[offset + 12..offset + 16].try_into().unwrap()) as u64;
            let align = u32::from_be_bytes(data[offset + 16..offset + 20].try_into().unwrap());
            offset += 20;
            (slice_offset, size, align)
        };

        if slice_offset.saturating_add(size) > data.len() as u64 {
            return Err(SlimError::InvalidSliceBounds);
        }

        slices.push(FatSlice {
            cputype,
            cpusubtype,
            offset: slice_offset,
            size,
            align,
        });
    }

    Ok(Some(FatHeader { is_64, slices }))
}

/// Reads only the initial header bytes of a file to check for fat binary structure.
pub fn parse_fat_header_from_file(path: &Path) -> Result<Option<FatHeader>, SlimError> {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return Ok(None),
    };
    let Ok(meta) = file.metadata() else {
        return Ok(None);
    };
    let file_len = meta.len();
    if file_len < 8 {
        return Ok(None);
    }

    let mut buf = [0u8; 4096];
    let to_read = (file_len as usize).min(buf.len());
    if file.read_exact(&mut buf[..to_read]).is_err() {
        return Ok(None);
    }

    let magic = u32::from_be_bytes(buf[0..4].try_into().unwrap());
    let is_64 = match magic {
        FAT_MAGIC => false,
        FAT_MAGIC_64 => true,
        _ => return Ok(None),
    };

    let nfat_arch = u32::from_be_bytes(buf[4..8].try_into().unwrap());
    if nfat_arch == 0 || nfat_arch >= 8 {
        return Ok(None);
    }

    let record_size = if is_64 { 32 } else { 20 };
    let required_len = 8 + (nfat_arch as usize) * record_size;
    if to_read < required_len {
        return Err(SlimError::TruncatedHeader);
    }

    let mut slices = Vec::with_capacity(nfat_arch as usize);
    let mut offset = 8;
    for _ in 0..nfat_arch {
        let cputype = u32::from_be_bytes(buf[offset..offset + 4].try_into().unwrap());
        let cpusubtype = u32::from_be_bytes(buf[offset + 4..offset + 8].try_into().unwrap());
        let (slice_offset, size, align) = if is_64 {
            let slice_offset = u64::from_be_bytes(buf[offset + 8..offset + 16].try_into().unwrap());
            let size = u64::from_be_bytes(buf[offset + 16..offset + 24].try_into().unwrap());
            let align = u32::from_be_bytes(buf[offset + 24..offset + 28].try_into().unwrap());
            offset += 32;
            (slice_offset, size, align)
        } else {
            let slice_offset =
                u32::from_be_bytes(buf[offset + 8..offset + 12].try_into().unwrap()) as u64;
            let size = u32::from_be_bytes(buf[offset + 12..offset + 16].try_into().unwrap()) as u64;
            let align = u32::from_be_bytes(buf[offset + 16..offset + 20].try_into().unwrap());
            offset += 20;
            (slice_offset, size, align)
        };

        if slice_offset.saturating_add(size) > file_len {
            return Err(SlimError::InvalidSliceBounds);
        }

        slices.push(FatSlice {
            cputype,
            cpusubtype,
            offset: slice_offset,
            size,
            align,
        });
    }

    Ok(Some(FatHeader { is_64, slices }))
}

/// Rewrites a fat Mach-O binary by keeping all ARM slices and stripping x86_64/i386 slices.
///
/// Keeps ARM slices byte-for-byte at their required power-of-two alignments.
/// Refuses files that have no ARM slice.
pub fn rewrite_fat_binary(data: &[u8]) -> Result<Vec<u8>, SlimError> {
    let Some(header) = parse_fat_header(data)? else {
        return Err(SlimError::NotFatBinary);
    };

    let remaining: Vec<&FatSlice> = header
        .slices
        .iter()
        .filter(|s| s.cputype == CPU_TYPE_ARM64)
        .collect();

    if remaining.is_empty() {
        return Err(SlimError::NoArmSlice);
    }

    let is_64 = header.is_64;
    let magic = if is_64 { FAT_MAGIC_64 } else { FAT_MAGIC };
    let record_size = if is_64 { 32 } else { 20 };
    let header_size = 8 + remaining.len() * record_size;

    let mut current_offset = header_size as u64;
    let mut new_slices = Vec::with_capacity(remaining.len());

    for slice in &remaining {
        let align_bytes = 1u64 << slice.align;
        current_offset = (current_offset + align_bytes - 1) & !(align_bytes - 1);
        new_slices.push(FatSlice {
            cputype: slice.cputype,
            cpusubtype: slice.cpusubtype,
            offset: current_offset,
            size: slice.size,
            align: slice.align,
        });
        current_offset += slice.size;
    }

    let mut out = Vec::with_capacity(current_offset as usize);
    out.extend_from_slice(&magic.to_be_bytes());
    out.extend_from_slice(&(new_slices.len() as u32).to_be_bytes());

    for slice in &new_slices {
        out.extend_from_slice(&slice.cputype.to_be_bytes());
        out.extend_from_slice(&slice.cpusubtype.to_be_bytes());
        if is_64 {
            out.extend_from_slice(&slice.offset.to_be_bytes());
            out.extend_from_slice(&slice.size.to_be_bytes());
            out.extend_from_slice(&slice.align.to_be_bytes());
            out.extend_from_slice(&0u32.to_be_bytes()); // reserved
        } else {
            out.extend_from_slice(&(slice.offset as u32).to_be_bytes());
            out.extend_from_slice(&(slice.size as u32).to_be_bytes());
            out.extend_from_slice(&slice.align.to_be_bytes());
        }
    }

    for (orig_slice, new_slice) in remaining.iter().zip(&new_slices) {
        if out.len() < new_slice.offset as usize {
            out.resize(new_slice.offset as usize, 0);
        }
        let start = orig_slice.offset as usize;
        let end = start + orig_slice.size as usize;
        out.extend_from_slice(&data[start..end]);
    }

    Ok(out)
}

/// Reads relative paths sealed by file hash from a bundle's `_CodeSignature/CodeResources`.
/// Files sealed by file hash must NOT be modified, as doing so breaks the bundle seal.
pub fn read_sealed_hashes(code_resources_path: &Path) -> HashSet<String> {
    let mut sealed = HashSet::new();
    let Ok(Value::Dictionary(dict)) = Value::from_file(code_resources_path) else {
        return sealed;
    };
    for key in ["files2", "files"] {
        if let Some(Value::Dictionary(files_dict)) = dict.get(key) {
            for (path, val) in files_dict {
                if let Value::Dictionary(entry) = val {
                    let has_file_hash = entry.contains_key("hash") || entry.contains_key("hash2");
                    let has_cdhash = entry.contains_key("cdhash");
                    if has_file_hash && !has_cdhash {
                        sealed.insert(path.clone());
                    }
                }
            }
        }
    }
    sealed
}

/// Discovers universal Mach-O binaries in an app bundle that qualify for slimming.
/// Returns the list of qualifying binaries and the total Intel code size in bytes.
pub fn find_universal_binaries(bundle: &Path) -> (Vec<PathBuf>, u64) {
    let code_resources = bundle.join("Contents/_CodeSignature/CodeResources");
    let sealed_paths = if code_resources.exists() {
        read_sealed_hashes(&code_resources)
    } else {
        HashSet::new()
    };

    let contents = bundle.join("Contents");
    if !contents.exists() {
        return (Vec::new(), 0);
    }

    let mut binaries = Vec::new();
    let mut total_intel_size = 0u64;

    for entry in jwalk::WalkDir::new(&contents)
        .skip_hidden(false)
        .follow_links(false)
        .into_iter()
        .flatten()
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.is_symlink() {
            continue;
        }

        if let Ok(rel) = path.strip_prefix(&contents) {
            let rel_str = rel.to_string_lossy();
            if sealed_paths.contains(rel_str.as_ref()) {
                continue;
            }
        }

        if let Ok(Some(header)) = parse_fat_header_from_file(&path) {
            let has_arm = header.slices.iter().any(|s| s.cputype == CPU_TYPE_ARM64);
            let intel_size: u64 = header
                .slices
                .iter()
                .filter(|s| s.cputype == CPU_TYPE_X86_64 || s.cputype == CPU_TYPE_I386)
                .map(|s| s.size)
                .sum();

            if has_arm && intel_size > 0 {
                binaries.push(path);
                total_intel_size += intel_size;
            }
        }
    }

    (binaries, total_intel_size)
}

/// Reads the set of bundle identifiers configured to "Open using Rosetta" in LaunchServices preferences.
pub fn rosetta_bundle_ids(layout: &Layout) -> HashSet<String> {
    let plist_path =
        layout.user("Library/Preferences/com.apple.LaunchServices/com.apple.LaunchServices.plist");
    let mut ids = HashSet::new();
    if let Ok(Value::Dictionary(dict)) = Value::from_file(&plist_path)
        && let Some(Value::Dictionary(rosetta_dict)) = dict.get("LSArchitecturesForX86_64")
    {
        for key in rosetta_dict.keys() {
            ids.insert(key.clone());
        }
    }
    ids
}

/// Checks whether the running machine is an Apple Silicon Mac using `sysctl hw.optional.arm64`.
pub fn is_apple_silicon(host: &dyn Host) -> bool {
    let out = host.run("sysctl", &["hw.optional.arm64"], Duration::from_secs(2));
    if out.ok && (out.stdout.contains("hw.optional.arm64: 1") || out.stdout.trim() == "1") {
        return true;
    }
    let out_full = host.run(
        "/usr/sbin/sysctl",
        &["hw.optional.arm64"],
        Duration::from_secs(2),
    );
    out_full.ok
        && (out_full.stdout.contains("hw.optional.arm64: 1") || out_full.stdout.trim() == "1")
}

/// Running processes table representation: `(pid, ppid, comm)`.
pub type Processes = [(u32, u32, String)];

/// Retrieves running processes from `ps`.
pub fn running_processes(host: &dyn Host) -> Vec<(u32, u32, String)> {
    let out = host.run(
        "/bin/ps",
        &["-axo", "pid=,ppid=,comm="],
        Duration::from_secs(5),
    );
    if !out.ok {
        return Vec::new();
    }
    out.stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid = parts.next()?.parse().ok()?;
            let ppid = parts.next()?.parse().ok()?;
            let comm = parts.collect::<Vec<_>>().join(" ");
            Some((pid, ppid, comm))
        })
        .collect()
}

/// Verifies whether an app qualifies for slimming according to safety and integrity rules.
/// Returns the qualifying universal binaries or an error describing why it stays.
#[allow(clippy::too_many_arguments)]
pub fn check_eligibility(
    app: &InstalledApp,
    host: &dyn Host,
    layout: &Layout,
    ignore: &IgnoreStore,
    rosetta_apps: &HashSet<String>,
    running: Option<&Processes>,
    skip_developer_id: bool,
    deep_verify: bool,
) -> Result<Vec<PathBuf>, String> {
    // 1. Apple apps: bundle id starting with com.apple.
    if app
        .bundle_id
        .as_deref()
        .is_some_and(|id| id.starts_with("com.apple."))
    {
        return Err("Apple apps are part of macOS".to_string());
    }

    // 2. App Store apps: Contents/_MASReceipt exists
    if app.path.join("Contents/_MASReceipt").exists() {
        return Err("App Store apps are managed by the App Store".to_string());
    }

    // 3. Rosetta: bundle id in LSArchitecturesForX86_64
    if app
        .bundle_id
        .as_deref()
        .is_some_and(|id| rosetta_apps.contains(id))
    {
        return Err("Set to open using Rosetta".to_string());
    }

    // 4. Running apps
    if let Some(processes) = running {
        let is_running = processes.iter().any(|(_, _, comm)| {
            let comm_path = Path::new(comm);
            comm_path.starts_with(&app.path)
                || comm.contains(&format!("{}/Contents/MacOS/", app.path.display()))
        });
        if is_running {
            return Err("App is currently running".to_string());
        }
    }

    // 5. Owned by another user
    if let Ok(meta) = fs::metadata(&app.path) {
        let uid = meta.uid();
        let current_uid = unsafe { libc::geteuid() };
        if uid != current_uid && uid != 0 {
            return Err("Belongs to another user".to_string());
        }
    }
    let users_dir = layout
        .home
        .ancestors()
        .find(|a| a.file_name() == Some(std::ffi::OsStr::new("Users")));
    if let Some(users) = users_dir {
        if app.path.starts_with(users) && !app.path.starts_with(&layout.home) {
            return Err("Belongs to another user".to_string());
        }
    } else if app.path.starts_with("/Users/") && !app.path.starts_with(&layout.home) {
        return Err("Belongs to another user".to_string());
    }

    // 6. Safety engine check
    if let Err(e) = Safety::can_uninstall(&app.path, ignore, layout) {
        return Err(e.to_string());
    }

    // 7. Check for universal binaries
    let (binaries, intel_size) = find_universal_binaries(&app.path);
    if binaries.is_empty() || intel_size == 0 {
        return Err("Already Apple Silicon only".to_string());
    }

    // 8. Developer ID signing check
    if !skip_developer_id {
        let out = host.run(
            "codesign",
            &["-dv", "--verbose=2", app.path.to_str().unwrap()],
            Duration::from_secs(5),
        );
        let combined = format!("{}\n{}", out.stdout, out.stderr);
        if !combined.contains("Authority=Developer ID Application:") {
            return Err("Not signed with a Developer ID".to_string());
        }
    }

    // 9. Initial codesign and Gatekeeper verification (run when deep_verify is requested)
    if deep_verify {
        let cs = host.run(
            "codesign",
            &["--verify", "--deep", "--strict", app.path.to_str().unwrap()],
            Duration::from_secs(30),
        );
        if !cs.ok {
            return Err("Signature verification failed".to_string());
        }

        if !skip_developer_id {
            let spctl = host.run(
                "spctl",
                &["--assess", "--type", "execute", app.path.to_str().unwrap()],
                Duration::from_secs(30),
            );
            if !spctl.ok {
                return Err("Gatekeeper assessment failed".to_string());
            }
        }
    }

    Ok(binaries)
}

/// Recursively removes `com.apple.FinderInfo` and `com.apple.ResourceFork` attributes from a tree.
pub fn clean_ext_attributes(bundle: &Path) {
    let attrs = ["com.apple.FinderInfo", "com.apple.ResourceFork"];
    for entry in jwalk::WalkDir::new(bundle).into_iter().flatten() {
        let p = entry.path();
        let Ok(c_path) = CString::new(p.as_os_str().as_bytes()) else {
            continue;
        };
        for attr in attrs {
            let Ok(c_attr) = CString::new(attr) else {
                continue;
            };
            unsafe {
                removexattr(c_path.as_ptr(), c_attr.as_ptr(), 0);
            }
        }
    }
}

/// Executes the transactional 8-step app slimming pipeline.
///
/// 1. Clones the app next to itself via `ditto --clone`.
/// 2. Rewrites the binaries in the copy.
/// 3. Checks that each rewritten file still has an ARM slice.
/// 4. Checks the copy with `codesign --verify --deep --strict` and `spctl --assess --type execute`.
/// 5. If verification fails, removes `com.apple.FinderInfo` and `com.apple.ResourceFork` and verifies again.
/// 6. Confirms that the original app hasn't changed and is not running.
/// 7. Atomically swaps the copy and original via `renamex_np` and `RENAME_SWAP`.
/// 8. Moves the unslimmed original app to the Trash via `Host::trash`.
///
/// On any failure before or during swap, the copy is deleted and the original stays unmodified.
/// Returns `Ok(intel_saved_bytes)` on success or `Err((failed_step_number, error_message))`.
pub fn slim_app(
    app: &InstalledApp,
    binaries: &[PathBuf],
    host: &dyn Host,
    skip_developer_id: bool,
) -> Result<u64, (usize, String)> {
    let orig_meta = fs::metadata(&app.path).map_err(|e| (1, e.to_string()))?;
    let orig_mtime = orig_meta.modified().ok();
    let orig_identity = FileIdentity::of(&app.path);

    let parent = app
        .path
        .parent()
        .unwrap_or_else(|| Path::new("/Applications"));
    let file_name = app.path.file_name().unwrap().to_str().unwrap();
    let copy_path = parent.join(format!("{file_name}.taert-slim-tmp"));

    if copy_path.exists() {
        let _ = fs::remove_dir_all(&copy_path);
    }

    // Step 1: Clone app next to itself with ditto --clone
    let clone_out = host.run(
        "ditto",
        &[
            "--clone",
            app.path.to_str().unwrap(),
            copy_path.to_str().unwrap(),
        ],
        Duration::from_secs(120),
    );
    if !clone_out.ok {
        let err = if clone_out.stderr.contains("Operation not permitted") {
            APP_MANAGEMENT_HINT.to_string()
        } else {
            format!("ditto clone failed: {}", clone_out.stderr.trim())
        };
        return Err((1, err));
    }

    // Helper to safely clean up the copy on failure
    let cleanup_copy = |step: usize, msg: String| {
        let _ = fs::remove_dir_all(&copy_path);
        Err((step, msg))
    };

    let mut total_intel_saved = 0u64;

    // Step 2: Rewrite binaries in the copy
    for bin in binaries {
        let Ok(rel) = bin.strip_prefix(&app.path) else {
            return cleanup_copy(2, format!("Invalid binary prefix: {}", bin.display()));
        };
        let copy_bin = copy_path.join(rel);
        let orig_bytes = match fs::read(&copy_bin) {
            Ok(b) => b,
            Err(e) => {
                return cleanup_copy(2, format!("Failed reading {}: {e}", copy_bin.display()));
            }
        };

        let orig_intel_size = match parse_fat_header(&orig_bytes) {
            Ok(Some(h)) => h
                .slices
                .iter()
                .filter(|s| s.cputype == CPU_TYPE_X86_64 || s.cputype == CPU_TYPE_I386)
                .map(|s| s.size)
                .sum(),
            _ => 0,
        };
        total_intel_saved += orig_intel_size;

        let rewritten = match rewrite_fat_binary(&orig_bytes) {
            Ok(b) => b,
            Err(e) => {
                return cleanup_copy(2, format!("Failed rewriting {}: {e}", copy_bin.display()));
            }
        };

        if let Err(e) = fs::write(&copy_bin, &rewritten) {
            return cleanup_copy(2, format!("Failed writing {}: {e}", copy_bin.display()));
        }
        if let Ok(meta) = fs::metadata(bin) {
            let _ = fs::set_permissions(&copy_bin, meta.permissions());
        }
    }

    // Step 3: Check that each rewritten file still has an ARM slice
    for bin in binaries {
        let Ok(rel) = bin.strip_prefix(&app.path) else {
            return cleanup_copy(3, format!("Invalid binary prefix: {}", bin.display()));
        };
        let copy_bin = copy_path.join(rel);
        let bytes = match fs::read(&copy_bin) {
            Ok(b) => b,
            Err(e) => {
                return cleanup_copy(3, format!("Failed reading {}: {e}", copy_bin.display()));
            }
        };
        match parse_fat_header(&bytes) {
            Ok(Some(header)) if header.slices.iter().any(|s| s.cputype == CPU_TYPE_ARM64) => {}
            _ => return cleanup_copy(3, format!("File {} missing ARM slice", copy_bin.display())),
        }
    }

    // Step 4 & 5: Check copy with codesign and spctl; remove xattrs and retry if needed
    let check_signatures = |path: &Path| -> bool {
        let cs = host.run(
            "codesign",
            &["--verify", "--deep", "--strict", path.to_str().unwrap()],
            Duration::from_secs(30),
        );
        if !cs.ok {
            return false;
        }
        if skip_developer_id {
            return true;
        }
        let sp = host.run(
            "spctl",
            &["--assess", "--type", "execute", path.to_str().unwrap()],
            Duration::from_secs(30),
        );
        sp.ok
    };

    if !check_signatures(&copy_path) {
        // Step 5: Remove FinderInfo and ResourceFork xattrs and verify again
        clean_ext_attributes(&copy_path);
        if !check_signatures(&copy_path) {
            return cleanup_copy(
                4,
                "Signature or Gatekeeper verification failed on copy".to_string(),
            );
        }
    }

    // Step 6: Confirm original app hasn't changed and isn't running
    if FileIdentity::of(&app.path) != orig_identity {
        return cleanup_copy(6, "Original app inode or device changed".to_string());
    }
    if fs::metadata(&app.path).ok().and_then(|m| m.modified().ok()) != orig_mtime {
        return cleanup_copy(6, "Original app modification time changed".to_string());
    }
    let running = running_processes(host);
    let is_running = running.iter().any(|(_, _, comm)| {
        let comm_path = Path::new(comm);
        comm_path.starts_with(&app.path)
            || comm.contains(&format!("{}/Contents/MacOS/", app.path.display()))
    });
    if is_running {
        return cleanup_copy(6, "App started running during slimming".to_string());
    }

    // Step 7: Atomic swap of copy and original via renamex_np(RENAME_SWAP)
    let Ok(c_copy) = CString::new(copy_path.as_os_str().as_bytes()) else {
        return cleanup_copy(7, "Failed to encode copy path for swap".to_string());
    };
    let Ok(c_orig) = CString::new(app.path.as_os_str().as_bytes()) else {
        return cleanup_copy(7, "Failed to encode original path for swap".to_string());
    };

    let res = unsafe { renamex_np(c_copy.as_ptr(), c_orig.as_ptr(), RENAME_SWAP) };
    if res != 0 {
        let err = std::io::Error::last_os_error();
        let _ = fs::remove_dir_all(&copy_path);
        let msg = if err.kind() == std::io::ErrorKind::PermissionDenied {
            APP_MANAGEMENT_HINT.to_string()
        } else {
            format!("Atomic swap failed: {err}")
        };
        return Err((7, msg));
    }

    // Step 8: Move the old unslimmed app (now located at copy_path) to Trash
    if let Err(e) = host.trash(&copy_path) {
        return Err((8, format!("Failed to move old app to Trash: {e}")));
    }

    Ok(total_intel_saved)
}

/// Runs a scan over installed apps to prepare a `CategoryScan` for the TUI and CLI.
pub fn slim_scan(
    layout: &Layout,
    host: &dyn Host,
    ignore: &IgnoreStore,
    progress: Option<Sender<ScanProgressEvent>>,
    cancel: Option<Arc<AtomicBool>>,
) -> CategoryScan {
    let mut scan = CategoryScan {
        id: SCAN_ID.to_string(),
        title: "Slim Universal Apps".to_string(),
        groups: Vec::new(),
        notes: Vec::new(),
    };

    let cancelled = || cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed));
    if cancelled() {
        return scan;
    }

    let installed = apps::find_apps(layout);
    let rosetta = rosetta_bundle_ids(layout);
    let running = running_processes(host);

    for root in layout.app_roots() {
        let mut items = Vec::new();
        let apps_in_root: Vec<&InstalledApp> = installed
            .iter()
            .filter(|app| app.path.starts_with(&root))
            .collect();

        for app in apps_in_root {
            if cancelled() {
                return scan;
            }
            if let Some(ref tx) = progress {
                let _ = tx.send(ScanProgressEvent::ScanningPath(app.name.clone()));
            }

            match check_eligibility(
                app,
                host,
                layout,
                ignore,
                &rosetta,
                Some(&running),
                false,
                false,
            ) {
                Ok(binaries) => {
                    let intel_size: u64 = binaries
                        .iter()
                        .filter_map(|p| parse_fat_header_from_file(p).ok().flatten())
                        .map(|h| {
                            h.slices
                                .iter()
                                .filter(|s| {
                                    s.cputype == CPU_TYPE_X86_64 || s.cputype == CPU_TYPE_I386
                                })
                                .map(|s| s.size)
                                .sum::<u64>()
                        })
                        .sum();

                    if intel_size > 0 {
                        // Crucial requirement: No app starts selected.
                        let mut item = ScanItem::new(app.path.clone(), intel_size, false, false);
                        item.modified = app.last_used;
                        items.push(item);
                        if let Some(ref tx) = progress {
                            let _ = tx.send(ScanProgressEvent::ItemFound { bytes: intel_size });
                        }
                    }
                }
                Err(reason) => {
                    scan.notes.push(format!("{} stays: {reason}.", app.name));
                }
            }
        }

        if !items.is_empty() {
            items.sort_by_key(|a| std::cmp::Reverse(a.size_bytes));
            scan.groups.push(ToolGroup {
                tool: root.display().to_string(),
                items,
            });
        }
    }

    scan
}

/// JSON-serializable status of an app's slimming eligibility.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlimAppInfo {
    pub name: String,
    pub path: PathBuf,
    pub bundle_id: Option<String>,
    pub intel_bytes: u64,
    pub eligible: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::host::fake::FakeHost;
    use tempfile::tempdir;

    fn make_fat_binary_32(slices: &[(u32, u32, u32, u32, u32)], payloads: &[&[u8]]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&FAT_MAGIC.to_be_bytes());
        buf.extend_from_slice(&(slices.len() as u32).to_be_bytes());
        for s in slices {
            buf.extend_from_slice(&s.0.to_be_bytes()); // cputype
            buf.extend_from_slice(&s.1.to_be_bytes()); // cpusubtype
            buf.extend_from_slice(&s.2.to_be_bytes()); // offset
            buf.extend_from_slice(&s.3.to_be_bytes()); // size
            buf.extend_from_slice(&s.4.to_be_bytes()); // align
        }
        for (s, p) in slices.iter().zip(payloads) {
            if buf.len() < s.2 as usize {
                buf.resize(s.2 as usize, 0);
            }
            buf.extend_from_slice(p);
        }
        buf
    }

    fn make_fat_binary_64(slices: &[(u32, u32, u64, u64, u32)], payloads: &[&[u8]]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&FAT_MAGIC_64.to_be_bytes());
        buf.extend_from_slice(&(slices.len() as u32).to_be_bytes());
        for s in slices {
            buf.extend_from_slice(&s.0.to_be_bytes()); // cputype
            buf.extend_from_slice(&s.1.to_be_bytes()); // cpusubtype
            buf.extend_from_slice(&s.2.to_be_bytes()); // offset
            buf.extend_from_slice(&s.3.to_be_bytes()); // size
            buf.extend_from_slice(&s.4.to_be_bytes()); // align
            buf.extend_from_slice(&0u32.to_be_bytes()); // reserved
        }
        for (s, p) in slices.iter().zip(payloads) {
            if buf.len() < s.2 as usize {
                buf.resize(s.2 as usize, 0);
            }
            buf.extend_from_slice(p);
        }
        buf
    }

    #[test]
    fn test_parse_fat_header_32() {
        let p_x86 = b"X86_64_CODE";
        let p_arm = b"ARM64_CODE_PAYLOAD";
        let slices = [
            (CPU_TYPE_X86_64, 3, 4096, p_x86.len() as u32, 12),
            (CPU_TYPE_ARM64, 0, 8192, p_arm.len() as u32, 14),
        ];
        let bytes = make_fat_binary_32(&slices, &[p_x86, p_arm]);

        let parsed = parse_fat_header(&bytes).unwrap().expect("header expected");
        assert!(!parsed.is_64);
        assert_eq!(parsed.slices.len(), 2);
        assert_eq!(parsed.slices[0].cputype, CPU_TYPE_X86_64);
        assert_eq!(parsed.slices[0].offset, 4096);
        assert_eq!(parsed.slices[0].size, p_x86.len() as u64);
        assert_eq!(parsed.slices[1].cputype, CPU_TYPE_ARM64);
        assert_eq!(parsed.slices[1].offset, 8192);
        assert_eq!(parsed.slices[1].size, p_arm.len() as u64);
    }

    #[test]
    fn test_parse_fat_header_64() {
        let p_x86 = b"X86_64_CODE_64";
        let p_arm = b"ARM64_CODE_64";
        let slices = [
            (CPU_TYPE_X86_64, 3, 4096u64, p_x86.len() as u64, 12u32),
            (CPU_TYPE_ARM64, 0, 8192u64, p_arm.len() as u64, 14u32),
        ];
        let bytes = make_fat_binary_64(&slices, &[p_x86, p_arm]);

        let parsed = parse_fat_header(&bytes).unwrap().expect("header expected");
        assert!(parsed.is_64);
        assert_eq!(parsed.slices.len(), 2);
        assert_eq!(parsed.slices[0].cputype, CPU_TYPE_X86_64);
        assert_eq!(parsed.slices[1].cputype, CPU_TYPE_ARM64);
    }

    #[test]
    fn test_java_class_file_rejected() {
        let mut java_bytes = Vec::new();
        java_bytes.extend_from_slice(&FAT_MAGIC.to_be_bytes()); // 0xCAFEBABE
        java_bytes.extend_from_slice(&0u16.to_be_bytes()); // minor version: 0
        java_bytes.extend_from_slice(&52u16.to_be_bytes()); // major version: 52 (Java 8)
        java_bytes.extend_from_slice(&[0u8; 100]);

        assert_eq!(parse_fat_header(&java_bytes).unwrap(), None);
    }

    #[test]
    fn test_rewrite_keeps_arm_and_removes_x86_64() {
        let p_x86 = b"INTEL_CODE_TO_REMOVE";
        let p_arm = b"ARM64_CODE_TO_PRESERVE";
        let slices = [
            (CPU_TYPE_X86_64, 3, 4096, p_x86.len() as u32, 12),
            (CPU_TYPE_ARM64, 0, 8192, p_arm.len() as u32, 14),
        ];
        let bytes = make_fat_binary_32(&slices, &[p_x86, p_arm]);

        let rewritten = rewrite_fat_binary(&bytes).expect("rewrite succeeds");
        let parsed = parse_fat_header(&rewritten)
            .unwrap()
            .expect("valid fat header");

        assert_eq!(parsed.slices.len(), 1);
        assert_eq!(parsed.slices[0].cputype, CPU_TYPE_ARM64);
        let arm_offset = parsed.slices[0].offset as usize;
        let arm_size = parsed.slices[0].size as usize;
        assert_eq!(&rewritten[arm_offset..arm_offset + arm_size], p_arm);
        assert_eq!(arm_offset % (1 << parsed.slices[0].align), 0);
    }

    #[test]
    fn test_refuse_file_with_no_arm_slice() {
        let p_x86 = b"INTEL_ONLY_CODE";
        let slices = [(CPU_TYPE_X86_64, 3, 4096, p_x86.len() as u32, 12)];
        let bytes = make_fat_binary_32(&slices, &[p_x86]);

        let err = rewrite_fat_binary(&bytes).unwrap_err();
        assert_eq!(err, SlimError::NoArmSlice);
    }

    fn write_test_app(bundle: &Path, id: &str) {
        let macos = bundle.join("Contents/MacOS");
        fs::create_dir_all(&macos).unwrap();
        let p_x86 = b"X86_PAYLOAD";
        let p_arm = b"ARM_PAYLOAD";
        let slices = [
            (CPU_TYPE_X86_64, 3, 4096, p_x86.len() as u32, 12),
            (CPU_TYPE_ARM64, 0, 8192, p_arm.len() as u32, 14),
        ];
        let bin = make_fat_binary_32(&slices, &[p_x86, p_arm]);
        fs::write(macos.join("testapp"), bin).unwrap();

        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>{id}</string>
<key>CFBundleExecutable</key><string>testapp</string>
</dict></plist>"#
        );
        fs::write(bundle.join("Contents/Info.plist"), plist).unwrap();
    }

    fn test_env() -> (tempfile::TempDir, Layout) {
        let temp = tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let layout = Layout {
            home: base.join("Users/test"),
            root: base.join("root"),
            user_cache: None,
        };
        fs::create_dir_all(layout.system("Applications")).unwrap();
        fs::create_dir_all(layout.user("Applications")).unwrap();
        (temp, layout)
    }

    #[test]
    fn test_exclusion_apple_app() {
        let (_dir, layout) = test_env();
        let app_path = layout.system("Applications").join("AppleApp.app");
        write_test_app(&app_path, "com.apple.Safari");

        let app = InstalledApp {
            name: "AppleApp".to_string(),
            path: app_path,
            bundle_id: Some("com.apple.Safari".to_string()),
            executable: Some("testapp".to_string()),
            size_bytes: 10000,
            last_used: None,
        };
        let host = FakeHost::idle();
        let ignore = IgnoreStore::default();
        let res = check_eligibility(
            &app,
            &host,
            &layout,
            &ignore,
            &HashSet::new(),
            None,
            true,
            false,
        );
        assert_eq!(res.unwrap_err(), "Apple apps are part of macOS");
    }

    #[test]
    fn test_exclusion_app_store_app() {
        let (_dir, layout) = test_env();
        let app_path = layout.system("Applications").join("StoreApp.app");
        write_test_app(&app_path, "com.vendor.store");
        fs::create_dir_all(app_path.join("Contents/_MASReceipt")).unwrap();

        let app = InstalledApp {
            name: "StoreApp".to_string(),
            path: app_path,
            bundle_id: Some("com.vendor.store".to_string()),
            executable: Some("testapp".to_string()),
            size_bytes: 10000,
            last_used: None,
        };
        let host = FakeHost::idle();
        let ignore = IgnoreStore::default();
        let res = check_eligibility(
            &app,
            &host,
            &layout,
            &ignore,
            &HashSet::new(),
            None,
            true,
            false,
        );
        assert_eq!(
            res.unwrap_err(),
            "App Store apps are managed by the App Store"
        );
    }

    #[test]
    fn test_exclusion_rosetta_app() {
        let (_dir, layout) = test_env();
        let app_path = layout.system("Applications").join("RosettaApp.app");
        write_test_app(&app_path, "com.vendor.rosetta");

        let app = InstalledApp {
            name: "RosettaApp".to_string(),
            path: app_path,
            bundle_id: Some("com.vendor.rosetta".to_string()),
            executable: Some("testapp".to_string()),
            size_bytes: 10000,
            last_used: None,
        };
        let host = FakeHost::idle();
        let ignore = IgnoreStore::default();
        let mut rosetta = HashSet::new();
        rosetta.insert("com.vendor.rosetta".to_string());
        let res = check_eligibility(&app, &host, &layout, &ignore, &rosetta, None, true, false);
        assert_eq!(res.unwrap_err(), "Set to open using Rosetta");
    }

    #[test]
    fn test_exclusion_running_app() {
        let (_dir, layout) = test_env();
        let app_path = layout.system("Applications").join("RunningApp.app");
        write_test_app(&app_path, "com.vendor.running");

        let app = InstalledApp {
            name: "RunningApp".to_string(),
            path: app_path.clone(),
            bundle_id: Some("com.vendor.running".to_string()),
            executable: Some("testapp".to_string()),
            size_bytes: 10000,
            last_used: None,
        };
        let host = FakeHost::idle();
        let ignore = IgnoreStore::default();
        let processes = vec![(
            123,
            1,
            format!("{}/Contents/MacOS/testapp", app_path.display()),
        )];
        let res = check_eligibility(
            &app,
            &host,
            &layout,
            &ignore,
            &HashSet::new(),
            Some(&processes),
            true,
            false,
        );
        assert_eq!(res.unwrap_err(), "App is currently running");
    }

    #[test]
    fn test_exclusion_other_user_app() {
        let (_dir, layout) = test_env();
        let other_home = layout.home.parent().unwrap().join("other/Applications");
        fs::create_dir_all(&other_home).unwrap();
        let app_path = other_home.join("OtherApp.app");
        write_test_app(&app_path, "com.vendor.other");

        let app = InstalledApp {
            name: "OtherApp".to_string(),
            path: app_path,
            bundle_id: Some("com.vendor.other".to_string()),
            executable: Some("testapp".to_string()),
            size_bytes: 10000,
            last_used: None,
        };
        let host = FakeHost::idle();
        let ignore = IgnoreStore::default();
        let res = check_eligibility(
            &app,
            &host,
            &layout,
            &ignore,
            &HashSet::new(),
            None,
            true,
            false,
        );
        assert_eq!(res.unwrap_err(), "Belongs to another user");
    }

    #[test]
    fn test_exclusion_not_developer_id() {
        let (_dir, layout) = test_env();
        let app_path = layout.system("Applications").join("AdHocApp.app");
        write_test_app(&app_path, "com.vendor.adhoc");

        let app = InstalledApp {
            name: "AdHocApp".to_string(),
            path: app_path.clone(),
            bundle_id: Some("com.vendor.adhoc".to_string()),
            executable: Some("testapp".to_string()),
            size_bytes: 10000,
            last_used: None,
        };
        let host = FakeHost::idle();
        host.reply("codesign", "Authority=Mac Developer\n");
        let ignore = IgnoreStore::default();
        let res = check_eligibility(
            &app,
            &host,
            &layout,
            &ignore,
            &HashSet::new(),
            None,
            false,
            false,
        );
        assert_eq!(res.unwrap_err(), "Not signed with a Developer ID");
    }

    #[test]
    fn test_exclusion_codesign_verify_failed() {
        let (_dir, layout) = test_env();
        let app_path = layout.system("Applications").join("BrokenApp.app");
        write_test_app(&app_path, "com.vendor.broken");

        let app = InstalledApp {
            name: "BrokenApp".to_string(),
            path: app_path.clone(),
            bundle_id: Some("com.vendor.broken".to_string()),
            executable: Some("testapp".to_string()),
            size_bytes: 10000,
            last_used: None,
        };
        let host = FakeHost::idle();
        host.reply(
            "codesign -dv",
            "Authority=Developer ID Application: Acme Corp (123)\n",
        );
        host.fail("codesign --verify", "code has been modified\n");
        let ignore = IgnoreStore::default();
        let res = check_eligibility(
            &app,
            &host,
            &layout,
            &ignore,
            &HashSet::new(),
            None,
            false,
            true,
        );
        assert_eq!(res.unwrap_err(), "Signature verification failed");
    }

    #[test]
    fn test_exclusion_spctl_failed() {
        let (_dir, layout) = test_env();
        let app_path = layout.system("Applications").join("RejectedApp.app");
        write_test_app(&app_path, "com.vendor.rejected");

        let app = InstalledApp {
            name: "RejectedApp".to_string(),
            path: app_path.clone(),
            bundle_id: Some("com.vendor.rejected".to_string()),
            executable: Some("testapp".to_string()),
            size_bytes: 10000,
            last_used: None,
        };
        let host = FakeHost::idle();
        host.reply(
            "codesign -dv",
            "Authority=Developer ID Application: Acme Corp (123)\n",
        );
        host.reply("codesign --verify", "");
        host.fail("spctl", "rejected\n");
        let ignore = IgnoreStore::default();
        let res = check_eligibility(
            &app,
            &host,
            &layout,
            &ignore,
            &HashSet::new(),
            None,
            false,
            true,
        );
        assert_eq!(res.unwrap_err(), "Gatekeeper assessment failed");
    }

    #[test]
    fn test_exclusion_safety_gate() {
        let (_dir, layout) = test_env();
        let ignored_app = layout.system("Applications").join("IgnoredApp.app");
        write_test_app(&ignored_app, "com.vendor.ignored");

        let app = InstalledApp {
            name: "IgnoredApp".to_string(),
            path: ignored_app.clone(),
            bundle_id: Some("com.vendor.ignored".to_string()),
            executable: Some("testapp".to_string()),
            size_bytes: 10000,
            last_used: None,
        };
        let host = FakeHost::idle();
        let mut ignore = IgnoreStore::default();
        ignore.add(ignored_app.to_string_lossy().to_string());
        let res = check_eligibility(
            &app,
            &host,
            &layout,
            &ignore,
            &HashSet::new(),
            None,
            true,
            false,
        );
        assert!(res.is_err());
    }

    #[test]
    fn test_exclusion_already_apple_silicon() {
        let (_dir, layout) = test_env();
        let app_path = layout.system("Applications").join("ArmOnlyApp.app");
        let macos = app_path.join("Contents/MacOS");
        fs::create_dir_all(&macos).unwrap();
        let p_arm = b"ARM_ONLY";
        let slices = [(CPU_TYPE_ARM64, 0, 8192, p_arm.len() as u32, 14)];
        let bin = make_fat_binary_32(&slices, &[p_arm]);
        fs::write(macos.join("testapp"), bin).unwrap();
        fs::write(
            app_path.join("Contents/Info.plist"),
            r#"<plist><dict><key>CFBundleIdentifier</key><string>com.vendor.arm</string></dict></plist>"#,
        )
        .unwrap();

        let app = InstalledApp {
            name: "ArmOnlyApp".to_string(),
            path: app_path,
            bundle_id: Some("com.vendor.arm".to_string()),
            executable: Some("testapp".to_string()),
            size_bytes: 10000,
            last_used: None,
        };
        let host = FakeHost::idle();
        let ignore = IgnoreStore::default();
        let res = check_eligibility(
            &app,
            &host,
            &layout,
            &ignore,
            &HashSet::new(),
            None,
            true,
            false,
        );
        assert_eq!(res.unwrap_err(), "Already Apple Silicon only");
    }

    #[test]
    fn test_failed_check_leaves_original_and_removes_copy() {
        let dir = tempdir().unwrap();
        let app_path = dir.path().join("TargetApp.app");
        write_test_app(&app_path, "com.vendor.target");

        let app = InstalledApp {
            name: "TargetApp".to_string(),
            path: app_path.clone(),
            bundle_id: Some("com.vendor.target".to_string()),
            executable: Some("testapp".to_string()),
            size_bytes: 10000,
            last_used: None,
        };
        let binaries = vec![app_path.join("Contents/MacOS/testapp")];
        let host = FakeHost::idle();
        host.reply("ditto", "");
        host.when("ditto", {
            let app_path = app_path.clone();
            let copy_path = dir.path().join("TargetApp.app.taert-slim-tmp");
            move || {
                let _ = fs::create_dir_all(&copy_path);
                for entry in jwalk::WalkDir::new(&app_path).into_iter().flatten() {
                    let rel = entry.path().strip_prefix(&app_path).unwrap().to_path_buf();
                    let dest = copy_path.join(rel);
                    if entry.file_type().is_dir() {
                        let _ = fs::create_dir_all(&dest);
                    } else if entry.file_type().is_file() {
                        let _ = fs::copy(entry.path(), dest);
                    }
                }
            }
        });
        // Make codesign verify fail
        host.fail("codesign", "verification failed\n");

        let res = slim_app(&app, &binaries, &host, true);
        assert!(res.is_err());
        let (step, _) = res.unwrap_err();
        assert_eq!(step, 4);

        // Original remains in place
        assert!(app_path.exists());
        // Copy has been cleaned up
        let copy_path = dir.path().join("TargetApp.app.taert-slim-tmp");
        assert!(!copy_path.exists());
    }

    #[test]
    fn test_failed_swap_leaves_original() {
        let dir = tempdir().unwrap();
        let app_path = dir.path().join("SwapFailApp.app");
        write_test_app(&app_path, "com.vendor.swapfail");

        let app = InstalledApp {
            name: "SwapFailApp".to_string(),
            path: app_path.clone(),
            bundle_id: Some("com.vendor.swapfail".to_string()),
            executable: Some("testapp".to_string()),
            size_bytes: 10000,
            last_used: None,
        };
        let binaries = vec![app_path.join("Contents/MacOS/testapp")];
        let host = FakeHost::idle();
        // Here ditto does not create the copy_path, so swap will fail
        host.reply("ditto", "");
        host.reply("codesign", "");
        host.reply("spctl", "");

        let res = slim_app(&app, &binaries, &host, true);
        assert!(res.is_err());
        // Original remains in place
        assert!(app_path.exists());
    }

    #[test]
    fn test_intel_mac_stops_with_message() {
        let host = FakeHost::idle();
        host.fail("sysctl", "sysctl: unknown oid 'hw.optional.arm64'\n");
        assert!(!is_apple_silicon(&host));
    }

    #[test]
    fn test_host_trash_call_comes_last() {
        let dir = tempdir().unwrap();
        let app_path = dir.path().join("SwapApp.app");
        write_test_app(&app_path, "com.vendor.swap");

        let app = InstalledApp {
            name: "SwapApp".to_string(),
            path: app_path.clone(),
            bundle_id: Some("com.vendor.swap".to_string()),
            executable: Some("testapp".to_string()),
            size_bytes: 10000,
            last_used: None,
        };
        let binaries = vec![app_path.join("Contents/MacOS/testapp")];
        let host = FakeHost::idle();
        host.reply("ditto", "");
        host.when("ditto", {
            let app_path = app_path.clone();
            let copy_path = dir.path().join("SwapApp.app.taert-slim-tmp");
            move || {
                let _ = fs::create_dir_all(&copy_path);
                for entry in jwalk::WalkDir::new(&app_path).into_iter().flatten() {
                    let rel = entry.path().strip_prefix(&app_path).unwrap().to_path_buf();
                    let dest = copy_path.join(rel);
                    if entry.file_type().is_dir() {
                        let _ = fs::create_dir_all(&dest);
                    } else if entry.file_type().is_file() {
                        let _ = fs::copy(entry.path(), dest);
                    }
                }
            }
        });
        host.reply("codesign", "");
        host.reply("spctl", "");

        let res = slim_app(&app, &binaries, &host, true);
        assert!(res.is_ok(), "Expected Ok but got: {:?}", res);
        // host.trashed() must contain the copy_path
        let copy_path = dir.path().join("SwapApp.app.taert-slim-tmp");
        assert_eq!(host.trashed(), vec![copy_path]);
    }
}

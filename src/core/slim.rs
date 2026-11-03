//! App slimming engine to remove Intel code from universal macOS app bundles.
//!
//! On Apple Silicon Macs, universal apps contain Intel (x86_64) slices that are never executed
//! unless the app is explicitly configured to run under Rosetta. This module identifies qualifying
//! universal apps, strips the Intel architecture slices from Mach-O fat binaries while preserving
//! ARM code and code signatures, and safely moves the original bundle to the Trash.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
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
use super::bytes::format_bytes;
use super::history::{HistorySession, ItemOutcome};
use super::host::Host;
use super::layout::Layout;
use super::safety::Safety;
use super::scanner::{CategoryScan, ScanItem, ScanProgressEvent, ToolGroup};
use super::sizer::DirSizer;
use super::stores::IgnoreStore;

pub const SCAN_ID: &str = "slim";

pub const FAT_MAGIC: u32 = 0xCAFEBABE;
pub const FAT_MAGIC_64: u32 = 0xCAFEBABF;
pub const CPU_TYPE_I386: u32 = 7;
pub const CPU_TYPE_X86_64: u32 = 0x01000007;
pub const CPU_TYPE_ARM64: u32 = 0x0100000c;

pub const PS_FAILED: &str = "Could not list running programs";

pub const LINKED_APP: &str = "The app is a link. Slim the app it points to";

pub const TRASH_NOTE: &str =
    "Clones share blocks, so the space comes back when the Trash is emptied.";

pub const APP_MANAGEMENT_HINT: &str = "macOS refused. Allow your terminal in System Settings > Privacy & Security > App Management, or move the app to the Trash in Finder";

const HOLDING_SUFFIX: &str = ".taert-slim-tmp";

const RENAME_SWAP: libc::c_uint = 0x0000_0002;
const XATTR_NOFOLLOW: libc::c_int = 0x0001;

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
    try_read_sealed_hashes(code_resources_path).unwrap_or_default()
}

fn try_read_sealed_hashes(code_resources_path: &Path) -> Option<HashSet<String>> {
    let Ok(Value::Dictionary(dict)) = Value::from_file(code_resources_path) else {
        return None;
    };
    let mut sealed = HashSet::new();
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
    Some(sealed)
}

/// The seal of the nearest nested bundle around `file`, with the folder its keys start from.
/// The last rule also finds `<nested>/Contents/_CodeSignature`, when `folder` is `<nested>/Contents`.
fn nested_seal(file: &Path, contents: &Path) -> Option<(PathBuf, PathBuf)> {
    for folder in file.ancestors().skip(1) {
        if folder == contents || !folder.starts_with(contents) {
            return None;
        }
        if let Ok(rest) = file.strip_prefix(folder.join("Versions"))
            && let Some(version) = rest.components().next()
        {
            let base = folder.join("Versions").join(version);
            for seal in ["_CodeSignature/CodeResources", "Resources/CodeResources"] {
                if base.join(seal).is_file() {
                    return Some((base.join(seal), base));
                }
            }
        }
        if folder.join("_CodeSignature/CodeResources").is_file() {
            return Some((
                folder.join("_CodeSignature/CodeResources"),
                folder.to_path_buf(),
            ));
        }
    }
    None
}

/// Whether the bundle's seal covers `file` by hash, so rewriting it would break the seal.
fn is_sealed(
    file: &Path,
    contents: &Path,
    top: &HashSet<String>,
    nested: &mut HashMap<PathBuf, Option<HashSet<String>>>,
) -> bool {
    let (sealed, base) = match nested_seal(file, contents) {
        Some((seal, base)) => {
            let hashes = nested
                .entry(seal.clone())
                .or_insert_with(|| try_read_sealed_hashes(&seal));
            match hashes {
                Some(hashes) => (&*hashes, base),
                None => return true,
            }
        }
        None => (top, contents.to_path_buf()),
    };
    file.strip_prefix(&base)
        .is_ok_and(|rel| sealed.contains(rel.to_string_lossy().as_ref()))
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
    let mut nested_seals = HashMap::new();
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

        if is_sealed(&path, &contents, &sealed_paths, &mut nested_seals) {
            continue;
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
pub fn running_processes(host: &dyn Host) -> Result<Vec<(u32, u32, String)>, String> {
    let out = host.run(
        "/bin/ps",
        &["-axo", "pid=,ppid=,comm="],
        Duration::from_secs(5),
    );
    if !out.ok {
        return Err(PS_FAILED.to_string());
    }
    Ok(out
        .stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid = parts.next()?.parse().ok()?;
            let ppid = parts.next()?.parse().ok()?;
            let comm = parts.collect::<Vec<_>>().join(" ");
            Some((pid, ppid, comm))
        })
        .collect())
}

/// Verifies whether an app qualifies for slimming according to safety and integrity rules.
/// Returns the qualifying universal binaries or an error describing why it stays.
pub fn check_eligibility(
    app: &InstalledApp,
    host: &dyn Host,
    layout: &Layout,
    ignore: &IgnoreStore,
    rosetta_apps: &HashSet<String>,
    running: Option<&Processes>,
    skip_developer_id: bool,
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

    // 5. Owned by another user, or holding a setuid or setgid file
    check_ownership(&app.path, unsafe { libc::geteuid() })?;
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

    // 9. Signature and Gatekeeper checks of the app as it is now
    let path = app.path.to_str().unwrap();
    let verify = host.run(
        "codesign",
        &["--verify", "--deep", "--strict", path],
        Duration::from_secs(30),
    );
    if !verify.ok {
        return Err("Already fails the signature check".to_string());
    }
    if !skip_developer_id {
        let assess = host.run(
            "spctl",
            &["--assess", "--type", "execute", path],
            Duration::from_secs(30),
        );
        if !assess.ok {
            return Err("Already fails the Gatekeeper check".to_string());
        }
    }

    Ok(binaries)
}

/// Every entry under `root` (and `root` itself), sorted, without following links.
fn walk_entries(root: &Path) -> std::io::Result<Vec<(PathBuf, fs::Metadata)>> {
    fn visit(path: &Path, out: &mut Vec<(PathBuf, fs::Metadata)>) -> std::io::Result<()> {
        let meta = fs::symlink_metadata(path)?;
        let is_dir = meta.is_dir();
        out.push((path.to_path_buf(), meta));
        if is_dir {
            let mut names = fs::read_dir(path)?
                .map(|entry| entry.map(|e| e.file_name()))
                .collect::<Result<Vec<_>, _>>()?;
            names.sort();
            for name in names {
                visit(&path.join(name), out)?;
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    visit(root, &mut out)?;
    Ok(out)
}

/// Refuses a bundle that is a link, has an entry that `expected_uid` does not own,
/// or has a setuid or setgid entry.
pub fn check_ownership(root: &Path, expected_uid: u32) -> Result<(), String> {
    let entries =
        walk_entries(root).map_err(|e| format!("Could not read {}: {e}", root.display()))?;
    if entries.first().is_some_and(|(_, meta)| meta.is_symlink()) {
        return Err(LINKED_APP.to_string());
    }
    for (_, meta) in entries {
        if meta.uid() != expected_uid {
            return Err("Belongs to another user".to_string());
        }
        if meta.mode() & 0o6000 != 0 {
            return Err("Contains a setuid or setgid file".to_string());
        }
    }
    Ok(())
}

/// A hash of every entry's path, type, size, modification time, inode and device.
pub fn fingerprint(root: &Path) -> std::io::Result<u64> {
    let mut hasher = DefaultHasher::new();
    for (path, meta) in walk_entries(root)? {
        path.strip_prefix(root).unwrap_or(&path).hash(&mut hasher);
        let kind = (meta.is_dir(), meta.is_file(), meta.is_symlink());
        let modified = i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec());
        (kind, meta.len(), modified, meta.ino(), meta.dev()).hash(&mut hasher);
    }
    Ok(hasher.finish())
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
                removexattr(c_path.as_ptr(), c_attr.as_ptr(), XATTR_NOFOLLOW);
            }
        }
    }
}

/// What a finished slim leaves behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlimDone {
    pub saved: u64,
    /// Where the old app stays, and why it could not go to the Trash.
    pub old_app: Option<(PathBuf, String)>,
    /// The holding folder left after the old app went to the Trash, and why it stays.
    pub holding_left: Option<(PathBuf, String)>,
}

impl SlimDone {
    /// What the slim saved, and anything it left behind.
    pub fn summary(&self, name: &str) -> String {
        let saved = format!("Slimmed {name}, saved {}.", format_bytes(self.saved));
        match (&self.old_app, &self.holding_left) {
            (Some((path, error)), _) => format!(
                "{saved} The old app is still at {} (could not move it to the Trash: {error})",
                path.display()
            ),
            (None, Some((path, error))) => format!(
                "{saved} The old app was moved to the Trash, but the folder {} is left (could not remove it: {error}). Move it to the Trash yourself before you slim {name} again.",
                path.display()
            ),
            (None, None) => format!("{saved} The old app was moved to the Trash."),
        }
    }
}

/// Slims an app in a holding folder `<name>.taert-slim-tmp` next to it, in 8 steps.
///
/// 1. Clones the app to `<holding>/<name>` via `ditto --clone`. A holding folder left from an
///    earlier run stops the call: it can hold the only copy of an app, so it is never removed.
/// 2. Rewrites the binaries in the clone.
/// 3. Checks that each rewritten file still has an ARM slice.
/// 4. Checks the clone with `codesign --verify --deep --strict` and `spctl --assess --type execute`.
/// 5. If `codesign` fails, removes `com.apple.FinderInfo` and `com.apple.ResourceFork` and checks again.
/// 6. Confirms that the original app is unchanged and not running.
/// 7. Atomically swaps the clone and the original via `renamex_np` and `RENAME_SWAP`.
/// 8. Moves the old app, now at `<holding>/<name>`, to the Trash via `Host::trash`.
///
/// On any failure up to step 7 the holding folder is removed and the original stays as it was.
/// After the swap nothing is removed: if the Trash refuses, the result says where the old app is.
/// Returns `Err((failed_step_number, error_message))` when the app was not slimmed.
pub fn slim_app(
    app: &InstalledApp,
    binaries: &[PathBuf],
    host: &dyn Host,
    skip_developer_id: bool,
) -> Result<SlimDone, (usize, String)> {
    check_ownership(&app.path, unsafe { libc::geteuid() }).map_err(|e| (1, e))?;
    let before = fingerprint(&app.path).map_err(|e| (1, e.to_string()))?;

    let parent = app
        .path
        .parent()
        .unwrap_or_else(|| Path::new("/Applications"));
    let file_name = app.path.file_name().unwrap().to_str().unwrap();
    let holding = parent.join(format!("{file_name}{HOLDING_SUFFIX}"));
    if let Err(error) = fs::create_dir(&holding) {
        return Err((
            1,
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                format!(
                    "{} is left from an earlier run; check it, move it to the Trash yourself, then run again",
                    holding.display()
                )
            } else {
                format!("Could not create {}: {error}", holding.display())
            },
        ));
    }
    let clone = holding.join(file_name);

    let saved = match stage_clone(app, binaries, host, skip_developer_id, &clone, before) {
        Ok(saved) => saved,
        Err(failure) => {
            let _ = fs::remove_dir_all(&holding);
            return Err(failure);
        }
    };

    // Step 7: Atomic swap of the clone and the original via renamex_np(RENAME_SWAP)
    let (Ok(c_clone), Ok(c_orig)) = (
        CString::new(clone.as_os_str().as_bytes()),
        CString::new(app.path.as_os_str().as_bytes()),
    ) else {
        let _ = fs::remove_dir_all(&holding);
        return Err((7, "Failed to encode paths for swap".to_string()));
    };
    if unsafe { renamex_np(c_clone.as_ptr(), c_orig.as_ptr(), RENAME_SWAP) } != 0 {
        let err = std::io::Error::last_os_error();
        let _ = fs::remove_dir_all(&holding);
        let msg = if err.kind() == std::io::ErrorKind::PermissionDenied {
            APP_MANAGEMENT_HINT.to_string()
        } else {
            format!("Atomic swap failed: {err}")
        };
        return Err((7, msg));
    }

    // Step 8: Move the old app to the Trash. The app is slimmed either way.
    match host.trash(&clone) {
        Ok(()) => Ok(SlimDone {
            saved,
            old_app: None,
            holding_left: fs::remove_dir(&holding)
                .err()
                .map(|error| (holding, error.to_string())),
        }),
        Err(error) => Ok(SlimDone {
            saved,
            old_app: Some((clone, error)),
            holding_left: None,
        }),
    }
}

/// Steps 1 to 6 of `slim_app`: builds the slimmed clone and checks it. Returns the bytes saved.
fn stage_clone(
    app: &InstalledApp,
    binaries: &[PathBuf],
    host: &dyn Host,
    skip_developer_id: bool,
    clone: &Path,
    before: u64,
) -> Result<u64, (usize, String)> {
    // Step 1: Clone the app with ditto --clone
    let clone_out = host.run(
        "ditto",
        &[
            "--clone",
            app.path.to_str().unwrap(),
            clone.to_str().unwrap(),
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

    let mut saved = 0u64;

    // Step 2: Rewrite binaries in the clone
    for bin in binaries {
        let Ok(rel) = bin.strip_prefix(&app.path) else {
            return Err((2, format!("Invalid binary prefix: {}", bin.display())));
        };
        let clone_bin = clone.join(rel);
        let orig_bytes = fs::read(&clone_bin)
            .map_err(|e| (2, format!("Failed reading {}: {e}", clone_bin.display())))?;
        let rewritten = rewrite_fat_binary(&orig_bytes)
            .map_err(|e| (2, format!("Failed rewriting {}: {e}", clone_bin.display())))?;
        let allocated_before = fs::metadata(bin)
            .map(|meta| DirSizer::allocated_size(&meta))
            .map_err(|e| (2, format!("Failed reading {}: {e}", bin.display())))?;
        fs::write(&clone_bin, &rewritten)
            .map_err(|e| (2, format!("Failed writing {}: {e}", clone_bin.display())))?;
        if let Ok(meta) = fs::metadata(bin) {
            let _ = fs::set_permissions(&clone_bin, meta.permissions());
        }
        let allocated_after = fs::metadata(&clone_bin)
            .map(|meta| DirSizer::allocated_size(&meta))
            .map_err(|e| (2, format!("Failed reading {}: {e}", clone_bin.display())))?;
        saved += allocated_before.saturating_sub(allocated_after);
    }

    // Step 3: Check that each rewritten file still has an ARM slice
    for bin in binaries {
        let Ok(rel) = bin.strip_prefix(&app.path) else {
            return Err((3, format!("Invalid binary prefix: {}", bin.display())));
        };
        let clone_bin = clone.join(rel);
        let bytes = fs::read(&clone_bin)
            .map_err(|e| (3, format!("Failed reading {}: {e}", clone_bin.display())))?;
        match parse_fat_header(&bytes) {
            Ok(Some(header)) if header.slices.iter().any(|s| s.cputype == CPU_TYPE_ARM64) => {}
            _ => {
                return Err((3, format!("File {} missing ARM slice", clone_bin.display())));
            }
        }
    }

    // Steps 4 and 5: Check the clone; only a failed codesign check cleans xattrs and tries again
    let clone_str = clone.to_str().unwrap();
    let verify = || {
        let out = host.run(
            "codesign",
            &["--verify", "--deep", "--strict", clone_str],
            Duration::from_secs(30),
        );
        out.ok
    };
    if !verify() {
        clean_ext_attributes(clone);
        if !verify() {
            return Err((4, "Signature verification failed on copy".to_string()));
        }
    }
    if !skip_developer_id {
        let assess = host.run(
            "spctl",
            &["--assess", "--type", "execute", clone_str],
            Duration::from_secs(30),
        );
        if !assess.ok {
            return Err((4, "Gatekeeper verification failed on copy".to_string()));
        }
    }

    // Step 6: Confirm the original app hasn't changed and isn't running
    if fingerprint(&app.path).ok() != Some(before) {
        return Err((6, "Original app changed".to_string()));
    }
    let running = running_processes(host).map_err(|e| (6, e))?;
    let is_running = running.iter().any(|(_, _, comm)| {
        let comm_path = Path::new(comm);
        comm_path.starts_with(&app.path)
            || comm.contains(&format!("{}/Contents/MacOS/", app.path.display()))
    });
    if is_running {
        return Err((6, "App started running during slimming".to_string()));
    }

    Ok(saved)
}

/// Slims an app bundle and logs the outcome to the history session on success.
pub fn slim_app_logged(
    session: &HistorySession,
    app: &InstalledApp,
    binaries: &[PathBuf],
    host: &dyn Host,
    skip_developer_id: bool,
) -> Result<SlimDone, (usize, String)> {
    let result = slim_app(app, binaries, host, skip_developer_id);
    if let Ok(done) = &result {
        session.record_item(&ItemOutcome::changed(app.path.clone(), done.saved));
    }
    result
}

/// Installed apps, except an old app that an earlier slim left in its holding folder.
pub fn candidate_apps(layout: &Layout) -> Vec<InstalledApp> {
    apps::find_apps(layout)
        .into_iter()
        .filter(|app| {
            !app.path
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|folder| folder.to_string_lossy().ends_with(HOLDING_SUFFIX))
        })
        .collect()
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

    let installed = candidate_apps(layout);
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

            let Ok(running) = &running else {
                scan.notes.push(format!("{} stays: {PS_FAILED}.", app.name));
                continue;
            };
            match check_eligibility(app, host, layout, ignore, &rosetta, Some(running), false) {
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
    use super::testing::*;
    use super::*;
    use crate::core::history::{HistoryLog, OutcomeKind};
    use crate::core::host::fake::FakeHost;
    use crate::core::sizer::DirSizer;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;
    use tempfile::tempdir;

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
        let res = check_eligibility(&app, &host, &layout, &ignore, &HashSet::new(), None, true);
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
        let res = check_eligibility(&app, &host, &layout, &ignore, &HashSet::new(), None, true);
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
        let res = check_eligibility(&app, &host, &layout, &ignore, &rosetta, None, true);
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
        let res = check_eligibility(&app, &host, &layout, &ignore, &HashSet::new(), None, true);
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
        let res = check_eligibility(&app, &host, &layout, &ignore, &HashSet::new(), None, false);
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
        let res = check_eligibility(&app, &host, &layout, &ignore, &HashSet::new(), None, false);
        assert_eq!(res.unwrap_err(), "Already fails the signature check");
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
        let res = check_eligibility(&app, &host, &layout, &ignore, &HashSet::new(), None, false);
        assert_eq!(res.unwrap_err(), "Already fails the Gatekeeper check");
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
        let res = check_eligibility(&app, &host, &layout, &ignore, &HashSet::new(), None, true);
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
        let res = check_eligibility(&app, &host, &layout, &ignore, &HashSet::new(), None, true);
        assert_eq!(res.unwrap_err(), "Already Apple Silicon only");
    }

    /// Cloning works and nothing is checked yet.
    fn cloning_host(app_path: &Path) -> FakeHost {
        let host = FakeHost::idle();
        host.reply("ditto", "");
        fake_ditto(&host, app_path);
        host
    }

    fn ready_host(app_path: &Path) -> FakeHost {
        let host = cloning_host(app_path);
        host.reply("codesign", "");
        host.reply("spctl", "");
        host
    }

    fn binaries_of(app_path: &Path) -> Vec<PathBuf> {
        vec![app_path.join("Contents/MacOS/testapp")]
    }

    fn assert_arm_only(binary: &Path) {
        let header = parse_fat_header_from_file(binary).unwrap().unwrap();
        assert_eq!(header.slices.len(), 1);
        assert_eq!(header.slices[0].cputype, CPU_TYPE_ARM64);
    }

    #[test]
    fn test_failed_check_leaves_original_and_removes_copy() {
        let dir = tempdir().unwrap();
        let app_path = dir.path().join("TargetApp.app");
        write_test_app(&app_path, "com.vendor.target");
        let app = app_for(&app_path, "com.vendor.target");
        let host = cloning_host(&app_path);
        host.fail("codesign", "verification failed\n");

        let (step, _) = slim_app(&app, &binaries_of(&app_path), &host, true).unwrap_err();

        assert_eq!(step, 4);
        assert!(app_path.exists());
        assert!(!holding_of(&app_path).exists());
    }

    #[test]
    fn test_leftover_holding_folder_is_never_removed() {
        let dir = tempdir().unwrap();
        let app_path = dir.path().join("SwapApp.app");
        write_test_app(&app_path, "com.vendor.swap");
        let holding = holding_of(&app_path);
        fs::create_dir_all(holding.join("SwapApp.app")).unwrap();
        fs::write(holding.join("SwapApp.app/marker"), "old").unwrap();
        let host = ready_host(&app_path);

        let (step, message) = slim_app(
            &app_for(&app_path, "com.vendor.swap"),
            &binaries_of(&app_path),
            &host,
            true,
        )
        .unwrap_err();

        assert_eq!(step, 1);
        assert!(message.contains(&holding.display().to_string()));
        assert!(message.contains("left from an earlier run"));
        assert!(holding.join("SwapApp.app/marker").exists());
        assert!(!host.ran("ditto"));
    }

    #[test]
    fn test_failed_ditto_removes_partial_copy() {
        let dir = tempdir().unwrap();
        let app_path = dir.path().join("X.app");
        write_test_app(&app_path, "com.vendor.x");
        let before = fs::read(app_path.join("Contents/MacOS/testapp")).unwrap();
        let host = FakeHost::idle();
        host.when("ditto", {
            let partial = holding_of(&app_path).join("X.app");
            move || {
                fs::create_dir_all(&partial).unwrap();
                fs::write(partial.join("partial"), "half").unwrap();
            }
        });
        host.fail("ditto", "boom");

        let result = slim_app(
            &app_for(&app_path, "com.vendor.x"),
            &binaries_of(&app_path),
            &host,
            true,
        );

        assert!(matches!(result, Err((1, _))));
        assert!(!holding_of(&app_path).exists());
        assert_eq!(
            fs::read(app_path.join("Contents/MacOS/testapp")).unwrap(),
            before
        );
    }

    #[test]
    fn test_failed_swap_leaves_original() {
        let dir = tempdir().unwrap();
        let app_path = dir.path().join("X.app");
        write_test_app(&app_path, "com.vendor.x");
        let before = fs::read(app_path.join("Contents/MacOS/testapp")).unwrap();
        let host = ready_host(&app_path);
        host.when("/bin/ps", {
            let clone = holding_of(&app_path).join("X.app");
            move || fs::remove_dir_all(&clone).unwrap()
        });

        let result = slim_app(
            &app_for(&app_path, "com.vendor.x"),
            &binaries_of(&app_path),
            &host,
            true,
        );

        assert!(matches!(result, Err((7, _))), "{result:?}");
        assert_eq!(
            fs::read(app_path.join("Contents/MacOS/testapp")).unwrap(),
            before
        );
        assert!(!holding_of(&app_path).exists());
    }

    #[test]
    fn test_intel_mac_stops_with_message() {
        let host = FakeHost::idle();
        host.fail("sysctl", "sysctl: unknown oid 'hw.optional.arm64'\n");
        assert!(!is_apple_silicon(&host));
    }

    #[test]
    fn test_failed_trash_reports_done_with_old_app_path() {
        let dir = tempdir().unwrap();
        let app_path = dir.path().join("X.app");
        write_test_app(&app_path, "com.vendor.x");
        let host = ready_host(&app_path);
        host.fail_trash("denied");

        let done = slim_app(
            &app_for(&app_path, "com.vendor.x"),
            &binaries_of(&app_path),
            &host,
            true,
        )
        .unwrap();

        let old = holding_of(&app_path).join("X.app");
        assert_eq!(done.old_app, Some((old.clone(), "denied".to_string())));
        assert_arm_only(&app_path.join("Contents/MacOS/testapp"));
        assert!(old.exists());
        assert!(holding_of(&app_path).exists());
    }

    #[test]
    fn test_host_trash_call_comes_last() {
        let dir = tempdir().unwrap();
        let app_path = dir.path().join("SwapApp.app");
        write_test_app(&app_path, "com.vendor.swap");
        let host = ready_host(&app_path);
        host.simulate_trash();

        let done = slim_app(
            &app_for(&app_path, "com.vendor.swap"),
            &binaries_of(&app_path),
            &host,
            true,
        )
        .unwrap();

        assert_eq!(done.old_app, None);
        assert_eq!(done.holding_left, None);
        assert_eq!(
            host.trashed(),
            vec![holding_of(&app_path).join("SwapApp.app")]
        );
        assert_arm_only(&app_path.join("Contents/MacOS/testapp"));
        assert!(!holding_of(&app_path).exists());
    }

    #[test]
    fn test_holding_folder_left_after_the_trash_is_reported() {
        let dir = tempdir().unwrap();
        let app_path = dir.path().join("X.app");
        write_test_app(&app_path, "com.vendor.x");
        // The Trash call succeeds but leaves the old app in the holding folder.
        let host = ready_host(&app_path);

        let done = slim_app(
            &app_for(&app_path, "com.vendor.x"),
            &binaries_of(&app_path),
            &host,
            true,
        )
        .unwrap();

        let holding = holding_of(&app_path);
        assert_eq!(done.old_app, None);
        let (left, error) = done.holding_left.clone().expect("the folder stays");
        assert_eq!(left, holding);
        assert!(!error.is_empty());
        assert!(holding.exists());
    }

    #[test]
    fn test_summary_says_what_a_slim_saved_and_left() {
        let saved = format_bytes(2048);
        let trashed = SlimDone {
            saved: 2048,
            old_app: None,
            holding_left: None,
        };
        assert_eq!(
            trashed.summary("X"),
            format!("Slimmed X, saved {saved}. The old app was moved to the Trash.")
        );

        let refused = SlimDone {
            old_app: Some((
                PathBuf::from("/A/X.app.taert-slim-tmp/X.app"),
                "denied".to_string(),
            )),
            ..trashed.clone()
        };
        assert_eq!(
            refused.summary("X"),
            format!(
                "Slimmed X, saved {saved}. The old app is still at /A/X.app.taert-slim-tmp/X.app (could not move it to the Trash: denied)"
            )
        );

        let left = SlimDone {
            holding_left: Some((
                PathBuf::from("/A/X.app.taert-slim-tmp"),
                "Directory not empty".to_string(),
            )),
            ..trashed
        };
        assert_eq!(
            left.summary("X"),
            format!(
                "Slimmed X, saved {saved}. The old app was moved to the Trash, but the folder /A/X.app.taert-slim-tmp is left (could not remove it: Directory not empty). Move it to the Trash yourself before you slim X again."
            )
        );
    }

    #[test]
    fn test_root_owned_files_and_setuid_are_refused() {
        let (_dir, layout) = test_env();
        let app_path = layout.system("Applications").join("SetuidApp.app");
        write_test_app(&app_path, "com.vendor.setuid");
        let binary = app_path.join("Contents/MacOS/testapp");
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o4755)).unwrap();
        let host = FakeHost::idle();

        let result = check_eligibility(
            &app_for(&app_path, "com.vendor.setuid"),
            &host,
            &layout,
            &IgnoreStore::default(),
            &HashSet::new(),
            None,
            true,
        );
        assert_eq!(result.unwrap_err(), "Contains a setuid or setgid file");

        let other = unsafe { libc::geteuid() } + 1;
        assert_eq!(
            check_ownership(&app_path, other).unwrap_err(),
            "Belongs to another user"
        );
        assert_eq!(
            check_ownership(&binary, other).unwrap_err(),
            "Belongs to another user"
        );
    }

    #[test]
    fn test_linked_app_is_refused() {
        let (_dir, layout) = test_env();
        let real = layout.home.join("Elsewhere/Linked.app");
        write_test_app(&real, "com.vendor.linked");
        let helper = real.join("Contents/MacOS/helper");
        fs::write(&helper, "helper").unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o4755)).unwrap();
        let binary_before = fs::read(real.join("Contents/MacOS/testapp")).unwrap();
        let link = layout.system("Applications").join("Linked.app");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let app = app_for(&link, "com.vendor.linked");
        let host = FakeHost::idle();
        host.reply(
            "codesign -dv",
            "Authority=Developer ID Application: Acme Corp (123)\n",
        );
        host.reply("codesign --verify", "");
        host.reply("spctl", "");
        host.reply("ditto", "");
        fake_ditto(&host, &link);
        let ignore = IgnoreStore::default();

        let eligibility =
            check_eligibility(&app, &host, &layout, &ignore, &HashSet::new(), None, false);
        assert_eq!(eligibility.unwrap_err(), LINKED_APP);

        let result = slim_app(&app, &binaries_of(&link), &host, false);
        assert_eq!(result, Err((1, LINKED_APP.to_string())));
        assert!(!host.ran("ditto"));
        assert_eq!(fs::read_link(&link).unwrap(), real);
        assert_eq!(
            fs::read(real.join("Contents/MacOS/testapp")).unwrap(),
            binary_before
        );
        assert_eq!(fs::metadata(&helper).unwrap().mode() & 0o7777, 0o4755);
        assert!(!holding_of(&link).exists());
        assert!(!holding_of(&real).exists());

        let scan = slim_scan(&layout, &host, &ignore, None, None);
        assert!(scan.groups.is_empty());
        assert!(
            scan.notes.contains(&format!("Linked stays: {LINKED_APP}.")),
            "{:?}",
            scan.notes
        );
    }

    #[test]
    fn test_ownership_passes_for_own_plain_files() {
        let dir = tempdir().unwrap();
        let app_path = dir.path().join("Plain.app");
        write_test_app(&app_path, "com.vendor.plain");

        let me = unsafe { libc::geteuid() };

        assert_eq!(check_ownership(&app_path, me), Ok(()));
    }

    fn app_with_scan_host(layout: &Layout, name: &str) -> FakeHost {
        let app_path = layout.system("Applications").join(format!("{name}.app"));
        write_test_app(&app_path, &format!("com.vendor.{name}"));
        let host = FakeHost::idle();
        host.reply(
            "codesign -dv",
            "Authority=Developer ID Application: Acme Corp (123)\n",
        );
        host
    }

    #[test]
    fn test_scan_lists_already_failing_signature() {
        let (_dir, layout) = test_env();
        let host = app_with_scan_host(&layout, "Broken");
        host.fail("codesign --verify", "code has been modified\n");

        let scan = slim_scan(&layout, &host, &IgnoreStore::default(), None, None);

        assert!(scan.groups.is_empty());
        assert!(
            scan.notes
                .contains(&"Broken stays: Already fails the signature check.".to_string()),
            "{:?}",
            scan.notes
        );
    }

    #[test]
    fn test_scan_lists_already_failing_gatekeeper() {
        let (_dir, layout) = test_env();
        let host = app_with_scan_host(&layout, "Rejected");
        host.reply("codesign --verify", "");
        host.fail("spctl", "rejected\n");

        let scan = slim_scan(&layout, &host, &IgnoreStore::default(), None, None);

        assert!(scan.groups.is_empty());
        assert!(
            scan.notes
                .contains(&"Rejected stays: Already fails the Gatekeeper check.".to_string()),
            "{:?}",
            scan.notes
        );
    }

    #[test]
    fn test_scan_lists_a_sound_app() {
        let (_dir, layout) = test_env();
        let host = app_with_scan_host(&layout, "Sound");
        host.reply("codesign --verify", "");
        host.reply("spctl", "");

        let scan = slim_scan(&layout, &host, &IgnoreStore::default(), None, None);

        assert_eq!(scan.groups.len(), 1, "{:?}", scan.notes);
    }

    #[test]
    fn test_scan_leaves_out_the_old_app_in_a_holding_folder() {
        let (_dir, layout) = test_env();
        let host = app_with_scan_host(&layout, "Sound");
        host.reply("codesign --verify", "");
        host.reply("spctl", "");
        let app_path = layout.system("Applications/Sound.app");
        write_test_app(&holding_of(&app_path).join("Sound.app"), "com.vendor.Sound");

        let scan = slim_scan(&layout, &host, &IgnoreStore::default(), None, None);

        let listed: Vec<&PathBuf> = scan
            .groups
            .iter()
            .flat_map(|group| &group.items)
            .map(|item| &item.path)
            .collect();
        assert_eq!(listed, vec![&app_path]);
    }

    #[test]
    fn test_xattr_retry_only_after_codesign_failure() {
        let dir = tempdir().unwrap();
        let app_path = dir.path().join("X.app");
        write_test_app(&app_path, "com.vendor.x");
        let marked = holding_of(&app_path).join("X.app/Contents/Info.plist");
        let host = cloning_host(&app_path);
        host.when("ditto", {
            let marked = marked.clone();
            move || set_finder_info(&marked)
        });
        host.reply("codesign", "");
        host.fail("spctl", "rejected\n");
        let seen = Arc::new(Mutex::new(Vec::new()));
        host.when("spctl", {
            let (marked, seen) = (marked.clone(), seen.clone());
            move || seen.lock().unwrap().push(has_finder_info(&marked))
        });

        let result = slim_app(
            &app_for(&app_path, "com.vendor.x"),
            &binaries_of(&app_path),
            &host,
            false,
        );

        assert!(matches!(result, Err((4, _))), "{result:?}");
        assert_eq!(*seen.lock().unwrap(), vec![true]);
    }

    #[test]
    fn test_xattr_cleanup_runs_after_codesign_failure() {
        let dir = tempdir().unwrap();
        let app_path = dir.path().join("X.app");
        write_test_app(&app_path, "com.vendor.x");
        let marked = holding_of(&app_path).join("X.app/Contents/Info.plist");
        let host = cloning_host(&app_path);
        host.when("ditto", {
            let marked = marked.clone();
            move || set_finder_info(&marked)
        });
        host.fail("codesign", "resource fork not allowed\n");
        host.reply("codesign", "");
        host.reply("spctl", "");
        let seen = Arc::new(Mutex::new(Vec::new()));
        host.when("spctl", {
            let (marked, seen) = (marked.clone(), seen.clone());
            move || seen.lock().unwrap().push(has_finder_info(&marked))
        });

        let result = slim_app(
            &app_for(&app_path, "com.vendor.x"),
            &binaries_of(&app_path),
            &host,
            false,
        );

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(*seen.lock().unwrap(), vec![false]);
    }

    fn write_nested_seal(path: &Path, key: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>files2</key><dict>
<key>{key}</key><dict><key>hash2</key><data>AAAA</data></dict>
</dict></dict></plist>"#
        );
        fs::write(path, plist).unwrap();
    }

    fn fat_x86_arm() -> Vec<u8> {
        let slices = [
            (CPU_TYPE_X86_64, 3, 4096, 4, 12),
            (CPU_TYPE_ARM64, 0, 16384, 4, 14),
        ];
        make_fat_binary_32(&slices, &[b"x86!", b"arm!"])
    }

    #[test]
    fn test_file_sealed_by_nested_framework_is_left_alone() {
        let dir = tempdir().unwrap();
        let app_path = dir.path().join("Nested.app");
        write_test_app(&app_path, "com.vendor.nested");
        let version = app_path.join("Contents/Frameworks/F.framework/Versions/A");
        fs::create_dir_all(version.join("Resources")).unwrap();
        fs::write(version.join("Resources/helper"), fat_x86_arm()).unwrap();
        write_nested_seal(
            &version.join("_CodeSignature/CodeResources"),
            "Resources/helper",
        );

        let (found, _) = find_universal_binaries(&app_path);

        assert_eq!(found, vec![app_path.join("Contents/MacOS/testapp")]);
    }

    #[test]
    fn test_file_sealed_by_a_versions_resources_seal_is_left_alone() {
        let dir = tempdir().unwrap();
        let app_path = dir.path().join("Nested.app");
        write_test_app(&app_path, "com.vendor.nested");
        let version = app_path.join("Contents/Frameworks/F.framework/Versions/A");
        fs::create_dir_all(version.join("Resources")).unwrap();
        fs::write(version.join("Resources/helper"), fat_x86_arm()).unwrap();
        write_nested_seal(&version.join("Resources/CodeResources"), "Resources/helper");

        let (found, _) = find_universal_binaries(&app_path);

        assert_eq!(found, vec![app_path.join("Contents/MacOS/testapp")]);
    }

    #[test]
    fn test_nested_app_and_flat_bundle_seals_are_read() {
        let dir = tempdir().unwrap();
        let app_path = dir.path().join("Nested.app");
        write_test_app(&app_path, "com.vendor.nested");
        let helper_app = app_path.join("Contents/Library/Helper.app");
        fs::create_dir_all(helper_app.join("Contents/MacOS")).unwrap();
        fs::write(helper_app.join("Contents/MacOS/h"), fat_x86_arm()).unwrap();
        write_nested_seal(
            &helper_app.join("Contents/_CodeSignature/CodeResources"),
            "MacOS/h",
        );
        let flat = app_path.join("Contents/PlugIns/P.bundle");
        fs::create_dir_all(&flat).unwrap();
        fs::write(flat.join("p"), fat_x86_arm()).unwrap();
        write_nested_seal(&flat.join("_CodeSignature/CodeResources"), "p");

        let (found, _) = find_universal_binaries(&app_path);

        assert_eq!(found, vec![app_path.join("Contents/MacOS/testapp")]);
    }

    #[test]
    fn test_file_under_an_unreadable_nested_seal_is_left_alone() {
        let dir = tempdir().unwrap();
        let app_path = dir.path().join("Nested.app");
        write_test_app(&app_path, "com.vendor.nested");
        let flat = app_path.join("Contents/PlugIns/P.bundle");
        fs::create_dir_all(flat.join("_CodeSignature")).unwrap();
        fs::write(flat.join("p"), fat_x86_arm()).unwrap();
        fs::write(flat.join("_CodeSignature/CodeResources"), "not a plist").unwrap();

        let (found, _) = find_universal_binaries(&app_path);

        assert_eq!(found, vec![app_path.join("Contents/MacOS/testapp")]);
    }

    #[test]
    fn test_unsealed_file_in_a_nested_bundle_is_still_listed() {
        let dir = tempdir().unwrap();
        let app_path = dir.path().join("Nested.app");
        write_test_app(&app_path, "com.vendor.nested");
        let flat = app_path.join("Contents/PlugIns/P.bundle");
        fs::create_dir_all(&flat).unwrap();
        fs::write(flat.join("p"), fat_x86_arm()).unwrap();
        write_nested_seal(&flat.join("_CodeSignature/CodeResources"), "other");

        let (found, _) = find_universal_binaries(&app_path);

        assert!(found.contains(&flat.join("p")));
    }

    #[test]
    fn test_change_inside_contents_stops_the_swap() {
        let dir = tempdir().unwrap();
        let app_path = dir.path().join("X.app");
        write_test_app(&app_path, "com.vendor.x");
        let host = ready_host(&app_path);
        host.when("spctl", {
            let plist = app_path.join("Contents/Info.plist");
            move || {
                let mut file = fs::OpenOptions::new().append(true).open(&plist).unwrap();
                file.write_all(b" ").unwrap();
            }
        });

        let result = slim_app(
            &app_for(&app_path, "com.vendor.x"),
            &binaries_of(&app_path),
            &host,
            false,
        );

        assert!(matches!(result, Err((6, _))), "{result:?}");
        let header = parse_fat_header_from_file(&app_path.join("Contents/MacOS/testapp"))
            .unwrap()
            .unwrap();
        assert_eq!(header.slices.len(), 2);
        assert!(!holding_of(&app_path).exists());
    }

    #[test]
    fn test_fingerprint_sees_deep_changes_and_ignores_none() {
        let dir = tempdir().unwrap();
        let app_path = dir.path().join("X.app");
        write_test_app(&app_path, "com.vendor.x");

        let first = fingerprint(&app_path).unwrap();
        assert_eq!(fingerprint(&app_path).unwrap(), first);

        fs::write(app_path.join("Contents/MacOS/extra"), "x").unwrap();
        assert_ne!(fingerprint(&app_path).unwrap(), first);
    }

    #[test]
    fn test_failed_ps_refuses() {
        let (_dir, layout) = test_env();
        let app_path = layout.system("Applications").join("Psy.app");
        write_test_app(&app_path, "com.vendor.psy");
        let host = FakeHost::default();
        host.fail("/bin/ps", "");
        host.reply(
            "codesign -dv",
            "Authority=Developer ID Application: Acme Corp (123)\n",
        );
        host.reply("codesign --verify", "");
        host.reply("spctl", "");
        host.reply("ditto", "");
        fake_ditto(&host, &app_path);

        assert_eq!(
            running_processes(&host).unwrap_err(),
            "Could not list running programs"
        );

        let result = slim_app(
            &app_for(&app_path, "com.vendor.psy"),
            &binaries_of(&app_path),
            &host,
            false,
        );
        assert!(matches!(result, Err((6, _))), "{result:?}");
        assert!(!holding_of(&app_path).exists());

        let scan = slim_scan(&layout, &host, &IgnoreStore::default(), None, None);
        assert!(scan.groups.is_empty());
        assert!(
            scan.notes
                .contains(&"Psy stays: Could not list running programs.".to_string()),
            "{:?}",
            scan.notes
        );
    }

    #[test]
    fn test_removexattr_leaves_symlink_target_alone() {
        let dir = tempdir().unwrap();
        let outside = dir.path().join("outside");
        fs::write(&outside, "data").unwrap();
        set_finder_info(&outside);
        let bundle = dir.path().join("B.app");
        fs::create_dir_all(&bundle).unwrap();
        std::os::unix::fs::symlink(&outside, bundle.join("link")).unwrap();

        clean_ext_attributes(&bundle);

        assert!(has_finder_info(&outside));
    }

    #[test]
    fn test_savings_use_allocated_sizes() {
        let dir = tempdir().unwrap();
        let app_path = dir.path().join("Big.app");
        write_test_app(&app_path, "com.vendor.big");
        let binary = app_path.join("Contents/MacOS/testapp");
        fs::write(&binary, big_fat_binary()).unwrap();
        let before = DirSizer::allocated_size(&fs::metadata(&binary).unwrap());
        let (found, intel) = find_universal_binaries(&app_path);
        let host = ready_host(&app_path);

        let done = slim_app(&app_for(&app_path, "com.vendor.big"), &found, &host, true).unwrap();

        let after = DirSizer::allocated_size(&fs::metadata(&binary).unwrap());
        assert_ne!(before - after, intel, "the fixture must tell the two apart");
        assert_eq!(done.saved, before - after);
    }

    #[test]
    fn slim_app_logged_records_the_bytes_saved() {
        let dir = tempdir().unwrap();
        let app_path = dir.path().join("SwapApp.app");
        write_test_app(&app_path, "com.vendor.swap");
        let app = app_for(&app_path, "com.vendor.swap");
        let host = ready_host(&app_path);

        let history_dir = tempdir().unwrap();
        let log = HistoryLog::new(history_dir.path().join("history.jsonl"));
        let session = log.start_run("slim", "force", Some(1));

        let done = slim_app_logged(&session, &app, &binaries_of(&app_path), &host, true).unwrap();
        session.finish();

        let runs = log.load_runs().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].items.len(), 1);
        assert_eq!(runs[0].items[0].outcome, OutcomeKind::Changed);
        assert_eq!(runs[0].items[0].path, app.path.to_string_lossy());
        assert_eq!(runs[0].items[0].bytes, done.saved);

        let dir2 = tempdir().unwrap();
        let app_path2 = dir2.path().join("TargetApp.app");
        write_test_app(&app_path2, "com.vendor.target");
        let host2 = cloning_host(&app_path2);
        host2.fail("codesign", "verification failed\n");

        let history_dir2 = tempdir().unwrap();
        let log2 = HistoryLog::new(history_dir2.path().join("history.jsonl"));
        let session2 = log2.start_run("slim", "force", Some(1));

        let res2 = slim_app_logged(
            &session2,
            &app_for(&app_path2, "com.vendor.target"),
            &binaries_of(&app_path2),
            &host2,
            true,
        );
        assert!(res2.is_err());
        session2.finish();

        let runs2 = log2.load_runs().unwrap();
        assert_eq!(runs2.len(), 1);
        assert!(runs2[0].items.is_empty());
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use crate::core::host::fake::FakeHost;

    pub fn make_fat_binary_32(slices: &[(u32, u32, u32, u32, u32)], payloads: &[&[u8]]) -> Vec<u8> {
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

    /// A fat binary whose Intel slice is large enough that slimming it frees blocks.
    pub fn big_fat_binary() -> Vec<u8> {
        let payload = vec![0xAB_u8; 1_000_003];
        let slices = [
            (CPU_TYPE_X86_64, 3, 4096, payload.len() as u32, 12),
            (CPU_TYPE_ARM64, 0, 1_015_808, 4, 14),
        ];
        make_fat_binary_32(&slices, &[&payload, b"arm!"])
    }

    pub fn write_test_app(bundle: &Path, id: &str) {
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

    pub fn app_for(path: &Path, id: &str) -> InstalledApp {
        InstalledApp {
            name: path.file_stem().unwrap().to_string_lossy().into_owned(),
            path: path.to_path_buf(),
            bundle_id: Some(id.to_string()),
            executable: Some("testapp".to_string()),
            size_bytes: 10000,
            last_used: None,
        }
    }

    pub fn holding_of(app: &Path) -> PathBuf {
        let name = app.file_name().unwrap().to_string_lossy();
        app.with_file_name(format!("{name}.taert-slim-tmp"))
    }

    fn copy_tree(from: &Path, to: &Path) {
        let meta = fs::symlink_metadata(from).unwrap();
        if meta.is_dir() {
            fs::create_dir_all(to).unwrap();
            for entry in fs::read_dir(from).unwrap().flatten() {
                copy_tree(&entry.path(), &to.join(entry.file_name()));
            }
        } else {
            fs::copy(from, to).unwrap();
        }
    }

    /// A `ditto` that copies the app to where `slim_app` asks: inside the holding folder.
    pub fn fake_ditto(host: &FakeHost, app: &Path) {
        let (app, clone) = (
            app.to_path_buf(),
            holding_of(app).join(app.file_name().unwrap()),
        );
        host.when("ditto", move || copy_tree(&app, &clone));
    }

    const FINDER_INFO: &[u8] = b"com.apple.FinderInfo\0";

    pub fn set_finder_info(path: &Path) {
        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        let mut value = [0_u8; 32];
        value[0] = 1;
        let result = unsafe {
            libc::setxattr(
                path.as_ptr(),
                FINDER_INFO.as_ptr().cast(),
                value.as_ptr().cast(),
                value.len(),
                0,
                0,
            )
        };
        assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
    }

    pub fn has_finder_info(path: &Path) -> bool {
        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        let size = unsafe {
            libc::getxattr(
                path.as_ptr(),
                FINDER_INFO.as_ptr().cast(),
                std::ptr::null_mut(),
                0,
                0,
                0,
            )
        };
        size >= 0
    }
}

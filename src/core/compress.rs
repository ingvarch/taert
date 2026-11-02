//! APFS transparent per-file compression (`decmpfs`) engine.
//!
//! Compresses files in place using macOS `decmpfs` with LZFSE (type 12)
//! chunks stored in the file's resource fork, verified via read-back and atomically
//! swapped via `renamex_np(RENAME_SWAP)`.

use std::collections::HashSet;
use std::ffi::{CStr, CString, OsStr};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::darwin::fs::MetadataExt as DarwinMetadataExt;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::apfs;
use super::cleaner::{RemovalFailure, RemovalReport};
use super::host::Host;
use super::layout::Layout;
use super::safety::{Safety, SafetyError};
use super::scanner::{CategoryScan, ScanItem, ToolGroup};
use super::stores::IgnoreStore;

pub const BLOCK_SIZE: usize = 65536; // 64 KiB blocks
pub const UF_COMPRESSED: u32 = 0x0000_0020;
const UF_IMMUTABLE: u32 = 0x0000_0002;
const UF_APPEND: u32 = 0x0000_0004;
const SF_IMMUTABLE: u32 = 0x0002_0000;
const SF_APPEND: u32 = 0x0004_0000;
pub const DECMPFS_MAGIC: u32 = 0x636d_7066; // 'cmpf' in LE

const XATTR_NOFOLLOW: libc::c_int = 0x0001;
const ACL_TYPE_EXTENDED: libc::c_int = 0x0000_0100;
const RENAME_SWAP: libc::c_uint = 0x0000_0002;
const LSOF: &str = "/usr/sbin/lsof";
const LSOF_TIMEOUT: Duration = Duration::from_secs(60);
/// Marks taert's own copy until right before the swap. The cleanup of leftovers
/// removes only marked files, so it never removes an original.
const COPY_MARK: &CStr = c"io.github.ingvarch.taert.copy";

const COMPRESSION_LZFSE: i32 = 0x801;

#[link(name = "compression")]
unsafe extern "C" {
    fn compression_encode_buffer(
        dst_buffer: *mut u8,
        dst_size: usize,
        src_buffer: *const u8,
        src_size: usize,
        scratch_buffer: *mut u8,
        algorithm: i32,
    ) -> usize;
    fn compression_encode_scratch_buffer_size(algorithm: i32) -> usize;
}

unsafe extern "C" {
    fn renamex_np(
        oldpath: *const libc::c_char,
        newpath: *const libc::c_char,
        flags: libc::c_uint,
    ) -> libc::c_int;
    fn listxattr(
        path: *const libc::c_char,
        namebuf: *mut libc::c_char,
        size: usize,
        options: libc::c_int,
    ) -> isize;
    fn getxattr(
        path: *const libc::c_char,
        name: *const libc::c_char,
        value: *mut libc::c_void,
        size: usize,
        position: u32,
        options: libc::c_int,
    ) -> isize;
    fn setxattr(
        path: *const libc::c_char,
        name: *const libc::c_char,
        value: *const libc::c_void,
        size: usize,
        position: u32,
        options: libc::c_int,
    ) -> libc::c_int;
    fn acl_get_file(path: *const libc::c_char, acl_type: libc::c_int) -> *mut libc::c_void;
    fn acl_set_file(
        path: *const libc::c_char,
        acl_type: libc::c_int,
        acl: *mut libc::c_void,
    ) -> libc::c_int;
    fn acl_free(obj: *mut libc::c_void) -> libc::c_int;
    fn acl_init(count: libc::c_int) -> *mut libc::c_void;
    fn acl_to_text(acl: *mut libc::c_void, len: *mut isize) -> *mut libc::c_char;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Lzfse,
}

impl Codec {
    pub const fn type_id(self) -> u32 {
        match self {
            Self::Lzfse => 12,
        }
    }

    pub const fn algorithm(self) -> i32 {
        match self {
            Self::Lzfse => COMPRESSION_LZFSE,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkipReason {
    AlreadyCompressed,
    SymlinkOrHardlink,
    SharesCloneBlocks,
    CloudOrDataless,
    HasResourceForkOrDecmpfs,
    NotUserOwned,
    Incompressible,
    NotCompressed,
    Locked,
    OpenForWriting,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressFailure {
    pub path: PathBuf,
    pub error: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkippedStats {
    pub already_compressed: usize,
    pub symlink_or_hardlink: usize,
    pub cloned: usize,
    pub cloud_or_dataless: usize,
    pub has_xattr: usize,
    pub not_user_owned: usize,
    pub incompressible: usize,
    pub not_compressed: usize,
    pub locked: usize,
    pub open_for_writing: usize,
}

impl SkippedStats {
    pub fn record(&mut self, reason: SkipReason) {
        match reason {
            SkipReason::AlreadyCompressed => self.already_compressed += 1,
            SkipReason::SymlinkOrHardlink => self.symlink_or_hardlink += 1,
            SkipReason::SharesCloneBlocks => self.cloned += 1,
            SkipReason::CloudOrDataless => self.cloud_or_dataless += 1,
            SkipReason::HasResourceForkOrDecmpfs => self.has_xattr += 1,
            SkipReason::NotUserOwned => self.not_user_owned += 1,
            SkipReason::Incompressible => self.incompressible += 1,
            SkipReason::NotCompressed => self.not_compressed += 1,
            SkipReason::Locked => self.locked += 1,
            SkipReason::OpenForWriting => self.open_for_writing += 1,
        }
    }

    pub fn total(&self) -> usize {
        self.already_compressed
            + self.symlink_or_hardlink
            + self.cloned
            + self.cloud_or_dataless
            + self.has_xattr
            + self.not_user_owned
            + self.incompressible
            + self.not_compressed
            + self.locked
            + self.open_for_writing
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompressReport {
    pub scanned_files: usize,
    pub processed_files: usize,
    pub original_bytes: u64,
    pub compressed_bytes: u64,
    pub saved_bytes: u64,
    pub original_blocks: u64,
    pub compressed_blocks: u64,
    pub saved_blocks: u64,
    pub skipped: SkippedStats,
    pub failures: Vec<CompressFailure>,
    pub dry_run: bool,
    pub undo: bool,
}

#[derive(Debug, Clone, Default)]
pub struct CompressOptions {
    pub dry_run: bool,
    pub undo: bool,
}

#[derive(Debug, Clone)]
pub struct CompressCandidate {
    pub name: String,
    pub path: PathBuf,
    pub category: &'static str,
    pub estimated_saving_bytes: u64,
    pub total_bytes: u64,
    pub selected: bool,
}

/// Outcome of processing an individual file.
#[derive(Debug, PartialEq, Eq)]
pub enum FileOutcome {
    Compressed {
        original_bytes: u64,
        compressed_bytes: u64,
        saved_bytes: u64,
        original_blocks: u64,
        compressed_blocks: u64,
        saved_blocks: u64,
    },
    Unpacked {
        original_bytes: u64,
        original_blocks: u64,
        restored_blocks: u64,
    },
    Skipped(SkipReason),
}

/// Generates a unique temporary path in the same parent folder as `path`.
fn temp_path_for(path: &Path) -> Result<PathBuf, std::io::Error> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no parent dir"))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    let pid = std::process::id();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Ok(parent.join(format!(".{name}.taert-tmp-{pid}-{nanos}-{seq}.tmp")))
}

/// A name beside `path` for a changed original that must stay. The cleanup of
/// leftovers does not match it.
fn kept_path_for(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    path.with_file_name(format!("{name}.taert-changed-{nanos}"))
}

fn has_xattr(c_path: &CStr, name: &CStr) -> bool {
    let res = unsafe {
        getxattr(
            c_path.as_ptr(),
            name.as_ptr(),
            std::ptr::null_mut(),
            0,
            0,
            XATTR_NOFOLLOW,
        )
    };
    res >= 0
}

/// Checks whether `path` already carries compression-related xattrs.
pub fn has_compression_xattrs(path: &Path) -> Result<bool, std::io::Error> {
    let c_file = c_path(path)?;
    Ok(has_xattr(&c_file, c"com.apple.decmpfs") || has_xattr(&c_file, c"com.apple.ResourceFork"))
}

fn mark_copy(file: &File) -> io::Result<()> {
    check(unsafe {
        libc::fsetxattr(
            file.as_raw_fd(),
            COPY_MARK.as_ptr(),
            b"".as_ptr().cast(),
            0,
            0,
            0,
        )
    })
}

fn unmark_copy(file: &File) -> io::Result<()> {
    check(unsafe { libc::fremovexattr(file.as_raw_fd(), COPY_MARK.as_ptr(), 0) })
}

fn has_copy_mark(path: &Path) -> bool {
    c_path(path).is_ok_and(|c_file| has_xattr(&c_file, COPY_MARK))
}

/// Compress a single buffer chunk into a valid decmpfs codec frame.
pub fn compress_chunk(src: &[u8], scratch: &mut [u8], codec: Codec) -> Option<Vec<u8>> {
    let mut dst = vec![0u8; src.len() + src.len() / 16 + 1024];
    let n = unsafe {
        compression_encode_buffer(
            dst.as_mut_ptr(),
            dst.len(),
            src.as_ptr(),
            src.len(),
            scratch.as_mut_ptr(),
            codec.algorithm(),
        )
    };
    if n == 0 {
        return None;
    }
    dst.truncate(n);
    Some(dst)
}

/// Build `com.apple.decmpfs` header: 16 bytes little-endian.
pub fn decmpfs_header(codec: Codec, uncompressed_size: u64) -> [u8; 16] {
    let mut hdr = [0u8; 16];
    hdr[0..4].copy_from_slice(&DECMPFS_MAGIC.to_le_bytes());
    hdr[4..8].copy_from_slice(&codec.type_id().to_le_bytes());
    hdr[8..16].copy_from_slice(&uncompressed_size.to_le_bytes());
    hdr
}

/// Build resource fork payload: `(num_blocks + 1) * 4` offsets table + block bytes.
pub fn build_resource_fork_payload(
    blocks: &[Vec<u8>],
) -> Result<(Vec<u8>, Vec<u8>), std::io::Error> {
    let num_blocks = blocks.len();
    let table_len = num_blocks
        .checked_add(1)
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "table overflow"))?;

    let mut table = Vec::with_capacity(table_len);
    let mut offset = u32::try_from(table_len)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "offset overflow"))?;
    table.extend_from_slice(&offset.to_le_bytes());

    let mut total_payload_len = 0usize;
    for b in blocks {
        let block_len = u32::try_from(b.len())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "block overflow"))?;
        offset = offset.checked_add(block_len).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "offset overflow")
        })?;
        table.extend_from_slice(&offset.to_le_bytes());
        total_payload_len += b.len();
    }

    let mut payload = Vec::with_capacity(total_payload_len);
    for b in blocks {
        payload.extend_from_slice(b);
    }

    Ok((table, payload))
}

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
}

fn check(res: libc::c_int) -> io::Result<()> {
    if res == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Copies extended attributes from `c_src` to `c_dst`, skipping `com.apple.decmpfs`
/// and `com.apple.ResourceFork`. Any error stops the copy.
fn copy_xattrs(c_src: &CString, c_dst: &CString) -> io::Result<()> {
    let list_size = unsafe { listxattr(c_src.as_ptr(), std::ptr::null_mut(), 0, XATTR_NOFOLLOW) };
    if list_size < 0 {
        return Err(io::Error::last_os_error());
    }
    if list_size == 0 {
        return Ok(());
    }

    let mut buf = vec![0u8; list_size as usize];
    let actual_size = unsafe {
        listxattr(
            c_src.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            XATTR_NOFOLLOW,
        )
    };
    if actual_size < 0 {
        return Err(io::Error::last_os_error());
    }

    let names = &buf[..actual_size as usize];
    for name in names.split(|&b| b == 0).filter(|p| !p.is_empty()) {
        if name == b"com.apple.decmpfs" || name == b"com.apple.ResourceFork" {
            continue;
        }
        let c_name =
            CString::new(name).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        let val_size = unsafe {
            getxattr(
                c_src.as_ptr(),
                c_name.as_ptr(),
                std::ptr::null_mut(),
                0,
                0,
                XATTR_NOFOLLOW,
            )
        };
        if val_size < 0 {
            return Err(io::Error::last_os_error());
        }

        let mut value = vec![0u8; val_size as usize];
        let got_size = unsafe {
            getxattr(
                c_src.as_ptr(),
                c_name.as_ptr(),
                value.as_mut_ptr() as *mut libc::c_void,
                value.len(),
                0,
                XATTR_NOFOLLOW,
            )
        };
        if got_size < 0 {
            return Err(io::Error::last_os_error());
        }

        check(unsafe {
            setxattr(
                c_dst.as_ptr(),
                c_name.as_ptr(),
                value.as_ptr() as *const libc::c_void,
                got_size as usize,
                0,
                XATTR_NOFOLLOW,
            )
        })?;
    }

    Ok(())
}

fn copy_acl(c_src: &CString, c_dst: &CString) -> io::Result<()> {
    let acl = unsafe { acl_get_file(c_src.as_ptr(), ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        let error = io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ENOENT) => Ok(()),
            _ => Err(error),
        };
    }
    let res = unsafe { acl_set_file(c_dst.as_ptr(), ACL_TYPE_EXTENDED, acl) };
    let error = io::Error::last_os_error();
    unsafe { acl_free(acl) };
    if res == 0 { Ok(()) } else { Err(error) }
}

/// Removes every ACL entry from `c_path`.
fn clear_acl(c_path: &CString) -> io::Result<()> {
    let existing = unsafe { acl_get_file(c_path.as_ptr(), ACL_TYPE_EXTENDED) };
    if existing.is_null() {
        return Ok(());
    }
    unsafe { acl_free(existing) };
    let empty = unsafe { acl_init(0) };
    if empty.is_null() {
        return Err(io::Error::last_os_error());
    }
    let res = unsafe { acl_set_file(c_path.as_ptr(), ACL_TYPE_EXTENDED, empty) };
    let error = io::Error::last_os_error();
    unsafe { acl_free(empty) };
    if res == 0 { Ok(()) } else { Err(error) }
}

/// Whether an ACL text has a deny entry for `right`.
fn text_denies(text: &str, right: &str) -> bool {
    text.lines().any(|line| {
        let mut fields = line.rsplitn(3, ':');
        let (Some(rights), Some(kind)) = (fields.next(), fields.next()) else {
            return false;
        };
        kind.starts_with("deny") && rights.split(',').any(|r| r == right)
    })
}

fn acl_denies(path: &Path, right: &str) -> bool {
    let Ok(c_file) = c_path(path) else {
        return false;
    };
    let acl = unsafe { acl_get_file(c_file.as_ptr(), ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        return false;
    }
    let text = unsafe { acl_to_text(acl, std::ptr::null_mut()) };
    let denied =
        !text.is_null() && text_denies(&unsafe { CStr::from_ptr(text) }.to_string_lossy(), right);
    unsafe {
        if !text.is_null() {
            acl_free(text.cast());
        }
        acl_free(acl);
    }
    denied
}

fn is_locked(meta: &fs::Metadata) -> bool {
    DarwinMetadataExt::st_flags(meta) & (UF_IMMUTABLE | SF_IMMUTABLE | UF_APPEND | SF_APPEND) != 0
}

/// Whether the file or its folder stops a swap: flags, or an ACL that denies delete.
/// A folder that cannot be read counts as locked.
fn blocks_replacement(path: &Path, meta: &fs::Metadata) -> bool {
    let folder_blocks = |folder: &Path| {
        fs::symlink_metadata(folder).map_or(true, |m| is_locked(&m))
            || acl_denies(folder, "delete_child")
    };
    is_locked(meta) || acl_denies(path, "delete") || path.parent().is_none_or(folder_blocks)
}

/// Removes a file that may carry flags or an ACL that stop the removal.
fn force_remove(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    let c_file = c_path(path)?;
    let flags = DarwinMetadataExt::st_flags(&meta);
    if flags & (UF_IMMUTABLE | UF_APPEND) != 0 {
        check(unsafe { libc::chflags(c_file.as_ptr(), flags & !(UF_IMMUTABLE | UF_APPEND)) })?;
    }
    let _ = clear_acl(&c_file);
    fs::remove_file(path)
}

/// Sets access, modification, and birth times. The birth time goes last,
/// because an earlier modification time pulls it back.
fn copy_times(fd: libc::c_int, meta: &fs::Metadata) -> io::Result<()> {
    let times = [
        libc::timespec {
            tv_sec: meta.atime(),
            tv_nsec: meta.atime_nsec(),
        },
        libc::timespec {
            tv_sec: meta.mtime(),
            tv_nsec: meta.mtime_nsec(),
        },
    ];
    check(unsafe { libc::futimens(fd, times.as_ptr()) })?;

    let mut attrs: libc::attrlist = unsafe { std::mem::zeroed() };
    attrs.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
    attrs.commonattr = libc::ATTR_CMN_CRTIME;
    let mut birth = libc::timespec {
        tv_sec: DarwinMetadataExt::st_birthtime(meta),
        tv_nsec: DarwinMetadataExt::st_birthtime_nsec(meta),
    };
    check(unsafe {
        libc::fsetattrlist(
            fd,
            (&mut attrs as *mut libc::attrlist).cast(),
            (&mut birth as *mut libc::timespec).cast(),
            std::mem::size_of::<libc::timespec>(),
            0,
        )
    })
}

/// Reads until `buf` is full or the file ends.
fn read_full(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Compares `temp_path` and `original_path` byte by byte.
fn verify_identical_content(
    temp_path: &Path,
    original_path: &Path,
    expected_len: u64,
) -> io::Result<()> {
    let mut f1 = File::open(temp_path)?;
    let mut f2 = File::open(original_path)?;

    let mut buf1 = vec![0u8; BLOCK_SIZE];
    let mut buf2 = vec![0u8; BLOCK_SIZE];
    let mut compared = 0u64;

    loop {
        let n1 = read_full(&mut f1, &mut buf1)?;
        let n2 = read_full(&mut f2, &mut buf2)?;

        if n1 != n2 || buf1[..n1] != buf2[..n2] {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "verification mismatch: byte contents differ",
            ));
        }

        if n1 == 0 {
            break;
        }

        compared += n1 as u64;
    }

    if compared != expected_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("verification mismatch: expected {expected_len} bytes, read {compared}"),
        ));
    }

    Ok(())
}

fn swap(temp_path: &Path, orig_path: &Path) -> io::Result<()> {
    let (c_temp, c_orig) = (c_path(temp_path)?, c_path(orig_path)?);
    check(unsafe { renamex_np(c_temp.as_ptr(), c_orig.as_ptr(), RENAME_SWAP) })
}

/// The steps of a replacement, so a test can fail or change the file at each one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    WriteData,
    WriteHeader,
    CopyMetadata,
    Verify,
    SetTimes,
    Permissions,
    BeforeSwap,
    Swap,
    AfterSwap,
}

type Probe<'a> = &'a dyn Fn(Step) -> io::Result<()>;

fn no_probe(_: Step) -> io::Result<()> {
    Ok(())
}

/// What tells that a file was written to: the inode, size, and modification time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Identity {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: i64,
    mtime_nsec: i64,
}

impl Identity {
    fn of(meta: &fs::Metadata) -> Self {
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.len(),
            mtime: meta.mtime(),
            mtime_nsec: meta.mtime_nsec(),
        }
    }
}

fn changed() -> io::Error {
    io::Error::other("the file changed while it was being processed, so it was left as it is")
}

/// Removes the temporary file when dropped, whatever way the work ends.
struct TempGuard {
    path: PathBuf,
    armed: bool,
}

impl TempGuard {
    fn remove(&mut self) -> io::Result<()> {
        if !self.armed {
            return Ok(());
        }
        self.armed = false;
        match force_remove(&self.path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}

/// A new copy of a file, built beside it and swapped in when it checks out.
struct Replacement<'a> {
    path: &'a Path,
    meta: &'a fs::Metadata,
    snapshot: Identity,
    temp: TempGuard,
    file: File,
    probe: Probe<'a>,
}

impl<'a> Replacement<'a> {
    /// Opens the original for reading and creates the temporary file.
    fn begin(path: &'a Path, meta: &'a fs::Metadata, probe: Probe<'a>) -> io::Result<(Self, File)> {
        let source = File::open(path)?;
        let snapshot = Identity::of(&source.metadata()?);
        if snapshot != Identity::of(meta) {
            return Err(changed());
        }
        let temp_path = temp_path_for(path)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp_path)?;
        let temp = TempGuard {
            path: temp_path,
            armed: true,
        };
        clear_acl(&c_path(&temp.path)?)?;
        mark_copy(&file)?;
        let replacement = Self {
            path,
            meta,
            snapshot,
            temp,
            file,
            probe,
        };
        Ok((replacement, source))
    }

    fn step(&self, step: Step) -> io::Result<()> {
        (self.probe)(step)
    }

    /// Copies owner, flags, and extended attributes. Any error fails the file.
    fn copy_attributes(&self, compressed: bool) -> io::Result<()> {
        self.step(Step::CopyMetadata)?;
        let fd = self.file.as_raw_fd();
        check(unsafe { libc::fchown(fd, self.meta.uid(), self.meta.gid()) })?;

        let flags = DarwinMetadataExt::st_flags(self.meta);
        let flags = if compressed {
            flags | UF_COMPRESSED
        } else {
            flags & !UF_COMPRESSED
        };
        check(unsafe { libc::fchflags(fd, flags) })?;

        copy_xattrs(&c_path(self.path)?, &c_path(&self.temp.path)?)
    }

    fn verify(&self, len: u64) -> io::Result<()> {
        self.step(Step::Verify)?;
        verify_identical_content(&self.temp.path, self.path, len)
    }

    /// Sets the times, ACL, and mode, swaps the copy in, and checks again that
    /// nobody wrote to the original. A changed original goes back in place.
    fn finish(&mut self) -> io::Result<()> {
        self.step(Step::SetTimes)?;
        copy_times(self.file.as_raw_fd(), self.meta)?;

        self.step(Step::Permissions)?;
        unmark_copy(&self.file)?;
        copy_acl(&c_path(self.path)?, &c_path(&self.temp.path)?)?;
        let mode = (self.meta.mode() & 0o7777) as libc::mode_t;
        check(unsafe { libc::fchmod(self.file.as_raw_fd(), mode) })?;

        self.step(Step::BeforeSwap)?;
        if Identity::of(&fs::symlink_metadata(self.path)?) != self.snapshot {
            return Err(changed());
        }

        self.step(Step::Swap)?;
        let copy = self.file.metadata()?;
        swap(&self.temp.path, self.path)?;

        let unchanged =
            self.step(Step::AfterSwap)
                .and_then(|()| match fs::symlink_metadata(&self.temp.path) {
                    Ok(old) if Identity::of(&old) == self.snapshot => Ok(()),
                    _ => Err(changed()),
                });
        match unchanged {
            Ok(()) => Ok(()),
            Err(error) => Err(self.swap_back(error, (copy.dev(), copy.ino()))),
        }
    }

    /// Puts the changed original back, if `path` still holds the copy. Otherwise the
    /// changed original is kept under a name that the cleanup does not match.
    fn swap_back(&mut self, error: io::Error, copy: (u64, u64)) -> io::Error {
        let holds_copy = fs::symlink_metadata(self.path).is_ok_and(|m| (m.dev(), m.ino()) == copy);
        if holds_copy && swap(&self.temp.path, self.path).is_ok() {
            return error;
        }
        self.temp.armed = false;
        let kept = kept_path_for(self.path);
        match fs::rename(&self.temp.path, &kept) {
            Ok(()) => io::Error::other(format!(
                "{error}; the changed file is kept at {}",
                kept.display()
            )),
            Err(back) => io::Error::other(format!(
                "{error}; the changed file is at {} ({back})",
                self.temp.path.display()
            )),
        }
    }

    /// Removes the temporary file and adds a failure to do so to `result`.
    fn cleanup<T>(
        mut self,
        result: Result<T, Box<dyn std::error::Error>>,
    ) -> Result<T, Box<dyn std::error::Error>> {
        let Err(e) = self.temp.remove() else {
            return result;
        };
        let leftover = format!(
            "the temporary file {} could not be removed: {e}",
            self.temp.path.display()
        );
        Err(match result {
            Ok(_) => leftover.into(),
            Err(error) => format!("{error}; {leftover}").into(),
        })
    }
}

/// Check eligibility of a single file for compression.
pub fn check_eligibility_compress(path: &Path, meta: &fs::Metadata) -> Result<(), SkipReason> {
    if !meta.file_type().is_file() || meta.file_type().is_symlink() || meta.nlink() > 1 {
        return Err(SkipReason::SymlinkOrHardlink);
    }

    let uid = unsafe { libc::geteuid() };
    if meta.uid() != uid {
        return Err(SkipReason::NotUserOwned);
    }

    if blocks_replacement(path, meta) {
        return Err(SkipReason::Locked);
    }

    if DarwinMetadataExt::st_flags(meta) & UF_COMPRESSED != 0 {
        return Err(SkipReason::AlreadyCompressed);
    }

    if Safety::is_cloud_or_dataless(path) {
        return Err(SkipReason::CloudOrDataless);
    }

    if let Some(id) = apfs::clone_identity(path)
        && id.shares_all_blocks
    {
        return Err(SkipReason::SharesCloneBlocks);
    }

    if has_compression_xattrs(path).unwrap_or(false) {
        return Err(SkipReason::HasResourceForkOrDecmpfs);
    }

    Ok(())
}

/// Estimate compression ratio on a file without modifying disk.
pub fn estimate_file(path: &Path, meta: &fs::Metadata) -> Result<FileOutcome, SkipReason> {
    check_eligibility_compress(path, meta)?;

    let len = meta.len();
    if len < 4096 {
        return Err(SkipReason::Incompressible);
    }

    let scratch_len = unsafe { compression_encode_scratch_buffer_size(COMPRESSION_LZFSE) };
    let mut scratch = vec![0u8; scratch_len];

    // For files > 2 MB, sample blocks evenly across the file
    let sample_threshold = 2 * 1024 * 1024;
    let (ratio, estimated_compressed_len) = if len > sample_threshold {
        let mut file = File::open(path).map_err(|_| SkipReason::Incompressible)?;
        let num_blocks = (len as usize).div_ceil(BLOCK_SIZE);
        let sample_count = 16.min(num_blocks);
        let mut sampled_raw_bytes = 0usize;
        let mut sampled_compressed_bytes = 0usize;

        let mut buf = vec![0u8; BLOCK_SIZE];
        for i in 0..sample_count {
            let block_idx = (i * (num_blocks - 1)) / (sample_count - 1).max(1);
            let offset = (block_idx * BLOCK_SIZE) as u64;
            if file.seek(SeekFrom::Start(offset)).is_ok() {
                let to_read = ((len - offset) as usize).min(BLOCK_SIZE);
                if let Ok(n) = file.read(&mut buf[..to_read])
                    && n > 0
                {
                    if let Some(compressed) = compress_chunk(&buf[..n], &mut scratch, Codec::Lzfse)
                    {
                        sampled_raw_bytes += n;
                        sampled_compressed_bytes += compressed.len();
                    } else {
                        sampled_raw_bytes += n;
                        sampled_compressed_bytes += n;
                    }
                }
            }
        }

        if sampled_raw_bytes == 0 {
            return Err(SkipReason::Incompressible);
        }

        let r = sampled_compressed_bytes as f64 / sampled_raw_bytes as f64;
        let table_overhead = ((num_blocks + 1) * 4) as u64;
        let estimated = ((len as f64 * r).round() as u64) + table_overhead;
        (r, estimated)
    } else {
        let data = fs::read(path).map_err(|_| SkipReason::Incompressible)?;
        let mut blocks = Vec::new();
        for chunk in data.chunks(BLOCK_SIZE) {
            let Some(compressed) = compress_chunk(chunk, &mut scratch, Codec::Lzfse) else {
                return Err(SkipReason::Incompressible);
            };
            blocks.push(compressed);
        }
        let (table, payload) =
            build_resource_fork_payload(&blocks).map_err(|_| SkipReason::Incompressible)?;
        let total = (table.len() + payload.len()) as u64;
        let r = total as f64 / len as f64;
        (r, total)
    };

    if estimated_compressed_len + 4096 > len || ratio >= 0.98 {
        return Err(SkipReason::Incompressible);
    }

    let orig_blocks = meta.blocks();
    // APFS allocates in 4 KB clusters
    let comp_blocks = ((estimated_compressed_len.div_ceil(4096)) * 8).min(orig_blocks);

    if orig_blocks <= comp_blocks || (orig_blocks - comp_blocks) * 512 < 4096 {
        return Err(SkipReason::Incompressible);
    }

    Ok(FileOutcome::Compressed {
        original_bytes: len,
        compressed_bytes: estimated_compressed_len,
        saved_bytes: len.saturating_sub(estimated_compressed_len),
        original_blocks: orig_blocks,
        compressed_blocks: comp_blocks,
        saved_blocks: orig_blocks.saturating_sub(comp_blocks),
    })
}

/// Compresses `src` into the resource fork at `rsrc_path` one block at a time.
/// Returns the size of the fork, or `None` when the gain is under 4 KB.
fn write_compressed_fork(
    src: &mut File,
    rsrc_path: &Path,
    raw_len: u64,
    probe: Probe,
) -> io::Result<Option<u64>> {
    let too_large = || io::Error::new(io::ErrorKind::InvalidInput, "file too large to compress");
    let num_blocks =
        usize::try_from(raw_len.div_ceil(BLOCK_SIZE as u64)).map_err(|_| too_large())?;
    let table_len = (num_blocks + 1) * 4;

    let mut rsrc = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(rsrc_path)?;
    rsrc.write_all(&vec![0u8; table_len])?;

    let scratch_len = unsafe { compression_encode_scratch_buffer_size(COMPRESSION_LZFSE) };
    let mut scratch = vec![0u8; scratch_len];
    let mut buf = vec![0u8; BLOCK_SIZE];
    let mut offset = u32::try_from(table_len).map_err(|_| too_large())?;
    let mut offsets = Vec::with_capacity(num_blocks + 1);
    offsets.push(offset);

    let mut remaining = raw_len;
    while remaining > 0 {
        probe(Step::WriteData)?;
        let want = remaining.min(BLOCK_SIZE as u64) as usize;
        src.read_exact(&mut buf[..want])?;
        let Some(block) = compress_chunk(&buf[..want], &mut scratch, Codec::Lzfse) else {
            return Ok(None);
        };
        let block_len = u32::try_from(block.len()).map_err(|_| too_large())?;
        offset = offset.checked_add(block_len).ok_or_else(too_large)?;
        if u64::from(offset) + 4096 > raw_len {
            return Ok(None);
        }
        rsrc.write_all(&block)?;
        offsets.push(offset);
        remaining -= want as u64;
    }

    let table: Vec<u8> = offsets.iter().flat_map(|o| o.to_le_bytes()).collect();
    rsrc.seek(SeekFrom::Start(0))?;
    rsrc.write_all(&table)?;
    rsrc.flush()?;
    rsrc.sync_all()?;
    Ok(Some(u64::from(offset)))
}

/// Compress an individual file following the atomic 4-step pipeline.
pub fn compress_file(
    path: &Path,
    dry_run: bool,
) -> Result<FileOutcome, Box<dyn std::error::Error>> {
    compress_file_probed(path, dry_run, &no_probe)
}

fn compress_file_probed(
    path: &Path,
    dry_run: bool,
    probe: Probe,
) -> Result<FileOutcome, Box<dyn std::error::Error>> {
    let meta = fs::symlink_metadata(path)?;

    if let Err(reason) = check_eligibility_compress(path, &meta) {
        return Ok(FileOutcome::Skipped(reason));
    }

    if dry_run {
        return match estimate_file(path, &meta) {
            Ok(outcome) => Ok(outcome),
            Err(reason) => Ok(FileOutcome::Skipped(reason)),
        };
    }

    let raw_len = meta.len();
    if raw_len < 4096 {
        return Ok(FileOutcome::Skipped(SkipReason::Incompressible));
    }

    let (mut replacement, mut source) = Replacement::begin(path, &meta, probe)?;
    let result = compress_into(&mut replacement, &mut source, &meta, probe);
    replacement.cleanup(result)
}

fn compress_into(
    replacement: &mut Replacement,
    source: &mut File,
    meta: &fs::Metadata,
    probe: Probe,
) -> Result<FileOutcome, Box<dyn std::error::Error>> {
    let raw_len = meta.len();

    // Step 1: Write to temporary file in same folder
    let rsrc_path = replacement.temp.path.join("..namedfork").join("rsrc");
    let Some(encoded_len) = write_compressed_fork(source, &rsrc_path, raw_len, probe)? else {
        return Ok(FileOutcome::Skipped(SkipReason::Incompressible));
    };

    replacement.step(Step::WriteHeader)?;
    let c_temp = c_path(&replacement.temp.path)?;
    let hdr = decmpfs_header(Codec::Lzfse, raw_len);
    let xattr_decmp = CString::new("com.apple.decmpfs")?;
    check(unsafe {
        setxattr(
            c_temp.as_ptr(),
            xattr_decmp.as_ptr(),
            hdr.as_ptr() as *const libc::c_void,
            hdr.len(),
            0,
            XATTR_NOFOLLOW,
        )
    })?;

    // Step 2: Copy owner, flags, and extended attributes
    replacement.copy_attributes(true)?;

    // Check actual blocks saved on the written temp file
    let temp_meta = fs::symlink_metadata(&replacement.temp.path)?;
    let orig_blocks = meta.blocks();
    let temp_blocks = temp_meta.blocks();
    if orig_blocks <= temp_blocks || (orig_blocks - temp_blocks) * 512 < 4096 {
        return Ok(FileOutcome::Skipped(SkipReason::Incompressible));
    }

    // Step 3: Read temporary file back and compare byte by byte with original
    replacement.verify(raw_len)?;

    // Step 4: Set times, ACL, and mode, then swap and check the original again
    replacement.finish()?;

    let saved_blocks = orig_blocks - temp_blocks;
    let saved_bytes = saved_blocks * 512;

    Ok(FileOutcome::Compressed {
        original_bytes: raw_len,
        compressed_bytes: encoded_len,
        saved_bytes,
        original_blocks: orig_blocks,
        compressed_blocks: temp_blocks,
        saved_blocks,
    })
}

/// Unpack a compressed file back to a normal file using the atomic 4-step pipeline.
pub fn undo_file(path: &Path, dry_run: bool) -> Result<FileOutcome, Box<dyn std::error::Error>> {
    undo_file_probed(path, dry_run, &no_probe)
}

fn undo_file_probed(
    path: &Path,
    dry_run: bool,
    probe: Probe,
) -> Result<FileOutcome, Box<dyn std::error::Error>> {
    let meta = fs::symlink_metadata(path)?;

    if !meta.file_type().is_file() || meta.file_type().is_symlink() {
        return Ok(FileOutcome::Skipped(SkipReason::SymlinkOrHardlink));
    }

    let uid = unsafe { libc::geteuid() };
    if meta.uid() != uid {
        return Ok(FileOutcome::Skipped(SkipReason::NotUserOwned));
    }

    let flags = DarwinMetadataExt::st_flags(&meta);
    if flags & UF_COMPRESSED == 0 {
        return Ok(FileOutcome::Skipped(SkipReason::NotCompressed));
    }
    if blocks_replacement(path, &meta) {
        return Ok(FileOutcome::Skipped(SkipReason::Locked));
    }

    let orig_blocks = meta.blocks();
    let len = meta.len();

    if dry_run {
        let estimated_blocks = len.div_ceil(4096) * 8;
        return Ok(FileOutcome::Unpacked {
            original_bytes: len,
            original_blocks: orig_blocks,
            restored_blocks: estimated_blocks,
        });
    }

    let (mut replacement, mut source) = Replacement::begin(path, &meta, probe)?;
    let result = undo_into(&mut replacement, &mut source, len, probe);
    replacement.cleanup(result)?;
    let restored_blocks = fs::symlink_metadata(path)?.blocks();
    Ok(FileOutcome::Unpacked {
        original_bytes: len,
        original_blocks: orig_blocks,
        restored_blocks,
    })
}

fn undo_into(
    replacement: &mut Replacement,
    source: &mut File,
    len: u64,
    probe: Probe,
) -> Result<(), Box<dyn std::error::Error>> {
    // Step 1: Write decompressed bytes to plain data fork of temp file
    let mut buf = vec![0u8; BLOCK_SIZE];
    let mut copied = 0u64;
    loop {
        probe(Step::WriteData)?;
        let n = read_full(source, &mut buf)?;
        if n == 0 {
            break;
        }
        replacement.file.write_all(&buf[..n])?;
        copied += n as u64;
    }
    if copied != len {
        return Err(Box::new(changed()));
    }
    replacement.file.sync_all()?;

    // Step 2: Copy owner, flags (without UF_COMPRESSED), and extended attributes
    replacement.copy_attributes(false)?;

    // Step 3: Read temporary file back and compare byte by byte with original
    replacement.verify(len)?;

    // Step 4: Set times, ACL, and mode, then swap and check the original again
    replacement.finish()?;
    Ok(())
}

/// Files that a program holds open for writing, from one `lsof` listing.
fn open_for_writing(host: &dyn Host) -> Result<HashSet<PathBuf>, String> {
    let listing = host.run(LSOF, &["-nP", "-w", "-F", "fan"], LSOF_TIMEOUT);
    if !listing.ok {
        return Err("could not list the files in use, so none were changed".to_string());
    }
    Ok(parse_open_for_writing(&listing.stdout))
}

fn parse_open_for_writing(listing: &str) -> HashSet<PathBuf> {
    let mut found = HashSet::new();
    let mut writing = false;
    let mut name: Option<&str> = None;
    let mut flush = |writing: &mut bool, name: &mut Option<&str>| {
        if let Some(name) = name.take()
            && *writing
        {
            found.insert(PathBuf::from(name));
        }
        *writing = false;
    };
    for line in listing.lines() {
        let (tag, rest) = line.split_at_checked(1).unwrap_or(("", ""));
        match tag {
            "p" | "f" => flush(&mut writing, &mut name),
            "a" => writing = rest == "w" || rest == "u",
            "n" => name = Some(rest),
            _ => {}
        }
    }
    flush(&mut writing, &mut name);
    found
}

/// For a leftover `.<name>.taert-tmp-<pid>-<nanos>-<seq>.tmp`: the name and the pid.
fn stale_temp_owner(file_name: &str) -> Option<(&str, u32)> {
    let rest = file_name.strip_prefix('.')?.strip_suffix(".tmp")?;
    let (name, tail) = rest.rsplit_once(".taert-tmp-")?;
    let mut parts = tail.split('-');
    let pid = parts.next()?.parse().ok()?;
    parts.next()?.parse::<u128>().ok()?;
    parts.next()?.parse::<u64>().ok()?;
    parts.next().is_none().then_some((name, pid))
}

fn process_is_alive(pid: u32) -> bool {
    let res = unsafe { libc::kill(pid as libc::pid_t, 0) };
    res == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Removes a temporary file that an earlier run left behind. A file without the
/// copy mark may be an original that a swap moved there, so it stays.
fn remove_stale_temp(path: &Path) -> Result<(), String> {
    let failed =
        |e: io::Error| format!("could not remove a temporary file from an earlier run: {e}");
    let meta = fs::symlink_metadata(path).map_err(failed)?;
    if !meta.file_type().is_file() {
        return Ok(());
    }
    if !has_copy_mark(path) {
        return Err(
            "a temporary file from an earlier run may hold the original with changes, \
             so it was left in place. Compare it with the file beside it and remove the one \
             you do not need"
                .to_string(),
        );
    }
    force_remove(path).map_err(failed)
}

/// What a run needs: commands, folders, and the ignore list.
pub struct Compressor<'a> {
    pub host: &'a dyn Host,
    pub layout: &'a Layout,
    pub ignore: &'a IgnoreStore,
}

impl CompressReport {
    fn record(&mut self, outcome: &FileOutcome) {
        match *outcome {
            FileOutcome::Compressed {
                original_bytes,
                compressed_bytes,
                saved_bytes,
                original_blocks,
                compressed_blocks,
                saved_blocks,
            } => {
                self.processed_files += 1;
                self.original_bytes += original_bytes;
                self.compressed_bytes += compressed_bytes;
                self.saved_bytes += saved_bytes;
                self.original_blocks += original_blocks;
                self.compressed_blocks += compressed_blocks;
                self.saved_blocks += saved_blocks;
            }
            FileOutcome::Unpacked {
                original_bytes,
                original_blocks,
                restored_blocks,
            } => {
                self.processed_files += 1;
                self.original_bytes += original_bytes;
                self.original_blocks += original_blocks;
                self.compressed_blocks += restored_blocks;
            }
            FileOutcome::Skipped(reason) => self.skipped.record(reason),
        }
    }
}

impl Compressor<'_> {
    /// Checks the path with the safety gate, then compresses or unpacks the file
    /// or every file below it. Both the command line and the TUI come here.
    pub fn process_path<F>(
        &self,
        path: &Path,
        options: &CompressOptions,
        cancelled: &AtomicBool,
        mut on_file: F,
    ) -> Result<CompressReport, SafetyError>
    where
        F: FnMut(&Path, &FileOutcome),
    {
        let path = Safety::absolute(path);
        Safety::can_compress_with_host(&path, self.ignore, self.layout, self.host)?;
        let path = path.canonicalize().unwrap_or(path);

        let mut report = CompressReport {
            dry_run: options.dry_run,
            undo: options.undo,
            ..Default::default()
        };

        let busy = if options.dry_run {
            HashSet::new()
        } else {
            match open_for_writing(self.host) {
                Ok(busy) => busy,
                Err(error) => {
                    report.failures.push(CompressFailure { path, error });
                    return Ok(report);
                }
            }
        };

        if path.is_file() {
            if !options.dry_run
                && let (Some(parent), Some(name)) = (path.parent(), path.file_name())
            {
                self.remove_stale_temps(parent, Some(name), &mut report);
            }
            self.process_file(&path, options, &busy, &mut report, &mut on_file);
            return Ok(report);
        }

        let mut dirs_to_visit = vec![path];
        while let Some(current_dir) = dirs_to_visit.pop() {
            if cancelled.load(Ordering::Relaxed) {
                break;
            }

            if Safety::is_ignored_or_protected(&current_dir, self.ignore)
                || Safety::is_live_data_path(&current_dir, self.layout)
            {
                continue;
            }

            if !options.dry_run {
                self.remove_stale_temps(&current_dir, None, &mut report);
            }

            let entries = match fs::read_dir(&current_dir) {
                Ok(e) => e,
                Err(err) => {
                    report.failures.push(CompressFailure {
                        path: current_dir,
                        error: err.to_string(),
                    });
                    continue;
                }
            };

            for entry in entries.flatten() {
                if cancelled.load(Ordering::Relaxed) {
                    break;
                }

                let entry_path = entry.path();
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };

                if file_type.is_dir() {
                    if file_type.is_symlink() || Safety::is_cloud_or_dataless(&entry_path) {
                        continue;
                    }
                    if let Some(refusal) = self.refuse_bundle(&entry_path, &current_dir) {
                        report.failures.push(refusal);
                        continue;
                    }
                    dirs_to_visit.push(entry_path);
                } else if file_type.is_file()
                    && stale_temp_owner(&entry.file_name().to_string_lossy()).is_none()
                {
                    self.process_file(&entry_path, options, &busy, &mut report, &mut on_file);
                }
            }
        }

        Ok(report)
    }

    /// Compresses each path the TUI selected and sums up the result.
    pub fn compress_selected(
        &self,
        paths: &[PathBuf],
        cancelled: &AtomicBool,
        mut on_progress: impl FnMut(usize, &Path),
        mut on_path_done: impl FnMut(&Path, u64),
    ) -> RemovalReport {
        let options = CompressOptions::default();
        let mut result = RemovalReport::default();
        for (index, path) in paths.iter().enumerate() {
            if cancelled.load(Ordering::Relaxed) {
                break;
            }
            on_progress(index, path);
            match self.process_path(path, &options, cancelled, |_, _| {}) {
                Ok(report) => {
                    if report.processed_files > 0 {
                        on_path_done(path, report.saved_bytes);
                    }
                    result.freed_bytes += report.saved_bytes;
                    result.removed_count += report.processed_files;
                    result
                        .failures
                        .extend(report.failures.into_iter().map(|f| RemovalFailure {
                            path: f.path,
                            error: f.error,
                        }));
                }
                Err(error) => result.failures.push(RemovalFailure {
                    path: path.clone(),
                    error: error.to_string(),
                }),
            }
        }
        let mut note = format!(
            "Compressed {} files, freed {} via APFS transparent compression.",
            result.removed_count,
            super::bytes::format_bytes(result.freed_bytes)
        );
        if cancelled.load(Ordering::Relaxed) {
            note.push_str(" Cancelled before the end.");
        }
        result.notes.push(note);
        result
    }

    /// An app bundle met on the way gets the same check as one named on the command line.
    fn refuse_bundle(&self, dir: &Path, parent: &Path) -> Option<CompressFailure> {
        let is_bundle = dir
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("app"));
        if !is_bundle || Safety::enclosing_app_bundle(parent).is_some() {
            return None;
        }
        Safety::can_compress_with_host(dir, self.ignore, self.layout, self.host)
            .err()
            .map(|error| CompressFailure {
                path: dir.to_path_buf(),
                error: error.to_string(),
            })
    }

    fn process_file<F>(
        &self,
        path: &Path,
        options: &CompressOptions,
        busy: &HashSet<PathBuf>,
        report: &mut CompressReport,
        on_file: &mut F,
    ) where
        F: FnMut(&Path, &FileOutcome),
    {
        report.scanned_files += 1;
        let in_use = !busy.is_empty() && path.canonicalize().is_ok_and(|real| busy.contains(&real));
        let res = if in_use {
            Ok(FileOutcome::Skipped(SkipReason::OpenForWriting))
        } else if options.undo {
            undo_file(path, options.dry_run)
        } else {
            compress_file(path, options.dry_run)
        };
        match res {
            Ok(outcome) => {
                on_file(path, &outcome);
                report.record(&outcome);
            }
            Err(e) => report.failures.push(CompressFailure {
                path: path.to_path_buf(),
                error: e.to_string(),
            }),
        }
    }

    /// Deletes temporary files left by runs that are gone, for one file or for all of `dir`.
    fn remove_stale_temps(&self, dir: &Path, only: Option<&OsStr>, report: &mut CompressReport) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let Some((name, pid)) =
                stale_temp_owner(&file_name.to_string_lossy()).map(|(n, p)| (n.to_string(), p))
            else {
                continue;
            };
            if only.is_some_and(|only| only != OsStr::new(&name))
                || pid == std::process::id()
                || process_is_alive(pid)
            {
                continue;
            }
            if let Err(error) = remove_stale_temp(&entry.path()) {
                report.failures.push(CompressFailure {
                    path: entry.path(),
                    error,
                });
            }
        }
    }
}

const SAMPLE_FILES: usize = 48;

/// Estimates how many bytes compressing `root` would free: compresses a sample
/// of its files, weighted by size, in memory and applies the ratio to `total_bytes`.
fn sampled_saving(root: &Path, total_bytes: u64) -> u64 {
    let mut files: Vec<(PathBuf, u64)> = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let file_type = meta.file_type();
            if file_type.is_dir() {
                dirs.push(entry.path());
            } else if file_type.is_file() && meta.len() >= 4096 {
                files.push((entry.path(), meta.len()));
            }
        }
    }

    let bytes: u64 = files.iter().map(|(_, len)| len).sum();
    if bytes == 0 {
        return 0;
    }

    let step = (bytes / SAMPLE_FILES as u64).max(1);
    let mut next_pick = step / 2;
    let mut seen = 0u64;
    let (mut sampled, mut saved) = (0u64, 0u64);
    for (path, len) in &files {
        seen += len;
        if seen <= next_pick {
            continue;
        }
        next_pick = seen + step;
        let Ok(meta) = fs::symlink_metadata(path) else {
            continue;
        };
        sampled += len;
        if let Ok(FileOutcome::Compressed { saved_bytes, .. }) = estimate_file(path, &meta) {
            saved += saved_bytes;
        }
    }
    if sampled == 0 {
        return 0;
    }
    (total_bytes as f64 * saved as f64 / sampled as f64) as u64
}

/// Find compression candidates for the TUI (user-owned apps, Homebrew Cellar, Rust toolchains).
pub fn find_compress_candidates(layout: &Layout) -> Vec<CompressCandidate> {
    let mut candidates = Vec::new();
    let uid = unsafe { libc::geteuid() };

    let mut push = |name: String, path: PathBuf, category: &'static str, min_bytes: u64| {
        let total = super::sizer::DirSizer::measure(&path).bytes;
        if total <= min_bytes {
            return false;
        }
        let estimate = sampled_saving(&path, total);
        if estimate == 0 {
            return false;
        }
        candidates.push(CompressCandidate {
            name,
            path,
            category,
            estimated_saving_bytes: estimate,
            total_bytes: total,
            selected: false,
        });
        true
    };

    // 1. User-owned app bundles in app roots
    for root in layout.app_roots() {
        let Ok(entries) = fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("app"))
                && let Ok(meta) = fs::metadata(&p)
                && (meta.uid() == uid || Safety::can_write_inside(&p))
            {
                let name = p
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "App".to_string());
                push(name, p, "Application", 5 * 1024 * 1024);
            }
        }
    }

    // 2. Homebrew Cellar
    let brew_paths = [
        layout.system("opt/homebrew/Cellar"),
        layout.system("usr/local/Cellar"),
    ];
    for bp in brew_paths {
        if bp.is_dir() && push("Homebrew Cellar".to_string(), bp, "Package Manager", 0) {
            break;
        }
    }

    // 3. Rust toolchains
    let rustup = layout.user(".rustup/toolchains");
    if rustup.is_dir() {
        push("Rust Toolchains".to_string(), rustup, "Developer Tools", 0);
    }

    candidates
}

/// Creates a `CategoryScan` with compression candidates for the TUI select screen.
pub fn compress_candidates_scan(layout: &Layout) -> CategoryScan {
    let candidates = find_compress_candidates(layout);
    let mut app_items = Vec::new();
    let mut brew_items = Vec::new();
    let mut rust_items = Vec::new();

    for c in candidates {
        let item = ScanItem::new(c.path, c.estimated_saving_bytes, false, false);
        match c.category {
            "Application" => app_items.push(item),
            "Package Manager" => brew_items.push(item),
            "Developer Tools" => rust_items.push(item),
            _ => app_items.push(item),
        }
    }

    let mut groups = Vec::new();
    if !app_items.is_empty() {
        groups.push(ToolGroup {
            tool: "Applications".to_string(),
            items: app_items,
        });
    }
    if !brew_items.is_empty() {
        groups.push(ToolGroup {
            tool: "Homebrew Cellar".to_string(),
            items: brew_items,
        });
    }
    if !rust_items.is_empty() {
        groups.push(ToolGroup {
            tool: "Rust Toolchains".to_string(),
            items: rust_items,
        });
    }

    CategoryScan {
        id: "compress".to_string(),
        title: "Transparent APFS Compression Candidates".to_string(),
        groups,
        notes: vec![
            "APFS transparent compression frees space without deleting files.".to_string(),
            "Writing to a file unpacks it, so this suits read-mostly folders.".to_string(),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::apps::testing::sandbox;
    use std::os::darwin::fs::MetadataExt as DarwinMetadataExt;
    use tempfile::tempdir;

    fn make_repetitive_file(dir: &Path, name: &str, size_bytes: usize) -> PathBuf {
        let file_path = dir.join(name);
        let pattern = "Hello APFS transparent decmpfs compression!\n".as_bytes();
        let mut data = Vec::with_capacity(size_bytes);
        while data.len() < size_bytes {
            let chunk_len = pattern.len().min(size_bytes - data.len());
            data.extend_from_slice(&pattern[..chunk_len]);
        }
        fs::write(&file_path, &data).unwrap();
        file_path
    }

    #[test]
    fn round_trip_gives_identical_content_with_flag_set_and_fewer_blocks() {
        let temp = tempdir().unwrap();
        let file = make_repetitive_file(temp.path(), "test.txt", 500_000);
        let original_data = fs::read(&file).unwrap();

        let meta_before = fs::symlink_metadata(&file).unwrap();
        let blocks_before = meta_before.blocks();

        let outcome = compress_file(&file, false).unwrap();
        match outcome {
            FileOutcome::Compressed {
                saved_bytes,
                original_blocks,
                compressed_blocks,
                ..
            } => {
                assert!(saved_bytes > 0);
                assert!(compressed_blocks < original_blocks);
            }
            other => panic!("expected Compressed outcome, got {other:?}"),
        }

        let meta_after = fs::symlink_metadata(&file).unwrap();
        assert_eq!(
            DarwinMetadataExt::st_flags(&meta_after) & UF_COMPRESSED,
            UF_COMPRESSED
        );
        assert!(meta_after.blocks() < blocks_before);

        let read_back = fs::read(&file).unwrap();
        assert_eq!(read_back, original_data);
    }

    #[test]
    fn mode_times_and_extended_attributes_survive() {
        let temp = tempdir().unwrap();
        let file = make_repetitive_file(temp.path(), "meta.txt", 200_000);

        // Set custom mode
        unsafe {
            libc::chmod(
                CString::new(file.to_str().unwrap()).unwrap().as_ptr(),
                0o755,
            )
        };

        // Set custom xattr
        let c_file = CString::new(file.to_str().unwrap()).unwrap();
        let xattr_name = CString::new("user.taert_test").unwrap();
        let xattr_val = b"hello_metadata";
        unsafe {
            setxattr(
                c_file.as_ptr(),
                xattr_name.as_ptr(),
                xattr_val.as_ptr() as *const _,
                xattr_val.len(),
                0,
                XATTR_NOFOLLOW,
            );
        }

        let old = (1_600_000_000, 0);
        set_times(&file, old, old, old);
        let meta_before = fs::symlink_metadata(&file).unwrap();
        let mode_before = meta_before.mode() & 0o7777;
        let mtime_before = meta_before.mtime();

        let outcome = compress_file(&file, false).unwrap();
        assert!(matches!(outcome, FileOutcome::Compressed { .. }));

        let meta_after = fs::symlink_metadata(&file).unwrap();
        assert_eq!(meta_after.mode() & 0o7777, mode_before);
        assert_eq!(meta_after.mtime(), mtime_before);

        let mut read_val = vec![0u8; 64];
        let n = unsafe {
            getxattr(
                c_file.as_ptr(),
                xattr_name.as_ptr(),
                read_val.as_mut_ptr() as *mut _,
                read_val.len(),
                0,
                XATTR_NOFOLLOW,
            )
        };
        assert_eq!(n, xattr_val.len() as isize);
        assert_eq!(&read_val[..n as usize], xattr_val);
    }

    fn set_times(path: &Path, atime: (i64, i64), mtime: (i64, i64), birth: (i64, i64)) {
        let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        let times = [
            libc::timespec {
                tv_sec: atime.0,
                tv_nsec: atime.1,
            },
            libc::timespec {
                tv_sec: mtime.0,
                tv_nsec: mtime.1,
            },
        ];
        assert_eq!(
            unsafe { libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), 0) },
            0
        );
        let mut attrs: libc::attrlist = unsafe { std::mem::zeroed() };
        attrs.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
        attrs.commonattr = libc::ATTR_CMN_CRTIME;
        let mut value = libc::timespec {
            tv_sec: birth.0,
            tv_nsec: birth.1,
        };
        let res = unsafe {
            libc::setattrlist(
                c_path.as_ptr(),
                (&mut attrs as *mut libc::attrlist).cast(),
                (&mut value as *mut libc::timespec).cast(),
                std::mem::size_of::<libc::timespec>(),
                0,
            )
        };
        assert_eq!(res, 0, "{}", std::io::Error::last_os_error());
    }

    fn times_of(path: &Path) -> [(i64, i64); 3] {
        let meta = fs::symlink_metadata(path).unwrap();
        [
            (meta.atime(), meta.atime_nsec()),
            (meta.mtime(), meta.mtime_nsec()),
            (
                DarwinMetadataExt::st_birthtime(&meta),
                DarwinMetadataExt::st_birthtime_nsec(&meta),
            ),
        ]
    }

    #[test]
    fn access_modification_and_birth_times_survive_to_the_nanosecond() {
        let temp = tempdir().unwrap();
        let file = make_repetitive_file(temp.path(), "times.txt", 300_000);
        set_times(
            &file,
            (1_600_000_000, 111_111_111),
            (1_650_000_000, 222_222_222),
            (1_500_000_000, 333_333_333),
        );
        let before = times_of(&file);

        let outcome = compress_file(&file, false).unwrap();

        assert!(matches!(outcome, FileOutcome::Compressed { .. }));
        assert_eq!(times_of(&file), before);
    }

    #[test]
    fn incompressible_file_stays_as_it_was() {
        let temp = tempdir().unwrap();
        let file = temp.path().join("incompressible.bin");
        // Pseudo-random high-entropy bytes
        let mut data = vec![0u8; 8192];
        let mut seed: u32 = 0x12345678;
        for byte in &mut data {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            *byte = (seed >> 16) as u8;
        }
        fs::write(&file, &data).unwrap();

        let meta_before = fs::symlink_metadata(&file).unwrap();
        let outcome = compress_file(&file, false).unwrap();

        assert_eq!(outcome, FileOutcome::Skipped(SkipReason::Incompressible));
        let meta_after = fs::symlink_metadata(&file).unwrap();
        assert_eq!(DarwinMetadataExt::st_flags(&meta_after) & UF_COMPRESSED, 0);
        assert_eq!(meta_after.blocks(), meta_before.blocks());
        assert_eq!(fs::read(&file).unwrap(), data);
    }

    #[test]
    fn hardlinked_file_and_cloned_file_are_skipped() {
        let temp = tempdir().unwrap();
        let file1 = make_repetitive_file(temp.path(), "link1.txt", 100_000);
        let file2 = temp.path().join("link2.txt");
        fs::hard_link(&file1, &file2).unwrap();

        let outcome = compress_file(&file1, false).unwrap();
        assert_eq!(outcome, FileOutcome::Skipped(SkipReason::SymlinkOrHardlink));

        let file3 = make_repetitive_file(temp.path(), "orig.txt", 100_000);
        let file4 = temp.path().join("clone.txt");
        let c_src = CString::new(file3.to_str().unwrap()).unwrap();
        let c_dst = CString::new(file4.to_str().unwrap()).unwrap();
        let ret = unsafe { libc::clonefile(c_src.as_ptr(), c_dst.as_ptr(), 0) };
        if ret == 0 {
            let outcome_clone = compress_file(&file4, false).unwrap();
            assert_eq!(
                outcome_clone,
                FileOutcome::Skipped(SkipReason::SharesCloneBlocks)
            );
        }
    }

    const ALL_STEPS: [Step; 9] = [
        Step::WriteData,
        Step::WriteHeader,
        Step::CopyMetadata,
        Step::Verify,
        Step::SetTimes,
        Step::Permissions,
        Step::BeforeSwap,
        Step::Swap,
        Step::AfterSwap,
    ];

    fn only_file_left(dir: &Path, name: &str) -> bool {
        let names: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names == [name]
    }

    fn is_compressed(path: &Path) -> bool {
        let meta = fs::symlink_metadata(path).unwrap();
        DarwinMetadataExt::st_flags(&meta) & UF_COMPRESSED != 0
    }

    fn temp_file_in(dir: &Path) -> PathBuf {
        fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.to_string_lossy().contains(".taert-tmp-"))
            .expect("a temporary file")
    }

    fn append(path: &Path, bytes: &[u8]) {
        OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }

    #[test]
    fn failure_at_each_step_leaves_the_original_readable() {
        for step in ALL_STEPS {
            let temp = tempdir().unwrap();
            let file = make_repetitive_file(temp.path(), "safe.txt", 300_000);
            let orig_bytes = fs::read(&file).unwrap();
            let fired = std::cell::Cell::new(false);
            let fail = |at: Step| {
                if at == step {
                    fired.set(true);
                    return Err(io::Error::other("injected"));
                }
                Ok(())
            };

            let result = compress_file_probed(&file, false, &fail);

            assert!(fired.get(), "{step:?} never ran");
            assert!(result.is_err(), "{step:?}");
            assert_eq!(fs::read(&file).unwrap(), orig_bytes, "{step:?}");
            assert!(!is_compressed(&file), "{step:?}");
            assert!(only_file_left(temp.path(), "safe.txt"), "{step:?}");
        }
    }

    #[test]
    fn failure_at_each_step_of_an_undo_leaves_the_compressed_file_readable() {
        for step in ALL_STEPS.into_iter().filter(|s| *s != Step::WriteHeader) {
            let temp = tempdir().unwrap();
            let file = make_repetitive_file(temp.path(), "safe.txt", 300_000);
            let orig_bytes = fs::read(&file).unwrap();
            compress_file(&file, false).unwrap();
            let fired = std::cell::Cell::new(false);
            let fail = |at: Step| {
                if at == step {
                    fired.set(true);
                    return Err(io::Error::other("injected"));
                }
                Ok(())
            };

            let result = undo_file_probed(&file, false, &fail);

            assert!(fired.get(), "{step:?} never ran");
            assert!(result.is_err(), "{step:?}");
            assert_eq!(fs::read(&file).unwrap(), orig_bytes, "{step:?}");
            assert!(is_compressed(&file), "{step:?}");
            assert!(only_file_left(temp.path(), "safe.txt"), "{step:?}");
        }
    }

    #[test]
    fn a_failure_halfway_through_the_blocks_leaves_nothing_behind() {
        let temp = tempdir().unwrap();
        let file = make_repetitive_file(temp.path(), "big.txt", 500_000);
        let orig_bytes = fs::read(&file).unwrap();
        let blocks = std::cell::Cell::new(0);
        let fail = |at: Step| {
            if at == Step::WriteData {
                blocks.set(blocks.get() + 1);
                if blocks.get() == 4 {
                    return Err(io::Error::other("injected"));
                }
            }
            Ok(())
        };

        assert!(compress_file_probed(&file, false, &fail).is_err());

        assert_eq!(fs::read(&file).unwrap(), orig_bytes);
        assert!(only_file_left(temp.path(), "big.txt"));
    }

    #[test]
    fn a_file_is_compressed_one_block_at_a_time() {
        let temp = tempdir().unwrap();
        let size = 8 * 1024 * 1024;
        let file = make_repetitive_file(temp.path(), "large.txt", size);
        let blocks = std::cell::Cell::new(0);
        let count = |at: Step| {
            if at == Step::WriteData {
                blocks.set(blocks.get() + 1);
            }
            Ok(())
        };

        let outcome = compress_file_probed(&file, false, &count).unwrap();

        assert!(matches!(outcome, FileOutcome::Compressed { .. }));
        assert_eq!(blocks.get(), size.div_ceil(BLOCK_SIZE));
        assert_eq!(fs::read(&file).unwrap().len(), size);
    }

    #[test]
    fn a_write_between_the_read_and_the_swap_keeps_the_original() {
        let temp = tempdir().unwrap();
        let file = make_repetitive_file(temp.path(), "live.log", 300_000);
        let mut expected = fs::read(&file).unwrap();
        expected.extend_from_slice(b"appended record\n");
        let write = |at: Step| {
            if at == Step::BeforeSwap {
                append(&file, b"appended record\n");
            }
            Ok(())
        };

        let result = compress_file_probed(&file, false, &write);

        assert!(result.is_err());
        assert_eq!(fs::read(&file).unwrap(), expected);
        assert!(!is_compressed(&file));
        assert!(only_file_left(temp.path(), "live.log"));
    }

    #[test]
    fn a_write_just_after_the_swap_goes_back_to_the_original() {
        let temp = tempdir().unwrap();
        let file = make_repetitive_file(temp.path(), "live.log", 300_000);
        let mut expected = fs::read(&file).unwrap();
        expected.extend_from_slice(b"appended record\n");
        let write = |at: Step| {
            if at == Step::AfterSwap {
                append(&temp_file_in(temp.path()), b"appended record\n");
            }
            Ok(())
        };

        let result = compress_file_probed(&file, false, &write);

        assert!(result.is_err());
        assert_eq!(fs::read(&file).unwrap(), expected);
        assert!(!is_compressed(&file));
        assert!(only_file_left(temp.path(), "live.log"));
    }

    #[test]
    fn a_file_replaced_before_the_swap_is_left_alone() {
        let temp = tempdir().unwrap();
        let file = make_repetitive_file(temp.path(), "doc.txt", 300_000);
        let replace = |at: Step| {
            if at == Step::BeforeSwap {
                let other = temp.path().join("new.txt");
                fs::write(&other, b"saved by an editor").unwrap();
                fs::rename(&other, &file).unwrap();
            }
            Ok(())
        };

        let result = compress_file_probed(&file, false, &replace);

        assert!(result.is_err());
        assert_eq!(fs::read(&file).unwrap(), b"saved by an editor");
        assert!(only_file_left(temp.path(), "doc.txt"));
    }

    #[test]
    fn undo_out_of_space_leaves_no_temporary_file() {
        let temp = tempdir().unwrap();
        let file = make_repetitive_file(temp.path(), "full.txt", 500_000);
        let orig_bytes = fs::read(&file).unwrap();
        compress_file(&file, false).unwrap();
        let blocks = std::cell::Cell::new(0);
        let fill_the_disk = |at: Step| {
            if at == Step::WriteData {
                blocks.set(blocks.get() + 1);
                if blocks.get() == 4 {
                    return Err(io::Error::from_raw_os_error(libc::ENOSPC));
                }
            }
            Ok(())
        };

        let result = undo_file_probed(&file, false, &fill_the_disk);

        assert!(result.is_err());
        assert!(only_file_left(temp.path(), "full.txt"));
        assert!(is_compressed(&file));
        assert_eq!(fs::read(&file).unwrap(), orig_bytes);
    }

    fn set_flags(path: &Path, flags: u32) {
        let c_file = c_path(path).unwrap();
        assert_eq!(unsafe { libc::chflags(c_file.as_ptr(), flags) }, 0);
    }

    #[test]
    fn a_file_with_uchg_is_skipped_and_leaves_nothing_behind() {
        let temp = tempdir().unwrap();
        let file = make_repetitive_file(temp.path(), "locked.txt", 300_000);
        set_flags(&file, UF_IMMUTABLE);

        let outcome = compress_file(&file, false);
        let undo = undo_file(&file, false);
        set_flags(&file, 0);

        assert_eq!(outcome.unwrap(), FileOutcome::Skipped(SkipReason::Locked));
        assert!(matches!(undo.unwrap(), FileOutcome::Skipped(_)));
        assert!(only_file_left(temp.path(), "locked.txt"));
    }

    fn deny(file: &Path, rights: &str) {
        let status = std::process::Command::new("/bin/chmod")
            .args(["+a", &format!("everyone deny {rights}")])
            .arg(file)
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn read_attribute(file: &Path, name: &str) -> Option<Vec<u8>> {
        let (c_file, c_name) = (c_path(file).unwrap(), CString::new(name).unwrap());
        let mut value = vec![0u8; 64];
        let n = unsafe {
            getxattr(
                c_file.as_ptr(),
                c_name.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
                0,
                XATTR_NOFOLLOW,
            )
        };
        (n >= 0).then(|| value[..n as usize].to_vec())
    }

    fn write_attribute(file: &Path, name: &str, value: &[u8]) {
        let (c_file, c_name) = (c_path(file).unwrap(), CString::new(name).unwrap());
        let res = unsafe {
            setxattr(
                c_file.as_ptr(),
                c_name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
                XATTR_NOFOLLOW,
            )
        };
        assert_eq!(res, 0);
    }

    #[test]
    fn an_acl_that_denies_writing_attributes_keeps_attributes_and_the_acl() {
        let temp = tempdir().unwrap();
        let file = make_repetitive_file(temp.path(), "acl.txt", 300_000);
        write_attribute(&file, "com.apple.metadata:kMDItemWhereFroms", b"from-here");
        write_attribute(&file, "user.tag", b"red");
        deny(&file, "writeextattr");

        let outcome = compress_file(&file, false).unwrap();

        assert!(matches!(outcome, FileOutcome::Compressed { .. }));
        assert_eq!(
            read_attribute(&file, "com.apple.metadata:kMDItemWhereFroms").as_deref(),
            Some(&b"from-here"[..])
        );
        assert_eq!(
            read_attribute(&file, "user.tag").as_deref(),
            Some(&b"red"[..])
        );
        let acl = unsafe { acl_get_file(c_path(&file).unwrap().as_ptr(), ACL_TYPE_EXTENDED) };
        assert!(!acl.is_null());
        unsafe { acl_free(acl) };
    }

    #[test]
    fn attributes_that_cannot_be_copied_fail_the_file_and_keep_it() {
        let temp = tempdir().unwrap();
        let file = make_repetitive_file(temp.path(), "acl.txt", 300_000);
        write_attribute(&file, "user.tag", b"red");
        deny(&file, "readextattr");
        let orig_bytes = fs::read(&file).unwrap();

        let result = compress_file(&file, false);

        assert!(result.is_err());
        assert!(!is_compressed(&file));
        assert_eq!(fs::read(&file).unwrap(), orig_bytes);
        assert!(only_file_left(temp.path(), "acl.txt"));
    }

    #[test]
    fn undo_restores_a_normal_file() {
        let temp = tempdir().unwrap();
        let file = make_repetitive_file(temp.path(), "round.txt", 300_000);
        let orig_bytes = fs::read(&file).unwrap();

        let compress_outcome = compress_file(&file, false).unwrap();
        assert!(matches!(compress_outcome, FileOutcome::Compressed { .. }));

        let meta_comp = fs::symlink_metadata(&file).unwrap();
        assert_eq!(
            DarwinMetadataExt::st_flags(&meta_comp) & UF_COMPRESSED,
            UF_COMPRESSED
        );

        let undo_outcome = undo_file(&file, false).unwrap();
        assert!(matches!(undo_outcome, FileOutcome::Unpacked { .. }));

        let meta_restored = fs::symlink_metadata(&file).unwrap();
        assert_eq!(
            DarwinMetadataExt::st_flags(&meta_restored) & UF_COMPRESSED,
            0
        );

        let read_back = fs::read(&file).unwrap();
        assert_eq!(read_back, orig_bytes);
    }

    #[test]
    fn dry_run_changes_nothing() {
        let temp = tempdir().unwrap();
        let file = make_repetitive_file(temp.path(), "dry.txt", 200_000);
        let orig_bytes = fs::read(&file).unwrap();
        let meta_before = fs::symlink_metadata(&file).unwrap();

        let outcome = compress_file(&file, true).unwrap();
        assert!(matches!(outcome, FileOutcome::Compressed { .. }));

        let meta_after = fs::symlink_metadata(&file).unwrap();
        assert_eq!(DarwinMetadataExt::st_flags(&meta_after) & UF_COMPRESSED, 0);
        assert_eq!(meta_after.blocks(), meta_before.blocks());
        assert_eq!(fs::read(&file).unwrap(), orig_bytes);
    }

    fn host_with(ps: &str, lsof: &str) -> crate::core::host::fake::FakeHost {
        let host = crate::core::host::fake::FakeHost::default();
        host.reply("/bin/ps", ps);
        host.reply("/usr/sbin/lsof", lsof);
        host
    }

    fn run(
        layout: &Layout,
        host: &crate::core::host::fake::FakeHost,
        path: &Path,
        options: &CompressOptions,
    ) -> Result<CompressReport, SafetyError> {
        let compressor = Compressor {
            host,
            layout,
            ignore: &IgnoreStore::default(),
        };
        compressor.process_path(path, options, &AtomicBool::new(false), |_, _| {})
    }

    fn real_run() -> CompressOptions {
        CompressOptions::default()
    }

    #[test]
    fn lsof_output_gives_the_files_open_for_writing() {
        let listing = "p1\nf3\nar\nn/read\nf4\naw\nn/write\nf5\nau\nn/both\nf6\nn/name-first\naw\np2\nf1\naw\n";

        let found = parse_open_for_writing(listing);

        let expected: HashSet<PathBuf> = ["/write", "/both", "/name-first"]
            .into_iter()
            .map(PathBuf::from)
            .collect();
        assert_eq!(found, expected);
    }

    #[test]
    fn only_taert_leftovers_are_told_apart() {
        assert_eq!(
            stale_temp_owner(".a b.txt.taert-tmp-123-456789-7.tmp"),
            Some(("a b.txt", 123))
        );
        assert_eq!(stale_temp_owner(".notes.tmp"), None);
        assert_eq!(stale_temp_owner("a.taert-tmp-1-2-3.tmp"), None);
        assert_eq!(stale_temp_owner(".a.taert-tmp-x-2-3.tmp"), None);
        assert_eq!(stale_temp_owner(".a.taert-tmp-1-2-3-4.tmp"), None);
    }

    #[test]
    fn a_run_skips_files_open_for_writing_and_compresses_the_rest() {
        let (_temp, layout) = sandbox();
        let dir = layout.user("Projects/data");
        fs::create_dir_all(&dir).unwrap();
        let busy = make_repetitive_file(&dir, "db.sqlite", 300_000);
        let idle = make_repetitive_file(&dir, "notes.txt", 300_000);
        let host = host_with(
            "    1     0 /sbin/launchd\n",
            &format!("p9\nf5\naw\nn{}\n", busy.display()),
        );

        let report = run(&layout, &host, &dir, &real_run()).unwrap();

        assert_eq!(report.skipped.open_for_writing, 1);
        assert_eq!(report.processed_files, 1);
        assert!(!is_compressed(&busy));
        assert!(is_compressed(&idle));
    }

    #[test]
    fn a_run_that_cannot_list_open_files_changes_nothing() {
        let (_temp, layout) = sandbox();
        let dir = layout.user("Projects/data");
        fs::create_dir_all(&dir).unwrap();
        let file = make_repetitive_file(&dir, "notes.txt", 300_000);
        let host = crate::core::host::fake::FakeHost::default();
        host.reply("/bin/ps", "    1     0 /sbin/launchd\n");
        host.fail("/usr/sbin/lsof", "lsof broke");

        let report = run(&layout, &host, &dir, &real_run()).unwrap();

        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.processed_files, 0);
        assert!(!is_compressed(&file));
    }

    #[test]
    fn a_dry_run_does_not_list_open_files() {
        let (_temp, layout) = sandbox();
        let dir = layout.user("Projects/data");
        fs::create_dir_all(&dir).unwrap();
        make_repetitive_file(&dir, "notes.txt", 300_000);
        let host = host_with("    1     0 /sbin/launchd\n", "");
        let options = CompressOptions {
            dry_run: true,
            undo: false,
        };

        let report = run(&layout, &host, &dir, &options).unwrap();

        assert_eq!(report.processed_files, 1);
        assert!(!host.ran("/usr/sbin/lsof"));
    }

    #[test]
    fn a_running_app_in_the_home_applications_folder_is_left_alone() {
        let (_temp, layout) = sandbox();
        let apps = layout.user("Applications");
        let running = apps.join("Run.app");
        let idle = apps.join("Idle.app");
        for bundle in [&running, &idle] {
            fs::create_dir_all(bundle.join("Contents/MacOS")).unwrap();
        }
        let in_running = make_repetitive_file(&running.join("Contents"), "data.txt", 300_000);
        let in_idle = make_repetitive_file(&idle.join("Contents"), "data.txt", 300_000);
        let host = host_with(
            &format!(
                "    1     0 /sbin/launchd\n  500     1 {}/Contents/MacOS/Run\n",
                running.display()
            ),
            "",
        );

        let report = run(&layout, &host, &apps, &real_run()).unwrap();

        assert!(!is_compressed(&in_running));
        assert!(is_compressed(&in_idle));
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].path, running);
        assert!(report.failures[0].error.contains("running"));
    }

    #[test]
    fn a_path_apps_write_to_is_refused_and_skipped_in_a_walk() {
        let (_temp, layout) = sandbox();
        let host = host_with("    1     0 /sbin/launchd\n", "");
        let data = make(&layout.user("Library/Application Support/Foo"));
        let refused = run(&layout, &host, &data, &real_run());
        assert_eq!(refused.unwrap_err(), SafetyError::AppDataPath);

        let brew = layout.system("opt/homebrew");
        let live = make_repetitive_file(&make(&brew.join("var/db")), "store.db", 300_000);
        let plain = make_repetitive_file(&make(&brew.join("Cellar/foo")), "foo.txt", 300_000);
        let report = run(&layout, &host, &brew, &real_run()).unwrap();

        assert!(!is_compressed(&live));
        assert!(is_compressed(&plain));
        assert_eq!(report.processed_files, 1);
    }

    fn make(dir: &Path) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        dir.to_path_buf()
    }

    #[test]
    fn leftovers_of_finished_runs_are_removed_and_those_of_live_runs_stay() {
        let (_temp, layout) = sandbox();
        let dir = make(&layout.user("Projects/data"));
        let dead_pid = dead_pid();
        let stale = dir.join(format!(".a.txt.taert-tmp-{dead_pid}-1-2.tmp"));
        let locked = dir.join(format!(".b.txt.taert-tmp-{dead_pid}-1-3.tmp"));
        let live = dir.join(format!(".c.txt.taert-tmp-{}-1-4.tmp", std::process::id()));
        for file in [&stale, &locked, &live] {
            fs::write(file, b"x").unwrap();
            write_attribute(file, COPY_MARK.to_str().unwrap(), b"");
        }
        set_flags(&locked, UF_IMMUTABLE);
        let host = host_with("    1     0 /sbin/launchd\n", "");

        let report = run(&layout, &host, &dir, &real_run()).unwrap();

        assert!(!stale.exists());
        assert!(!locked.exists());
        assert!(live.exists());
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.scanned_files, 0);
    }

    #[test]
    fn cancelling_stops_before_the_next_path() {
        let (_temp, layout) = sandbox();
        let dir = make(&layout.user("Projects/data"));
        let first = make_repetitive_file(&make(&dir.join("one")), "a.txt", 300_000);
        let second = make_repetitive_file(&make(&dir.join("two")), "b.txt", 300_000);
        let host = host_with("    1     0 /sbin/launchd\n", "");
        let compressor = Compressor {
            host: &host,
            layout: &layout,
            ignore: &IgnoreStore::default(),
        };
        let cancelled = AtomicBool::new(false);
        let paths = [first.clone(), second.clone()];

        let report = compressor.compress_selected(
            &paths,
            &cancelled,
            |index, _| {
                if index == 0 {
                    cancelled.store(true, Ordering::Relaxed);
                }
            },
            |_, _| {},
        );

        assert!(report.notes[0].contains("Cancelled"));
        assert!(!is_compressed(&second));
    }

    #[test]
    fn a_refused_path_is_reported_as_a_failure_of_the_selection() {
        let (_temp, layout) = sandbox();
        let data = make(&layout.user("Library/Application Support/Foo"));
        let host = host_with("    1     0 /sbin/launchd\n", "");
        let compressor = Compressor {
            host: &host,
            layout: &layout,
            ignore: &IgnoreStore::default(),
        };

        let report = compressor.compress_selected(
            std::slice::from_ref(&data),
            &AtomicBool::new(false),
            |_, _| {},
            |_, _| {},
        );

        assert_eq!(report.removed_count, 0);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].path, data);
    }

    #[test]
    fn compress_selected_reports_each_path_that_changed() {
        let (_temp, layout) = sandbox();
        let dir = make(&layout.user("Projects/data"));
        let first = make_repetitive_file(&make(&dir.join("one")), "a.txt", 300_000);
        let second = make_repetitive_file(&make(&dir.join("two")), "b.txt", 300_000);
        let refused = make(&layout.user("Library/Application Support/Foo"));

        let host = host_with("    1     0 /sbin/launchd\n", "");
        let compressor = Compressor {
            host: &host,
            layout: &layout,
            ignore: &IgnoreStore::default(),
        };

        let paths = [first.clone(), second.clone(), refused.clone()];
        let mut reported = Vec::new();

        let report = compressor.compress_selected(
            &paths,
            &AtomicBool::new(false),
            |_, _| {},
            |path, saved| {
                reported.push((path.to_path_buf(), saved));
            },
        );

        assert_eq!(reported.len(), 2);
        assert_eq!(reported[0].0, first);
        assert!(reported[0].1 > 0);
        assert_eq!(reported[1].0, second);
        assert!(reported[1].1 > 0);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].path, refused);
    }

    fn make_random_file(dir: &Path, name: &str, size_bytes: usize) -> PathBuf {
        let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
        let data: Vec<u8> = (0..size_bytes)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                (seed >> 24) as u8
            })
            .collect();
        let path = dir.join(name);
        fs::write(&path, data).unwrap();
        path
    }

    #[test]
    fn candidate_estimates_come_from_sampled_data() {
        let (_temp, layout) = sandbox();
        let toolchain = make(&layout.user(".rustup/toolchains/stable/lib"));
        make_repetitive_file(&toolchain, "libstd.rlib", 6 * 1024 * 1024);
        let apps = layout.system("Applications");
        let packed = make(&apps.join("Packed.app/Contents/Resources"));
        make_random_file(&packed, "movie.bin", 6 * 1024 * 1024);
        let text = make(&apps.join("Text.app/Contents/Resources"));
        make_repetitive_file(&text, "strings.txt", 6 * 1024 * 1024);

        let found = find_compress_candidates(&layout);

        let names: Vec<_> = found.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names.contains(&"Rust Toolchains") && names.contains(&"Text"));
        for candidate in found {
            assert!(candidate.estimated_saving_bytes > candidate.total_bytes * 8 / 10);
            assert!(candidate.estimated_saving_bytes <= candidate.total_bytes);
        }
    }

    fn lift_acl(path: &Path) {
        let status = std::process::Command::new("/bin/chmod")
            .arg("-N")
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn flags_of(path: &Path) -> u32 {
        DarwinMetadataExt::st_flags(&fs::symlink_metadata(path).unwrap())
    }

    fn names_in(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_file_with_uappnd_is_skipped_and_leaves_nothing_behind() {
        let temp = tempdir().unwrap();
        let file = make_repetitive_file(temp.path(), "append.log", 300_000);
        set_flags(&file, UF_APPEND);
        let outcome = compress_file(&file, false);
        set_flags(&file, 0);
        compress_file(&file, false).unwrap();
        set_flags(&file, flags_of(&file) | UF_APPEND);
        let undo = undo_file(&file, false);
        set_flags(&file, flags_of(&file) & !UF_APPEND);

        assert_eq!(outcome.unwrap(), FileOutcome::Skipped(SkipReason::Locked));
        assert_eq!(undo.unwrap(), FileOutcome::Skipped(SkipReason::Locked));
        assert!(only_file_left(temp.path(), "append.log"));
    }

    #[test]
    fn a_file_whose_acl_denies_delete_is_skipped_and_leaves_nothing_behind() {
        let temp = tempdir().unwrap();
        let file = make_repetitive_file(temp.path(), "acl.txt", 300_000);
        deny(&file, "delete");

        let outcome = compress_file(&file, false);
        let leftovers = names_in(temp.path());
        lift_acl(&file);

        assert_eq!(outcome.unwrap(), FileOutcome::Skipped(SkipReason::Locked));
        assert_eq!(leftovers, ["acl.txt"]);
    }

    #[test]
    fn a_folder_that_denies_deleting_its_files_is_skipped() {
        let temp = tempdir().unwrap();
        let file = make_repetitive_file(temp.path(), "acl.txt", 300_000);
        deny(temp.path(), "delete_child");

        let outcome = compress_file(&file, false);
        let leftovers = names_in(temp.path());
        lift_acl(temp.path());

        assert_eq!(outcome.unwrap(), FileOutcome::Skipped(SkipReason::Locked));
        assert_eq!(leftovers, ["acl.txt"]);
    }

    #[test]
    fn a_file_in_an_append_only_or_locked_folder_is_skipped_and_leaves_nothing_behind() {
        for flag in [UF_APPEND, UF_IMMUTABLE] {
            let temp = tempdir().unwrap();
            let dir = make(&temp.path().join("ap"));
            let plain = make_repetitive_file(&dir, "plain.txt", 300_000);
            let packed = make_repetitive_file(&dir, "packed.txt", 300_000);
            compress_file(&packed, false).unwrap();
            set_flags(&dir, flag);

            let outcome = compress_file(&plain, false);
            let undo = undo_file(&packed, false);
            let mut leftovers = names_in(&dir);
            leftovers.sort();
            set_flags(&dir, 0);

            let locked = FileOutcome::Skipped(SkipReason::Locked);
            assert_eq!(outcome.unwrap(), locked, "{flag:#x}");
            assert_eq!(undo.unwrap(), locked, "{flag:#x}");
            assert_eq!(leftovers, ["packed.txt", "plain.txt"], "{flag:#x}");
        }
    }

    #[test]
    fn an_acl_the_folder_hands_to_new_files_does_not_reach_the_copy() {
        let temp = tempdir().unwrap();
        let file = make_repetitive_file(temp.path(), "plain.txt", 300_000);
        deny(temp.path(), "delete,file_inherit");

        let outcome = compress_file(&file, false);
        let leftovers = names_in(temp.path());
        let acl = unsafe { acl_get_file(c_path(&file).unwrap().as_ptr(), ACL_TYPE_EXTENDED) };
        let has_acl = !acl.is_null();
        lift_acl(temp.path());

        assert!(matches!(outcome.unwrap(), FileOutcome::Compressed { .. }));
        assert_eq!(leftovers, ["plain.txt"]);
        assert!(!has_acl);
    }

    #[test]
    fn a_temporary_file_with_the_append_flag_is_still_removed() {
        let temp = tempdir().unwrap();
        let file = make_repetitive_file(temp.path(), "safe.txt", 300_000);
        let fail = |at: Step| {
            if at == Step::Verify {
                set_flags(&temp_file_in(temp.path()), UF_APPEND);
                return Err(io::Error::other("injected"));
            }
            Ok(())
        };

        assert!(compress_file_probed(&file, false, &fail).is_err());

        assert!(only_file_left(temp.path(), "safe.txt"));
    }

    #[test]
    fn a_temporary_file_that_cannot_be_removed_is_reported() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempdir().unwrap();
        let file = make_repetitive_file(temp.path(), "safe.txt", 300_000);
        let fail = |at: Step| {
            if at == Step::Verify {
                fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o555)).unwrap();
                return Err(io::Error::other("injected"));
            }
            Ok(())
        };

        let result = compress_file_probed(&file, false, &fail);
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755)).unwrap();

        let message = result.unwrap_err().to_string();
        assert!(message.contains("injected"), "{message}");
        assert!(message.contains("could not be removed"), "{message}");
    }

    #[test]
    fn the_temporary_file_is_private_until_the_swap() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempdir().unwrap();
        let file = make_repetitive_file(temp.path(), "secret.env", 300_000);
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        let modes = std::cell::RefCell::new(Vec::new());
        let look = |at: Step| {
            if at == Step::Verify {
                let mode = fs::metadata(temp_file_in(temp.path())).unwrap().mode();
                modes.borrow_mut().push(mode & 0o777);
            }
            Ok(())
        };

        compress_file_probed(&file, false, &look).unwrap();
        undo_file_probed(&file, false, &look).unwrap();

        assert_eq!(*modes.borrow(), [0o600, 0o600]);
        assert_eq!(fs::metadata(&file).unwrap().mode() & 0o777, 0o600);
    }

    fn kept_files(dir: &Path, expected: &[u8], not: &str) -> Vec<String> {
        names_in(dir)
            .into_iter()
            .filter(|name| name != not && fs::read(dir.join(name)).is_ok_and(|b| b == expected))
            .collect()
    }

    #[test]
    fn a_changed_original_is_kept_when_the_file_is_moved_away_after_the_swap() {
        let temp = tempdir().unwrap();
        let file = make_repetitive_file(temp.path(), "live.log", 300_000);
        let mut expected = fs::read(&file).unwrap();
        expected.extend_from_slice(b"appended record\n");
        let rotate = |at: Step| {
            if at == Step::AfterSwap {
                append(&temp_file_in(temp.path()), b"appended record\n");
                fs::rename(&file, temp.path().join("live.log.1")).unwrap();
            }
            Ok(())
        };

        let result = compress_file_probed(&file, false, &rotate);

        assert!(result.is_err());
        let kept = kept_files(temp.path(), &expected, "live.log.1");
        assert_eq!(kept.len(), 1, "{:?}", names_in(temp.path()));
        assert!(stale_temp_owner(&kept[0]).is_none());
        assert!(result.unwrap_err().to_string().contains(&kept[0]));
    }

    #[test]
    fn a_changed_original_is_kept_when_another_file_took_its_place() {
        let temp = tempdir().unwrap();
        let file = make_repetitive_file(temp.path(), "live.log", 300_000);
        let mut expected = fs::read(&file).unwrap();
        expected.extend_from_slice(b"appended record\n");
        let replace = |at: Step| {
            if at == Step::AfterSwap {
                append(&temp_file_in(temp.path()), b"appended record\n");
                let other = temp.path().join("new.log");
                fs::write(&other, b"saved by an editor").unwrap();
                fs::rename(&other, &file).unwrap();
            }
            Ok(())
        };

        assert!(compress_file_probed(&file, false, &replace).is_err());

        assert_eq!(fs::read(&file).unwrap(), b"saved by an editor");
        let kept = kept_files(temp.path(), &expected, "live.log");
        assert_eq!(kept.len(), 1, "{:?}", names_in(temp.path()));
        assert!(stale_temp_owner(&kept[0]).is_none());
    }

    fn dead_pid() -> u32 {
        let mut child = std::process::Command::new("/usr/bin/true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    #[test]
    fn a_changed_original_stuck_at_the_temporary_name_survives_a_later_run() {
        use std::os::unix::fs::PermissionsExt;
        let (_temp, layout) = sandbox();
        let dir = make(&layout.user("Projects/data"));
        let file = make_repetitive_file(&dir, "live.log", 300_000);
        let mut expected = fs::read(&file).unwrap();
        expected.extend_from_slice(b"appended record\n");
        let write_and_freeze = |at: Step| {
            if at == Step::AfterSwap {
                append(&temp_file_in(&dir), b"appended record\n");
                fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
            }
            Ok(())
        };

        let result = compress_file_probed(&file, false, &write_and_freeze);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let stuck = temp_file_in(&dir);
        let left_by_a_gone_run = dir.join(format!(".live.log.taert-tmp-{}-1-2.tmp", dead_pid()));
        fs::rename(&stuck, &left_by_a_gone_run).unwrap();
        let host = host_with("    1     0 /sbin/launchd\n", "");
        let report = run(&layout, &host, &dir, &real_run()).unwrap();

        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains(&*stuck.to_string_lossy())
        );
        assert_eq!(fs::read(&left_by_a_gone_run).unwrap(), expected);
        assert_eq!(report.failures.len(), 1, "{:?}", report.failures);
        assert_eq!(report.failures[0].path, left_by_a_gone_run);
    }

    #[test]
    fn the_copy_mark_never_reaches_the_finished_file() {
        let temp = tempdir().unwrap();
        let file = make_repetitive_file(temp.path(), "mark.txt", 300_000);

        compress_file(&file, false).unwrap();
        let after_compress = read_attribute(&file, COPY_MARK.to_str().unwrap());
        undo_file(&file, false).unwrap();
        let after_undo = read_attribute(&file, COPY_MARK.to_str().unwrap());

        assert_eq!(after_compress, None);
        assert_eq!(after_undo, None);
    }

    mod memory {
        use std::alloc::{GlobalAlloc, Layout, System};
        use std::cell::Cell;

        thread_local! {
            static BIGGEST: Cell<usize> = const { Cell::new(0) };
        }

        pub struct Tracker;

        fn note(size: usize) {
            BIGGEST.with(|biggest| biggest.set(biggest.get().max(size)));
        }

        unsafe impl GlobalAlloc for Tracker {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                note(layout.size());
                unsafe { System.alloc(layout) }
            }

            unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
                unsafe { System.dealloc(ptr, layout) }
            }

            unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
                note(new_size);
                unsafe { System.realloc(ptr, layout, new_size) }
            }
        }

        /// The largest single allocation of this thread since the last call.
        pub fn take_biggest() -> usize {
            BIGGEST.with(|biggest| biggest.replace(0))
        }
    }

    #[global_allocator]
    static TRACKER: memory::Tracker = memory::Tracker;

    #[test]
    fn compressing_and_unpacking_never_hold_the_whole_file_in_memory() {
        let temp = tempdir().unwrap();
        let size = 8 * 1024 * 1024;
        let file = make_repetitive_file(temp.path(), "large.txt", size);

        memory::take_biggest();
        let compressed = compress_file(&file, false).unwrap();
        let compress_peak = memory::take_biggest();
        let unpacked = undo_file(&file, false).unwrap();
        let undo_peak = memory::take_biggest();

        assert!(matches!(compressed, FileOutcome::Compressed { .. }));
        assert!(matches!(unpacked, FileOutcome::Unpacked { .. }));
        assert!(
            compress_peak < size / 4,
            "compress allocated {compress_peak}"
        );
        assert!(undo_peak < size / 4, "undo allocated {undo_peak}");
    }

    #[test]
    fn a_root_with_dots_does_not_skip_the_live_data_check() {
        let (_temp, layout) = sandbox();
        let host = host_with("    1     0 /sbin/launchd\n", "");
        let brew = layout.system("opt/homebrew");
        let live = make_repetitive_file(&make(&brew.join("var/db")), "store.db", 300_000);
        let plain = make_repetitive_file(&make(&brew.join("Cellar/foo")), "foo.txt", 300_000);

        let report = run(&layout, &host, &brew.join("Cellar/.."), &real_run()).unwrap();

        assert!(!is_compressed(&live));
        assert!(is_compressed(&plain));
        assert_eq!(report.processed_files, 1);
    }

    #[test]
    fn a_root_in_the_wrong_letter_case_does_not_skip_the_live_data_check() {
        let (_temp, layout) = sandbox();
        let host = host_with("    1     0 /sbin/launchd\n", "");
        let brew = layout.system("opt/homebrew");
        let live = make_repetitive_file(&make(&brew.join("var/db")), "store.db", 300_000);
        let plain = make_repetitive_file(&make(&brew.join("Cellar/foo")), "foo.txt", 300_000);

        let report = run(&layout, &host, &layout.system("opt/HomeBrew"), &real_run()).unwrap();

        assert!(!is_compressed(&live));
        assert!(is_compressed(&plain));
        assert_eq!(report.processed_files, 1);
    }

    #[test]
    #[ignore]
    fn live_test_on_signed_app() {
        let app_candidates = [
            "/Applications/NinjaMouse.app",
            "/Applications/Middle.app",
            "/Applications/Stats.app",
            "/Applications/Keka.app",
        ];
        let Some(app_path) = app_candidates.iter().map(Path::new).find(|p| p.exists()) else {
            return;
        };

        let (_sandbox, layout) = sandbox();
        let dest = layout.user("Applications/Test.app");
        fs::create_dir_all(dest.parent().unwrap()).unwrap();

        let status = std::process::Command::new("cp")
            .arg("-R")
            .arg(app_path)
            .arg(&dest)
            .status()
            .unwrap();
        if !status.success() {
            return;
        }

        let cancelled = AtomicBool::new(false);
        let options = CompressOptions {
            dry_run: false,
            undo: false,
        };
        let compressor = Compressor {
            host: &crate::core::host::MacHost,
            layout: &layout,
            ignore: &IgnoreStore::default(),
        };
        let _report = compressor
            .process_path(&dest, &options, &cancelled, |_, _| {})
            .unwrap();

        let verify_status = std::process::Command::new("codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(&dest)
            .status()
            .unwrap();
        assert!(verify_status.success());
    }
}

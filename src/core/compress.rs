//! APFS transparent per-file compression (`decmpfs`) engine.
//!
//! Compresses files in place using macOS `decmpfs` with LZFSE (type 12) or LZVN (type 8)
//! chunks stored in the file's resource fork, verified via read-back and atomically
//! swapped via `renamex_np(RENAME_SWAP)`.

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::darwin::fs::MetadataExt as DarwinMetadataExt;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use super::apfs;
use super::layout::Layout;
use super::safety::Safety;
use super::scanner::{CategoryScan, ScanItem, ToolGroup};
use super::stores::IgnoreStore;

pub const BLOCK_SIZE: usize = 65536; // 64 KiB blocks
pub const UF_COMPRESSED: u32 = 0x0000_0020;
pub const DECMPFS_MAGIC: u32 = 0x636d_7066; // 'cmpf' in LE

const XATTR_NOFOLLOW: libc::c_int = 0x0001;
const ACL_TYPE_EXTENDED: libc::c_int = 0x0000_0100;
const RENAME_SWAP: libc::c_uint = 0x0000_0002;

const COMPRESSION_LZFSE: i32 = 0x801;
const COMPRESSION_LZVN: i32 = 0x900;

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Lzfse,
    Lzvn,
}

impl Codec {
    pub const fn type_id(self) -> u32 {
        match self {
            Self::Lzfse => 12,
            Self::Lzvn => 8,
        }
    }

    pub const fn algorithm(self) -> i32 {
        match self {
            Self::Lzfse => COMPRESSION_LZFSE,
            Self::Lzvn => COMPRESSION_LZVN,
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

/// Checks whether `path` already carries compression-related xattrs.
pub fn has_compression_xattrs(path: &Path) -> Result<bool, std::io::Error> {
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

    let decmp = CString::new("com.apple.decmpfs").unwrap();
    let res1 = unsafe {
        getxattr(
            c_path.as_ptr(),
            decmp.as_ptr(),
            std::ptr::null_mut(),
            0,
            0,
            XATTR_NOFOLLOW,
        )
    };
    if res1 >= 0 {
        return Ok(true);
    }

    let rf = CString::new("com.apple.ResourceFork").unwrap();
    let res2 = unsafe {
        getxattr(
            c_path.as_ptr(),
            rf.as_ptr(),
            std::ptr::null_mut(),
            0,
            0,
            XATTR_NOFOLLOW,
        )
    };
    if res2 >= 0 {
        return Ok(true);
    }

    Ok(false)
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

/// Copy extended attributes from `c_src` to `c_dst`, skipping `com.apple.decmpfs`
/// and `com.apple.ResourceFork`.
fn copy_xattrs(
    c_src: *const libc::c_char,
    c_dst: *const libc::c_char,
    copy_compression_xattrs: bool,
) -> Result<(), std::io::Error> {
    let list_size = unsafe { listxattr(c_src, std::ptr::null_mut(), 0, XATTR_NOFOLLOW) };
    if list_size <= 0 {
        return Ok(());
    }

    let mut buf = vec![0u8; list_size as usize];
    let actual_size = unsafe {
        listxattr(
            c_src,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            XATTR_NOFOLLOW,
        )
    };
    if actual_size <= 0 {
        return Ok(());
    }

    let names_slice = &buf[..actual_size as usize];
    for name_bytes in names_slice.split(|&b| b == 0).filter(|p| !p.is_empty()) {
        if !copy_compression_xattrs
            && (name_bytes == b"com.apple.decmpfs" || name_bytes == b"com.apple.ResourceFork")
        {
            continue;
        }

        let Ok(c_name) = CString::new(name_bytes) else {
            continue;
        };

        let val_size = unsafe {
            getxattr(
                c_src,
                c_name.as_ptr(),
                std::ptr::null_mut(),
                0,
                0,
                XATTR_NOFOLLOW,
            )
        };
        if val_size < 0 {
            continue;
        }

        let mut val_buf = vec![0u8; val_size as usize];
        let got_size = if val_size > 0 {
            unsafe {
                getxattr(
                    c_src,
                    c_name.as_ptr(),
                    val_buf.as_mut_ptr() as *mut libc::c_void,
                    val_buf.len(),
                    0,
                    XATTR_NOFOLLOW,
                )
            }
        } else {
            0
        };

        if got_size >= 0 {
            unsafe {
                setxattr(
                    c_dst,
                    c_name.as_ptr(),
                    val_buf.as_ptr() as *const libc::c_void,
                    got_size as usize,
                    0,
                    XATTR_NOFOLLOW,
                );
            }
        }
    }

    Ok(())
}

/// Copies owner, mode, times, flags, ACL, and extended attributes.
fn copy_metadata(
    orig_path: &Path,
    temp_path: &Path,
    temp_fd: libc::c_int,
    orig_meta: &fs::Metadata,
    compressed: bool,
) -> Result<(), std::io::Error> {
    // 1. Mode / permissions
    let mode = (orig_meta.mode() & 0o7777) as libc::mode_t;
    unsafe { libc::fchmod(temp_fd, mode) };

    // 2. Owner (best effort for non-root)
    let _ = unsafe { libc::fchown(temp_fd, orig_meta.uid(), orig_meta.gid()) };

    // 3. Timestamps
    let times = [
        libc::timespec {
            tv_sec: orig_meta.atime(),
            tv_nsec: orig_meta.atime_nsec(),
        },
        libc::timespec {
            tv_sec: orig_meta.mtime(),
            tv_nsec: orig_meta.mtime_nsec(),
        },
    ];
    unsafe { libc::futimens(temp_fd, times.as_ptr()) };

    // 4. Flags
    let orig_flags = DarwinMetadataExt::st_flags(orig_meta);
    let target_flags = if compressed {
        orig_flags | UF_COMPRESSED
    } else {
        orig_flags & !UF_COMPRESSED
    };
    unsafe { libc::fchflags(temp_fd, target_flags) };

    let c_orig = CString::new(orig_path.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let c_temp = CString::new(temp_path.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

    // 5. ACL
    let acl = unsafe { acl_get_file(c_orig.as_ptr(), ACL_TYPE_EXTENDED) };
    if !acl.is_null() {
        unsafe { acl_set_file(c_temp.as_ptr(), ACL_TYPE_EXTENDED, acl) };
        unsafe { acl_free(acl) };
    }

    // 6. Extended attributes (omit compression xattrs)
    let _ = copy_xattrs(c_orig.as_ptr(), c_temp.as_ptr(), false);

    Ok(())
}

/// Compares `temp_path` and `original_path` byte by byte.
fn verify_identical_content(
    temp_path: &Path,
    original_path: &Path,
    expected_len: u64,
) -> Result<(), std::io::Error> {
    let mut f1 = File::open(temp_path)?;
    let mut f2 = File::open(original_path)?;

    let mut buf1 = [0u8; 65536];
    let mut buf2 = [0u8; 65536];
    let mut compared = 0u64;

    loop {
        let n1 = f1.read(&mut buf1)?;
        let n2 = f2.read(&mut buf2)?;

        if n1 != n2 || buf1[..n1] != buf2[..n2] {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "verification mismatch: byte contents differ",
            ));
        }

        if n1 == 0 {
            break;
        }

        compared += n1 as u64;
    }

    if compared != expected_len {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("verification mismatch: expected {expected_len} bytes, read {compared}"),
        ));
    }

    Ok(())
}

/// Atomically swap `temp_path` and `orig_path`, then remove `temp_path`.
fn swap_and_remove_temp(temp_path: &Path, orig_path: &Path) -> Result<(), std::io::Error> {
    let c_temp = CString::new(temp_path.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let c_orig = CString::new(orig_path.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

    let res = unsafe { renamex_np(c_temp.as_ptr(), c_orig.as_ptr(), RENAME_SWAP) };
    if res != 0 {
        let err = std::io::Error::last_os_error();
        let _ = fs::remove_file(temp_path);
        return Err(err);
    }

    let _ = fs::remove_file(temp_path);
    Ok(())
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

/// Compress an individual file following the atomic 4-step pipeline.
pub fn compress_file(
    path: &Path,
    dry_run: bool,
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

    let data = fs::read(path)?;
    let raw_len = data.len() as u64;
    if raw_len < 4096 {
        return Ok(FileOutcome::Skipped(SkipReason::Incompressible));
    }

    let scratch_len = unsafe { compression_encode_scratch_buffer_size(COMPRESSION_LZFSE) };
    let mut scratch = vec![0u8; scratch_len];
    let mut blocks = Vec::new();

    for chunk in data.chunks(BLOCK_SIZE) {
        let Some(comp) = compress_chunk(chunk, &mut scratch, Codec::Lzfse) else {
            return Ok(FileOutcome::Skipped(SkipReason::Incompressible));
        };
        blocks.push(comp);
    }

    let (table, payload) = build_resource_fork_payload(&blocks)?;
    let encoded_len = (table.len() + payload.len()) as u64;

    if encoded_len + 4096 > raw_len {
        return Ok(FileOutcome::Skipped(SkipReason::Incompressible));
    }

    // Step 1: Write to temporary file in same folder
    let temp_path = temp_path_for(path)?;
    let temp_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&temp_path)?;
    let temp_fd = temp_file.as_raw_fd();

    // Write resource fork via namedfork
    let rsrc_path = temp_path.join("..namedfork").join("rsrc");
    let write_rf = (|| -> Result<(), std::io::Error> {
        let mut rsrc_file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&rsrc_path)?;
        rsrc_file.write_all(&table)?;
        rsrc_file.write_all(&payload)?;
        rsrc_file.flush()?;
        rsrc_file.sync_all()?;
        Ok(())
    })();

    if let Err(e) = write_rf {
        let _ = fs::remove_file(&temp_path);
        return Err(Box::new(e));
    }

    // Write decmpfs header xattr
    let c_temp = CString::new(temp_path.as_os_str().as_bytes())?;
    let hdr = decmpfs_header(Codec::Lzfse, raw_len);
    let xattr_decmp = CString::new("com.apple.decmpfs")?;
    let res = unsafe {
        setxattr(
            c_temp.as_ptr(),
            xattr_decmp.as_ptr(),
            hdr.as_ptr() as *const libc::c_void,
            hdr.len(),
            0,
            XATTR_NOFOLLOW,
        )
    };
    if res != 0 {
        let _ = fs::remove_file(&temp_path);
        return Err(Box::new(std::io::Error::last_os_error()));
    }

    // Step 2: Copy owner, mode, times, flags, ACL, and extended attributes
    if let Err(e) = copy_metadata(path, &temp_path, temp_fd, &meta, true) {
        let _ = fs::remove_file(&temp_path);
        return Err(Box::new(e));
    }

    // Check actual blocks saved on the written temp file
    let temp_meta = fs::symlink_metadata(&temp_path)?;
    let orig_blocks = meta.blocks();
    let temp_blocks = temp_meta.blocks();
    if orig_blocks <= temp_blocks || (orig_blocks - temp_blocks) * 512 < 4096 {
        let _ = fs::remove_file(&temp_path);
        return Ok(FileOutcome::Skipped(SkipReason::Incompressible));
    }

    // Step 3: Read temporary file back and compare byte by byte with original
    if let Err(e) = verify_identical_content(&temp_path, path, raw_len) {
        let _ = fs::remove_file(&temp_path);
        return Err(Box::new(e));
    }

    // Step 4: Replace original with renamex_np and RENAME_SWAP, then remove old copy
    if let Err(e) = swap_and_remove_temp(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(Box::new(e));
    }

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
    let meta = fs::symlink_metadata(path)?;

    if !meta.file_type().is_file() || meta.file_type().is_symlink() {
        return Ok(FileOutcome::Skipped(SkipReason::SymlinkOrHardlink));
    }

    let uid = unsafe { libc::geteuid() };
    if meta.uid() != uid {
        return Ok(FileOutcome::Skipped(SkipReason::NotUserOwned));
    }

    if DarwinMetadataExt::st_flags(&meta) & UF_COMPRESSED == 0 {
        return Ok(FileOutcome::Skipped(SkipReason::NotCompressed));
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

    // Step 1: Write decompressed bytes to plain data fork of temp file
    let temp_path = temp_path_for(path)?;
    let mut temp_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&temp_path)?;
    let temp_fd = temp_file.as_raw_fd();

    let data = fs::read(path)?;
    temp_file.write_all(&data)?;
    temp_file.flush()?;
    temp_file.sync_all()?;

    // Step 2: Copy owner, mode, times, flags (without UF_COMPRESSED), ACL, xattrs
    if let Err(e) = copy_metadata(path, &temp_path, temp_fd, &meta, false) {
        let _ = fs::remove_file(&temp_path);
        return Err(Box::new(e));
    }

    // Step 3: Read temporary file back and compare byte by byte with original
    if let Err(e) = verify_identical_content(&temp_path, path, data.len() as u64) {
        let _ = fs::remove_file(&temp_path);
        return Err(Box::new(e));
    }

    // Step 4: Atomic swap and remove old copy
    if let Err(e) = swap_and_remove_temp(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(Box::new(e));
    }

    let new_meta = fs::symlink_metadata(path)?;
    Ok(FileOutcome::Unpacked {
        original_bytes: len,
        original_blocks: orig_blocks,
        restored_blocks: new_meta.blocks(),
    })
}

/// Recursively traverses `path` and compresses or uncompresses files.
pub fn process_path<F>(
    path: &Path,
    options: &CompressOptions,
    ignore: &IgnoreStore,
    cancelled: &AtomicBool,
    mut on_file: F,
) -> CompressReport
where
    F: FnMut(&Path, &FileOutcome),
{
    let mut report = CompressReport {
        dry_run: options.dry_run,
        undo: options.undo,
        ..Default::default()
    };

    if !path.exists() {
        report.failures.push(CompressFailure {
            path: path.to_path_buf(),
            error: "path does not exist".to_string(),
        });
        return report;
    }

    if path.is_file() {
        report.scanned_files += 1;
        let res = if options.undo {
            undo_file(path, options.dry_run)
        } else {
            compress_file(path, options.dry_run)
        };
        match res {
            Ok(outcome) => {
                on_file(path, &outcome);
                match outcome {
                    FileOutcome::Compressed {
                        original_bytes,
                        compressed_bytes,
                        saved_bytes,
                        original_blocks,
                        compressed_blocks,
                        saved_blocks,
                    } => {
                        report.processed_files += 1;
                        report.original_bytes += original_bytes;
                        report.compressed_bytes += compressed_bytes;
                        report.saved_bytes += saved_bytes;
                        report.original_blocks += original_blocks;
                        report.compressed_blocks += compressed_blocks;
                        report.saved_blocks += saved_blocks;
                    }
                    FileOutcome::Unpacked {
                        original_bytes,
                        original_blocks,
                        restored_blocks,
                    } => {
                        report.processed_files += 1;
                        report.original_bytes += original_bytes;
                        report.original_blocks += original_blocks;
                        report.compressed_blocks += restored_blocks;
                    }
                    FileOutcome::Skipped(reason) => {
                        report.skipped.record(reason);
                    }
                }
            }
            Err(e) => {
                report.failures.push(CompressFailure {
                    path: path.to_path_buf(),
                    error: e.to_string(),
                });
            }
        }
        return report;
    }

    // Directory traversal
    let mut dirs_to_visit = vec![path.to_path_buf()];

    while let Some(current_dir) = dirs_to_visit.pop() {
        if cancelled.load(Ordering::Relaxed) {
            break;
        }

        if Safety::is_ignored_or_protected(&current_dir, ignore) {
            continue;
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
                if !file_type.is_symlink() && !Safety::is_cloud_or_dataless(&entry_path) {
                    dirs_to_visit.push(entry_path);
                }
            } else if file_type.is_file() {
                report.scanned_files += 1;
                let res = if options.undo {
                    undo_file(&entry_path, options.dry_run)
                } else {
                    compress_file(&entry_path, options.dry_run)
                };

                match res {
                    Ok(outcome) => {
                        on_file(&entry_path, &outcome);
                        match outcome {
                            FileOutcome::Compressed {
                                original_bytes,
                                compressed_bytes,
                                saved_bytes,
                                original_blocks,
                                compressed_blocks,
                                saved_blocks,
                            } => {
                                report.processed_files += 1;
                                report.original_bytes += original_bytes;
                                report.compressed_bytes += compressed_bytes;
                                report.saved_bytes += saved_bytes;
                                report.original_blocks += original_blocks;
                                report.compressed_blocks += compressed_blocks;
                                report.saved_blocks += saved_blocks;
                            }
                            FileOutcome::Unpacked {
                                original_bytes,
                                original_blocks,
                                restored_blocks,
                            } => {
                                report.processed_files += 1;
                                report.original_bytes += original_bytes;
                                report.original_blocks += original_blocks;
                                report.compressed_blocks += restored_blocks;
                            }
                            FileOutcome::Skipped(reason) => {
                                report.skipped.record(reason);
                            }
                        }
                    }
                    Err(e) => {
                        report.failures.push(CompressFailure {
                            path: entry_path,
                            error: e.to_string(),
                        });
                    }
                }
            }
        }
    }

    report
}

/// Find compression candidates for the TUI (user-owned apps, Homebrew Cellar, Rust toolchains).
pub fn find_compress_candidates(layout: &Layout) -> Vec<CompressCandidate> {
    let mut candidates = Vec::new();
    let uid = unsafe { libc::geteuid() };

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
                let total = super::sizer::DirSizer::measure(&p).bytes;
                if total > 5 * 1024 * 1024 {
                    // Quick estimate around 50%
                    let est = total / 2;
                    candidates.push(CompressCandidate {
                        name,
                        path: p,
                        category: "Application",
                        estimated_saving_bytes: est,
                        total_bytes: total,
                        selected: false,
                    });
                }
            }
        }
    }

    // 2. Homebrew Cellar
    let brew_paths = [
        layout.system("opt/homebrew/Cellar"),
        layout.system("usr/local/Cellar"),
        PathBuf::from("/opt/homebrew/Cellar"),
        PathBuf::from("/usr/local/Cellar"),
    ];
    for bp in brew_paths {
        if bp.is_dir() {
            let total = super::sizer::DirSizer::measure(&bp).bytes;
            if total > 0 {
                candidates.push(CompressCandidate {
                    name: "Homebrew Cellar".to_string(),
                    path: bp,
                    category: "Package Manager",
                    estimated_saving_bytes: total * 6 / 10,
                    total_bytes: total,
                    selected: false,
                });
                break;
            }
        }
    }

    // 3. Rust toolchains
    let rustup = layout.user(".rustup/toolchains");
    if rustup.is_dir() {
        let total = super::sizer::DirSizer::measure(&rustup).bytes;
        if total > 0 {
            candidates.push(CompressCandidate {
                name: "Rust Toolchains".to_string(),
                path: rustup,
                category: "Developer Tools",
                estimated_saving_bytes: total * 65 / 100,
                total_bytes: total,
                selected: false,
            });
        }
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

    #[test]
    fn failure_at_each_step_leaves_the_original_readable() {
        let temp = tempdir().unwrap();
        let file = make_repetitive_file(temp.path(), "safe.txt", 100_000);
        let orig_bytes = fs::read(&file).unwrap();

        let meta = fs::symlink_metadata(&file).unwrap();
        assert!(check_eligibility_compress(&file, &meta).is_ok());

        assert_eq!(fs::read(&file).unwrap(), orig_bytes);
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

        let temp = tempdir().unwrap();
        let dest = temp.path().join("Test.app");

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
        let _report = process_path(
            &dest,
            &options,
            &IgnoreStore::default(),
            &cancelled,
            |_, _| {},
        );

        let verify_status = std::process::Command::new("codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(&dest)
            .status()
            .unwrap();
        assert!(verify_status.success());
    }
}

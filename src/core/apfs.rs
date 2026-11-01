//! APFS clone detection and block sharing queries via macOS `getattrlist`.

use std::path::Path;

/// Information about a file's clone status on APFS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloneIdentity {
    pub clone_id: u64,
    pub shares_all_blocks: bool,
}

#[cfg(target_os = "macos")]
mod sys {
    use super::CloneIdentity;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    /// File shares all of its blocks with another file (defined in `<sys/stat.h>`).
    pub const EF_SHARES_ALL_BLOCKS: u64 = 0x0000_0040;

    /// Reply buffer for `getattrlist` when requesting `ATTR_CMNEXT_CLONEID` and `ATTR_CMNEXT_EXT_FLAGS`.
    ///
    /// The reply structure:
    /// - 4-byte `u32`: length of returned attribute data (including the length field itself)
    /// - 8-byte `u64`: clone ID (at offset 4, unaligned)
    /// - 8-byte `u64`: extended flags (at offset 12, unaligned)
    ///
    /// Total: 20 bytes.
    #[repr(C, align(4))]
    struct AttrReplyBuf {
        len: u32,
        data: [u8; 16],
    }

    pub fn clone_identity(path: &Path) -> Option<CloneIdentity> {
        let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut attrlist: libc::attrlist = unsafe { std::mem::zeroed() };
        attrlist.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
        // ATTR_CMNEXT_* attributes belong in forkattr, not commonattr.
        // If placed in commonattr, getattrlist returns unrelated attributes with no error.
        attrlist.forkattr = libc::ATTR_CMNEXT_CLONEID | libc::ATTR_CMNEXT_EXT_FLAGS;

        let mut buf = AttrReplyBuf {
            len: 0,
            data: [0u8; 16],
        };

        let ret = unsafe {
            libc::getattrlist(
                c_path.as_ptr(),
                &mut attrlist as *mut _ as *mut libc::c_void,
                &mut buf as *mut _ as *mut libc::c_void,
                std::mem::size_of::<AttrReplyBuf>(),
                (libc::FSOPT_NOFOLLOW | libc::FSOPT_ATTR_CMN_EXTENDED) as libc::c_uint,
            )
        };

        if ret != 0 || buf.len < 20 {
            return None;
        }

        let clone_id = u64::from_ne_bytes(buf.data[0..8].try_into().ok()?);
        let ext_flags = u64::from_ne_bytes(buf.data[8..16].try_into().ok()?);

        Some(CloneIdentity {
            clone_id,
            shares_all_blocks: (ext_flags & EF_SHARES_ALL_BLOCKS) != 0,
        })
    }
}

/// Query APFS clone identity for the file at `path`.
///
/// Returns `None` if the call fails, the filesystem does not support clone queries,
/// or the OS is not macOS.
#[cfg(target_os = "macos")]
pub fn clone_identity(path: &Path) -> Option<CloneIdentity> {
    sys::clone_identity(path)
}

/// Fallback for non-macOS systems.
#[cfg(not(target_os = "macos"))]
pub fn clone_identity(_path: &Path) -> Option<CloneIdentity> {
    None
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::tempdir;

    fn make_clone(src: &Path, dst: &Path) {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let c_src = CString::new(src.as_os_str().as_bytes()).unwrap();
        let c_dst = CString::new(dst.as_os_str().as_bytes()).unwrap();
        let ret = unsafe { libc::clonefile(c_src.as_ptr(), c_dst.as_ptr(), 0) };
        assert_eq!(
            ret,
            0,
            "clonefile failed: {}",
            std::io::Error::last_os_error()
        );
    }

    #[test]
    fn plain_file_does_not_share_all_blocks() {
        let temp = tempdir().unwrap();
        let file = temp.path().join("plain.bin");
        let mut f = File::create(&file).unwrap();
        f.write_all(&vec![0xAA; 8192]).unwrap();
        drop(f);

        let id = clone_identity(&file).expect("clone_identity should query plain file");
        assert!(!id.shares_all_blocks);
    }

    #[test]
    fn cloned_files_share_all_blocks_and_have_same_clone_id() {
        let temp = tempdir().unwrap();
        let file1 = temp.path().join("f1.bin");
        let file2 = temp.path().join("f2.bin");
        let mut f = File::create(&file1).unwrap();
        f.write_all(&vec![0xBB; 8192]).unwrap();
        drop(f);

        make_clone(&file1, &file2);

        let id1 = clone_identity(&file1).expect("clone_identity for f1");
        let id2 = clone_identity(&file2).expect("clone_identity for f2");

        assert_eq!(id1.clone_id, id2.clone_id);
        assert!(id1.shares_all_blocks);
        assert!(id2.shares_all_blocks);
    }

    #[test]
    fn modified_clone_no_longer_shares_all_blocks() {
        let temp = tempdir().unwrap();
        let file1 = temp.path().join("f1.bin");
        let file2 = temp.path().join("f2.bin");
        let mut f = File::create(&file1).unwrap();
        f.write_all(&vec![0xCC; 8192]).unwrap();
        drop(f);

        make_clone(&file1, &file2);

        // Modify file2
        {
            use std::fs::OpenOptions;
            let mut f2 = OpenOptions::new().write(true).open(&file2).unwrap();
            f2.write_all(b"modified prefix").unwrap();
        }

        let id1 = clone_identity(&file1).expect("clone_identity for f1");
        let id2 = clone_identity(&file2).expect("clone_identity for f2");

        assert!(!id2.shares_all_blocks);
        assert!(!id1.shares_all_blocks);
    }
}

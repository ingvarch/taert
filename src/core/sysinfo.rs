//! macOS system and disk info reporting.
//! Reports factual APFS disk space without misleading claims.

use std::mem::MaybeUninit;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskUsage {
    pub mount_point: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub available_bytes: u64,
}

pub struct SysInfo;

impl SysInfo {
    pub fn disk_usage(path: &Path) -> Option<DiskUsage> {
        let path_str = path.to_str()?;
        let c_path = std::ffi::CString::new(path_str).ok()?;

        let mut stat = MaybeUninit::<libc::statfs>::uninit();
        let res = unsafe { libc::statfs(c_path.as_ptr(), stat.as_mut_ptr()) };

        if res == 0 {
            let s = unsafe { stat.assume_init() };
            let bsize = s.f_bsize as u64;
            let total_bytes = s.f_blocks.saturating_mul(bsize);
            let free_bytes = s.f_bfree.saturating_mul(bsize);
            let available_bytes = s.f_bavail.saturating_mul(bsize);

            Some(DiskUsage {
                mount_point: path_str.to_string(),
                total_bytes,
                free_bytes,
                available_bytes,
            })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_disk_usage() {
        let disk = SysInfo::disk_usage(Path::new("/")).expect("root disk usage");
        assert!(disk.total_bytes > 0);
        assert_eq!(disk.mount_point, "/");
    }
}

//! macOS system and disk info reporting.
//! Reports factual memory pressure and APFS disk space without misleading claims.

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryUsage {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub swap_total_bytes: u64,
    pub swap_used_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemStatus {
    pub memory: MemoryUsage,
    pub main_disk: Option<DiskUsage>,
}

pub struct SysInfo;

impl SysInfo {
    pub fn current_status() -> SystemStatus {
        let mut sys = sysinfo::System::new_all();
        sys.refresh_memory();

        let memory = MemoryUsage {
            total_bytes: sys.total_memory(),
            used_bytes: sys.used_memory(),
            swap_total_bytes: sys.total_swap(),
            swap_used_bytes: sys.used_swap(),
        };

        let main_disk = Self::disk_usage(Path::new("/"));

        SystemStatus { memory, main_disk }
    }

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
    fn test_system_status() {
        let status = SysInfo::current_status();
        assert!(status.memory.total_bytes > 0);
        assert!(status.main_disk.is_some());
        let disk = status.main_disk.unwrap();
        assert!(disk.total_bytes > 0);
    }
}

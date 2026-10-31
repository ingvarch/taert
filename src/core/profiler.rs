//! Hardware, macOS, Memory, and APFS Storage information queries via sysctl and libc.

use std::ffi::CString;
use std::mem::MaybeUninit;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::iokit;
use super::telemetry::{self, LiveStatus};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineInfo {
    pub chip: Option<String>,
    pub cores: Option<String>,
    pub memory_bytes: Option<u64>,
    pub macos_version: String,
    pub model: Option<String>,
    pub model_name: Option<String>,
    pub uptime_seconds: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryPressureLevel {
    Normal,
    Warning,
    Critical,
}

impl MemoryPressureLevel {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Normal => "Normal",
            Self::Warning => "Warning",
            Self::Critical => "Critical",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetailedMemoryStatus {
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub app_bytes: u64,
    pub wired_bytes: u64,
    pub compressed_bytes: u64,
    pub cached_bytes: u64,
    pub swap_used_bytes: u64,
    pub pressure: Option<MemoryPressureLevel>,
}

impl DetailedMemoryStatus {
    pub fn used_bytes(&self) -> u64 {
        self.app_bytes
            .saturating_add(self.wired_bytes)
            .saturating_add(self.compressed_bytes)
    }

    pub fn used_fraction(&self) -> f64 {
        if self.total_bytes > 0 {
            self.used_bytes() as f64 / self.total_bytes as f64
        } else {
            0.0
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetailedStorageStatus {
    pub mount_point: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub free_bytes: u64,
    pub managed_bytes: u64,
}

impl DetailedStorageStatus {
    pub fn free_fraction(&self) -> f64 {
        if self.total_bytes > 0 {
            self.available_bytes as f64 / self.total_bytes as f64
        } else {
            0.0
        }
    }

    pub fn used_fraction(&self) -> f64 {
        (1.0 - self.free_fraction()).clamp(0.0, 1.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FullSystemStatus {
    pub machine: MachineInfo,
    pub memory: DetailedMemoryStatus,
    pub storage: DetailedStorageStatus,
    pub live: LiveStatus,
}

pub struct SystemProfiler;

impl SystemProfiler {
    pub fn read_sysctl_string(name: &str) -> Option<String> {
        let c_name = CString::new(name).ok()?;
        let mut size: libc::size_t = 0;
        let res = unsafe {
            libc::sysctlbyname(
                c_name.as_ptr(),
                std::ptr::null_mut(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if res != 0 || size == 0 {
            return None;
        }

        let mut buf: Vec<u8> = vec![0; size];
        let res = unsafe {
            libc::sysctlbyname(
                c_name.as_ptr(),
                buf.as_mut_ptr() as *mut libc::c_void,
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if res != 0 {
            return None;
        }

        while let Some(&0) = buf.last() {
            buf.pop();
        }
        String::from_utf8(buf).ok()
    }

    pub fn read_sysctl_u64(name: &str) -> Option<u64> {
        let c_name = CString::new(name).ok()?;
        let mut size: libc::size_t = 0;
        let res = unsafe {
            libc::sysctlbyname(
                c_name.as_ptr(),
                std::ptr::null_mut(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if res != 0 || size == 0 {
            return None;
        }

        if size == 4 {
            let mut val: u32 = 0;
            let res = unsafe {
                libc::sysctlbyname(
                    c_name.as_ptr(),
                    &mut val as *mut _ as *mut libc::c_void,
                    &mut size,
                    std::ptr::null_mut(),
                    0,
                )
            };
            if res == 0 {
                return Some(val as u64);
            }
        } else if size == 8 {
            let mut val: u64 = 0;
            let res = unsafe {
                libc::sysctlbyname(
                    c_name.as_ptr(),
                    &mut val as *mut _ as *mut libc::c_void,
                    &mut size,
                    std::ptr::null_mut(),
                    0,
                )
            };
            if res == 0 {
                return Some(val);
            }
        }
        None
    }

    pub fn machine_info() -> MachineInfo {
        let chip = Self::read_sysctl_string("machdep.cpu.brand_string");
        let model = Self::read_sysctl_string("hw.model");
        let model_name =
            iokit::entry("IODeviceTree:/product").and_then(|p| telemetry::product_name(&p));
        let memory_bytes = Self::read_sysctl_u64("hw.memsize");

        let cores = match (
            Self::read_sysctl_u64("hw.perflevel0.physicalcpu"),
            Self::read_sysctl_u64("hw.perflevel1.physicalcpu"),
        ) {
            (Some(perf), Some(eff)) => Some(format!(
                "{} cores ({} performance, {} efficiency)",
                perf + eff,
                perf,
                eff
            )),
            _ => Self::read_sysctl_u64("hw.ncpu").map(|c| format!("{c} cores")),
        };

        let product_version =
            Self::read_sysctl_string("kern.osproductversion").unwrap_or_else(|| "macOS".into());
        let os_build = Self::read_sysctl_string("kern.osversion");
        let macos_version = if let Some(build) = os_build {
            format!("{product_version} ({build})")
        } else {
            product_version
        };

        MachineInfo {
            chip,
            cores,
            memory_bytes,
            macos_version,
            model,
            model_name,
            uptime_seconds: Self::uptime_seconds(),
        }
    }

    pub fn uptime_seconds() -> u64 {
        unsafe {
            let mut boot_time = MaybeUninit::<libc::timeval>::uninit();
            let mut size = std::mem::size_of::<libc::timeval>();
            let c_name = CString::new("kern.boottime").unwrap();
            if libc::sysctlbyname(
                c_name.as_ptr(),
                boot_time.as_mut_ptr() as *mut libc::c_void,
                &mut size,
                std::ptr::null_mut(),
                0,
            ) == 0
            {
                let bt = boot_time.assume_init();
                let now = libc::time(std::ptr::null_mut());
                if now > bt.tv_sec {
                    (now - bt.tv_sec) as u64
                } else {
                    0
                }
            } else {
                0
            }
        }
    }

    pub fn memory_pressure() -> Option<MemoryPressureLevel> {
        Self::read_sysctl_u64("kern.memorystatus_vm_pressure_level").and_then(|val| match val {
            1 => Some(MemoryPressureLevel::Normal),
            2 => Some(MemoryPressureLevel::Warning),
            4 => Some(MemoryPressureLevel::Critical),
            _ => None,
        })
    }

    pub fn detailed_memory() -> DetailedMemoryStatus {
        let total_bytes = Self::read_sysctl_u64("hw.memsize").unwrap_or(0);
        let pressure = Self::memory_pressure();

        // Query Darwin Mach host_statistics64 for accurate VM breakdown
        #[allow(deprecated)]
        #[cfg(target_os = "macos")]
        unsafe {
            use libc::{mach_port_t, mach_task_self, natural_t};

            // Structure definition for Darwin vm_statistics64
            #[repr(C)]
            struct VmStatistics64 {
                free_count: natural_t,
                active_count: natural_t,
                inactive_count: natural_t,
                wire_count: natural_t,
                zero_fill_count: u64,
                reactivations: u64,
                pageins: u64,
                pageouts: u64,
                faults: u64,
                cow_faults: u64,
                lookups: u64,
                hits: u64,
                purges: u64,
                purgeable_count: natural_t,
                speculative_count: natural_t,
                decompressions: u64,
                compressions: u64,
                swapins: u64,
                swapouts: u64,
                compressor_page_count: natural_t,
                throttled_count: natural_t,
                external_page_count: natural_t,
                internal_page_count: natural_t,
                total_uncompressed_pages_in_compressor: u64,
            }

            unsafe extern "C" {
                fn mach_host_self() -> mach_port_t;
                fn host_statistics64(
                    host_priv: mach_port_t,
                    flavor: libc::c_int,
                    host_info_out: *mut libc::c_int,
                    host_info_outCnt: *mut libc::mach_msg_type_number_t,
                ) -> libc::kern_return_t;
                fn mach_port_deallocate(
                    task: mach_port_t,
                    name: mach_port_t,
                ) -> libc::kern_return_t;
            }

            const HOST_VM_INFO64: libc::c_int = 4;
            let host = mach_host_self();
            let mut vm_stat = MaybeUninit::<VmStatistics64>::uninit();
            let mut count = (std::mem::size_of::<VmStatistics64>()
                / std::mem::size_of::<libc::c_int>())
                as libc::mach_msg_type_number_t;

            let kr = host_statistics64(
                host,
                HOST_VM_INFO64,
                vm_stat.as_mut_ptr() as *mut libc::c_int,
                &mut count,
            );
            mach_port_deallocate(mach_task_self(), host);

            if kr == 0 {
                let stat = vm_stat.assume_init();
                let page_size = Self::page_size();
                let bytes = |pages: u32| (pages as u64).saturating_mul(page_size);

                // The same split as Activity Monitor: purgeable pages count as cache.
                return DetailedMemoryStatus {
                    total_bytes,
                    free_bytes: bytes(stat.free_count),
                    app_bytes: bytes(
                        stat.internal_page_count
                            .saturating_sub(stat.purgeable_count),
                    ),
                    wired_bytes: bytes(stat.wire_count),
                    compressed_bytes: bytes(stat.compressor_page_count),
                    cached_bytes: bytes(
                        stat.external_page_count
                            .saturating_add(stat.purgeable_count),
                    ),
                    swap_used_bytes: 0,
                    pressure,
                };
            }
        }

        DetailedMemoryStatus {
            total_bytes,
            free_bytes: total_bytes / 2,
            app_bytes: total_bytes / 4,
            wired_bytes: total_bytes / 8,
            compressed_bytes: 0,
            cached_bytes: total_bytes / 8,
            swap_used_bytes: 0,
            pressure,
        }
    }

    // 16 KB on Apple Silicon and 4 KB on Intel.
    fn page_size() -> u64 {
        Self::read_sysctl_u64("hw.pagesize").unwrap_or(16384)
    }

    pub fn detailed_storage(path: &Path) -> DetailedStorageStatus {
        let path_str = path.to_str().unwrap_or("/");
        if let Some((avail, imp, total)) = Self::query_foundation_volume_capacities(path_str) {
            let managed_bytes = imp.saturating_sub(avail);
            return DetailedStorageStatus {
                mount_point: path_str.to_string(),
                total_bytes: total,
                available_bytes: avail,
                free_bytes: imp,
                managed_bytes,
            };
        }

        let c_path = CString::new(path_str).unwrap();
        let mut stat = MaybeUninit::<libc::statfs>::uninit();
        let res = unsafe { libc::statfs(c_path.as_ptr(), stat.as_mut_ptr()) };

        if res == 0 {
            let s = unsafe { stat.assume_init() };
            let bsize = s.f_bsize as u64;
            let total_bytes = s.f_blocks.saturating_mul(bsize);
            let free_bytes = s.f_bfree.saturating_mul(bsize);
            let available_bytes = s.f_bavail.saturating_mul(bsize);
            let managed_bytes = free_bytes.saturating_sub(available_bytes);

            DetailedStorageStatus {
                mount_point: path_str.to_string(),
                total_bytes,
                available_bytes,
                free_bytes,
                managed_bytes,
            }
        } else {
            DetailedStorageStatus {
                mount_point: path_str.to_string(),
                total_bytes: 0,
                available_bytes: 0,
                free_bytes: 0,
                managed_bytes: 0,
            }
        }
    }

    // Foundation autoreleases what it returns, and these threads have no pool to drain it.
    #[cfg(target_os = "macos")]
    fn query_foundation_volume_capacities(path_str: &str) -> Option<(u64, u64, u64)> {
        use std::ffi::c_void;

        #[link(name = "objc")]
        unsafe extern "C" {
            fn objc_autoreleasePoolPush() -> *mut c_void;
            fn objc_autoreleasePoolPop(pool: *mut c_void);
        }

        unsafe {
            let pool = objc_autoreleasePoolPush();
            let capacities = Self::read_foundation_volume_capacities(path_str);
            objc_autoreleasePoolPop(pool);
            capacities
        }
    }

    #[cfg(target_os = "macos")]
    fn read_foundation_volume_capacities(path_str: &str) -> Option<(u64, u64, u64)> {
        use std::ffi::c_void;

        #[link(name = "Foundation", kind = "framework")]
        #[link(name = "objc")]
        unsafe extern "C" {
            fn objc_getClass(name: *const libc::c_char) -> *mut c_void;
            fn sel_registerName(name: *const libc::c_char) -> *mut c_void;
            static NSURLVolumeAvailableCapacityKey: *mut c_void;
            static NSURLVolumeAvailableCapacityForImportantUsageKey: *mut c_void;
            static NSURLVolumeTotalCapacityKey: *mut c_void;
        }

        type Id = *mut c_void;
        type Sel = *mut c_void;

        unsafe {
            let msg_send_str: unsafe extern "C" fn(Id, Sel, *const libc::c_char) -> Id =
                std::mem::transmute(libc::dlsym(libc::RTLD_DEFAULT, c"objc_msgSend".as_ptr()));
            let msg_send_url: unsafe extern "C" fn(Id, Sel, Id) -> Id =
                std::mem::transmute(libc::dlsym(libc::RTLD_DEFAULT, c"objc_msgSend".as_ptr()));
            let msg_send_arr: unsafe extern "C" fn(Id, Sel, *const Id, libc::size_t) -> Id =
                std::mem::transmute(libc::dlsym(libc::RTLD_DEFAULT, c"objc_msgSend".as_ptr()));
            let msg_send_dict: unsafe extern "C" fn(Id, Sel, Id, *mut Id) -> Id =
                std::mem::transmute(libc::dlsym(libc::RTLD_DEFAULT, c"objc_msgSend".as_ptr()));
            let msg_send_obj: unsafe extern "C" fn(Id, Sel, Id) -> Id =
                std::mem::transmute(libc::dlsym(libc::RTLD_DEFAULT, c"objc_msgSend".as_ptr()));
            let msg_send_u64: unsafe extern "C" fn(Id, Sel) -> u64 =
                std::mem::transmute(libc::dlsym(libc::RTLD_DEFAULT, c"objc_msgSend".as_ptr()));

            let nsstring_cls = objc_getClass(c"NSString".as_ptr());
            let nsurl_cls = objc_getClass(c"NSURL".as_ptr());
            let nsarray_cls = objc_getClass(c"NSArray".as_ptr());

            if nsstring_cls.is_null() || nsurl_cls.is_null() || nsarray_cls.is_null() {
                return None;
            }

            let sel_str = sel_registerName(c"stringWithUTF8String:".as_ptr());
            let sel_file_url = sel_registerName(c"fileURLWithPath:".as_ptr());
            let sel_array = sel_registerName(c"arrayWithObjects:count:".as_ptr());
            let sel_res_vals = sel_registerName(c"resourceValuesForKeys:error:".as_ptr());
            let sel_obj_for_key = sel_registerName(c"objectForKeyedSubscript:".as_ptr());
            let sel_long_val = sel_registerName(c"unsignedLongLongValue".as_ptr());

            let c_path = CString::new(path_str).ok()?;
            let path_obj = msg_send_str(nsstring_cls, sel_str, c_path.as_ptr());
            if path_obj.is_null() {
                return None;
            }

            let url_obj = msg_send_url(nsurl_cls, sel_file_url, path_obj);
            if url_obj.is_null() {
                return None;
            }

            let keys = [
                NSURLVolumeAvailableCapacityKey,
                NSURLVolumeAvailableCapacityForImportantUsageKey,
                NSURLVolumeTotalCapacityKey,
            ];
            let keys_arr = msg_send_arr(nsarray_cls, sel_array, keys.as_ptr(), keys.len());

            let dict_obj = msg_send_dict(url_obj, sel_res_vals, keys_arr, std::ptr::null_mut());
            if dict_obj.is_null() {
                return None;
            }

            let avail_obj =
                msg_send_obj(dict_obj, sel_obj_for_key, NSURLVolumeAvailableCapacityKey);
            let imp_obj = msg_send_obj(
                dict_obj,
                sel_obj_for_key,
                NSURLVolumeAvailableCapacityForImportantUsageKey,
            );
            let total_obj = msg_send_obj(dict_obj, sel_obj_for_key, NSURLVolumeTotalCapacityKey);

            let avail = if !avail_obj.is_null() {
                msg_send_u64(avail_obj, sel_long_val)
            } else {
                0
            };
            let imp = if !imp_obj.is_null() {
                msg_send_u64(imp_obj, sel_long_val)
            } else {
                avail
            };
            let total = if !total_obj.is_null() {
                msg_send_u64(total_obj, sel_long_val)
            } else {
                0
            };

            Some((avail, imp, total))
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn query_foundation_volume_capacities(_path_str: &str) -> Option<(u64, u64, u64)> {
        None
    }

    pub fn current_full_status() -> FullSystemStatus {
        FullSystemStatus {
            machine: Self::machine_info(),
            memory: Self::detailed_memory(),
            storage: Self::detailed_storage(Path::new("/")),
            live: LiveStatus::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_size_matches_the_system() {
        let system = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;

        assert_eq!(SystemProfiler::page_size(), system);
    }

    #[test]
    fn memory_parts_stay_within_the_total() {
        let memory = SystemProfiler::detailed_memory();

        assert!(memory.total_bytes > 0);
        assert!(memory.used_bytes() + memory.free_bytes <= memory.total_bytes);
    }

    #[test]
    fn used_memory_leaves_out_the_cache() {
        let memory = DetailedMemoryStatus {
            total_bytes: 100,
            free_bytes: 10,
            app_bytes: 20,
            wired_bytes: 10,
            compressed_bytes: 10,
            cached_bytes: 50,
            swap_used_bytes: 0,
            pressure: None,
        };

        assert_eq!(memory.used_bytes(), 40);
        assert_eq!(memory.used_fraction(), 0.4);
    }
}

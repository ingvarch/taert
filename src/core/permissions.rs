//! Checks for the macOS permissions taert needs, and the page of System Settings that grants them.

use std::fs::File;
use std::io::ErrorKind;
use std::time::Duration;

use super::host::Host;
use super::layout::Layout;

const TCC_DB: &str = "Library/Application Support/com.apple.TCC/TCC.db";
const FULL_DISK_ACCESS_PAGE: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles";
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullDiskAccess {
    On,
    Off,
    Unknown,
}

/// Opens the permissions database of the user for reading and reads nothing from it.
pub fn full_disk_access(layout: &Layout) -> FullDiskAccess {
    match File::open(layout.home.join(TCC_DB)) {
        Ok(_) => FullDiskAccess::On,
        Err(error) if error.kind() == ErrorKind::PermissionDenied => FullDiskAccess::Off,
        Err(_) => FullDiskAccess::Unknown,
    }
}

/// Shows the Full Disk Access page of System Settings.
pub fn open_full_disk_access(host: &dyn Host) {
    host.run("/usr/bin/open", &[FULL_DISK_ACCESS_PAGE], OPEN_TIMEOUT);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::apps::testing::sandbox;
    use crate::core::host::fake::FakeHost;
    use std::os::unix::fs::PermissionsExt;

    fn write_tcc(layout: &Layout, mode: u32) {
        let path = layout.home.join(TCC_DB);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn a_readable_database_means_full_disk_access_is_on() {
        let (_dir, layout) = sandbox();
        write_tcc(&layout, 0o644);

        assert_eq!(full_disk_access(&layout), FullDiskAccess::On);
    }

    #[test]
    fn a_database_that_refuses_reading_means_it_is_off() {
        let (_dir, layout) = sandbox();
        write_tcc(&layout, 0o000);

        assert_eq!(full_disk_access(&layout), FullDiskAccess::Off);
    }

    #[test]
    fn a_missing_database_means_it_is_unknown() {
        let (_dir, layout) = sandbox();

        assert_eq!(full_disk_access(&layout), FullDiskAccess::Unknown);
    }

    #[test]
    fn the_page_opens_through_the_host() {
        let host = FakeHost::default();

        open_full_disk_access(&host);

        assert!(host.ran(
            "/usr/bin/open x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles"
        ));
    }
}

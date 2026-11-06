//! Turns Touch ID for sudo on and off on the real Mac, through the real /etc/pam.d/sudo_local.
//!
//! Ignored by default, because it needs administrator rights (macOS asks for the password)
//! and writes in /etc/pam.d. Only the owner runs it by hand, never an agent.
//! `cargo test --test touch_id_live -- --ignored --nocapture`

use std::fs;
use std::os::unix::fs::MetadataExt;
use taert::core::admin::{TouchId, TouchIdChange, set_touch_id, touch_id_change, touch_id_status};
use taert::core::host::MacHost;
use taert::core::layout::Layout;

const TEMPLATE: &str = "# sudo_local: local config file which survives system update and is included for sudo\n# uncomment following line to enable Touch ID for sudo\n#auth       sufficient     pam_tid.so\n";
const ON: &str = "# sudo_local: local config file which survives system update and is included for sudo\n# uncomment following line to enable Touch ID for sudo\nauth       sufficient     pam_tid.so\n";
const FILE: &str = "/etc/pam.d/sudo_local";

#[test]
#[ignore = "needs administrator rights and writes /etc/pam.d/sudo_local"]
fn touch_id_turns_on_and_off_through_sudo_local() {
    let (host, layout) = (MacHost, Layout::current());
    assert_eq!(
        touch_id_status(&host, &layout),
        TouchId::Off,
        "start with fingerprints enrolled, Touch ID for sudo off, and no sudo_local of your own"
    );
    assert_eq!(
        touch_id_change(&layout, TouchId::Off),
        Ok(Some(TouchIdChange::TurnOn))
    );

    set_touch_id(&host, &layout, TouchIdChange::TurnOn).unwrap();
    let meta = fs::metadata(FILE).unwrap();
    assert_eq!(fs::read_to_string(FILE).unwrap(), ON);
    assert_eq!(meta.mode() & 0o7777, 0o444);
    assert_eq!(meta.uid(), 0);
    assert_eq!(touch_id_status(&host, &layout), TouchId::On);

    set_touch_id(&host, &layout, TouchIdChange::TurnOff).unwrap();
    let meta = fs::metadata(FILE).unwrap();
    assert_eq!(fs::read_to_string(FILE).unwrap(), TEMPLATE);
    assert_eq!(meta.mode() & 0o7777, 0o444);
    assert_eq!(touch_id_status(&host, &layout), TouchId::Off);
}

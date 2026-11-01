//! The check of the pf anchor against the real `pfctl` of the Mac the tests run on.

use std::path::Path;

use js_agent::native::{NativeFlags, NativeSettings, PfFlags};

use super::fake_helper::FakeHelper;
use crate::support::Scratch;

/// Asserts that, run by a user who is not root, the check the flags make over the Mac's own
/// `pfctl` is not "enforced": no anchor is loaded, and `pfctl` cannot read the packet filter
/// without root. It catches a check that reads a `pfctl` that failed as a limit in force. (As
/// root, the verdict depends on the Mac, so the test says nothing.)
#[test]
fn the_real_pfctl_is_not_enforced_for_a_user_who_is_not_root() {
    if rustix::process::geteuid().is_root() {
        return;
    }
    let dir = Scratch::new("native-mac-pf");
    let fake = FakeHelper::start(&dir);
    let state = dir.path("pf-state");
    let flags = NativeFlags {
        socket: Some(fake.socket()),
        homes: Some(fake.homes()),
        network: Some("on"),
        tool_roots: &[],
        deny_read: &[],
        max_file_mib: None,
        max_processes: None,
        pf: PfFlags {
            state_dir: Some(&state),
            uid_start: Some(600),
            uid_count: Some(10),
            ..PfFlags::default()
        },
    };
    let settings = NativeSettings::from_flags(&flags, &[], Path::new("/srv/journal"))
        .unwrap()
        .unwrap();
    let verdict = settings.config().egress.as_ref().unwrap().enforced();
    assert!(verdict.is_err(), "enforced with no anchor loaded");
}

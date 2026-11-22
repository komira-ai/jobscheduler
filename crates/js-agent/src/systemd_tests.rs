//! The version read from what `systemctl --version` prints.

use super::{OWN_CGROUP_START, version};

#[test]
fn the_version_is_the_number_after_systemd_on_the_first_line() {
    let printed = "systemd 259 (259.5-0ubuntu3.4)\n+PAM +AUDIT +SELINUX\n";
    assert_eq!(version(printed), Some(259));
    assert_eq!(version("systemd 255 (255.4-1ubuntu8.18)"), Some(255));
    assert!(version(printed).is_some_and(|found| found >= OWN_CGROUP_START));
    assert!(version("systemd 257 (257.9)").is_some_and(|found| found < OWN_CGROUP_START));
}

#[test]
fn anything_else_is_no_version() {
    for printed in [
        "",
        "\nsystemd 259",
        "launchd 259",
        "systemd",
        "systemd two-five-nine",
        "systemd -1",
    ] {
        assert_eq!(version(printed), None, "{printed:?}");
    }
}

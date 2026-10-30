//! The macOS version, the runner image and the tools the runtime would find.

use std::path::Path;

use super::exec::{first_line, lines};
use crate::report::Probe;

/// `df -h`, printed to stderr as well so it is in the log before anything else runs.
pub fn df() -> Probe {
    let rows = lines("/bin/df", &["-h"]);
    for row in &rows {
        eprintln!("{row}");
    }
    Probe::info("system/df").fact("df_h", rows.join(" | "))
}

pub fn probe() -> Probe {
    let xcodebuild = lines("/usr/bin/xcodebuild", &["-version"]);
    let xcode_line = |at: usize| xcodebuild.get(at).cloned().unwrap_or_default();
    let var = |name: &str| std::env::var(name).unwrap_or_default();
    let exists = |path: &str| Path::new(path).exists();
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    Probe::info("system/facts")
        .fact(
            "product_name",
            first_line("/usr/bin/sw_vers", &["-productName"]),
        )
        .fact(
            "product_version",
            first_line("/usr/bin/sw_vers", &["-productVersion"]),
        )
        .fact(
            "build_version",
            first_line("/usr/bin/sw_vers", &["-buildVersion"]),
        )
        .fact("machine", first_line("/usr/bin/uname", &["-m"]))
        .fact("image_os", var("ImageOS"))
        .fact("image_version", var("ImageVersion"))
        .fact("runner_os", var("RUNNER_OS"))
        .fact("runner_arch", var("RUNNER_ARCH"))
        .fact("euid", euid)
        .fact(
            "developer_dir",
            first_line("/usr/bin/xcode-select", &["-p"]),
        )
        .fact("clang", first_line("/usr/bin/clang", &["--version"]))
        .fact("swiftc", first_line("/usr/bin/swiftc", &["--version"]))
        .fact(
            "boot_session_uuid_length",
            first_line("/usr/sbin/sysctl", &["-n", "kern.bootsessionuuid"]).len(),
        )
        .fact("xcode_version", xcode_line(0))
        .fact("xcode_build", xcode_line(1))
        .fact(
            "csrutil_status",
            first_line("/usr/bin/csrutil", &["status"]),
        )
        .fact(
            "vm_cs_library_validation",
            first_line("/usr/sbin/sysctl", &["-n", "vm.cs_library_validation"]),
        )
        .fact("spctl_status", first_line("/usr/sbin/spctl", &["--status"]))
        .fact("sandbox_exec_present", exists("/usr/bin/sandbox-exec"))
        .fact("pfctl_present", exists("/sbin/pfctl"))
        .fact("hdiutil_present", exists("/usr/bin/hdiutil"))
        .fact("launchctl_present", exists("/bin/launchctl"))
}

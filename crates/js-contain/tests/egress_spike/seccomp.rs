//! The real seccomp profile of the container runner, as a `linux.seccomp` section.
//!
//! The section is the one the driver itself resolves from its pinned profile
//! (`src/oci/seccomp.json`, containers/common 0.57.4), through `js_contain::oci::seccomp_section`,
//! for a job that holds NO capability, which is what the plan's hardening step gives the job.

use serde_json::Value;

/// The Go name of this machine's architecture.
fn go_arch() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "amd64"
    }
}

/// The `linux.seccomp` section, and the number of syscall entries in it.
pub fn section() -> (Value, usize) {
    let section =
        js_contain::oci::seccomp_section(go_arch(), &[]).expect("the pinned profile resolves");
    let count = section["syscalls"].as_array().map_or(0, Vec::len);
    (section, count)
}

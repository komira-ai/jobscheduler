//! `js-vm-guest`: the smallest program of the Linux VM prototype that runs in the guest. It
//! takes "start this container" over the VM's virtio socket and answers with the exit, one
//! JSON line each way (`js_agent::linux_vm::wire`). Not shipped; Linux only, run as root:
//!
//! ```text
//! js-vm-guest --port 7000 --crun /usr/bin/crun --rootfs <image root> --runs <empty dir>
//!     [--busybox <static busybox> --job-user <uid>:<gid> --hardened-work <empty dir>]
//! ```
//!
//! A `run` is `crun run` of a bundle the program writes from the root and the request's
//! arguments (tests/vm_guest/plain.rs). A `hardened` run goes through the agent's own crun
//! runtime with its egress firewall (tests/vm_guest/hardened.rs), so the container gets
//! what a Linux node gives it.

#[cfg(target_os = "linux")]
#[path = "../vm_guest/hardened.rs"]
mod hardened;
#[cfg(target_os = "linux")]
#[path = "../vm_guest/plain.rs"]
mod plain;
#[cfg(target_os = "linux")]
#[path = "../runtime_host/mod.rs"]
mod runtime_host;
#[cfg(target_os = "linux")]
#[path = "../vm_guest/serve.rs"]
mod serve;

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    serve::main()
}

/// The guest is Linux: on any other target the program only says so.
#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("js-vm-guest: linux only");
    std::process::ExitCode::from(2)
}

//! `js-launch-measure`: the launch time of a `crun` job through the agent's own
//! `CrunRuntime`, with egress mode none and with an allow-list wired, for the manual workflow
//! `js-launch-measure.yml` (a measurement, not a test; not shipped). Linux only, run as root:
//!
//! ```text
//! js-launch-measure --image-name <runner image> --crun <path> --busybox <path> \
//!     --job-user <uid>:<gid> --work-dir <empty dir> [--runs 50] [--warmup 5] \
//!     [--summary-file <path>]
//! ```
//!
//! What one sample is and how they are taken: tests/launch_measure/run.rs. The flags:
//! tests/launch_measure/flags.rs. The statistics and the table: tests/launch_measure/table.rs.
//! It asserts nothing about any duration: it prints what it measured.

#[cfg(target_os = "linux")]
#[path = "../launch_measure/flags.rs"]
mod flags;
#[cfg(target_os = "linux")]
#[path = "../launch_measure/out.rs"]
mod out;
#[cfg(target_os = "linux")]
#[path = "../launch_measure/run.rs"]
mod run;
#[cfg(target_os = "linux")]
#[path = "../runtime_host/mod.rs"]
mod runtime_host;
#[cfg(target_os = "linux")]
#[path = "../launch_measure/table.rs"]
mod table;

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    run::main()
}

/// The measurement drives `crun`: on any other target the program only says so.
#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("js-launch-measure: linux only");
    std::process::ExitCode::from(2)
}

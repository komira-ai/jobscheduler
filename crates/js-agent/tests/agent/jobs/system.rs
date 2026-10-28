//! The machine the agent runs on: its clock, its view of the host and its engine. These are
//! the parts of the job half that no test can swap, so they are read here against the real
//! thing: the monotonic clock, this machine's `/proc`, and a program that prints what
//! Docker prints.

use std::time::Duration;

use js_agent::usage::UsageClock;
use js_agent::{FileReader, Machine, Platform, RuntimeBuild, System};

use crate::support::Scratch;

/// Asserts that the system clock only goes forward and starts near zero, and that two
/// machines have clocks of their own. It catches a clock that reads the wall clock (a
/// change of the time would make a job run long or die early).
#[test]
fn the_system_clock_is_monotonic() {
    let clock = System::new().clock();
    let first = clock.now_ms();
    let second = clock.now_ms();
    assert!(second >= first);
    assert!(first < 60_000, "it starts when the machine does: {first}");
    assert!(System::default().clock().now_ms() < 60_000);
}

/// Asserts that the system's usage clock counts microseconds on the same zero as its clock
/// of the jobs (a reading of the clock in milliseconds, then one of the usage clock, is at
/// least as many thousand microseconds) and does not go back, and that its cgroup files are
/// this machine's. It catches a lease start stamped on one clock and sampled on another,
/// which would put every minute boundary in the wrong place.
#[test]
fn the_system_usage_clock_shares_the_jobs_zero() {
    let machine = System::new();
    let ms = machine.clock().now_ms();
    let usage = machine.usage_clock();
    let first = usage.now_usec();
    assert!(first >= ms * 1000);
    assert!(usage.now_usec() >= first);
    assert!(machine.cgroup_files().read("/proc/self/cgroup").is_ok());
}

/// Asserts that the system's view of the host is this machine's `/proc`: the process table
/// holds this very process. It catches a view over the wrong files.
#[test]
fn the_system_view_reads_this_machines_processes() {
    let processes = System::new().view().processes().expect("/proc is readable");
    let own = std::process::id();
    assert!(processes.iter().any(|process| process.pid == own));
}

/// Asserts that the system opens an engine by the program's own `--version`: a program that
/// prints what Docker prints is a runtime, with no warning needed; one that prints something
/// else is refused with its path. It catches an agent that takes any program for an engine.
#[test]
fn the_system_opens_a_runtime_by_its_version_text() {
    let dir = Scratch::new("jobs-system-engine");
    let platform = Platform {
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
    };
    let open = |script: &str| {
        let settings = settings_with(&dir, script);
        let build = RuntimeBuild {
            settings: &settings,
            host_id: "h1",
            platform: &platform,
            stop_grace: Duration::from_secs(1),
        };
        // A program just written can be busy for a moment while another test forks.
        let mut tried = System::new().runtime(&build);
        for _ in 0..20 {
            match &tried {
                Err(why) if why.contains("Text file busy") => {
                    tried = System::new().runtime(&build);
                }
                _ => break,
            }
        }
        (
            settings.bin().to_owned(),
            tried.map(|(_, warnings)| warnings),
        )
    };

    let (_, engine) = open("#!/bin/sh\necho 'Docker version 27.3.1, build ce12230'\n");
    assert!(engine.is_ok(), "{engine:?}");
    let (bin, other) = open("#!/bin/sh\necho 'a program of some other kind'\n");
    let why = other.expect_err("not an engine");
    assert!(
        why.contains(&bin) && why.contains("neither docker nor podman"),
        "{why}"
    );
}

/// The container flags with the program `script`.
fn settings_with(dir: &Scratch, script: &str) -> js_agent::cli_runtime::Settings {
    let exec = dir.write("engine", script, 0o755);
    let (user, uid, gid) = super::user();
    let settings = js_agent::cli_runtime::Settings::new(&exec, None, Some(&user)).unwrap();
    settings.with_agent(uid, gid).unwrap()
}

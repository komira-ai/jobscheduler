//! `proc_pid_rusage`: of the probe's own child, unprivileged; and, as the agent reads a run's
//! processes, of a process of another uid by root.

use std::io;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use super::exec;
use super::sys::{pid_usage, timebase};
use crate::parse::field;
use crate::report::{Probe, Report};

/// How many times to look for the child's CPU time to show before giving up.
const LOOKS: u32 = 200;

pub fn probes(exe: &Path, report: &mut Report) {
    let spawned = Command::new("/bin/sh")
        .args(["-c", "while :; do :; done"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(err) => {
            report.push(Probe::asserted("proc/rusage-own-child", false).fact("error", err));
            return;
        }
    };
    let pid = i32::try_from(child.id()).unwrap_or(0);
    // The child burns until it shows CPU time (the assertion is on accuracy, not on time).
    let mut last = pid_usage(pid);
    for _ in 0..LOOKS {
        if last.as_ref().is_ok_and(|u| u.user_time + u.system_time > 0) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
        last = pid_usage(pid);
    }
    let _ = child.kill();
    let _ = child.wait();

    let (numer, denom) = timebase().unwrap_or((0, 0));
    let probe = match last {
        Ok(u) => Probe::asserted(
            "proc/rusage-own-child",
            u.user_time + u.system_time > 0 && u.resident > 0 && u.footprint > 0,
        )
        .fact("user_time_ticks", u.user_time)
        .fact("system_time_ticks", u.system_time)
        .fact("resident_bytes", u.resident)
        .fact("footprint_bytes", u.footprint),
        Err(err) => Probe::asserted("proc/rusage-own-child", false).fact("error", err),
    };
    report.push(probe.fact("timebase", format!("{numer}/{denom}")));

    report.push(other_uid(exe));
}

/// The `rusage` child mode's text: the reader's effective uid and the usage of `pid`.
pub fn reading(pid: i32) -> io::Result<String> {
    let reader = rustix::process::geteuid().as_raw();
    pid_usage(pid).map(|u| {
        format!(
            "reader_uid={reader} user_time={} system_time={} resident={} footprint={}\n",
            u.user_time, u.system_time, u.resident, u.footprint
        )
    })
}

/// Asserted: root reads `proc_pid_rusage` of a process of another uid, as the agent (root)
/// reads each process of a run's account. The reader is this binary's `rusage` mode under
/// `sudo -n` (the hosted runner's sudo needs no password); the process read is this probe,
/// of the runner's uid. It holds when the read succeeds, the reader's uid is not the
/// target's, and the resident size and footprint are above zero. What the unprivileged
/// probe gets for launchd (uid 0) and for itself is reported beside it.
fn other_uid(exe: &Path) -> Probe {
    let own = std::process::id();
    let target_uid = rustix::process::geteuid().as_raw();
    let mut cmd = Command::new("/usr/bin/sudo");
    cmd.arg("-n")
        .arg(exe)
        .args(["child", "rusage", &own.to_string()]);
    let outcome = exec::run(cmd);
    let read = |key: &str| field(&outcome.stdout, key);
    let held = outcome.ok
        && read("reader_uid").is_some_and(|uid| uid != u64::from(target_uid))
        && read("resident").is_some_and(|bytes| bytes > 0)
        && read("footprint").is_some_and(|bytes| bytes > 0);
    let describe = |pid: u32| {
        let usage = i32::try_from(pid)
            .map_err(io::Error::other)
            .and_then(pid_usage);
        match usage {
            Ok(u) => format!("ok, resident {}", u.resident),
            Err(err) => format!("error: {err}"),
        }
    };
    let probe = Probe::asserted("proc/rusage-other-processes", held)
        .fact("target_pid", own)
        .fact("target_uid", target_uid)
        .fact("root_read", outcome.stdout.trim())
        .fact("unprivileged_launchd_pid_1", describe(1))
        .fact("unprivileged_self", describe(own));
    exec::describe(probe, "sudo_", &outcome)
}

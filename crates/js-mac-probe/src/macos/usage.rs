//! `proc_pid_rusage` from an unprivileged process.

use std::process::{Command, Stdio};
use std::time::Duration;

use super::sys::{pid_usage, timebase};
use crate::report::{Probe, Report};

/// How many times to look for the child's CPU time to show before giving up.
const LOOKS: u32 = 200;

pub fn probes(report: &mut Report) {
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

    // Reported: what an unprivileged caller gets for a process of another uid (launchd)
    // and for itself.
    let describe = |pid: i32| match pid_usage(pid) {
        Ok(u) => format!("ok, resident {}", u.resident),
        Err(err) => format!("error: {err}"),
    };
    let own = i32::try_from(std::process::id()).unwrap_or(0);
    report.push(
        Probe::info("proc/rusage-other-processes")
            .fact("launchd_pid_1", describe(1))
            .fact("self", describe(own)),
    );
}

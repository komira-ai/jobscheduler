//! A process group under a launchd job runs on after the job's bootout, with parent pid 1
//! (`docs/design/mac-one-program.md`, section 4.3 and section 7 step 1). Unprivileged, in the
//! runner user's own domain: `gui/<uid>`, or `user/<uid>` when there is no GUI session.
//! The plain parts and their tests are in `crate::launchd`.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use super::Scratch;
use super::exec::{Outcome, describe, first_line, run};
use crate::launchd::{Seen, plist, ready, seen, survived, under_job};
use crate::report::{Probe, Report, Verdict};

const NAME: &str = "launchd/own-group-survives-bootout";

/// How many times to look, 50 ms apart, for the job's line and for the job to be gone.
const LOOKS: u32 = 400;

/// How long the held child sleeps: past the probe, so it is the probe that ends it.
const HOLD_SECS: u64 = 600;

pub fn probes(scratch: &Scratch, report: &mut Report) {
    report.push(probe(scratch));
}

fn launchctl(args: &[&str]) -> Outcome {
    let mut cmd = Command::new("/bin/launchctl");
    cmd.args(args);
    run(cmd)
}

fn ps(pid: u32) -> String {
    let mut cmd = Command::new("/bin/ps");
    cmd.args([
        "-ww",
        "-o",
        "pid=,ppid=,pgid=,command=",
        "-p",
        &pid.to_string(),
    ]);
    // `ps -p` of a pid that is gone exits 1 with no rows, which reads as gone.
    run(cmd).stdout
}

fn pause() {
    std::thread::sleep(Duration::from_millis(50));
}

struct Job {
    domain: String,
    label: String,
    plist: PathBuf,
    ready: PathBuf,
    log: PathBuf,
}

fn probe(scratch: &Scratch) -> Probe {
    let uid = first_line("/usr/bin/id", &["-u"]);
    let gui = format!("gui/{uid}");
    let domain = if launchctl(&["print", &gui]).ok {
        gui
    } else {
        format!("user/{uid}")
    };
    let label = format!("js-mac-probe.{}", std::process::id());
    let dir = scratch.base.join("launchd");
    let job = Job {
        plist: dir.join(format!("{label}.plist")),
        ready: dir.join("ready"),
        log: dir.join("job.log"),
        domain,
        label,
    };
    let start = Probe::info(NAME)
        .fact("domain", &job.domain)
        .fact("label", &job.label);
    let mut seen_hold = None;
    let mut probe = match std::fs::create_dir_all(&dir) {
        Ok(()) => observe(scratch, &job, start, &mut seen_hold),
        Err(err) => start.fact("error", err),
    };
    clean_up(&job, seen_hold);
    if probe.verdict == Verdict::Info {
        probe.verdict = Verdict::Fail;
    }
    if probe.verdict == Verdict::Fail {
        let log = std::fs::read_to_string(&job.log).unwrap_or_default();
        probe = probe.fact("job_log", log.trim());
    }
    probe
}

/// Bootstraps the job, waits for its held child, boots the job out and judges what is
/// left. `hold` is set to the held child's pid as soon as it is known, for the clean-up.
fn observe(scratch: &Scratch, job: &Job, probe: Probe, hold: &mut Option<u32>) -> Probe {
    let exe = scratch.places.exe.to_string_lossy().into_owned();
    let ready_path = job.ready.to_string_lossy().into_owned();
    let program = [
        exe,
        "child".to_owned(),
        "launchd-job".to_owned(),
        "--out".to_owned(),
        ready_path,
        "--secs".to_owned(),
        HOLD_SECS.to_string(),
        "--tag".to_owned(),
        job.label.clone(),
    ];
    if let Err(err) = std::fs::write(&job.plist, plist(&job.label, &program, &job.log)) {
        return probe.fact("error", err);
    }
    let plist_path = job.plist.to_string_lossy().into_owned();
    let bootstrap = launchctl(&["bootstrap", &job.domain, &plist_path]);
    let probe = describe(probe, "bootstrap_", &bootstrap);
    if !bootstrap.ok {
        return probe;
    }
    let mut pids = None;
    for _ in 0..LOOKS {
        pids = std::fs::read_to_string(&job.ready)
            .ok()
            .and_then(|t| ready(&t));
        if pids.is_some() {
            break;
        }
        pause();
    }
    let Some((job_pid, hold_pid)) = pids else {
        return probe.fact("ready", "the job never wrote its line");
    };
    *hold = Some(hold_pid);
    let before = seen(&ps(hold_pid), hold_pid, &job.label);
    let probe = probe
        .fact("job_pid", job_pid)
        .fact("hold_pid", hold_pid)
        .fact("before", crate::launchd::describe(&before));
    let target = format!("{}/{}", job.domain, job.label);
    let bootout = launchctl(&["bootout", &target]);
    let probe = describe(probe, "bootout_", &bootout);
    // The job is gone once its pid is gone or no longer the job (the tag is the label).
    let mut job_gone = false;
    for _ in 0..LOOKS {
        job_gone = !matches!(seen(&ps(job_pid), job_pid, &job.label), Seen::Ours(_));
        if job_gone {
            break;
        }
        pause();
    }
    let after = seen(&ps(hold_pid), hold_pid, &job.label);
    // Judged on what is left, not on bootout's exit status (recorded above): bootout can exit
    // non-zero while it tears the job down.
    let held = under_job(&before, job_pid) && job_gone && survived(&after);
    let mut probe = probe
        .fact("job_gone", job_gone)
        .fact("after", crate::launchd::describe(&after));
    probe.verdict = [Verdict::Fail, Verdict::Pass][usize::from(held)];
    probe
}

/// Boots the job out if it is still loaded and ends the held child if it is still the
/// probe's. Best effort: the runner is discarded anyway.
fn clean_up(job: &Job, hold: Option<u32>) {
    let target = format!("{}/{}", job.domain, job.label);
    if launchctl(&["print", &target]).ok {
        let _ = launchctl(&["bootout", &target]);
    }
    if let Some(pid) = hold
        && matches!(seen(&ps(pid), pid, &job.label), Seen::Ours(_))
    {
        let mut kill = Command::new("/bin/kill");
        kill.args(["-9", &pid.to_string()]);
        let _ = run(kill);
    }
    let _ = std::fs::remove_file(&job.plist);
}

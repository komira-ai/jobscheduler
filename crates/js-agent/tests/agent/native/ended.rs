//! A run whose end was read, a supervisor that cannot be asked, and the signals a stop or a
//! removal sends: an ended run is answered from what was stored, its pid is signalled no
//! more, a job still running is ended by its removal, and a pid that names no process group
//! is signalled nothing.

use std::fs;
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command};
use std::time::Duration;

use js_agent::lifecycle::{Exit, RunHandle, Runtime, Status};
use rustix::io::Errno;
use rustix::process::{Pid, Signal, WaitOptions, kill_process, test_kill_process, waitpid};

use super::{Failing, POLLS, Rig};

fn exited(code: i32) -> Status {
    Status::Exited(Exit {
        code,
        oom_killed: false,
    })
}

/// Starts a job that writes its pid to a file and then sleeps for ten minutes, waits until the
/// file says the job runs, and returns the pid (the job's process group too).
fn start_sleeper(rig: &mut Rig, run_id: &str) -> (RunHandle, i32) {
    let file = rig.dir.path("job.pid");
    let script = format!("echo $$ > {}; exec /bin/sleep 600", file.display());
    let id = rig.start(run_id, "/bin/sh", &["-c", &script]);
    for _ in 0..POLLS {
        if let Some(pid) = fs::read_to_string(&file)
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok())
        {
            return (id, pid);
        }
        std::thread::yield_now();
    }
    panic!("the job did not write its pid");
}

/// Asserts that a run whose end was read once is answered from the stored status on every
/// later poll: the supervisor is looked at once, though the fake finds nothing the second
/// time.
///
/// Catches: a poll that asks the supervisor again for a run that ended (it would answer
/// running, as the supervisor has nothing more to say).
#[test]
fn an_ended_run_is_answered_from_its_stored_status() {
    let mut rig = Rig::new("ended-stored");
    rig.launcher.fail(Failing::ExitOnce);
    let id = rig.start("r0", "/usr/bin/true", &[]);
    let before = rig.launcher.looks();
    assert_eq!(rig.rt.poll(&id).unwrap(), exited(7));
    for _ in 0..3 {
        assert_eq!(rig.rt.poll(&id).unwrap(), exited(7));
    }
    assert_eq!(rig.launcher.looks() - before, 1);
    rig.rt.remove(&id).unwrap();
}

/// Asserts that a supervisor that cannot be asked leaves the run gone, that the answer is
/// kept (the supervisor is not asked again, and a poll is not running or exited later), and
/// that the removal still retires the account.
///
/// Catches: an error of the supervisor taken as "still running" (a run that never ends), or
/// forgotten at the next poll.
#[test]
fn a_supervisor_that_cannot_be_asked_leaves_the_run_gone() {
    let mut rig = Rig::new("ended-ask-fails");
    let id = rig.start("r0", "/usr/bin/true", &[]);
    rig.launcher.fail_looks(true);
    let before = rig.launcher.looks();
    assert_eq!(rig.rt.poll(&id).unwrap(), Status::Gone);
    rig.launcher.fail_looks(false);
    assert_eq!(rig.rt.poll(&id).unwrap(), Status::Gone);
    assert_eq!(rig.launcher.looks() - before, 1);
    rig.rt.remove(&id).unwrap();
    assert!(rig.pool.held().is_empty(), "{:?}", rig.pool.held());
}

/// Asserts that a supervisor that reports a failed setup step after the job was ready ends
/// the run as gone, not as an exit.
///
/// Catches: a setup failure told as an exit status (a run that never ran reported as ended
/// with a code).
#[test]
fn a_setup_failure_found_at_the_end_leaves_the_run_gone() {
    let mut rig = Rig::new("ended-setup");
    rig.launcher.fail(Failing::SetupAtExit);
    let id = rig.start("r0", "/usr/bin/true", &[]);
    let status = rig.rt.poll(&id).unwrap();
    assert_eq!(status, Status::Gone);
    rig.rt.remove(&id).unwrap();
}

/// Asserts that a stop of a run that has ended signals no process group: the run's recorded
/// pid is made another process's (a bystander's, in a group of its own), and the bystander
/// goes on after the stop.
///
/// Catches: a stop that signals the group of a job that was reaped, whose pid may name
/// another process by now.
#[test]
fn a_stop_of_an_ended_run_signals_no_process_group() {
    let mut rig = Rig::new("ended-stop");
    let mut bystander: Child = Command::new("/bin/sleep")
        .arg("600")
        .process_group(0)
        .spawn()
        .unwrap();
    rig.launcher
        .report_job_pid(i32::try_from(bystander.id()).unwrap());
    let id = rig.start("r0", "/bin/sh", &["-c", "exit 0"]);
    assert_eq!(rig.settle(&id), exited(0));
    rig.rt.stop(&id, Duration::ZERO).unwrap();
    // A signal that was sent ends the bystander; look for that for as long as a bounded
    // number of turns, with no clock.
    for _ in 0..(1 << 20) {
        assert!(bystander.try_wait().unwrap().is_none(), "signalled");
        std::thread::yield_now();
    }
    bystander.kill().unwrap();
    bystander.wait().unwrap();
    rig.rt.remove(&id).unwrap();
}

/// Asserts that the removal of a run whose end was not read ends its job: the job's pid is
/// gone (killed, then reaped by the supervisor's last look) without a stop.
///
/// Catches: a removal that leaves a running job to the account's retirement alone (the
/// processes of the account stay while its outputs are collected and its logs closed).
#[test]
fn a_removal_ends_a_job_that_still_runs() {
    let mut rig = Rig::new("ended-remove-kills");
    let (id, pid) = start_sleeper(&mut rig, "r0");
    assert_eq!(rig.rt.poll(&id).unwrap(), Status::Running);
    rig.rt.remove(&id).unwrap();
    let pid = Pid::from_raw(pid).unwrap();
    for _ in 0..POLLS {
        match waitpid(Some(pid), WaitOptions::NOHANG) {
            // The job ended by the kill, and nobody had reaped it yet.
            Ok(Some((_, status))) => {
                assert_eq!(status.terminating_signal(), Some(9));
                return;
            }
            // The removal's last look at the supervisor reaped it: it had ended.
            Err(Errno::CHILD) => return,
            Ok(None) => std::thread::yield_now(),
            Err(why) => panic!("waiting for the job: {why}"),
        }
    }
    panic!("the job still runs after the removal");
}

/// Asserts that a removal looks at the supervisor once more after the account is retired
/// (its look reaps the supervisor, so the agent keeps no zombie), and that a supervisor that
/// cannot be asked then does not fail the removal.
///
/// Catches: a removal that leaves the supervisor unreaped, and one that fails on an error of
/// the last look.
#[test]
fn a_removal_looks_at_the_supervisor_once_more_after_the_retirement() {
    let mut rig = Rig::new("ended-last-look");
    let id = rig.start("r0", "/bin/sleep", &["600"]);
    assert_eq!(rig.rt.poll(&id).unwrap(), Status::Running);
    let before = rig.launcher.looks();
    rig.rt.remove(&id).unwrap();
    assert_eq!(rig.launcher.looks() - before, 1);

    let id = rig.start("r1", "/bin/sleep", &["600"]);
    rig.launcher.fail_looks(true);
    let before = rig.launcher.looks();
    rig.rt.remove(&id).unwrap();
    assert_eq!(rig.launcher.looks() - before, 1);
    assert!(rig.pool.held().is_empty(), "{:?}", rig.pool.held());
}

/// Asserts that a job whose reported pid names no process group (0, or negative) is signalled
/// nothing by a stop: the call returns, and the job's real process goes on.
///
/// Catches: a pid that cannot name a group passed on to a group kill (which would signal the
/// agent's own group, or panic).
#[test]
fn a_job_pid_that_names_no_group_is_signalled_nothing() {
    for (name, reported) in [("zero", 0), ("negative", -1)] {
        let mut rig = Rig::new(&format!("ended-nogroup-{name}"));
        rig.launcher.report_job_pid(reported);
        let (id, real) = start_sleeper(&mut rig, "r0");
        // A grace that does not pass in the test: the poll after it would sweep the run's
        // processes, which is not what this test looks at.
        rig.rt.stop(&id, Duration::from_secs(600)).unwrap();
        assert_eq!(rig.rt.poll(&id).unwrap(), Status::Running, "{name}");
        let real = Pid::from_raw(real).unwrap();
        assert!(
            test_kill_process(real).is_ok(),
            "{name}: the job was signalled"
        );
        kill_process(real, Signal::KILL).unwrap();
        rig.rt.remove(&id).unwrap();
    }
}

//! A stop request (in the example, SIGINT, SIGTERM or SIGHUP) before the launch, during
//! the boot wait and during the ssh step, and the teardown command printed up front. No
//! signal is sent: the rig's stop flag stands in for the handler's, and its read counter
//! places a request between two reads the runner makes.
//!
//! The reads of a run that launches, in order: 1 before the launch, then per wait
//! iteration one at the top and one before the pause; after the instance is active, one
//! before the ssh step and one after it.

use js_provider_lambdalabs::{Call, Status, TerminateEffect};

use crate::rig::{FIRST, Rig, TestRemote, activate};

fn ssh_flags() -> [&'static str; 4] {
    ["--ssh-key", "/keys/id", "--ssh-user", "ubuntu"]
}

/// The flags of the teardown command the run printed.
fn printed_flags(out: &str) -> &str {
    out.lines()
        .find_map(|l| l.strip_prefix(PRINTED))
        .unwrap_or_else(|| panic!("no teardown line:\n{out}"))
}

const PRINTED: &str = "teardown command (run it if this process dies): first_launch ";

fn assert_interrupted_and_terminated(rig: &Rig, status: i32, out: &str) {
    assert_eq!(status, 1, "{out}");
    assert!(out.contains("teardown forced: interrupted"), "{out}");
    assert!(
        out.contains("STOP: the run did not finish its check; the instance is terminated"),
        "{out}"
    );
    assert!(out.contains("result: terminated"), "{out}");
    assert_eq!(rig.fake.count(Call::Launch), 1);
    assert_eq!(rig.fake.count(Call::Terminate), 1);
    assert_eq!(rig.status_of(FIRST), Status::Terminating);
}

#[test]
fn a_stop_during_the_boot_wait_terminates_the_instance_and_says_interrupted() {
    let rig = Rig::new();
    let args = rig.go_args(&[]);
    let (status, out) = rig.exec_with(
        &args,
        &|n| {
            if n == 2 {
                rig.stop.set(true);
            }
        },
        &TestRemote::answering(Err("not asked".to_owned())),
    );
    assert_interrupted_and_terminated(&rig, status, &out);
    // The request came during the second pause; the next look at it is the next
    // iteration's first, so there is no third pause.
    assert_eq!(rig.sleeps.borrow().len(), 2, "{out}");
    assert_eq!(rig.clock_secs(), 20);
}

#[test]
fn a_stop_is_honoured_within_one_poll_step() {
    // Read 3 is the one before the first pause: a request that came during the list is
    // honoured without sleeping at all.
    let rig = Rig::new();
    let args = rig.go_args(&[]);
    rig.stop_from_read.set(3);
    let (status, out) = rig.exec(&args);
    assert_interrupted_and_terminated(&rig, status, &out);
    assert!(rig.sleeps.borrow().is_empty(), "{out}");
    assert_eq!(rig.clock_secs(), 0);
    let lists_with_one_look = rig.fake.count(Call::ListInstances);

    // Read 2 is the first iteration's top, which a request that came during the launch
    // reaches before any list: one look fewer than above.
    let rig = Rig::new();
    let args = rig.go_args(&[]);
    rig.stop_from_read.set(2);
    let (status, out) = rig.exec(&args);
    assert_interrupted_and_terminated(&rig, status, &out);
    assert!(rig.sleeps.borrow().is_empty(), "{out}");
    assert_eq!(rig.fake.count(Call::ListInstances) + 1, lists_with_one_look, "{out}");
}

#[test]
fn a_stop_during_the_ssh_step_terminates_the_instance_and_says_interrupted() {
    for answer in [Ok("fine".to_owned()), Err("killed".to_owned())] {
        let rig = Rig::new();
        let args = rig.go_args(&ssh_flags());
        let remote = TestRemote::answering(answer);
        let (status, out) = rig.exec_with(
            &args,
            &|n| {
                if n == 1 {
                    activate(&rig.fake);
                    // Reads 1 to 3 are the launch and the first iteration, 4 the top of
                    // the second, 5 the one before the ssh step, 6 the one after it: a
                    // request that came while ssh ran.
                    rig.stop_from_read.set(6);
                }
            },
            &remote,
        );
        assert_interrupted_and_terminated(&rig, status, &out);
        assert_eq!(remote.asked.borrow().len(), 1, "{out}");
    }
}

#[test]
fn a_stop_after_active_and_before_ssh_does_not_start_the_ssh_step() {
    let rig = Rig::new();
    let args = rig.go_args(&ssh_flags());
    let remote = TestRemote::answering(Ok("fine".to_owned()));
    let (status, out) = rig.exec_with(
        &args,
        &|n| {
            if n == 1 {
                activate(&rig.fake);
                rig.stop_from_read.set(5);
            }
        },
        &remote,
    );
    assert_interrupted_and_terminated(&rig, status, &out);
    assert!(remote.asked.borrow().is_empty(), "{out}");
}

#[test]
fn a_stop_before_the_launch_launches_nothing_and_leaves_no_state_file() {
    let rig = Rig::new();
    let args = rig.go_args(&[]);
    rig.stop.set(true);
    let (status, out) = rig.exec(&args);
    assert_eq!(status, 1, "{out}");
    assert!(
        out.contains("error: interrupted: a stop was requested (SIGINT, SIGTERM or SIGHUP); nothing was launched"),
        "{out}"
    );
    assert!(
        out.contains("STOP: nothing was launched; paste back this output."),
        "{out}"
    );
    assert_eq!(rig.fake.count(Call::Launch), 0);
    assert_eq!(rig.fake.count(Call::Terminate), 0);
    assert!(rig.fake.instances().is_empty());
    assert!(!rig.state.exists(), "the state file was left behind");
    assert!(!out.contains("launched:"), "{out}");
}

#[test]
fn a_dry_run_ignores_a_stop_request() {
    let rig = Rig::new();
    rig.stop.set(true);
    let (status, out) = rig.exec(&rig.args(&[]));
    assert_eq!(status, 0, "{out}");
    assert_eq!(rig.stop_reads.get(), 0);
}

#[test]
fn the_teardown_command_is_printed_right_after_the_state_file_is_written() {
    let rig = Rig::new();
    let args = rig.go_args(&[]);
    let (status, out) = rig.exec_active(&args, 1, &TestRemote::answering(Ok(String::new())));
    assert_eq!(status, 0, "{out}");
    let line = format!(
        "teardown command (run it if this process dies): first_launch --base-url {} --key-file {} --teardown-only {}",
        crate::rig::BASE,
        rig.key.display(),
        rig.state.display(),
    );
    let lines: Vec<&str> = out.lines().collect();
    let at = lines
        .iter()
        .position(|l| *l == line)
        .unwrap_or_else(|| panic!("no teardown line:\n{out}"));
    // Before the launch is announced and before any wait line.
    let launched = lines
        .iter()
        .position(|l| l.starts_with("launched:"))
        .unwrap();
    let waiting = lines.iter().position(|l| l.contains(": status ")).unwrap();
    assert!(at < launched && at < waiting, "{out}");
}

#[test]
fn the_printed_teardown_command_works_and_carries_a_ca_bundle() {
    let rig = Rig::new();
    let args = rig.go_args(&["--ca-bundle", "/roots.pem"]);
    let (status, out) = rig.exec_active(&args, 1, &TestRemote::answering(Ok(String::new())));
    assert_eq!(status, 0, "{out}");
    let printed = printed_flags(&out);
    assert!(printed.contains(" --ca-bundle /roots.pem "), "{printed}");

    // Run the printed flags against an instance left behind the way a kill leaves one.
    let rig = Rig::new();
    rig.fake.set_terminate_effect(TerminateEffect::Nothing);
    let args = rig.go_args(&["--boot-deadline-secs", "10"]);
    let (status, out) = rig.exec(&args);
    assert_eq!(status, 1, "{out}");
    rig.fake.set_terminate_effect(TerminateEffect::Terminating);
    let printed = printed_flags(&out);
    let words: Vec<String> = printed.split(' ').map(str::to_owned).collect();
    let (status, out) = rig.exec(&words);
    assert_eq!(status, 0, "{out}");
    assert!(out.contains("tearing down: fake0001"), "{out}");
    assert_eq!(rig.status_of(FIRST), Status::Terminating);
}

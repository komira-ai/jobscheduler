//! `poll`, `stop` and `remove`: what each makes of the runtime's answers, and the id it
//! accepts.

use std::time::Duration;

use js_agent::lifecycle::{Exit, RunHandle, Runtime, RuntimeError, Status};

use super::super::{docker, open};
use super::{start, strings};

/// Asserts what `poll` makes of each state the runtime prints, and what it does when it
/// cannot tell: running and the states that mean it still exists, exited with its code
/// and the memory controller's flag, a container the runtime no longer has as `Gone`
/// (not an error), and an error for a failure while it is there, for output that is not
/// three words, for a code that is not a number and for a state it does not know. A
/// `dead` container (one the daemon failed to remove) is a failure and never a success:
/// it keeps its code when it has one, and a code of 0 is not taken for a success, so it
/// is reported as gone, which ends the job as failed. It catches a gone container taken
/// for a failure (the job would be polled for ever), a failure taken for gone (the job
/// would end as failed while it runs), an unknown state taken for running and a dead
/// container reported `succeeded`.
#[test]
fn poll_maps_every_state() {
    let world = docker();
    let mut rt = open(&world);
    let id = start(&mut rt, &world, "r1");
    assert_eq!(rt.poll(&id), Ok(Status::Running));
    let ok = |code, oom_killed| Ok(Status::Exited(Exit { code, oom_killed }));
    let table = [
        ("running 0 false", Ok(Status::Running)),
        ("paused 0 false", Ok(Status::Running)),
        ("restarting 0 false", Ok(Status::Running)),
        ("removing 0 false", Ok(Status::Running)),
        ("created 0 false", Ok(Status::Running)),
        ("configured 0 false", Ok(Status::Running)),
        ("exited 3 false\n", ok(3, false)),
        ("dead 1 false", ok(1, false)),
        ("dead 0 false", Ok(Status::Gone)),
        ("dead 0 true", ok(0, true)),
        ("stopped 0 false", ok(0, false)),
        ("exited 137 true", ok(137, true)),
        ("exited -1 false", ok(-1, false)),
        (
            "dead x false",
            Err(RuntimeError::new("inspect: unreadable output")),
        ),
        (
            "exited x false",
            Err(RuntimeError::new("inspect: unreadable output")),
        ),
        (
            "exited 1",
            Err(RuntimeError::new("inspect: unreadable output")),
        ),
        (
            "exited 1 false extra",
            Err(RuntimeError::new("inspect: unreadable output")),
        ),
        (
            "weird 0 false",
            Err(RuntimeError::new("inspect: unreadable output")),
        ),
        ("", Err(RuntimeError::new("inspect: unreadable output"))),
    ];
    for (printed, expected) in table {
        world.say("inspect", printed);
        assert_eq!(rt.poll(&id), expected, "{printed:?}");
    }
}

/// Asserts the `Gone` rule and its guard: a container removed from outside is `Gone`
/// (the failed `inspect` is followed by a look at `ps`, which no longer lists it); a
/// failed `inspect` of a container that is still listed is an error; when `ps` fails too
/// the runtime cannot say; and an id that is not a container id is refused before any
/// call. It catches a missing container read as a transient failure, and the reverse.
#[test]
fn a_missing_container_is_gone_and_a_failing_one_is_an_error() {
    let world = docker();
    let mut rt = open(&world);
    let id = start(&mut rt, &world, "r1");
    world.fail("inspect", 1);
    let said = rt.poll(&id).unwrap_err();
    assert_eq!(said.0, "inspect: /bin/sh: exit status: 1");
    world.fail("inspect", 1);
    world.fail("ps", 1);
    let said = rt.poll(&id).unwrap_err();
    assert_eq!(said.0, "ps: /bin/sh: exit status: 1");
    world.vanish("r1");
    assert_eq!(rt.poll(&id), Ok(Status::Gone));
    let ps = world.calls_of("ps");
    let last = ps.last().unwrap();
    assert_eq!(
        last[1..],
        strings(&[
            "-a",
            "-q",
            "--no-trunc",
            "--filter",
            &format!("id={}", id.0)
        ])[..]
    );

    let calls = world.calls().len();
    for bad in ["", "r1", "--all", "ABC123456789", "0123456789abcdefg"] {
        let said = rt.poll(&RunHandle(bad.to_owned())).unwrap_err();
        assert_eq!(said.0, "not a container id", "{bad:?}");
    }
    assert_eq!(world.calls().len(), calls, "no call with a bad id");
}

/// Asserts that an id of fewer than 64 digits is refused before any call by every verb,
/// even when it is the front of the id of a container that is there. `ps --no-trunc`
/// prints 64 digits and the agent compares whole lines, so a short id whose `inspect`,
/// `stop` or `rm` failed would be read as "gone" while the container runs. It catches that
/// confusion, and a listing that accepts a short id the engine printed.
#[test]
fn a_short_id_is_refused_while_the_container_is_there() {
    let world = docker();
    let mut rt = open(&world);
    let id = start(&mut rt, &world, "r1");
    let short = RunHandle(id.0[..12].to_owned());
    let calls = world.calls().len();
    assert_eq!(rt.poll(&short).unwrap_err().0, "not a container id");
    let stop = rt.stop(&short, Duration::from_secs(1)).unwrap_err();
    assert_eq!(stop.0, "not a container id");
    assert_eq!(rt.remove(&short).unwrap_err().0, "not a container id");
    assert_eq!(world.calls().len(), calls, "no call was made");
    assert_eq!(
        world.ids(),
        std::slice::from_ref(&id.0),
        "the container is still there"
    );

    world.say_once("ps", &format!("{}\n", short.0));
    let said = rt.list_labelled().unwrap_err();
    assert_eq!(said.0, "not a container id");
}

/// Asserts the `inspect` call itself: a container type, the format that asks for the
/// state, and the id after `--`. It catches an id that could be read as an option.
#[test]
fn poll_asks_for_the_container_state_after_dashes() {
    let world = docker();
    let mut rt = open(&world);
    let id = start(&mut rt, &world, "r1");
    rt.poll(&id).unwrap();
    let inspect = world.calls_of("inspect").pop().unwrap();
    assert_eq!(
        inspect[..4],
        strings(&["inspect", "--type", "container", "--format"])[..]
    );
    assert_eq!(
        inspect[4],
        "{{.State.Status}} {{.State.ExitCode}} {{.State.OOMKilled}}"
    );
    assert_eq!(inspect[5..], strings(&["--", &id.0])[..]);
}

/// Asserts `stop`: the grace in whole seconds rounded up (so a container is never given
/// less than it was promised), the id after `--`, the container exits, a stop of a
/// container that is gone succeeds, and one that fails while the container is there is
/// an error (and so is one whose `ps` also fails). It catches a stop that waits less
/// than the grace, one that fails on a missing container, which the lifecycle would
/// retry for ever, and one that hides a real failure.
#[test]
fn stop_rounds_the_grace_up_and_a_gone_container_is_stopped() {
    let world = docker();
    let mut rt = open(&world);
    let id = start(&mut rt, &world, "r1");
    for (grace, secs) in [
        (Duration::from_secs(10), "10"),
        (Duration::from_millis(1500), "2"),
        (Duration::from_millis(1), "1"),
        (Duration::ZERO, "0"),
    ] {
        rt.stop(&id, grace).unwrap();
        let stop = world.calls_of("stop").pop().unwrap();
        assert_eq!(stop, strings(&["stop", "--time", secs, "--", &id.0]));
    }
    assert_eq!(
        rt.poll(&id),
        Ok(Status::Exited(Exit {
            code: 137,
            oom_killed: false
        }))
    );

    world.fail("stop", 1);
    let said = rt.stop(&id, Duration::from_secs(1)).unwrap_err();
    assert_eq!(said.0, "stop: /bin/sh: exit status: 1");
    world.fail("stop", 1);
    world.fail("ps", 1);
    let said = rt.stop(&id, Duration::from_secs(1)).unwrap_err();
    assert_eq!(said.0, "ps: /bin/sh: exit status: 1");

    world.vanish("r1");
    rt.stop(&id, Duration::from_secs(1)).unwrap();
    let said = rt
        .stop(&RunHandle("x".to_owned()), Duration::ZERO)
        .unwrap_err();
    assert_eq!(said.0, "not a container id");
}

/// Asserts `remove`: it forces, takes anonymous volumes with it, names the id after
/// `--`, succeeds on a container that is gone, and fails when the container is still
/// there and the call failed. It catches a removal that leaves a volume behind, one
/// that fails on a missing container, and one that hides a real failure.
#[test]
fn remove_forces_and_a_gone_container_is_removed() {
    let world = docker();
    let mut rt = open(&world);
    let id = start(&mut rt, &world, "r1");
    world.fail("rm", 1);
    let said = rt.remove(&id).unwrap_err();
    assert_eq!(said.0, "rm: /bin/sh: exit status: 1");
    world.fail("rm", 1);
    world.fail("ps", 1);
    let said = rt.remove(&id).unwrap_err();
    assert_eq!(said.0, "ps: /bin/sh: exit status: 1");
    rt.remove(&id).unwrap();
    let rm = world.calls_of("rm").pop().unwrap();
    assert_eq!(rm, strings(&["rm", "--force", "--volumes", "--", &id.0]));
    assert!(world.ids().is_empty());
    rt.remove(&id).unwrap();
    let said = rt.remove(&RunHandle("x".to_owned())).unwrap_err();
    assert_eq!(said.0, "not a container id");
}

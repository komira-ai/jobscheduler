//! That the agent sees the host its containers run on: the process the engine reports for a
//! started container must be in the agent's own `/proc`.

use js_agent::cli_runtime::pid_in_proc;
use js_agent::lifecycle::ContainerRuntime;

use super::super::{docker, open, spec};
use super::strings;

/// Asserts the look at the process: the `inspect` that asks for `State.Pid` and the state
/// comes after `start`, names the id after `--`, and the pid it prints is the one given to
/// the agent's `/proc` (the runtime of these tests sees the scripted engine's pid alone); a
/// container whose process is seen starts. It catches a pid taken from anywhere but the engine, and a
/// look made before the start, when the process does not exist yet.
#[test]
fn the_process_of_a_started_container_is_looked_for_after_the_start() {
    let world = docker();
    let mut rt = open(&world);
    let id = rt.create_and_start(&spec("r1")).unwrap();
    let calls = world.calls();
    let at = calls.iter().position(|call| call[0] == "start").unwrap();
    assert_eq!(
        calls[at + 1],
        strings(&[
            "inspect",
            "--type",
            "container",
            "--format",
            "{{.State.Pid}} {{.State.Status}}",
            "--",
            &id.0
        ])
    );
    assert_eq!(calls.len(), at + 2, "and nothing after it");
}

/// Asserts that a container whose process is not in the agent's `/proc` is not kept: the
/// start fails with the process named, the container is removed (by the id, with `rm`),
/// the failure is marked as the agent not seeing the host, and when the removal fails too
/// the error says so and the container is left standing in the listing. It catches an
/// agent in a container, or under `PrivatePIDs`, that runs jobs on the host through a
/// socket and then checks a host that is not theirs.
#[test]
fn a_container_whose_process_is_not_in_proc_is_removed_and_the_host_is_not_seen() {
    let world = docker();
    let mut rt = open(&world).with_pid_seen(|_| false);
    let said = rt.create_and_start(&spec("r1")).unwrap_err();
    assert_eq!(
        said.0,
        "the process 4000 of the container is not in the agent's /proc: the agent does not see the host its jobs run on"
    );
    assert!(said.is_blind());
    assert!(world.ids().is_empty(), "the container was removed");
    assert_eq!(world.calls_of("start").len(), 1, "it had started");
    assert_eq!(world.calls_of("rm").len(), 1);

    world.fail("rm", 1);
    let said = rt.create_and_start(&spec("r2")).unwrap_err();
    assert!(
        said.0
            .contains("; the container could not be removed: rm: "),
        "{}",
        said.0
    );
    assert!(said.is_blind(), "and still the host that is not seen");
    assert_eq!(world.ids().len(), 1, "the leak is reported, not hidden");
}

/// Asserts what happens when the look at the process cannot be used: output that is not a
/// pid and a state, a pid of 0 for a container that is not over, a failed call, and a
/// container that went away in between each fail the start with the call named, remove the
/// container, and are not marked as the host not being seen (the agent does not know). It
/// catches an answer the agent did not understand being taken for a process it saw.
#[test]
fn a_look_at_the_process_that_cannot_be_used_fails_the_start() {
    for printed in [
        "garbage",
        "4000",
        "x running",
        "4000 running extra",
        "",
        "0 running",
        "0 created",
        "-1 running",
    ] {
        let world = docker();
        let mut rt = open(&world);
        world.pid_says(printed);
        let said = rt.create_and_start(&spec("r1")).unwrap_err();
        assert_eq!(said.0, "inspect: unreadable output", "{printed:?}");
        assert!(!said.is_blind(), "{printed:?}");
        assert!(world.ids().is_empty(), "{printed:?}: removed");
    }

    let world = docker();
    let mut rt = open(&world);
    world.fail_pid();
    let said = rt.create_and_start(&spec("r2")).unwrap_err();
    assert_eq!(said.0, "inspect: /bin/sh: exit status: 1");
    assert!(!said.is_blind());
    assert!(world.ids().is_empty());

    // The container went away between the start and the look.
    let world = docker();
    let mut rt = open(&world);
    world.fail_pid();
    world.say_once("ps", "");
    let said = rt.create_and_start(&spec("r3")).unwrap_err();
    assert_eq!(said.0, "inspect: unreadable output");
    assert!(!said.is_blind());
}

/// Asserts that a container that is over when the agent looks (it exited at once: pid 0,
/// state `exited`, `stopped` or `dead`) is let through, and kept for the lifecycle to find
/// its exit, since there is no process left to look for. It catches a job that fails fast
/// (a wrong command, an instant exit) tainting a host that is as visible as any other.
#[test]
fn a_container_that_is_over_when_the_agent_looks_is_started() {
    for printed in ["0 exited\n", "0 stopped\n", "0 dead\n"] {
        let world = docker();
        let mut rt = open(&world).with_pid_seen(|_| false);
        world.pid_says(printed);
        let id = rt.create_and_start(&spec("r1")).unwrap();
        assert_eq!(world.ids(), [id.0], "{printed:?}: kept");
        assert!(world.calls_of("rm").is_empty(), "{printed:?}");
    }
}

/// Asserts that the agent's own `/proc` is what the runtime looks in by default: the
/// agent's own process is there, and a pid beyond every pid a host gives is not. It catches
/// a default that answers yes or no whatever the pid.
#[test]
fn the_default_looks_in_the_agents_own_proc() {
    assert!(pid_in_proc(std::process::id()));
    assert!(!pid_in_proc(u32::MAX));
}

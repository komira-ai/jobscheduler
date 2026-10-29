//! That the agent sees the host its containers run on: the process the engine reports for a
//! started container must be in the agent's own `/proc`.

use js_agent::cli_runtime::{ProcLink, ProcRead};
#[cfg(target_os = "linux")]
use js_agent::cli_runtime::{pid_in_proc, read_proc, read_proc_link};
use js_agent::lifecycle::ContainerRuntime;

use super::super::scripted::Scripted;
use super::super::{
    docker, in_another_pid_namespace, in_the_agents_pid_namespace, no_file, open, spec,
};
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

/// The calls that ask for the process and state of a container.
fn pid_looks(world: &Scripted) -> usize {
    let asks = |call: &Vec<String>| call.iter().any(|arg| arg.contains("State.Pid"));
    world.calls_of("inspect").iter().filter(|c| asks(c)).count()
}

/// Asserts that a job which ends between the engine's answer and the look at `/proc` does
/// not make the host blind: the first answer is `running` with a process that is gone from
/// `/proc` by the time of the look, the second says the container is `exited`, `stopped` or
/// `dead` (or the engine no longer has it), and the start succeeds with the container kept,
/// after two looks and no removal. It catches a job such as `true`, exited and reaped
/// before the look, being called blind: the container removed and the host tainted for good.
#[test]
fn a_job_that_ends_before_the_look_at_proc_is_not_blind() {
    let ended = [
        Some("4000 running\n"),
        Some("0 exited\n"),
        Some("0 stopped\n"),
        Some("0 dead\n"),
        None,
    ];
    for second in &ended[1..] {
        let world = docker();
        let mut rt = open(&world).with_pid_seen(|_| false);
        world.pid_looks(&[ended[0], *second]);
        if second.is_none() {
            world.say_once("ps", "");
        }
        let id = rt.create_and_start(&spec("r1")).unwrap();
        assert_eq!(world.ids(), [id.0], "{second:?}: kept");
        assert!(world.calls_of("rm").is_empty(), "{second:?}");
        assert_eq!(pid_looks(&world), 2, "{second:?}");
    }
}

/// Asserts that a container that still runs with its process still missing at the second
/// look is blind: the start fails naming the process, the failure is marked as the host
/// not being seen, the container is removed, and the process was asked for exactly twice.
/// It catches a re-look that lets a real blind agent through, and one that loops.
#[test]
fn a_container_that_runs_with_its_process_missing_twice_is_blind() {
    let world = docker();
    let mut rt = open(&world).with_pid_seen(|_| false);
    world.pid_looks(&[Some("4000 running\n"), Some("4001 running\n")]);
    let said = rt.create_and_start(&spec("r1")).unwrap_err();
    assert!(
        said.0.starts_with("the process 4001 of the container"),
        "{}",
        said.0
    );
    assert!(said.is_blind());
    assert!(world.ids().is_empty());
    assert_eq!(pid_looks(&world), 2);
}

/// The control group of an unrelated process.
fn unrelated_cgroup(_path: &str) -> Option<String> {
    Some("0::/user.slice/user-1000.slice/session-3.scope\n".to_owned())
}

/// The control group of the first container of a world.
fn first_containers_cgroup(_path: &str) -> Option<String> {
    Some(format!("0::/system.slice/docker-{:064x}.scope\n", 1))
}

/// Asserts that a process that is in `/proc` is also the container's: with a control group
/// that does not hold the container's id and the agent's own pid namespace, or with
/// namespace links that cannot be read, the process is another's and the start is blind
/// (the container removed); a control group that holds the id, or a pid namespace that is
/// not the agent's, is the container's. It catches the pid of an unrelated process (a pid
/// the engine reported that another process holds now) being taken for the container's.
#[test]
fn a_process_in_proc_must_be_the_containers() {
    for (read, link) in [
        (
            unrelated_cgroup as ProcRead,
            in_the_agents_pid_namespace as ProcLink,
        ),
        (no_file, in_the_agents_pid_namespace),
        (unrelated_cgroup, no_file),
    ] {
        let world = docker();
        let mut rt = open(&world).with_proc_files(read, link);
        let said = rt.create_and_start(&spec("r1")).unwrap_err();
        assert!(said.is_blind(), "{}", said.0);
        assert!(world.ids().is_empty());
    }

    let world = docker();
    let mut rt = open(&world).with_proc_files(first_containers_cgroup, in_the_agents_pid_namespace);
    let id = rt.create_and_start(&spec("r1")).unwrap();
    assert_eq!(world.ids(), [id.0], "the id in its control group");

    let world = docker();
    let mut rt = open(&world).with_proc_files(unrelated_cgroup, in_another_pid_namespace);
    let id = rt.create_and_start(&spec("r1")).unwrap();
    assert_eq!(world.ids(), [id.0], "another pid namespace");
}

/// Asserts the files the runtime reads by default: the agent's own control group is text,
/// its pid namespace link is `pid:[..]`, and a path that does not exist reads as nothing.
/// It catches a default that reads nothing, or the wrong thing.
#[cfg(target_os = "linux")]
#[test]
fn the_default_files_are_the_agents_own_proc() {
    assert!(read_proc("/proc/self/cgroup").is_some());
    assert!(read_proc_link("/proc/self/ns/pid").is_some_and(|link| link.starts_with("pid:[")));
    assert!(read_proc("/proc/no-such-file").is_none());
    assert!(read_proc_link("/proc/no-such-link").is_none());
}

/// Asserts that the agent's own `/proc` is what the runtime looks in by default: the
/// agent's own process is there, and a pid beyond every pid a host gives is not. It catches
/// a default that answers yes or no whatever the pid.
#[cfg(target_os = "linux")]
#[test]
fn the_default_looks_in_the_agents_own_proc() {
    assert!(pid_in_proc(std::process::id()));
    assert!(!pid_in_proc(u32::MAX));
}

//! The engine's environment, the hand-over of the scratch directory, the swap-limit
//! warning and the clock.

use std::cell::RefCell;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use js_agent::cli_runtime::{CliRuntime, hand_over, unix_ms};
use js_agent::lifecycle::Runtime;

use super::super::scripted::{Env, Scripted};
use super::super::{BIN, docker, open, open_with, settings, spec};
use super::{start, strings};
use crate::support::Scratch;

/// The `PATH` every call of the engine is given.
const PINNED_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";

/// Asserts that the engine is run with an environment of its own: every call, the
/// `--version` that opens the runtime included, goes through the runner's environment
/// call with exactly one variable, the pinned `PATH`, and with an engine host also
/// `DOCKER_HOST` and `CONTAINER_HOST` set to it, in that order and nothing else. None
/// goes through the call that passes the agent's environment on. It catches `LD_PRELOAD`,
/// `DOCKER_CONFIG`, `DOCKER_CONTEXT` or a `PATH` of the agent's steering the engine.
#[test]
fn the_engine_runs_with_a_pinned_path_and_nothing_inherited() {
    let exercise = |world: &Scripted, rt: &mut CliRuntime| {
        let id = start(rt, world, "r1");
        rt.poll(&id).unwrap();
        rt.stop(&id, Duration::from_secs(1)).unwrap();
        rt.list_labelled().unwrap();
        rt.remove(&id).unwrap();
    };
    let pinned = |extra: &[(&str, &str)]| -> Env {
        let mut env = vec![("PATH".to_owned(), PINNED_PATH.to_owned())];
        env.extend(
            extra
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned())),
        );
        Some(env)
    };

    let world = docker();
    let mut rt = open(&world);
    exercise(&world, &mut rt);
    let envs = world.envs();
    assert_eq!(envs.len(), world.calls().len());
    assert!(envs.len() > 8);
    assert!(envs.iter().all(|env| *env == pinned(&[])), "{envs:?}");

    let world = docker();
    let uri = "unix:///run/engine/engine.sock";
    let set = settings().with_host(Some(uri)).unwrap();
    let mut rt = open_with(&world, "linux", set);
    exercise(&world, &mut rt);
    let expected = pinned(&[("DOCKER_HOST", uri), ("CONTAINER_HOST", uri)]);
    let envs = world.envs();
    assert!(envs.len() > 8);
    assert!(envs.iter().all(|env| *env == expected), "{envs:?}");
}

/// Asserts that the system runner started with an environment gives the program exactly
/// that environment: the program prints what it received, and nothing of the test
/// process's own environment (`HOME`, `PATH`, `CARGO_*`) is there. It catches a runner
/// that adds the given variables to the inherited ones.
#[test]
fn the_system_runner_gives_a_program_only_the_environment_it_is_given() {
    use js_agent::CommandRunner;
    let runner = js_agent::cli_runtime::system_runner(Duration::from_secs(10));
    let said = runner
        .run_in("/usr/bin/env", &[], &[("PATH", "/p"), ("A", "1")])
        .unwrap();
    let mut lines: Vec<&str> = said.lines().collect();
    lines.sort_unstable();
    assert_eq!(lines, ["A=1", "PATH=/p"]);
    let none = runner.run_in("/usr/bin/env", &[], &[]).unwrap();
    assert_eq!(none, "");
    let inherited = runner.run("/usr/bin/env", &[]).unwrap();
    assert!(
        inherited.contains("PATH="),
        "run still passes the environment on"
    );
}

thread_local! {
    static GIVEN: RefCell<Vec<(PathBuf, u32, u32)>> = const { RefCell::new(Vec::new()) };
}

fn record(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    GIVEN.with(|given| given.borrow_mut().push((path.to_path_buf(), uid, gid)));
    Ok(())
}

fn refuse(_path: &Path, _uid: u32, _gid: u32) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::PermissionDenied))
}

fn given() -> Vec<(PathBuf, u32, u32)> {
    GIVEN.with(|given| given.borrow().clone())
}

/// Asserts that an agent that runs as root gives the run's scratch directory to the
/// container's user before the container is created (the directory is `0700` and the
/// agent's, so the container's user could not write it), and that an agent whose own
/// user is the container's gives nothing away. A directory that cannot be given away is
/// an error before anything is created. It catches the default configuration mounting
/// `/scratch` read-write for a process that cannot write it.
#[test]
fn the_scratch_goes_to_the_containers_user_when_the_agent_is_root() {
    let world = docker();
    let root = js_agent::cli_runtime::Settings::new(Path::new(BIN), None, Some("2000:3000"))
        .unwrap()
        .with_agent(0, 0)
        .unwrap();
    let mut rt = open_with(&world, "linux", root.clone()).with_chown(record);
    rt.create_and_start(&spec("r1")).unwrap();
    assert_eq!(given(), [(PathBuf::from("/work/scratch/r1"), 2000, 3000)]);

    let own = root.with_agent(2000, 3000).unwrap();
    let mut rt = open_with(&world, "linux", own).with_chown(record);
    rt.create_and_start(&spec("r2")).unwrap();
    assert_eq!(given().len(), 1, "nothing was given away");

    let root = settings().with_agent(0, 0).unwrap();
    let mut rt = open_with(&world, "linux", root).with_chown(refuse);
    let creates = world.calls_of("create").len();
    let said = rt.create_and_start(&spec("r3")).unwrap_err();
    assert_eq!(said.0, "scratch: permission denied");
    assert_eq!(
        world.calls_of("create").len(),
        creates,
        "nothing was created"
    );
}

/// Asserts that the real hand-over works on a directory (to the owner it already has,
/// which any user may do), does not follow a link, and fails for a path that is not there.
#[test]
fn hand_over_changes_the_owner_of_the_directory_itself() {
    let dir = Scratch::new("cli-hand-over");
    let (uid, gid) = (
        rustix::process::geteuid().as_raw(),
        rustix::process::getegid().as_raw(),
    );
    let run = dir.path("run");
    std::fs::create_dir(&run).unwrap();
    hand_over(&run, uid, gid).unwrap();
    let link = dir.path("link");
    std::os::unix::fs::symlink(&run, &link).unwrap();
    hand_over(&link, uid, gid).unwrap();
    assert!(hand_over(&dir.path("absent"), uid, gid).is_err());
    // A link to nothing has an owner of its own to change; following it would fail.
    let dangling = dir.path("dangling");
    std::os::unix::fs::symlink(dir.path("nothing-here"), &dangling).unwrap();
    hand_over(&dangling, uid, gid).unwrap();
}

/// Asserts that a Docker engine that reports no swap limit support is warned about when
/// the runtime opens (`--memory-swap` is then ignored and a job can use swap beyond its
/// memory limit), and that nothing is said when it supports them, when the answer cannot
/// be had, or for Podman, whose `info` does not say. It catches a limit the operator
/// believes in that the host does not enforce.
#[test]
fn an_engine_without_swap_limits_is_warned_about() {
    let world = docker();
    world.say("info", "false\n");
    let rt = open(&world);
    assert_eq!(
        rt.warnings(),
        [
            "the engine reports no swap limit support: --memory-swap is ignored, so a job can use swap beyond its memory limit"
        ]
    );
    assert_eq!(
        world.calls_of("info"),
        [strings(&["info", "--format", "{{.SwapLimit}}"])]
    );
    for printed in ["true\n", "weird\n"] {
        let world = docker();
        world.say("info", printed);
        assert!(open(&world).warnings().is_empty(), "{printed:?}");
    }
    let world = docker();
    world.fail("info", 1);
    assert!(open(&world).warnings().is_empty());
    let podman = Scripted::new("podman version 5.2.1");
    assert!(
        open_with(&podman, "linux", settings())
            .warnings()
            .is_empty()
    );
    assert!(podman.calls_of("info").is_empty());
}

/// The wall clock the program stamps containers with reads a plausible time, and the
/// runner it gives the runtime is the bounded system runner.
#[test]
fn the_system_clock_and_runner_are_usable() {
    assert!(
        unix_ms() > 1_700_000_000_000,
        "milliseconds since the epoch"
    );
    let runner = js_agent::cli_runtime::system_runner(Duration::from_secs(10));
    let said = js_agent::CommandRunner::run(&runner, "/bin/sh", &["-c", "printf ok"]).unwrap();
    assert_eq!(said, "ok");
}

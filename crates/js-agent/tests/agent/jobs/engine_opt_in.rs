//! The container engine is a fallback the operator opts in to: its flags run only with
//! `--allow-container-engine`, the opt-in runs only with `--container-bin`, an engine flag
//! without the opt-in is a crun start that refuses it, and an agent on the engine says once, at
//! start, that its runtime is a fallback whose jobs get no network.

use std::sync::Arc;

use js_agent::platform::{ENGINE_WARNING, NOT_READY};
const ENGINE_NEEDS_OPT_IN: &str = "--container-bin: needs --allow-container-engine (a flag of the container engine; without it this agent runs crun)";

use super::drain::container_flags;
use super::fake::FakeMachine;
use crate::crun_default::start_on;
use crate::crun_host::Fixture;
use crate::support::{
    CountingSleeper, Lowest, Ran, Scratch, Script, ScriptConnector, code, flags, linux_host,
    refused, run_defaulted, run_on,
};

/// Runs the whole agent with `args` until the server refuses its hello for good; returns what
/// it printed, how many messages it sent, and the calls the fake engine recorded.
fn engine_start(args: &[String]) -> (Ran, usize, Vec<String>) {
    let all: Vec<&str> = args.iter().map(String::as_str).collect();
    let (script, sent) = Script::new(vec![refused(403, "host-bound")]);
    let connector = ScriptConnector::of(script);
    let mut sleeper = CountingSleeper::stopping_at(1);
    let machine = Arc::new(FakeMachine::new());
    let world = machine.world.clone();
    let ran = run_on(
        &flags(&all),
        &linux_host(),
        &connector,
        &mut sleeper,
        &mut Lowest,
        (machine, None),
    );
    let sent = sent.borrow().len();
    (ran, sent, world.log())
}

/// `owned` less each word in `drop` (a flag without a value).
fn less(owned: &[String], drop: &[&str]) -> Vec<String> {
    owned
        .iter()
        .filter(|word| !drop.contains(&word.as_str()))
        .cloned()
        .collect()
}

/// Proves the starts of the opt-in, each on a Linux host whose engine program is a fake:
/// `--container-bin <fake>` without `--allow-container-engine` is a crun start that exits 2
/// naming `--container-bin` and the opt-in, with nothing sent and the engine never called; the
/// same with the opt-in starts and says hello; `--allow-container-engine` without
/// `--container-bin` exits 2 naming `--container-bin` (given alone, and through the default
/// start); and `--allow-container-engine` with `--no-jobs` exits 2 naming `--no-jobs`. It
/// catches any of these accepted, and an engine that runs jobs without the opt-in.
#[test]
fn docker_without_the_opt_in_flag_is_refused() {
    let dir = Scratch::new("engine-opt-in");
    let owned = container_flags(&dir);
    assert!(owned.iter().any(|word| word == "--allow-container-engine"));

    let without = less(&owned, &["--allow-container-engine"]);
    assert!(without.iter().any(|word| word == "--container-bin"));
    let (ran, sent, calls) = engine_start(&without);
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert_eq!(
        calls,
        Vec::<String>::new(),
        "the engine was called: {}",
        ran.err
    );
    assert_eq!(sent, 0, "{}", ran.err);
    assert!(
        ran.err
            .contains(&format!("js-agent: {ENGINE_NEEDS_OPT_IN}\n")),
        "{}",
        ran.err
    );
    assert!(ran.err.contains(NOT_READY), "{}", ran.err);
    assert_eq!(
        ENGINE_NEEDS_OPT_IN,
        "--container-bin: needs --allow-container-engine (a flag of the container engine; \
         without it this agent runs crun)"
    );

    let (ran, sent, _) = engine_start(&owned);
    assert_eq!(sent, 1, "the engine agent says hello: {}", ran.err);

    let alone = "js-agent: --allow-container-engine: needs --container-bin\n";
    let given = vec!["--allow-container-engine".to_owned()];
    let (ran, sent, _) = engine_start(&given);
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert_eq!(sent, 0, "{}", ran.err);
    assert!(ran.err.ends_with(alone), "{}", ran.err);
    let mut sleeper = CountingSleeper::stopping_at(1);
    let (script, _) = Script::new(vec![refused(403, "host-bound")]);
    let bare = run_defaulted(
        &flags(&["--allow-container-engine"]),
        &linux_host(),
        &ScriptConnector::of(script),
        &mut sleeper,
        &mut Lowest,
    );
    assert_eq!(bare.status(), code(2), "{}", bare.err);
    assert!(bare.err.ends_with(alone), "{}", bare.err);

    let given = vec![
        "--no-jobs".to_owned(),
        "--allow-container-engine".to_owned(),
    ];
    let (ran, sent, _) = engine_start(&given);
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert_eq!(sent, 0, "{}", ran.err);
    assert!(
        ran.err.ends_with(
            "js-agent: --allow-container-engine: not with --no-jobs (an agent that runs no job)\n"
        ),
        "{}",
        ran.err
    );
}

/// Proves that an agent started on the engine with the opt-in writes the fallback warning once
/// to its error stream, as one line, and that an agent on crun (the default, on a host that
/// passes the crun check) writes none and says nothing of the engine. It catches a warning that
/// is missing, printed twice, or printed for crun.
#[test]
fn the_engine_runtime_warns_that_it_is_a_fallback() {
    let dir = Scratch::new("engine-warning");
    let (ran, sent, _) = engine_start(&container_flags(&dir));
    assert_eq!(sent, 1, "{}", ran.err);
    let line = format!("js-agent: warning: {ENGINE_WARNING}");
    let lines: Vec<&str> = ran
        .err
        .lines()
        .filter(|l| l.contains(ENGINE_WARNING))
        .collect();
    assert_eq!(lines, [line.as_str()], "{}", ran.err);
    assert!(ENGINE_WARNING.contains("network none"), "{ENGINE_WARNING}");
    assert!(ENGINE_WARNING.contains("fallback"), "{ENGINE_WARNING}");

    let (uid, gid) = (
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw(),
    );
    let fx = Fixture::with_ids("engine-warning-crun", uid, gid);
    let crun = start_on(&fx, &[]);
    assert_eq!(crun.sent, 1, "the crun agent says hello: {}", crun.ran.err);
    assert!(!crun.ran.err.contains(ENGINE_WARNING), "{}", crun.ran.err);
    assert!(
        !crun.ran.err.contains("container engine"),
        "{}",
        crun.ran.err
    );
}

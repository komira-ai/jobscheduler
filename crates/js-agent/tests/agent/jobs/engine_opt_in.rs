//! The container engine is a fallback the operator opts in to: `--runtime docker` runs only with
//! `--allow-container-engine`, the opt-in runs only with `--runtime docker`, an engine flag with
//! no `--runtime` is a crun start that refuses it, and an agent on the engine says once, at
//! start, that its runtime is a fallback whose jobs get no network.

use std::sync::Arc;

use js_agent::{ENGINE_NEEDS_OPT_IN, ENGINE_WARNING};

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

/// Proves the four starts of the opt-in, each on a Linux host whose engine program is a fake:
/// `--runtime docker --container-bin <fake>` without `--allow-container-engine` exits 2 naming
/// the opt-in, with nothing sent; the same with it starts and says hello;
/// `--allow-container-engine` without `--runtime docker` exits 2 naming `--runtime docker`
/// (with no `--runtime`, with `none` and with `crun`); and `--container-bin` with no `--runtime`
/// exits 2 naming `--container-bin` and the opt-in, which crun, the default, does not take. It
/// catches any of the four accepted, and an engine that runs jobs without the opt-in.
#[test]
fn docker_without_the_opt_in_flag_is_refused() {
    let dir = Scratch::new("engine-opt-in");
    let owned = container_flags(&dir);
    assert!(owned.iter().any(|word| word == "--allow-container-engine"));

    let (ran, sent, _) = engine_start(&less(&owned, &["--allow-container-engine"]));
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert_eq!(sent, 0, "{}", ran.err);
    assert!(
        ran.err
            .ends_with(&format!("js-agent: {ENGINE_NEEDS_OPT_IN}\n")),
        "{}",
        ran.err
    );
    assert_eq!(
        ENGINE_NEEDS_OPT_IN,
        "--runtime docker: needs --allow-container-engine (crun is the default Linux runtime; \
         the engine is a fallback for a host that cannot run crun and for GPU jobs)"
    );

    let (ran, sent, _) = engine_start(&owned);
    assert_eq!(sent, 1, "the engine agent says hello: {}", ran.err);

    let alone = "js-agent: --allow-container-engine: needs --runtime docker\n";
    for given in [
        &["--allow-container-engine"][..],
        &["--runtime", "none", "--allow-container-engine"],
        &["--runtime", "crun", "--allow-container-engine"],
    ] {
        let given: Vec<String> = given.iter().map(|word| (*word).to_owned()).collect();
        let (ran, sent, _) = engine_start(&given);
        assert_eq!(ran.status(), code(2), "{given:?}: {}", ran.err);
        assert_eq!(sent, 0, "{given:?}: {}", ran.err);
        assert!(ran.err.ends_with(alone), "{given:?}: {}", ran.err);
    }
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

    let defaulted = less(&owned, &["--runtime", "docker", "--allow-container-engine"]);
    assert!(defaulted.iter().any(|word| word == "--container-bin"));
    let (ran, sent, calls) = engine_start(&defaulted);
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
            .contains("js-agent: --container-bin: not accepted with --runtime crun"),
        "{}",
        ran.err
    );
    assert!(
        ran.err
            .contains("--runtime docker --allow-container-engine selects the engine"),
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

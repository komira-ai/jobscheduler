//! `--warm-sandboxes` belongs to the crun runtime: a container engine agent and an agent that
//! runs no job refuse it by name. (Linux only: only Linux runs containers.)

use clap::Parser as _;
use js_agent::Cli;

use crate::support::{
    CountingSleeper, Lowest, Script, ScriptConnector, code, flags, linux_host, run_defaulted,
    welcome,
};

/// Runs the agent with `extra` exactly as given; returns (status, stderr, whether it sent
/// anything).
fn run_agent(extra: &[&str]) -> (String, String, bool) {
    let (script, sent) = Script::new(vec![welcome(1)]);
    let mut sleeper = CountingSleeper::stopping_at(1);
    let ran = run_defaulted(
        &flags(extra),
        &linux_host(),
        &ScriptConnector::of(script),
        &mut sleeper,
        &mut Lowest,
    );
    let sent = !sent.borrow().is_empty();
    (ran.status(), ran.err, sent)
}

/// Proves the flag, given with the container engine or with no jobs, stops the agent with
/// status 2 before anything is sent, naming the flag and the reason, and that the default is
/// 2 on the command line. Catches a crun flag accepted and ignored by a runtime that has no
/// warm sandboxes.
#[test]
fn warm_sandboxes_are_refused_with_the_container_engine_and_with_no_jobs() {
    let engine = [
        "--container-bin",
        "/usr/bin/docker",
        "--allow-container-engine",
        "--warm-sandboxes",
        "0",
    ];
    let (status, err, sent) = run_agent(&engine);
    assert_eq!(status, code(2), "{err}");
    assert!(
        err.contains(
            "js-agent: --warm-sandboxes: not with --allow-container-engine (a flag of the crun runtime)"
        ),
        "{err}"
    );
    assert!(!sent);
    let (status, err, sent) = run_agent(&["--no-jobs", "--warm-sandboxes", "0"]);
    assert_eq!(status, code(2), "{err}");
    assert!(
        err.contains("js-agent: --warm-sandboxes: not with --no-jobs (an agent that runs no job)"),
        "{err}"
    );
    assert!(!sent);
    let cli =
        Cli::try_parse_from(["js-agent", "--server", "http://127.0.0.1:1", "--pool", "p"]).unwrap();
    assert_eq!(cli.os.warm_sandboxes, 2);
}

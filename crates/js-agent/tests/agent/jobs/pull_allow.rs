//! `--container-pull-allow` at start-up: a bad entry ends the agent before it registers,
//! the flag needs containers, and `--container-pull missing` without an entry warns.

use super::drain::container_flags;
use super::fake::FakeMachine;
use super::{STEP_NEVER, plan, settings};
use crate::support::{Scratch, code, linux_host, run_scripted, welcome};
use js_agent::Worker;

/// Runs the agent with `owned` and returns (status, stderr, hellos sent).
fn start(owned: &[String]) -> (String, String, usize) {
    let all: Vec<&str> = owned.iter().map(String::as_str).collect();
    let (ran, sent, _) = run_scripted(&all, &linux_host(), vec![welcome(1)], 1);
    let sent = sent.borrow().len();
    (ran.status(), ran.err, sent)
}

/// Asserts that a bad `--container-pull-allow` entry (a bare host, a scheme, an upper-case
/// path) ends the agent with status 2, names the flag and the entry on stderr, and sends
/// nothing. It catches an entry that is only found wrong when the first job arrives, and
/// one that is dropped silently so that the list is wider than the operator wrote.
#[test]
fn a_bad_pull_allow_entry_ends_the_agent_before_it_registers() {
    let dir = Scratch::new("pull-allow-flags-bad");
    for bad in [
        "ghcr.io",
        "https://ghcr.io/komira-ai/",
        "ghcr.io/Komira-AI/",
    ] {
        let mut owned = container_flags(&dir);
        owned.extend(["--container-pull-allow".to_owned(), bad.to_owned()]);
        let (status, err, sent) = start(&owned);
        assert_eq!(status, code(2), "{bad}: {err}");
        assert!(err.contains("--container-pull-allow"), "{err}");
        assert!(err.contains(bad), "{err}");
        assert_eq!(sent, 0, "{bad}");
    }
}

/// Asserts that `--container-pull-allow` without `--container-bin` ends the agent with
/// status 2 like the other container flags. It catches a list that reads as configured
/// while no job can ever run.
#[test]
fn a_pull_allow_entry_needs_containers() {
    let owned = vec![
        "--container-pull-allow".to_owned(),
        "ghcr.io/komira-ai/".to_owned(),
    ];
    let (status, err, sent) = start(&owned);
    assert_eq!(status, code(2), "{err}");
    assert!(err.contains("container"), "{err}");
    assert_eq!(sent, 0);
}

/// Asserts that the flag is repeatable and that the agent starts with good entries given
/// more than once: the clap definition collects them. It catches a flag that keeps only
/// the last value.
#[test]
fn the_flag_collects_every_entry() {
    use clap::Parser;
    let cli = js_agent::Cli::try_parse_from([
        "js-agent",
        "--server",
        "http://127.0.0.1:1",
        "--token-file",
        "/nonexistent/token",
        "--pool",
        "burst",
        "--container-pull-allow",
        "ghcr.io/komira-ai/",
        "--container-pull-allow",
        "registry.example:5000/team/",
    ])
    .unwrap();
    assert_eq!(
        cli.container_pull_allow,
        ["ghcr.io/komira-ai/", "registry.example:5000/team/"]
    );
}

/// The lines the worker says at start for `settings`.
fn lines_with(name: &str, pull: &str, allow: &[&str]) -> Vec<String> {
    let dir = Scratch::new(name);
    let machine = FakeMachine::new();
    let allow: Vec<String> = allow.iter().map(|entry| (*entry).to_owned()).collect();
    let mut plan = plan(&dir, &machine, STEP_NEVER);
    plan.settings = settings(&dir)
        .with_pull(Some(pull))
        .unwrap()
        .with_pull_allow(&allow)
        .unwrap();
    plan.config.pull_allow = plan.settings.pull_allow().clone();
    let worker = match Worker::start(plan) {
        Ok(worker) => worker,
        Err(why) => panic!("the worker did not start: {why}"),
    };
    worker.take_lines()
}

const WARNING: &str = "pull: --container-pull missing without --container-pull-allow: the engine may pull a job's image from any registry the host can reach";

/// Asserts that `--container-pull missing` with no entry says at start, once, that any
/// reachable registry may be pulled; and that it says nothing with an entry, or with
/// `never`, with or without an entry. It catches a silent widening of what the host
/// fetches, and a warning that cries wolf.
#[test]
fn missing_without_an_allow_list_warns_and_nothing_else_does() {
    let said = lines_with("pull-allow-warn", "missing", &[]);
    assert_eq!(
        said.iter().filter(|l| l.as_str() == WARNING).count(),
        1,
        "{said:?}"
    );
    for (pull, allow) in [
        ("missing", &["ghcr.io/komira-ai/"][..]),
        ("never", &[][..]),
        ("never", &["ghcr.io/komira-ai/"][..]),
    ] {
        let said = lines_with("pull-allow-quiet", pull, allow);
        assert!(
            said.iter().all(|line| !line.contains("pull:")),
            "{pull} {allow:?}: {said:?}"
        );
    }
}

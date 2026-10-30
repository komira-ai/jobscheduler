//! The whole agent and `--actions-cgroup`: the flag turns the load of the heartbeat on.

use std::sync::Arc;

use serde_json::Value;

use super::drain::container_flags;
use super::fake::FakeMachine;
use crate::support::{
    CountingSleeper, Jobs, Lowest, Scratch, Script, ScriptConnector, beat_ok, flags, linux_host,
    run_on, welcome,
};

const SLICE: &str = "/sys/fs/cgroup/jobs.slice";

/// The first heartbeat of an agent run over containers with `extra` flags on a host that
/// has the cgroup files of [`SLICE`], and the text the agent printed.
fn first_heartbeat(name: &str, extra: &[&str]) -> (Value, String) {
    let dir = Scratch::new(name);
    let mut owned = container_flags(&dir);
    owned.extend(extra.iter().map(|flag| (*flag).to_owned()));
    let all: Vec<&str> = owned.iter().map(String::as_str).collect();
    let args = flags(&all);
    let mut host = linux_host();
    host.files = host
        .files
        .with(&format!("{SLICE}/cpu.stat"), "usage_usec 5000000\n")
        .with(&format!("{SLICE}/memory.current"), "8000000000\n")
        .with(
            &format!("{SLICE}/memory.stat"),
            "inactive_file 1000000000\nactive_file 250\n",
        )
        .with(&format!("{SLICE}/cpu.max"), "max 100000\n");
    let replies = vec![welcome(1), beat_ok(), beat_ok(), beat_ok(), beat_ok()];
    let (script, sent) = Script::new(replies);
    let connector = ScriptConnector::of(script);
    let mut sleeper = CountingSleeper::stopping_at(2);
    let jobs: Jobs<'_> = (Arc::new(FakeMachine::new()), None);
    let ran = run_on(&args, &host, &connector, &mut sleeper, &mut Lowest, jobs);
    let beat = sent.borrow()[1].1.clone();
    (beat, ran.err)
}

/// Asserts that an agent started with `--actions-cgroup` reads that directory and sends its
/// load in the first heartbeat, and that one started without the flag sends none. It
/// catches a flag that is checked but never wired to the heartbeat.
#[test]
fn the_flag_turns_the_load_of_the_heartbeat_on() {
    let (with, _) = first_heartbeat("actions-cgroup-on", &["--actions-cgroup", SLICE]);
    assert_eq!(with["load"]["working_set_bytes"], 7_000_000_000u64);
    assert_eq!(with["load"]["cpu_usage_usec"], 5_000_000);
    let (without, _) = first_heartbeat("actions-cgroup-off", &[]);
    assert!(without.get("load").is_none());
}

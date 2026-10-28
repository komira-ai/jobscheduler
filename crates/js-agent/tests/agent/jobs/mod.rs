//! The job half of the agent wired to the session: the worker thread and its snapshot, the
//! session carrying what the worker holds, the drain, and the whole path against a real
//! lease server in the test process. The container runtime is in memory and the clock is
//! moved by the test; the worker is a real thread, and every wait on it is for an event
//! (an order done, a call begun) with a hang guard, never for a time.

mod drain;
mod e2e;
mod ends;
mod exclusive;
pub mod fake;
mod session;
mod system;
mod worker;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

use js_agent::cli_runtime::Settings;
use js_agent::lifecycle::{Capacity, Config, MountRoots, ScratchRoot, StateRoot};
use js_agent::{Plan, Platform, Rescue, Worker};
use js_agent_wire::{Command, HeartbeatReply};
use serde_json::{Value, json};

use crate::lifecycle::job;
use crate::support::Scratch;
use fake::FakeMachine;

/// A step interval no test waits for: the worker steps when an order or a settle asks.
pub const STEP_NEVER: Duration = Duration::from_secs(3600);

/// The user containers run as, and the agent's own `(uid, gid)`.
pub fn user() -> (String, u32, u32) {
    let uid = rustix::process::geteuid().as_raw();
    let gid = rustix::process::getegid().as_raw();
    match uid {
        0 => ("1:1".to_owned(), uid, gid),
        _ => (format!("{uid}:{gid}"), uid, gid),
    }
}

/// A private directory `name` under `dir`, made when it is not there.
pub fn private(dir: &Scratch, name: &str) -> std::path::PathBuf {
    let path = dir.path(name);
    fs::create_dir_all(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

/// The checked container flags: a program that is never run, the agent's own user.
pub fn settings(dir: &Scratch) -> Settings {
    let exec = dir.write("docker", "#!/bin/sh\n", 0o755);
    let (user, uid, gid) = user();
    let settings = Settings::new(&exec, None, Some(&user)).unwrap();
    settings.with_agent(uid, gid).unwrap()
}

/// What a worker is made from, under `dir`: eight CPUs and 16 GiB, a scratch root and a
/// state root of their own, and `machine`.
pub fn plan(dir: &Scratch, machine: &FakeMachine, step_every: Duration) -> Plan {
    let config = Config {
        capacity: Capacity {
            cpu_millis: 8000,
            memory_bytes: 1 << 34,
            gpus: Default::default(),
        },
        allow_host_jobs: false,
        mounts: MountRoots::new(&[]).unwrap(),
        scratch: ScratchRoot::open(&private(dir, "scratch")).unwrap(),
        state: StateRoot::open(&private(dir, "state")).unwrap(),
        stop_grace: Duration::from_secs(1),
    };
    Plan {
        config,
        settings: settings(dir),
        host_id: "h1".to_owned(),
        platform: Platform {
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
        },
        watch: Vec::new(),
        machine: Arc::new(machine.clone()),
        step_every,
        rescue_bound: Some(crate::support::RESCUE_TEST_BOUND),
    }
}

/// How the agent stops its containers without a worker, over `machine`.
pub fn rescue(dir: &Scratch, machine: &FakeMachine) -> Rescue {
    let platform = Platform {
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
    };
    let machine = Arc::new(machine.clone());
    let grace = Duration::from_secs(1);
    Rescue::new(machine, settings(dir), "h1".to_owned(), platform, grace)
}

/// A worker over `machine` that steps only when asked.
pub fn start(dir: &Scratch, machine: &FakeMachine) -> Worker {
    match Worker::start(plan(dir, machine, STEP_NEVER)) {
        Ok(worker) => worker,
        Err(why) => panic!("the worker did not start: {why}"),
    }
}

/// A reply that starts these runs.
pub fn starts(runs: &[&str]) -> HeartbeatReply {
    let commands = runs
        .iter()
        .map(|run| Command::Start(Box::new(job(run))))
        .collect();
    HeartbeatReply {
        commands,
        ..HeartbeatReply::default()
    }
}

/// A reply that stops this run.
pub fn stops(run: &str) -> HeartbeatReply {
    HeartbeatReply {
        commands: vec![Command::Stop {
            run_id: run.to_owned(),
        }],
        ..HeartbeatReply::default()
    }
}

/// A reply that acknowledges this run's result.
pub fn acks(run: &str, verdict: &str) -> HeartbeatReply {
    let body = json!({ "commands": [], "result_acks": [{ "run_id": run, "ack": verdict }] });
    serde_json::from_value(body).unwrap()
}

/// The JSON body of a heartbeat answer that starts these runs.
pub fn start_answer(runs: &[&str]) -> Value {
    serde_json::to_value(starts(runs)).unwrap()
}

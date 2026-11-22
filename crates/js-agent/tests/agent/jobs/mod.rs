//! The job half of the agent wired to the session: the worker thread and its snapshot, the
//! session carrying what the worker holds, the drain, and the whole path against a real
//! lease server in the test process. The container runtime is in memory and the clock is
//! moved by the test; the worker is a real thread, and every wait on it is for an event
//! (an order done, a call begun) with a hang guard, never for a time.
//!
//! The container runtime is the Linux build's alone (crun, or the container engine it falls
//! back to with `--allow-container-engine`), so the modules that run jobs through it are
//! compiled on Linux only; the macOS build runs native jobs, whose worker is tested in
//! `native`.

mod answers;
#[cfg(target_os = "linux")]
mod bell;
#[cfg(target_os = "linux")]
mod contract;
#[cfg(target_os = "linux")]
mod contract_e2e;
#[cfg(target_os = "linux")]
mod crun_budget;
#[cfg(target_os = "linux")]
mod crun_e2e;
#[cfg(target_os = "linux")]
mod crun_egress;
#[cfg(target_os = "linux")]
mod crun_gpu_hello;
#[cfg(target_os = "linux")]
mod crun_host_ports;
#[cfg(target_os = "linux")]
mod crun_logs;
#[cfg(target_os = "linux")]
mod crun_machine;
#[cfg(target_os = "linux")]
mod crun_roots;
#[cfg(target_os = "linux")]
mod crun_tmp;
#[cfg(target_os = "linux")]
mod dispatch;
#[cfg(target_os = "linux")]
mod drain;
#[cfg(target_os = "linux")]
mod e2e;
#[cfg(target_os = "linux")]
mod ends;
#[cfg(target_os = "linux")]
mod engine_opt_in;
#[cfg(target_os = "linux")]
mod exclusive;
pub mod fake;
#[cfg(target_os = "linux")]
mod files_flags;
#[cfg(target_os = "linux")]
mod handover;
#[cfg(target_os = "linux")]
mod handover_egress;
#[cfg(target_os = "linux")]
mod hold_e2e;
#[cfg(target_os = "linux")]
mod per_job;
#[cfg(target_os = "linux")]
mod providers;
#[cfg(target_os = "linux")]
mod quiesce;
#[cfg(target_os = "linux")]
mod quota_start;
#[cfg(target_os = "linux")]
mod session;
#[cfg(target_os = "linux")]
mod silence;
mod system;
#[cfg(target_os = "linux")]
mod taint_health;
#[cfg(target_os = "linux")]
mod unmounted;
#[cfg(target_os = "linux")]
mod usage;
#[cfg(target_os = "linux")]
mod worker;

use std::fs;
use std::os::unix::fs::PermissionsExt;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

#[cfg(target_os = "linux")]
use js_agent::Rescue;
#[cfg(target_os = "linux")]
use js_agent::Worker;
#[cfg(target_os = "linux")]
use js_agent::cli_runtime::Settings;
use js_agent::lifecycle::{
    Capacity, Config, FilesRoot, MountProviders, MountRoots, ScratchRoot, StateRoot,
};
#[cfg(target_os = "linux")]
use js_agent::usage::Outbox;
use js_agent::{Backend, Plan, Platform};
#[cfg(target_os = "linux")]
use js_agent_wire::Command;
use js_agent_wire::HeartbeatReply;
#[cfg(target_os = "linux")]
use serde_json::Value;
use serde_json::json;

#[cfg(target_os = "linux")]
use crate::lifecycle::job;
use crate::support::Scratch;
use fake::FakeMachine;

/// A step interval no test waits for: the worker steps when an order or a settle asks.
pub const STEP_NEVER: Duration = Duration::from_secs(3600);

/// The user containers run as, and the agent's own `(uid, gid)`.
#[cfg(target_os = "linux")]
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
#[cfg(target_os = "linux")]
pub fn settings(dir: &Scratch) -> Settings {
    let exec = dir.write("docker", "#!/bin/sh\n", 0o755);
    let (user, uid, gid) = user();
    let settings = Settings::new(&exec, Some(&user)).unwrap();
    settings.with_agent(uid, gid).unwrap()
}

/// The checked container flags with `--actions-cgroup` set to `actions`.
#[cfg(target_os = "linux")]
pub fn settings_in(dir: &Scratch, actions: Option<&str>) -> Settings {
    let actions = actions.map(Path::new);
    settings(dir).with_actions_cgroup(actions).unwrap()
}

/// What the jobs run with, under `dir`: the container flags of a program that is never run.
#[cfg(target_os = "linux")]
pub fn backend(dir: &Scratch) -> Backend {
    Backend::Container(settings(dir))
}

/// What the jobs run with, under `dir`: the native settings of an account pool in `dir`
/// (which no test opens: the settings hold no Full Disk Access, so the runtime opens no pool).
#[cfg(target_os = "macos")]
pub fn backend(dir: &Scratch) -> Backend {
    let accounts = private(dir, "accounts");
    let flags = js_agent::native::NativeFlags {
        accounts: Some(accounts.as_path()),
        account_range: Some("600-609"),
        account_ready: None,
        homes: None,
        network: None,
        tool_roots: &[],
        deny_read: &[],
        max_file_mib: None,
        max_processes: None,
        pf: js_agent::native::PfFlags::default(),
    };
    let journal = dir.path("native-leases");
    let settings = js_agent::native::NativeSettings::from_flags(&flags, &[], &journal);
    Backend::Native(settings.unwrap().unwrap())
}

/// What a worker is made from, under `dir`: eight CPUs and 16 GiB, a scratch root and a
/// state root of their own, and `machine`.
pub fn plan(dir: &Scratch, machine: &FakeMachine, step_every: Duration) -> Plan {
    let config = Config {
        capacity: Capacity {
            cpu_millis: 8000,
            memory_bytes: 1 << 34,
            disk_bytes: 0,
            gpus: Default::default(),
        },
        allow_host_jobs: false,
        allow_fuse_jobs: false,
        mounts: MountRoots::new(&[]).unwrap(),
        scratch: ScratchRoot::open(&private(dir, "scratch")).unwrap(),
        files: Some(FilesRoot::open(&private(dir, "files")).unwrap()),
        state: StateRoot::open(&private(dir, "state")).unwrap(),
        stop_grace: Duration::from_secs(1),
        bundle: None,
        quota: None,
    };
    Plan {
        config,
        backend: backend(dir),
        host_id: "h1".to_owned(),
        providers: MountProviders::default(),
        platform: Platform {
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
        },
        watch: Vec::new(),
        settle: js_agent::lifecycle::DEFAULT_SETTLE,
        taint_recheck: js_agent::lifecycle::DEFAULT_TAINT_RECHECK,
        machine: Arc::new(machine.clone()),
        step_every,
        usage: None,
        rescue_bound: Some(crate::support::RESCUE_TEST_BOUND),
        readiness: None,
        silence_stop: None,
        vm: None,
        cleanup_threads: 0,
    }
}

#[cfg(target_os = "linux")]
/// A worker over `machine` that steps only when asked and publishes its usage in `outbox`,
/// with `actions` as the actions control group.
pub fn start_with_usage(
    dir: &Scratch,
    machine: &FakeMachine,
    outbox: &Arc<Outbox>,
    actions: Option<&str>,
) -> Worker {
    let mut plan = plan(dir, machine, STEP_NEVER);
    plan.backend = Backend::Container(settings_in(dir, actions));
    plan.usage = Some(outbox.clone());
    match Worker::start(plan) {
        Ok(worker) => worker,
        Err(why) => panic!("the worker did not start: {why}"),
    }
}

/// How the agent stops its containers without a worker, over `machine`.
#[cfg(target_os = "linux")]
pub fn rescue(dir: &Scratch, machine: &FakeMachine) -> Rescue {
    let platform = Platform {
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
    };
    let machine = Arc::new(machine.clone());
    let grace = Duration::from_secs(1);
    Rescue::new(machine, backend(dir), "h1".to_owned(), platform, grace)
}

/// A worker over `machine` that steps only when asked.
#[cfg(target_os = "linux")]
pub fn start(dir: &Scratch, machine: &FakeMachine) -> Worker {
    match Worker::start(plan(dir, machine, STEP_NEVER)) {
        Ok(worker) => worker,
        Err(why) => panic!("the worker did not start: {why}"),
    }
}

/// A reply that starts these runs.
#[cfg(target_os = "linux")]
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
#[cfg(target_os = "linux")]
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
#[cfg(target_os = "linux")]
pub fn start_answer(runs: &[&str]) -> Value {
    serde_json::to_value(starts(runs)).unwrap()
}

//! `CrunRuntime` under the lifecycle: the runtime puts every container in a control group
//! the agent can read, so a running run whose group the agent cannot name leaves the host
//! unchecked.

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::PermissionsExt as _;

use js_agent::lifecycle::taint::reason;
use js_agent::lifecycle::{
    Capacity, CheckFinding, CheckHook, Clock, Config, Finished, Lifecycle, MountRoots, Prepared,
    Runtime, ScratchRoot, StateRoot, code,
};
use js_agent_wire::{Command, HeartbeatReply, Isolation, JobSpec, Limits, Taint};

use crate::node::{LONG, Node};

struct Pass;

impl CheckHook for Pass {
    fn run(&mut self, _job: &Finished<'_>) -> Result<(), CheckFinding> {
        Ok(())
    }
}

struct Still;

impl Clock for Still {
    fn now_ms(&self) -> u64 {
        0
    }
}

fn private(node: &Node, name: &str) -> std::path::PathBuf {
    let path = node.dir.join(name);
    std::fs::create_dir_all(&path).expect("a directory");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).expect("private");
    path
}

fn job(node: &Node, run_id: &str) -> JobSpec {
    JobSpec {
        run_id: run_id.to_owned(),
        lease_id: format!("lease-{run_id}"),
        image: node.image(),
        args: vec!["--go".to_owned()],
        limits: Limits {
            cpu_millis: 1000,
            memory_bytes: 1 << 30,
            disk_bytes: None,
            pids: Some(100),
            wall_ms: 60_000,
        },
        gpu_indices: Vec::new(),
        mounts: Vec::new(),
        isolation: Isolation::Contained,
        labels: BTreeMap::new(),
        heartbeat_seq: 0,
        files: Vec::new(),
        valid_for_ms: 0,
        bundle: None,
        native: None,
    }
}

/// Proves the wiring of the blind-runtime taint to `CrunRuntime`: a run that holds (it never
/// ends by itself) whose container cgroup the stand-in never makes is seen by the lifecycle
/// while it runs, and the host is tainted as unchecked for that run, as for a start the
/// agent cannot see. Catches a
/// crun runtime that does not say it places its runs in cgroups (a run no one can sample
/// would pass unnoticed).
#[test]
fn a_crun_run_whose_group_is_never_seen_taints_the_host() {
    let node = Node::new("blind-group");
    let config = Config {
        capacity: Capacity {
            cpu_millis: 4000,
            memory_bytes: 8 << 30,
            disk_bytes: 10 << 30,
            gpus: BTreeSet::new(),
        },
        allow_host_jobs: false,
        mounts: MountRoots::new(&[]).expect("no mount roots"),
        scratch: ScratchRoot::open(&private(&node, "life-scratch")).expect("scratch"),
        files: None,
        state: StateRoot::open(&private(&node, "life-state")).expect("state"),
        stop_grace: LONG,
        bundle: None,
        quota: None,
    };
    // The image is fetched and unpacked before the lifecycle asks, so that its start does
    // not wait on a pull running in the background.
    let mut runtime = node.open();
    let _ = runtime.prepare(&node.image(), None);
    assert_eq!(runtime.wait_prepared(&node.image()), Prepared::Ready);
    let (mut life, _) =
        Lifecycle::adopt(config, Box::new(runtime), Box::new(Pass), Box::new(Still))
            .expect("adopts");
    let start = |spec| HeartbeatReply {
        commands: vec![Command::Start(Box::new(spec))],
        ..HeartbeatReply::default()
    };
    let started = life.apply(&start(job(&node, "r1-hold")));
    assert!(!life.is_tainted(), "the start alone says nothing");
    let stepped = life.step();
    assert_eq!(
        life.pending_taint(),
        Some(Taint {
            reason: reason::UNCHECKED.to_owned(),
            run_id: Some("r1-hold".to_owned()),
            check: Some(code::HOST_NOT_VISIBLE.to_owned()),
        }),
        "start: {started:?}; step: {stepped:?}"
    );
    life.apply(&HeartbeatReply {
        commands: vec![Command::Stop {
            run_id: "r1-hold".to_owned(),
        }],
        ..HeartbeatReply::default()
    });
    life.step();
}

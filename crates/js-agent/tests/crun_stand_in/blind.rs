//! `CrunRuntime` under the lifecycle: the runtime puts every container in a control group
//! the agent can read, so a running run whose group the agent cannot name, and whose first
//! process runs in another group, leaves the host unchecked; one whose process is gone does
//! not. And `CrunRuntime::whereabouts` itself, on the stand-in.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::PermissionsExt as _;
use std::rc::Rc;
use std::sync::Arc;

use js_agent::cli_runtime::ProcRead;
use js_agent::crun::Hooks;
use js_agent::lifecycle::taint::reason;
use js_agent::lifecycle::{
    Capacity, CheckFinding, CheckHook, Clock, Config, DEFAULT_SETTLE, Event, Finished, Lifecycle,
    MountRoots, Prepared, RunHandle, Runtime, ScratchRoot, StateRoot, Status, Whereabouts, code,
};
use js_agent_wire::{Command, HeartbeatReply, Isolation, JobSpec, Limits, Taint};

use crate::node::{ELSEWHERE, LONG, NoImages, Node, Ringing, no_process};

struct Pass;

impl CheckHook for Pass {
    fn run(&mut self, _job: &Finished<'_>) -> Result<(), CheckFinding> {
        Ok(())
    }
}

/// A clock that moves only when the test moves it.
#[derive(Clone)]
struct Manual(Rc<Cell<u64>>);

impl Clock for Manual {
    fn now_ms(&self) -> u64 {
        self.0.get()
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
        network: Default::default(),
        ports: Vec::new(),
        run: None,
        os: None,
        arch: None,
    }
}

/// Runs the holding run `r1-hold`, whose container cgroup the stand-in never makes, under the
/// lifecycle on a node whose `/proc` is `proc_read`: one step, then one a settle wait after the
/// first sighting. Returns the taint the lifecycle holds then and the `cgroup-not-found`
/// findings of that step, and stops the run.
fn past_the_wait(name: &str, proc_read: ProcRead) -> (Option<Taint>, Vec<String>) {
    let node = Node::new(name);
    let config = Config {
        capacity: Capacity {
            cpu_millis: 4000,
            memory_bytes: 8 << 30,
            disk_bytes: 10 << 30,
            gpus: BTreeSet::new(),
        },
        allow_host_jobs: false,
        allow_fuse_jobs: false,
        mounts: MountRoots::new(&[]).expect("no mount roots"),
        scratch: ScratchRoot::open(&private(&node, "life-scratch")).expect("scratch"),
        files: None,
        state: StateRoot::open(&private(&node, "life-state")).expect("state"),
        stop_grace: LONG,
        bundle: None,
        quota: None,
    };
    let hooks = Hooks {
        proc_read,
        ..Node::hooks(LONG, LONG)
    };
    // The image is fetched and unpacked before the lifecycle asks, so that its start does
    // not wait on a pull running in the background.
    let mut runtime = node.open_with(hooks, Arc::new(NoImages));
    let _ = runtime.prepare(&node.image(), None);
    assert_eq!(runtime.wait_prepared(&node.image()), Prepared::Ready);
    // The stand-in says it is up through a pipe named for the container, which must exist
    // before the container does. The name is learnt by staging the same run and removing
    // it again. The stop at the end waits for that word, so that it never reaches a script
    // that has not yet set its handler.
    let probe = runtime.stage(&node.spec("r1-hold")).expect("staged");
    runtime.remove(&probe).expect("removed");
    node.pipe(&probe.0);
    let clock = Manual(Rc::new(Cell::new(0)));
    let (life, _) = Lifecycle::adopt(
        config,
        Box::new(runtime),
        Box::new(Pass),
        Box::new(clock.clone()),
    )
    .expect("adopts");
    let mut life = Ringing::new(life);
    let start = |spec| HeartbeatReply {
        commands: vec![Command::Start(Box::new(spec))],
        ..HeartbeatReply::default()
    };
    life.apply_and_create(&start(job(&node, "r1-hold")));
    assert!(!life.is_tainted(), "the start alone says nothing");
    let first = life.step();
    assert!(
        !life.is_tainted(),
        "one sighting of a run with no group is not blindness; step: {first:?}"
    );
    // The settle wait after the first sighting: the second look comes that long after it.
    let wait: u64 = DEFAULT_SETTLE.as_millis().try_into().expect("small");
    clock.0.set(wait);
    let findings = life
        .step()
        .into_iter()
        .filter_map(|event| match event {
            Event::Finding(finding) if finding.code == code::CGROUP_NOT_FOUND => {
                Some(format!("{}: {}", finding.code, finding.message))
            }
            _ => None,
        })
        .collect();
    let taint = life.pending_taint();
    node.up(&probe);
    life.apply(&HeartbeatReply {
        commands: vec![Command::Stop {
            run_id: "r1-hold".to_owned(),
        }],
        ..HeartbeatReply::default()
    });
    life.step();
    (taint, findings)
}

/// Proves the wiring of the blind-runtime taint to `CrunRuntime`: a run that holds (it never
/// ends by itself) whose container cgroup the stand-in never makes, and whose first process
/// (the pid the stand-in's `crun state` names) runs in another group, is seen by the lifecycle
/// while it runs, and the host is tainted as unchecked for that run by the first step a settle
/// wait after the first sighting (the first sighting alone taints nothing), as for a start the
/// agent cannot see, with a finding that names that group. Catches a crun runtime that does
/// not say it places its runs in cgroups, or does not say where a container is (a run no one
/// can sample would pass unnoticed).
#[test]
fn a_crun_run_whose_group_is_never_seen_taints_the_host() {
    let (taint, findings) = past_the_wait("blind-group", crate::node::elsewhere);
    assert_eq!(
        taint,
        Some(Taint {
            reason: reason::UNCHECKED.to_owned(),
            run_id: Some("r1-hold".to_owned()),
            check: Some(code::CGROUP_NOT_FOUND.to_owned()),
        })
    );
    assert_eq!(
        findings,
        [format!(
            "cgroup-not-found: run r1-hold: its container group was not found within the settle \
             wait of 1000 ms, and its first process runs in {ELSEWHERE}"
        )]
    );
}

/// Proves that the same run, whose container cgroup is not there a settle wait after the first
/// sighting, taints nothing when the process `crun state` names is not in the agent's `/proc`:
/// the container ended (or `crun` names a process that is gone), and a group removed with it
/// says nothing against the host. Catches the farm's taint of a host whose job ran clean.
#[test]
fn a_crun_run_whose_process_is_gone_is_not_blind() {
    let (taint, findings) = past_the_wait("gone-process", no_process);
    assert_eq!(taint, None);
    assert_eq!(findings, Vec::<String>::new());
}

/// Proves what `CrunRuntime::whereabouts` answers on the stand-in: for a running container
/// whose group is not there, `Elsewhere` with the group its first process (the pid `crun
/// state` names) runs in; once the group is made, `Grouped` with it; for a container `crun
/// state` does not know though it runs, `NoProcess`; and for a run that ended, and a run the
/// runtime does not hold, `NoProcess` without asking `crun`. Catches a runtime that asks the
/// wrong place, or names a container that is not there as one that runs elsewhere.
#[test]
fn where_a_crun_container_is() {
    let node = Node::new("whereabouts");
    let mut runtime = node.open();
    let id = runtime.stage(&node.spec("r1-hold")).expect("staged");
    node.pipe(&id.0);
    runtime.start_staged(&id).expect("started");
    node.up(&id);
    assert_eq!(
        runtime.whereabouts(&id),
        Whereabouts::Elsewhere(ELSEWHERE.to_owned())
    );
    let ctr = node.cgroup_of(&id.0).join("ctr");
    std::fs::create_dir(&ctr).expect("the container's cgroup");
    assert_eq!(runtime.whereabouts(&id), Whereabouts::Grouped(ctr));

    let deaf = runtime
        .create_and_start(&node.spec("r2-deaf"))
        .expect("started");
    assert_eq!(runtime.whereabouts(&deaf), Whereabouts::NoProcess);

    assert_eq!(runtime.stop(&id, LONG), Ok(()));
    assert!(matches!(runtime.poll(&id), Ok(Status::Exited(_))));
    assert_eq!(runtime.whereabouts(&id), Whereabouts::NoProcess);
    let unknown = RunHandle("js-nobody-r9".to_owned());
    assert_eq!(runtime.whereabouts(&unknown), Whereabouts::NoProcess);
}

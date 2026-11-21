//! The makes and starts of leases off the worker (`Runtime::begin_create` and
//! `Runtime::poll_create` of `CrunRuntime`), against the stand-in `crun`: a start that waits
//! (in the network wiring, which a test holds with a channel) holds only its own lease, the
//! sidecar of a start is written once more, in the state running, and a start that fails, or
//! whose thread dies, is taken apart by what the worker kept. Nothing waits for a clock: a
//! held wiring is released by a message, and an answer is awaited by the runtime's ring.

use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_agent::crun::{CrunRuntime, EgressPort, Hooks, Reason, Wired};
use js_agent::egress::EgressError;
use js_agent::lifecycle::{
    Created, Event, Lifecycle, Prepared, RunHandle, RunSpec, Runtime, RuntimeError, Status, Wake,
};
use js_agent_wire::{
    Command, HeartbeatReply, HeldState, Isolation, JobSpec, Limits, NetworkCeiling,
};
use js_contain::oci::{SupervisorId, recorded_supervisor};
use js_types::{JobNetwork, PortMap};
use serde_json::Value;

use crate::egress::lifecycle;
use crate::node::{LONG, NoImages, Node};

/// What a held wiring is released with.
enum Release {
    /// The wiring succeeds.
    Wire,
    /// The thread that wires panics.
    Panic,
}

/// An egress port whose attach of a held run waits for the test: it says the run it waits
/// for (the container's name) and returns when the test sends its release.
#[derive(Debug)]
struct Gate {
    entered: Mutex<Sender<String>>,
    holds: Mutex<BTreeMap<String, Receiver<Release>>>,
}

impl Gate {
    fn new() -> (Arc<Self>, Receiver<String>) {
        let (entered, said) = channel();
        let gate = Self {
            entered: Mutex::new(entered),
            holds: Mutex::new(BTreeMap::new()),
        };
        (Arc::new(gate), said)
    }

    /// Holds the attach of the run `run_id`; it returns the release.
    fn hold(&self, run_id: &str) -> Sender<Release> {
        let (release, held) = channel();
        self.holds
            .lock()
            .expect("holds")
            .insert(run_id.to_owned(), held);
        release
    }
}

impl EgressPort for Gate {
    fn check(&self) -> Result<(), EgressError> {
        Ok(())
    }

    fn ceiling(&self) -> NetworkCeiling {
        NetworkCeiling {
            level: JobNetwork::AllowList,
            ports: Vec::new(),
        }
    }

    fn attach(
        &self,
        run: &str,
        _pid: u32,
        _level: JobNetwork,
        _ports: &[PortMap],
    ) -> Result<Box<dyn Wired>, EgressError> {
        let mut holds = self.holds.lock().expect("holds");
        let key = holds
            .keys()
            .find(|id| run.ends_with(&format!("-{id}")))
            .cloned();
        let held = key.and_then(|key| holds.remove(&key));
        drop(holds);
        if let Some(held) = held {
            let _ = self.entered.lock().expect("entered").send(run.to_owned());
            match held.recv() {
                Ok(Release::Wire) => {}
                Ok(Release::Panic) => panic!("the wiring of {run} panicked"),
                Err(_) => return Err(EgressError::Exhausted),
            }
        }
        Ok(Box::new(Wire))
    }

    fn adopt(
        &self,
        _run: &str,
        _level: JobNetwork,
        _ports: &[PortMap],
    ) -> Result<Box<dyn Wired>, EgressError> {
        Ok(Box::new(Wire))
    }

    fn settle(&self) -> Result<(), EgressError> {
        Ok(())
    }
}

/// A wiring that is taken down at once.
struct Wire;

impl Wired for Wire {
    fn release(self: Box<Self>) -> Result<(), EgressError> {
        Ok(())
    }

    fn leave(self: Box<Self>) {}
}

/// A runtime on `node` behind `gate`, whose waits after a kill are spent at once, and what
/// rings when one of its creates has answered.
fn gated(node: &Node, gate: &Arc<Gate>) -> (CrunRuntime, Receiver<()>) {
    let mut settings = node.settings("/root");
    let port: Arc<dyn EgressPort> = Arc::clone(gate) as Arc<dyn EgressPort>;
    settings.egress = Some(port);
    let hooks: Hooks = Node::hooks(Duration::ZERO, LONG);
    let mut runtime = CrunRuntime::open_with(settings, Arc::new(NoImages), hooks).expect("opens");
    let (ring, rung) = channel();
    runtime.set_wake(Wake::new(move || {
        let _ = ring.send(());
    }));
    (runtime, rung)
}

/// The run `run_id` asking for the allow-list: the run the gate wires.
fn wired(node: &Node, run_id: &str) -> RunSpec {
    RunSpec {
        network: JobNetwork::AllowList,
        ..node.spec(run_id)
    }
}

/// Asks the create of `run_id` until it is done, each ask after a ring.
fn settled(
    runtime: &mut CrunRuntime,
    rung: &Receiver<()>,
    run_id: &str,
) -> Result<RunHandle, RuntimeError> {
    loop {
        match runtime.poll_create(run_id) {
            Created::Done(done) => return done,
            Created::Pending => rung.recv_timeout(LONG).expect("the runtime rang"),
        }
    }
}

/// Asks the create of `run_id` until it is done, without a ring: a thread that died rings
/// nothing.
fn settled_unrung(runtime: &mut CrunRuntime, run_id: &str) -> Result<RunHandle, RuntimeError> {
    loop {
        match runtime.poll_create(run_id) {
            Created::Done(done) => return done,
            Created::Pending => std::thread::yield_now(),
        }
    }
}

/// The failure a `Created` that is done with one carries, as text.
fn refused(created: Created) -> String {
    match created {
        Created::Done(Err(why)) => why.to_string(),
        other => panic!("not refused: {other:?}"),
    }
}

/// The sidecar of the lease `name` as JSON.
fn sidecar_json(node: &Node, name: &str) -> Value {
    let text = std::fs::read_to_string(node.sidecar(name)).expect("the sidecar");
    serde_json::from_str(&text).expect("json")
}

/// What is left of the lease `name`: its directory, cgroup and sidecar.
fn left_of(node: &Node, name: &str) -> Vec<&'static str> {
    [
        ("directory", node.lease_of(name)),
        ("cgroup", node.cgroup_of(name)),
        ("sidecar", node.sidecar(name)),
    ]
    .into_iter()
    .filter(|(_, path)| path.exists())
    .map(|(what, _)| what)
    .collect()
}

/// Whether nothing of any lease of the run `run_id` is left on the node.
fn nothing_of(node: &Node, run_id: &str) -> bool {
    let suffix = format!("-{run_id}");
    [
        node.scratch.join("oci-labels"),
        node.scratch.clone(),
        node.cgroups.join("actions"),
    ]
    .iter()
    .all(|dir| {
        std::fs::read_dir(dir).is_ok_and(|entries| {
            !entries.flatten().any(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                name.trim_end_matches(".json").ends_with(&suffix)
            })
        })
    })
}

/// Returns once the process `id` is gone.
fn wait_gone(id: SupervisorId) {
    while id.alive() {
        std::thread::yield_now();
    }
}

/// Proves a start that waits does not hold another lease: with the wiring of run A held, A
/// polls pending while run B, begun after it, is made, started and polled to done, runs and
/// is listed; A is not in the runtime's books yet. Once A's wiring is released it answers
/// done and runs. Catches a create that runs on the worker (B would not start while A
/// waits) and an answer that is put in the books before it is taken.
#[test]
fn a_start_held_in_its_wiring_does_not_hold_another_start() {
    let node = Node::new("creates-concurrent");
    let (gate, entered) = Gate::new();
    let release = gate.hold("ra-wired");
    let (mut runtime, rung) = gated(&node, &gate);

    let begun = runtime.begin_create(&wired(&node, "ra-wired"));
    assert_eq!(begun, Created::Pending);
    let name_a = entered.recv_timeout(LONG).expect("A reached its wiring");
    let begun = runtime.begin_create(&node.spec("rb-wired"));
    assert_eq!(begun, Created::Pending);
    let b = settled(&mut runtime, &rung, "rb-wired").expect("B started while A is held");
    assert_eq!(runtime.poll(&b), Ok(Status::Running));
    let listed: Vec<_> = runtime
        .list_labelled()
        .expect("lists")
        .into_iter()
        .map(|found| found.id)
        .collect();
    assert_eq!(listed, std::slice::from_ref(&b), "only B is made yet");
    assert_eq!(runtime.poll_create("ra-wired"), Created::Pending);

    release.send(Release::Wire).expect("released");
    let a = settled(&mut runtime, &rung, "ra-wired").expect("A started");
    assert_eq!(a.0, name_a);
    assert_eq!(runtime.poll(&a), Ok(Status::Running));
    runtime.remove(&a).expect("A removed");
    runtime.remove(&b).expect("B removed");
    assert!(nothing_of(&node, "ra-wired") && nothing_of(&node, "rb-wired"));
}

/// Proves a lease whose create is under way is not removed: with run A held in its wiring
/// (its sidecar recorded, its supervisor up), a removal of its name and an orphan removal of
/// its run are both refused saying a create of it is under way, and its sidecar stays; once
/// released, A answers done and runs, and its removal then takes everything away. Catches a
/// removal that tears a lease down while a pool thread is still making it, which would leave
/// a supervisor nobody records.
#[test]
fn a_lease_whose_create_is_under_way_is_not_removed() {
    let node = Node::new("creates-no-remove");
    let (gate, entered) = Gate::new();
    let release = gate.hold("ra-wired");
    let (mut runtime, rung) = gated(&node, &gate);
    assert_eq!(
        runtime.begin_create(&wired(&node, "ra-wired")),
        Created::Pending
    );
    let name = entered.recv_timeout(LONG).expect("A reached its wiring");

    let removed = runtime
        .remove(&RunHandle(name.clone()))
        .expect_err("under way");
    assert!(
        removed.to_string().contains("a create of it is under way"),
        "{removed}"
    );
    let orphan = runtime.remove_orphan("ra-wired").expect_err("under way");
    assert!(
        orphan.to_string().contains("a create of it is under way"),
        "{orphan}"
    );
    assert_eq!(sidecar_json(&node, &name)["state"], "starting");

    release.send(Release::Wire).expect("released");
    let a = settled(&mut runtime, &rung, "ra-wired").expect("A started");
    assert_eq!(runtime.poll(&a), Ok(Status::Running));
    runtime.remove(&a).expect("A removed");
    assert!(nothing_of(&node, "ra-wired"));
}

/// Proves the runtime names a lease whose create is under way, from the begin until the
/// worker takes the answer: with run A held in its wiring, `creating` answers A's lease name
/// while A's control group already exists (so A's processes may run in it), and answers it
/// still after the create rang its answer but before the poll took it; once the poll took it,
/// for a run never begun, and for an id no lease can be named for, it answers none. Catches a
/// runtime that names only the runs in its books, which leaves a started container's group
/// unnamed to the post-job checks of a run that ends meanwhile, so the neighbour's processes
/// are counted as that run's leftover.
#[test]
fn a_lease_whose_create_is_under_way_is_named_until_its_answer_is_taken() {
    let node = Node::new("creates-creating");
    let (gate, entered) = Gate::new();
    let release = gate.hold("ra-wired");
    let (mut runtime, rung) = gated(&node, &gate);
    assert_eq!(runtime.creating("ra-wired"), None, "not begun");
    assert_eq!(
        runtime.begin_create(&wired(&node, "ra-wired")),
        Created::Pending
    );
    let name = entered.recv_timeout(LONG).expect("A reached its wiring");
    assert_eq!(runtime.creating("ra-wired"), Some(RunHandle(name.clone())));
    assert!(node.cgroup_of(&name).is_dir(), "A's group is made");
    assert_eq!(runtime.creating("rb-never"), None);
    assert_eq!(runtime.creating(""), None, "a name the grammar refuses");

    release.send(Release::Wire).expect("released");
    rung.recv_timeout(LONG).expect("the create answered");
    assert_eq!(
        runtime.creating("ra-wired"),
        Some(RunHandle(name.clone())),
        "answered, not yet taken"
    );
    let a = settled(&mut runtime, &rung, "ra-wired").expect("A started");
    assert_eq!(a.0, name);
    assert_eq!(runtime.creating("ra-wired"), None, "taken");
    runtime.remove(&a).expect("A removed");
    assert!(nothing_of(&node, "ra-wired"));
}

/// Proves the lifecycle holds a start that waits as starting and lets another job start: with
/// job A's wiring held, job B (no network) is started, is held as running, and A is still
/// held as starting; once A is released it starts too. Catches a lifecycle that waits for a
/// create it has begun before it steps the other jobs.
#[test]
fn a_job_held_in_its_start_lets_another_job_start_and_run() {
    let node = Node::new("creates-lifecycle");
    let (gate, entered) = Gate::new();
    let release = gate.hold("ja-wired");
    let (runtime, _) = gated(&node, &gate);
    assert_eq!(runtime.wait_prepared(&node.image()), Prepared::Ready);
    let mut life = lifecycle(&node, runtime);
    let job = |run_id: &str, network| JobSpec {
        run_id: run_id.to_owned(),
        lease_id: format!("lease-{run_id}"),
        image: node.image(),
        args: vec!["--go".to_owned()],
        limits: Limits {
            cpu_millis: 1000,
            memory_bytes: 1 << 20,
            disk_bytes: None,
            pids: Some(256),
            wall_ms: 600_000,
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
        network,
        ports: Vec::new(),
        run: None,
        os: None,
        arch: None,
    };
    let start = |spec| HeartbeatReply {
        commands: vec![Command::Start(Box::new(spec))],
        ..HeartbeatReply::default()
    };
    let started = |events: &[Event], run: &str| {
        events
            .iter()
            .any(|event| matches!(event, Event::Started { run_id, .. } if run_id == run))
    };
    let state_of = |life: &Lifecycle, run: &str| {
        life.held()
            .into_iter()
            .find(|held| held.run_id == run)
            .map(|held| held.state)
    };

    let events = life.apply(&start(job("ja-wired", JobNetwork::AllowList)));
    assert!(!started(&events, "ja-wired"));
    entered.recv_timeout(LONG).expect("A reached its wiring");
    assert_eq!(state_of(&life, "ja-wired"), Some(HeldState::Starting));

    let mut events = life.apply(&start(job("jb-wired", JobNetwork::None)));
    events.extend(life.until(|life| state_of(life, "jb-wired") == Some(HeldState::Running)));
    assert!(started(&events, "jb-wired"), "{events:?}");
    assert_eq!(state_of(&life, "ja-wired"), Some(HeldState::Starting));

    release.send(Release::Wire).expect("released");
    let events = life.until(|life| state_of(life, "ja-wired") == Some(HeldState::Running));
    assert!(started(&events, "ja-wired"), "{events:?}");
}

/// Proves the sidecar is written once in a start after it is recorded: while the wiring is
/// held, the container runs under its supervisor (the lease directory names it, and it is
/// alive) yet the sidecar still says starting with no supervisor, so no intermediate record
/// with the supervisor is written; once the start is done it says running, with that
/// supervisor. Catches a start that rewrites the sidecar a second time before the wiring.
#[test]
fn the_sidecar_is_rewritten_once_in_a_start_in_the_state_running() {
    let node = Node::new("creates-one-rewrite");
    let (gate, entered) = Gate::new();
    let release = gate.hold("ra-wired");
    let (mut runtime, rung) = gated(&node, &gate);
    let begun = runtime.begin_create(&wired(&node, "ra-wired"));
    assert_eq!(begun, Created::Pending);
    let name = entered.recv_timeout(LONG).expect("A reached its wiring");

    let supervisor = recorded_supervisor(&node.lease_of(&name)).expect("the lease names it");
    assert!(supervisor.alive());
    let held = sidecar_json(&node, &name);
    assert_eq!(held["state"], "starting");
    assert!(held["supervisor"].is_null(), "{held}");

    release.send(Release::Wire).expect("released");
    let id = settled(&mut runtime, &rung, "ra-wired").expect("started");
    let done = sidecar_json(&node, &id.0);
    assert_eq!(done["state"], "running");
    assert_eq!(done["supervisor"]["pid"], supervisor.pid);
    assert_eq!(done["supervisor"]["start"], supervisor.start);
    runtime.remove(&id).expect("removed");
    assert!(nothing_of(&node, "ra-wired"));
}

/// Proves a sweep ends the supervisor of a lease whose sidecar says starting with no
/// supervisor, as a start that died between its supervisor and its running record leaves it:
/// the new runtime finds the supervisor by the record in the lease directory, ends it and
/// removes the lease with its sidecar, reporting it never ran. Catches a teardown that
/// needs the sidecar to name the supervisor and leaves the job running with no lease.
#[test]
fn a_sweep_ends_the_supervisor_of_a_starting_lease_that_names_none() {
    let node = Node::new("creates-sweep-unnamed");
    let mut first = node.open();
    let id = first.stage(&node.job("r1-hold", 60_000)).expect("staged");
    node.pipe(&id.0);
    first.start_staged(&id).expect("started");
    node.up(&id);
    let supervisor = recorded_supervisor(&node.lease_of(&id.0)).expect("the lease names it");
    assert!(supervisor.alive());
    let mut record = sidecar_json(&node, &id.0);
    record["state"] = Value::from("starting");
    record
        .as_object_mut()
        .expect("an object")
        .remove("supervisor");
    std::fs::write(node.sidecar(&id.0), record.to_string()).expect("write the sidecar");
    drop(first);

    let second = node.open();
    let report = second.swept();
    assert!(report.adopted.is_empty(), "{report:?}");
    assert_eq!(report.removed.len(), 1, "{report:?}");
    assert_eq!(report.removed[0].reason, Reason::NeverRan);
    wait_gone(supervisor);
    assert!(
        left_of(&node, &id.0).is_empty(),
        "{:?}",
        left_of(&node, &id.0)
    );
}

/// Proves the asks that name no create: a run that was never begun answers a failure naming
/// the run, and so does one whose answer was taken already. Catches a poll that invents a
/// pending create and one that answers a create twice.
#[test]
fn a_poll_of_a_create_that_is_not_under_way_names_the_run() {
    let node = Node::new("creates-not-begun");
    let (gate, _) = Gate::new();
    let (mut runtime, rung) = gated(&node, &gate);
    assert_eq!(
        refused(runtime.poll_create("r9-exit")),
        "no create of r9-exit is under way"
    );

    let begun = runtime.begin_create(&node.spec("r1-exit"));
    assert_eq!(begun, Created::Pending);
    let id = settled(&mut runtime, &rung, "r1-exit").expect("made");
    assert_eq!(
        refused(runtime.poll_create("r1-exit")),
        "no create of r1-exit is under way"
    );
    runtime.remove(&id).expect("removed");
}

/// Proves a begin refused on the worker is done at once and makes nothing: a run id whose
/// create is under way is refused as in use and the create under way is not disturbed (it
/// still answers done), a run id that is in the books is refused the same way, an
/// environment the runtime refuses and a host job this agent does not launch are refused
/// before any sidecar is recorded. Catches two creates of one lease name, and checks made
/// after the lease is.
#[test]
fn a_begin_the_worker_refuses_is_done_at_once_and_makes_nothing() {
    let node = Node::new("creates-begin-denied");
    let (gate, entered) = Gate::new();
    let release = gate.hold("ra-wired");
    let (mut runtime, rung) = gated(&node, &gate);
    let begun = runtime.begin_create(&wired(&node, "ra-wired"));
    assert_eq!(begun, Created::Pending);
    entered.recv_timeout(LONG).expect("A reached its wiring");

    let under_way = refused(runtime.begin_create(&wired(&node, "ra-wired")));
    assert_eq!(under_way, "a container of run ra-wired exists");

    release.send(Release::Wire).expect("released");
    let a = settled(&mut runtime, &rung, "ra-wired").expect("A was left alone");
    let made = refused(runtime.begin_create(&wired(&node, "ra-wired")));
    assert_eq!(made, "a container of run ra-wired exists");
    assert_eq!(runtime.poll(&a), Ok(Status::Running));

    let mut spec = node.spec("r2-exit");
    spec.env = vec![
        ("X".to_owned(), "1".to_owned()),
        ("X".to_owned(), "2".to_owned()),
    ];
    assert_eq!(
        refused(runtime.begin_create(&spec)),
        "env names a variable twice"
    );
    let mut host = node.spec("r3-exit");
    host.isolation = Isolation::Host;
    assert_eq!(
        refused(runtime.begin_create(&host)),
        "a host job is refused: this agent was started without --allow-host-jobs"
    );
    assert!(nothing_of(&node, "r2-exit") && nothing_of(&node, "r3-exit"));
    runtime.remove(&a).expect("removed");
}

/// Proves a thread that dies in the middle of a start leaves nothing: the wiring panics after
/// the supervisor was started and the lease made, the poll answers a failure naming the
/// step, the supervisor the lease directory named is ended, no sidecar, lease directory or
/// cgroup is left, nothing is listed, and the run id can be begun again. Catches a create
/// that is forgotten with its lease on the host, and one that is never answered.
#[test]
fn a_create_whose_thread_panics_is_taken_apart_by_its_kept_sidecar() {
    let node = Node::new("creates-panic");
    let (gate, entered) = Gate::new();
    let release = gate.hold("ra-wired");
    let (mut runtime, rung) = gated(&node, &gate);
    let begun = runtime.begin_create(&wired(&node, "ra-wired"));
    assert_eq!(begun, Created::Pending);
    let name = entered.recv_timeout(LONG).expect("A reached its wiring");
    let supervisor = recorded_supervisor(&node.lease_of(&name)).expect("the lease names it");
    assert!(supervisor.alive());

    release.send(Release::Panic).expect("released");
    let why = settled_unrung(&mut runtime, "ra-wired").expect_err("no answer came");
    assert!(why.to_string().contains("make the lease"), "{why}");
    wait_gone(supervisor);
    assert!(nothing_of(&node, "ra-wired"), "{:?}", left_of(&node, &name));
    assert!(runtime.list_labelled().expect("lists").is_empty());

    let begun = runtime.begin_create(&node.spec("ra-wired"));
    assert_eq!(begun, Created::Pending);
    let again = settled(&mut runtime, &rung, "ra-wired").expect("the run id is free");
    runtime.remove(&again).expect("removed");
}

/// Proves a make that fails after its sidecar was recorded is taken apart: a run whose
/// scratch directory cannot be given away fails with the make's own message and leaves no
/// sidecar, lease directory or cgroup. Catches a failed make through `begin_create` that
/// keeps its lease for the next sweep.
#[test]
fn a_make_that_fails_after_the_record_leaves_nothing() {
    let node = Node::new("creates-make-fails");
    let (gate, _) = Gate::new();
    let (mut runtime, rung) = gated(&node, &gate);
    let mut spec = node.spec("r1-exit");
    let refused_dir = node.dir.join("refuse");
    std::fs::create_dir_all(&refused_dir).expect("dir");
    spec.scratch = refused_dir;
    assert_eq!(runtime.begin_create(&spec), Created::Pending);
    let why = settled(&mut runtime, &rung, "r1-exit").expect_err("refused");
    assert!(why.to_string().contains("refused"), "{why}");
    assert!(nothing_of(&node, "r1-exit"));
    assert!(runtime.list_labelled().expect("lists").is_empty());
}

/// Proves a start that cannot run `crun` is removed by the worker: the answer is the start's
/// failure, and the lease made for it (sidecar, directory, cgroup) is gone and nothing is
/// listed. Catches a failed start that stays in the books or in the ledger.
#[test]
fn a_start_that_fails_after_the_make_is_removed() {
    let node = Node::new("creates-start-fails");
    let (gate, _) = Gate::new();
    let (mut runtime, rung) = gated(&node, &gate);
    let begun = runtime.begin_create(&node.spec("r1-exit"));
    assert_eq!(begun, Created::Pending);
    let why = settled(&mut runtime, &rung, "r1-exit").expect_err("not started");
    assert!(why.to_string().contains("start crun"), "{why}");
    assert!(nothing_of(&node, "r1-exit"));
    assert!(runtime.list_labelled().expect("lists").is_empty());
}

/// Proves a record that fails leaves the lease that holds the name alone: with the sidecar of
/// the name written by another runtime of the node (which made the lease), a begin of the
/// same run answers a failure naming the labels, and the other lease's directory, cgroup and
/// sidecar are as they were. Catches a failed record that discards the lease of another
/// start.
#[test]
fn a_record_that_fails_leaves_the_other_runtimes_lease_intact() {
    let node = Node::new("creates-record-fails");
    let mut one = node.open();
    let (gate, _) = Gate::new();
    let (mut two, rung) = gated(&node, &gate);
    let id = one.stage(&node.spec("r1-exit")).expect("staged");
    let before = std::fs::read(node.sidecar(&id.0)).expect("the sidecar");
    assert_eq!(two.begin_create(&node.spec("r1-exit")), Created::Pending);
    let why = settled(&mut two, &rung, "r1-exit").expect_err("not recorded");
    assert!(
        why.to_string().contains("record the lease's labels"),
        "{why}"
    );
    assert_eq!(left_of(&node, &id.0).len(), 3);
    assert_eq!(
        std::fs::read(node.sidecar(&id.0)).expect("the sidecar"),
        before
    );
    one.remove(&id).expect("removed");
}

/// Proves a failed start whose lease cannot be removed is still answered as the start's
/// failure and the lease is kept for a later removal: with `crun delete` failing, the answer
/// names the start, the runtime lists the lease, and once the delete works the removal
/// leaves nothing. Catches an answer that hides the start's failure behind the removal's, and
/// a lease dropped from the books while still on the host.
#[test]
fn a_failed_start_whose_removal_fails_keeps_its_lease_for_a_later_removal() {
    let node = Node::new("creates-start-fails-kept");
    let (gate, _) = Gate::new();
    let (mut runtime, rung) = gated(&node, &gate);
    std::fs::write(node.dir.join("delete-fails"), "").expect("switch");
    let begun = runtime.begin_create(&node.spec("r1-exit"));
    assert_eq!(begun, Created::Pending);
    let why = settled(&mut runtime, &rung, "r1-exit").expect_err("not started");
    assert!(why.to_string().contains("start crun"), "{why}");
    let kept = runtime.list_labelled().expect("lists");
    assert_eq!(kept.len(), 1, "the lease is listed");

    std::fs::remove_file(node.dir.join("delete-fails")).expect("switch off");
    runtime.remove(&kept[0].id).expect("removed");
    assert!(nothing_of(&node, "r1-exit"));
}

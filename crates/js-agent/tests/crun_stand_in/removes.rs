//! The removals of leases off the worker (`Runtime::removing` of `CrunRuntime`), against the
//! stand-in `crun`: a removal that waits (in `crun delete`, which a test holds with a named
//! pipe) holds only its own lease, a removal that fails keeps the run for the next, and one
//! whose thread dies leaves the lease to its sidecar. Nothing waits for a clock: a held
//! delete is released by a write to its pipe, and an answer is awaited by the runtime's ring.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use js_agent::crun::{CrunRuntime, EgressPort, Hooks, Wired};
use js_agent::egress::EgressError;
use js_agent::lifecycle::{Removal, RunHandle, RunSpec, Runtime, RuntimeError, Status, Wake};
use js_agent_wire::NetworkCeiling;
use js_types::{JobNetwork, PortMap};

use crate::node::{LONG, NoImages, Node};

/// A runtime on `node` whose waits after a kill are spent at once, with `egress` when
/// given, and what rings when one of its removals has answered.
fn ringing(node: &Node, egress: Option<Arc<dyn EgressPort>>) -> (CrunRuntime, Receiver<()>) {
    let mut settings = node.settings("/root");
    settings.egress = egress;
    let hooks: Hooks = Node::hooks(Duration::ZERO, LONG);
    let mut runtime = CrunRuntime::open_with(settings, Arc::new(NoImages), hooks).expect("opens");
    let (ring, rung) = channel();
    runtime.set_wake(Wake::new(move || {
        let _ = ring.send(());
    }));
    (runtime, rung)
}

/// Asks the removal of `id` until it is done, each ask after a ring.
fn removed(runtime: &mut CrunRuntime, rung: &Receiver<()>, id: &RunHandle) -> Removal {
    loop {
        match runtime.removing(id) {
            Removal::Pending => rung.recv_timeout(LONG).expect("the runtime rang"),
            done => return done,
        }
    }
}

/// The pipe the stand-in's `crun delete` of the lease `name` (a `*-delhold` run) waits on,
/// made before the lease is removed.
fn hold_delete(node: &Node, name: &str) -> PathBuf {
    let pipe = node.dir.join(format!("release-{name}"));
    let made = Command::new("mkfifo").arg(&pipe).status().expect("mkfifo");
    assert!(made.success());
    pipe
}

/// Proves a removal off the worker holds its lease and only it until it answers: while its
/// `crun delete` waits, every ask answers pending, a create of the same run and a removal
/// of it on the caller's thread are refused, and another run is made and removed beside it;
/// once released it rings, answers done once, and the lease's directory, cgroup and sidecar
/// are gone and nothing is listed. Catches a removal that blocks its caller, one answered
/// before it ran, one whose lease a second removal or a create races, and one that keeps
/// the run in the books once it is gone.
#[test]
fn a_removal_off_the_worker_holds_its_lease_until_it_answers() {
    let node = Node::new("removes-held");
    let (mut runtime, rung) = ringing(&node, None);
    let id = runtime
        .create_and_start(&node.spec("r1-delhold"))
        .expect("started");
    let pipe = hold_delete(&node, &id.0);
    runtime.wait_ended(&id);
    assert_eq!(runtime.removing(&id), Removal::Pending);
    assert_eq!(runtime.removing(&id), Removal::Pending, "asked again");

    let why = runtime
        .create_and_start(&node.spec("r1-delhold"))
        .expect_err("the lease is the removal's")
        .to_string();
    assert_eq!(why, "a container of run r1-delhold exists");
    let why = runtime.remove(&id).expect_err("held").to_string();
    assert_eq!(
        why,
        format!(
            "remove the leftovers of {}: a removal of it is under way",
            id.0
        )
    );
    let other = runtime
        .create_and_start(&node.spec("r2-exit"))
        .expect("another run is made meanwhile");
    runtime.wait_ended(&other);
    assert_eq!(removed(&mut runtime, &rung, &other), Removal::Done(Ok(())));
    assert_eq!(runtime.removing(&id), Removal::Pending, "still held");

    std::fs::write(&pipe, "go\n").expect("released");
    assert_eq!(removed(&mut runtime, &rung, &id), Removal::Done(Ok(())));
    assert!(!node.lease_of(&id.0).exists());
    assert!(!node.cgroup_of(&id.0).exists());
    assert!(!node.sidecar(&id.0).exists());
    assert_eq!(runtime.poll(&id), Ok(Status::Gone));
    assert!(runtime.list_labelled().expect("lists").is_empty());
}

/// Proves a removal that fails off the worker keeps the run for the next: with `crun
/// delete` failing, the answer names the step, the run is listed again and keeps its
/// sidecar; once the delete works, the next removal off the worker takes the lease apart.
/// Catches a failed removal that loses the run (its container would be left on the host
/// with nobody to remove it) and one that is answered as done.
#[test]
fn a_removal_that_fails_off_the_worker_keeps_the_run_for_the_next() {
    let node = Node::new("removes-fails");
    let (mut runtime, rung) = ringing(&node, None);
    let id = runtime
        .create_and_start(&node.spec("r1-exit"))
        .expect("started");
    runtime.wait_ended(&id);
    let marker = node.dir.join("delete-fails");
    std::fs::write(&marker, "").expect("switch");
    let Removal::Done(Err(why)) = removed(&mut runtime, &rung, &id) else {
        panic!("the removal did not fail");
    };
    assert!(
        why.to_string().starts_with("delete the container: "),
        "{why}"
    );
    assert_eq!(runtime.list_labelled().expect("lists").len(), 1);
    assert!(node.sidecar(&id.0).exists());

    std::fs::remove_file(&marker).expect("switch off");
    assert_eq!(removed(&mut runtime, &rung, &id), Removal::Done(Ok(())));
    assert!(!node.sidecar(&id.0).exists());
    assert!(runtime.list_labelled().expect("lists").is_empty());
}

/// An egress whose wiring panics when it is taken down, as a thread lost to a fault does.
#[derive(Debug)]
struct Fragile;

impl EgressPort for Fragile {
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
        _run: &str,
        _pid: u32,
        _level: JobNetwork,
        _ports: &[PortMap],
    ) -> Result<Box<dyn Wired>, EgressError> {
        Ok(Box::new(Brittle))
    }

    fn adopt(
        &self,
        _run: &str,
        _level: JobNetwork,
        _ports: &[PortMap],
    ) -> Result<Box<dyn Wired>, EgressError> {
        Ok(Box::new(Brittle))
    }

    fn settle(&self) -> Result<(), EgressError> {
        Ok(())
    }
}

/// A wiring whose release panics.
struct Brittle;

impl Wired for Brittle {
    fn release(self: Box<Self>) -> Result<(), EgressError> {
        panic!("the release of the network panicked")
    }

    fn leave(self: Box<Self>) {}
}

/// Proves a removal whose thread dies leaves the lease to its sidecar: a running wired run
/// whose network release panics is answered a failure that says the removal ended without
/// an answer, and the next removal finds the lease by its sidecar and takes it apart, so no
/// sidecar, directory or cgroup is left. Catches a removal lost to a panic that stays
/// pending for good (the job's slot held for ever) and one whose lease nobody removes.
#[test]
fn a_removal_whose_thread_panics_leaves_the_lease_to_its_sidecar() {
    let node = Node::new("removes-panic");
    let egress: Arc<dyn EgressPort> = Arc::new(Fragile);
    let (mut runtime, rung) = ringing(&node, Some(egress));
    let wired = RunSpec {
        network: JobNetwork::AllowList,
        ..node.spec("r1-wired")
    };
    let id = runtime.create_and_start(&wired).expect("started");
    assert_eq!(runtime.removing(&id), Removal::Pending);
    // A thread that dies rings nothing.
    let lost = loop {
        match runtime.removing(&id) {
            Removal::Pending => std::thread::yield_now(),
            done => break done,
        }
    };
    let expected = format!("remove {}: the removal ended without an answer", id.0);
    assert_eq!(lost, Removal::Done(Err(RuntimeError::new(expected))));
    assert!(node.sidecar(&id.0).exists(), "the sidecar is removed last");

    assert_eq!(removed(&mut runtime, &rung, &id), Removal::Done(Ok(())));
    assert!(!node.sidecar(&id.0).exists());
    assert!(!node.lease_of(&id.0).exists());
    assert!(!node.cgroup_of(&id.0).exists());
    assert!(runtime.list_labelled().expect("lists").is_empty());
}

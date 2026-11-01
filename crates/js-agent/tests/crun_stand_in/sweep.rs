//! The labels, the sweep and the removal of an orphan, against the stand-in `crun`: what a
//! runtime that opens on the store of a process that died does with its leases. The dead
//! process is a runtime of the same node that the test does not use again.

use std::sync::Arc;

use js_agent::crun::{CrunRuntime, Left, SweepReport};
use js_agent::lifecycle::{RunHandle, Runtime, label};

use crate::node::{LONG, NoImages, Node};

const OTHER: &str = "node-2";

/// A runtime on `node` that registers as `host_id`.
fn open_as(node: &Node, host_id: &str) -> CrunRuntime {
    let mut settings = node.settings("/root");
    settings.host_id = host_id.to_owned();
    CrunRuntime::open_with(settings, Arc::new(NoImages), Node::hooks(LONG, LONG)).expect("opens")
}

/// The sidecar of the lease `name`.
fn sidecar(node: &Node, name: &str) -> std::path::PathBuf {
    node.scratch.join("oci-labels").join(format!("{name}.json"))
}

/// Whether anything of the lease `name` is left: its directory, cgroup or sidecar.
fn left_of(node: &Node, name: &str) -> Vec<&'static str> {
    let mut left = Vec::new();
    for (what, path) in [
        ("directory", node.lease_of(name)),
        ("cgroup", node.cgroup_of(name)),
        ("sidecar", sidecar(node, name)),
    ] {
        if path.exists() {
            left.push(what);
        }
    }
    left
}

/// Stages `run` and returns its handle.
fn staged(node: &Node, runtime: &mut CrunRuntime, run: &str) -> RunHandle {
    runtime.stage(&node.spec(run)).expect("staged")
}

/// Proves the sidecar of a lease: it exists once the container is made, holds the agent,
/// the run, the lease's name and the labels the container lists with (the agent's and the
/// time among them), is readable by the owner only, and is gone after the run's removal,
/// with the lease. Catches a lease that cannot be attributed after a crash, labels that
/// differ from the container's, and a removal that leaves a sidecar for the next sweep.
#[test]
fn a_lease_has_a_sidecar_from_its_making_to_its_removal() {
    use std::os::unix::fs::PermissionsExt as _;
    let node = Node::new("sweep-sidecar");
    let mut runtime = node.open();
    let id = staged(&node, &mut runtime, "r1-exit");
    let file = sidecar(&node, &id.0);
    let body = std::fs::read_to_string(&file).expect("the sidecar");
    let json: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(json["agent"], "node-1");
    assert_eq!(json["run_id"], "r1-exit");
    assert_eq!(json["name"], id.0.as_str());
    let listed = runtime.list_labelled().expect("lists");
    assert_eq!(
        json["labels"],
        serde_json::to_value(&listed[0].labels).unwrap()
    );
    assert_eq!(json["labels"][label::AGENT], "node-1");
    let mode = std::fs::metadata(&file).expect("stat").permissions().mode();
    assert_eq!(mode & 0o777, 0o600);

    runtime.remove(&id).expect("removed");
    assert!(
        left_of(&node, &id.0).is_empty(),
        "{:?}",
        left_of(&node, &id.0)
    );
}

/// Proves a start that fails leaves no sidecar: a run whose scratch directory cannot be
/// given away is refused, and neither its lease nor its sidecar is left. Catches a failed
/// start that leaves a sidecar the next sweep would report as a removed run.
#[test]
fn a_start_that_fails_leaves_no_sidecar() {
    let node = Node::new("sweep-nogive");
    let mut runtime = node.open();
    let mut spec = node.spec("r1-exit");
    let refused = node.dir.join("refuse");
    std::fs::create_dir_all(&refused).expect("dir");
    spec.scratch = refused;
    runtime.stage(&spec).expect_err("refused");
    let ledger = node.scratch.join("oci-labels");
    let names: Vec<_> = std::fs::read_dir(ledger).expect("ledger").collect();
    assert!(names.is_empty(), "{names:?}");
}

/// Proves the sweep of a restart: the leases of a runtime that is gone, one running and one
/// made and never started, are removed when a runtime of the same agent opens (directory,
/// cgroup and sidecar), each is reported with the run and the labels it had, nothing is
/// adopted (the new runtime lists no container), and the run id can be used again. Catches
/// leases left behind by a dead process, a sweep that reports nothing, and one that adopts.
#[test]
fn a_restart_sweeps_the_leases_of_a_dead_process() {
    let node = Node::new("sweep-dead");
    let mut dead = node.open();
    let up = staged(&node, &mut dead, "r1-hold");
    node.pipe(&up.0);
    dead.start_staged(&up).expect("started");
    node.up(&up);
    let made = staged(&node, &mut dead, "r2-exit");
    assert!(left_of(&node, &up.0).len() == 3 && left_of(&node, &made.0).len() == 3);

    let mut second = node.open();
    let swept = second.swept();
    let mut runs: Vec<_> = swept.removed.iter().map(|r| r.run_id.as_str()).collect();
    runs.sort_unstable();
    assert_eq!(runs, ["r1-hold", "r2-exit"]);
    let removed = swept.removed.iter().find(|r| r.name == up.0).expect("up");
    assert_eq!(removed.labels[label::RUN], "r1-hold");
    assert_eq!(removed.labels[label::AGENT], "node-1");
    assert!(
        swept.failed.is_empty() && swept.left.is_empty(),
        "{swept:?}"
    );
    for id in [&up, &made] {
        assert!(
            left_of(&node, &id.0).is_empty(),
            "{:?}",
            left_of(&node, &id.0)
        );
    }
    assert!(second.list_labelled().expect("lists").is_empty());
    let again = second
        .stage(&node.spec("r2-exit"))
        .expect("the run id is free");
    assert_eq!(again, made);
    drop(dead);
}

/// Proves a runtime that opens on a store with nothing in it reports nothing, and that one
/// that opens twice in a row sweeps the leases once: the second sweep finds none. Catches a
/// report that invents leftovers.
#[test]
fn a_sweep_of_a_clean_store_reports_nothing_and_is_not_repeated() {
    let node = Node::new("sweep-clean");
    let first = node.open();
    assert_eq!(first.swept(), &SweepReport::default());
    let mut dead = node.open();
    staged(&node, &mut dead, "r1-exit");
    assert_eq!(node.open().swept().removed.len(), 1);
    assert_eq!(node.open().swept(), &SweepReport::default());
}

/// Proves two agents on one store never take each other's leases: the runtime of node 2
/// opens beside node 1's lease and removes nothing (it reports node 1's sidecar as another
/// agent's); node 1 opening again removes its own lease and none of node 2's (directory,
/// cgroup and sidecar stay, and node 2 removes them itself). Catches an agent that sweeps,
/// or reports as removed, a lease of the agent beside it.
#[test]
fn two_agents_on_one_store_never_sweep_each_others_leases() {
    let node = Node::new("sweep-two");
    let mut one = node.open();
    let mine = staged(&node, &mut one, "r1-exit");

    let mut two = open_as(&node, OTHER);
    assert!(two.swept().removed.is_empty(), "{:?}", two.swept());
    assert!(
        matches!(&two.swept().left[..], [Left::Foreign { agent, .. }] if agent == "node-1"),
        "{:?}",
        two.swept()
    );
    assert_eq!(left_of(&node, &mine.0).len(), 3);
    let theirs = staged(&node, &mut two, "r2-exit");
    assert_ne!(theirs.0, mine.0);

    let again = node.open();
    let swept = again.swept();
    assert_eq!(swept.removed.len(), 1, "{swept:?}");
    assert_eq!(swept.removed[0].name, mine.0);
    assert!(
        matches!(&swept.left[..], [Left::Foreign { agent, .. }] if agent == OTHER),
        "{swept:?}"
    );
    assert!(left_of(&node, &mine.0).is_empty());
    assert_eq!(left_of(&node, &theirs.0).len(), 3, "node 2's lease went");

    two.remove(&theirs).expect("removed");
    assert!(left_of(&node, &theirs.0).is_empty());
}

/// Proves the removal of an orphan by run id: a lease another process of the agent made
/// after this runtime opened is removed by its run id (directory, cgroup and sidecar), a
/// second call finds nothing, the run id of another agent's lease finds nothing, and
/// removing the other agent's handle is refused and changes nothing. Catches an orphan
/// that stays, a removal that fails on what is gone, and one that reaches another agent's
/// lease.
#[test]
fn an_orphan_is_removed_by_its_run_id_and_only_this_agents() {
    let node = Node::new("sweep-orphan");
    let mut runtime = node.open();
    let mut other = open_as(&node, OTHER);
    let mut dead = node.open();
    let orphan = staged(&node, &mut dead, "r1-exit");
    // `dead` opened after `runtime`, so the sweep of the first did not see this lease.
    assert!(runtime.remove_orphan("r1-exit").expect("removed"));
    assert!(left_of(&node, &orphan.0).is_empty());
    assert!(!runtime.remove_orphan("r1-exit").expect("again"));

    let theirs = staged(&node, &mut other, "r2-exit");
    assert!(
        !runtime
            .remove_orphan("r2-exit")
            .expect("another agent's run id")
    );
    let refused = runtime.remove(&theirs).expect_err("another agent's lease");
    assert!(refused.to_string().contains(OTHER), "{refused}");
    assert_eq!(left_of(&node, &theirs.0).len(), 3);

    let held = staged(&node, &mut runtime, "r3-exit");
    let why = runtime.remove_orphan("r3-exit").expect_err("a held run");
    assert!(why.to_string().contains("not an orphan"), "{why}");
    assert_eq!(left_of(&node, &held.0).len(), 3);
}

/// Proves `Runtime::remove` of a handle the runtime does not hold removes the leftover of
/// that name, as the lifecycle's eviction by container id does, and is quiet for a name
/// nothing made. Catches an eviction that reports a removal and leaves the lease.
#[test]
fn removing_a_handle_the_runtime_does_not_hold_removes_the_leftover() {
    let node = Node::new("sweep-handle");
    let mut runtime = node.open();
    let mut dead = node.open();
    let orphan = staged(&node, &mut dead, "r1-exit");
    runtime.remove(&orphan).expect("removed");
    assert!(left_of(&node, &orphan.0).is_empty());
    runtime.remove(&orphan).expect("quiet");
    runtime
        .remove(&RunHandle("js-0000000000000000-never".to_owned()))
        .expect("quiet for a name nothing made");
}

/// Proves a crash between the sidecar and the lease is swept, and the other order is not
/// touched: a sidecar with no directory, no cgroup and no container is removed and
/// reported; a lease directory with no sidecar (not made by this code) stays. Catches a
/// sweep that needs the whole lease to be there, and one that removes what it cannot
/// attribute.
#[test]
fn a_sidecar_without_its_lease_is_swept_and_a_lease_without_one_is_not() {
    let node = Node::new("sweep-crash");
    let mut runtime = node.open();
    let id = staged(&node, &mut runtime, "r1-exit");
    let body = std::fs::read(sidecar(&node, &id.0)).expect("read");
    runtime.remove(&id).expect("removed");
    assert!(left_of(&node, &id.0).is_empty());
    std::fs::write(sidecar(&node, &id.0), body).expect("write back");
    let stray = node.lease_of("js-ffffffffffffffff-r9");
    std::fs::create_dir_all(&stray).expect("a lease with no sidecar");

    let second = node.open();
    assert_eq!(second.swept().removed.len(), 1, "{:?}", second.swept());
    assert_eq!(second.swept().removed[0].name, id.0);
    assert!(second.swept().failed.is_empty() && second.swept().left.is_empty());
    assert!(!sidecar(&node, &id.0).exists());
    assert!(stray.is_dir());
}

/// Proves a lease that cannot be removed keeps its sidecar and is tried again at the next
/// open: with `crun delete` failing, the lease is reported as failed, nothing of it is
/// reported as removed and its sidecar and directory stay; once `delete` works the next open
/// removes it. Catches a sweep that forgets a lease it could not remove.
#[test]
fn a_lease_that_cannot_be_removed_is_kept_for_the_next_open() {
    let node = Node::new("sweep-stuck");
    let mut dead = node.open();
    let id = dead
        .create_and_start(&node.spec("r1-exit"))
        .expect("started");
    dead.wait_ended(&id);
    std::fs::write(node.dir.join("delete-fails"), "").expect("switch");

    let second = node.open();
    let swept = second.swept();
    assert!(swept.removed.is_empty(), "{swept:?}");
    assert_eq!(swept.failed.len(), 1, "{swept:?}");
    assert_eq!(swept.failed[0].name, id.0);
    // `crun delete` failing is where it stops, or (the stand-in's state being gone) the
    // cgroup it left; either way the lease is reported with the step.
    let why = &swept.failed[0].why;
    assert!(
        why.contains("delete the container") || why.contains("remove the lease's cgroup"),
        "{swept:?}"
    );
    assert!(sidecar(&node, &id.0).is_file() && node.lease_of(&id.0).is_dir());

    std::fs::remove_file(node.dir.join("delete-fails")).expect("switch off");
    let third = node.open();
    assert_eq!(third.swept().removed.len(), 1, "{:?}", third.swept());
    assert!(left_of(&node, &id.0).is_empty());
}

/// Proves a sidecar the runtime cannot read as its own is left and reported, and the
/// runtime still opens and sweeps the good one beside it: garbage and a sidecar under
/// another lease's name stay where they are. Catches a sweep that acts on, or stops at, a
/// file it cannot read.
#[test]
fn a_malformed_sidecar_is_left_alone_and_reported() {
    let node = Node::new("sweep-malformed");
    let mut dead = node.open();
    let id = staged(&node, &mut dead, "r1-exit");
    let good = std::fs::read(sidecar(&node, &id.0)).expect("read");
    let junk = sidecar(&node, "js-0000000000000001-junk");
    let moved = sidecar(&node, "js-0000000000000002-moved");
    std::fs::write(&junk, "not json").expect("write");
    std::fs::write(&moved, good).expect("write");

    let second = node.open();
    let swept = second.swept();
    assert_eq!(swept.removed.len(), 1, "{swept:?}");
    let paths: Vec<_> = swept
        .left
        .iter()
        .map(|left| match left {
            Left::Malformed { path, .. } => path.clone(),
            Left::Foreign { .. } => panic!("not foreign: {left:?}"),
        })
        .collect();
    assert_eq!(paths, [junk.clone(), moved.clone()]);
    assert!(junk.is_file() && moved.is_file());
}

/// Proves a run that cannot be recorded is refused and makes nothing: with the sidecar of
/// its name there already (a lease of another process of the agent), the start fails naming
/// the labels, and neither a directory nor a cgroup is made for it. Catches two leases of
/// one name, the second hiding the first from the sweep.
#[test]
fn a_run_whose_sidecar_exists_is_refused_before_anything_is_made() {
    let node = Node::new("sweep-twice");
    let mut one = node.open();
    let mut two = node.open();
    let id = staged(&node, &mut one, "r1-exit");
    let why = two
        .stage(&node.spec("r1-exit"))
        .expect_err("a second lease");
    assert!(
        why.to_string().contains("record the lease's labels"),
        "{why}"
    );
    assert_eq!(left_of(&node, &id.0).len(), 3, "the first lease is intact");
    assert!(two.list_labelled().expect("lists").is_empty());
}

/// Proves the orphan removal refuses what it cannot attribute: a run id that cannot name a
/// lease is an error, and a sidecar of the lease's name that is not well formed is left
/// where it is with the reason. Catches an orphan removal that removes a lease on the word
/// of a file it cannot read.
#[test]
fn an_orphan_removal_refuses_a_bad_run_id_and_a_sidecar_it_cannot_read() {
    let node = Node::new("sweep-unreadable");
    let mut runtime = node.open();
    assert!(runtime.remove_orphan("a/b").is_err());
    let mut dead = node.open();
    let id = staged(&node, &mut dead, "r1-exit");
    std::fs::write(sidecar(&node, &id.0), "not json").expect("write");
    let why = runtime.remove_orphan("r1-exit").expect_err("malformed");
    assert!(why.to_string().contains("not well formed"), "{why}");
    assert_eq!(left_of(&node, &id.0).len(), 3);
}

/// Proves a runtime does not open where it cannot keep its ledger: with a file where the
/// ledger's directory goes, opening fails naming it, and nothing is swept. Catches a
/// runtime that runs without the record that lets its leases be cleaned up.
#[test]
fn a_runtime_does_not_open_without_its_ledger() {
    let node = Node::new("sweep-noledger");
    std::fs::create_dir_all(&node.scratch).expect("scratch");
    std::fs::write(node.scratch.join("oci-labels"), "a file").expect("write");
    let why = CrunRuntime::open_with(
        node.settings("/root"),
        Arc::new(NoImages),
        Node::hooks(LONG, LONG),
    )
    .expect_err("no ledger");
    assert!(why.to_string().contains("oci-labels"), "{why}");
}

/// Proves a removal that cannot take the sidecar last is an error and keeps the container
/// listed: with a directory where the sidecar was, the lease is gone and the call fails
/// naming the sidecar. Catches a removal reported done while a sidecar would make the next
/// start report a run it already ended.
#[test]
fn a_removal_that_cannot_remove_the_sidecar_fails() {
    let node = Node::new("sweep-sidecar-stuck");
    let mut runtime = node.open();
    let id = staged(&node, &mut runtime, "r1-exit");
    let file = sidecar(&node, &id.0);
    std::fs::remove_file(&file).expect("remove");
    std::fs::create_dir(&file).expect("a directory");
    let why = runtime.remove(&id).expect_err("not removed");
    assert!(
        why.to_string().contains("remove the lease's sidecar"),
        "{why}"
    );
    assert_eq!(runtime.list_labelled().expect("lists").len(), 1);
}

/// A runtime on `node` that makes its leases in `runs`, registered as `host_id`.
fn open_beside(node: &Node, host_id: &str, runs: &std::path::Path) -> CrunRuntime {
    let mut settings = node.settings("/root");
    settings.host_id = host_id.to_owned();
    settings.oci.leases_in = Some(runs.to_owned());
    CrunRuntime::open_with(settings, Arc::new(NoImages), Node::hooks(LONG, LONG)).expect("opens")
}

/// The directory name a lease of `run` has in the store for the agent `runtime` is: the
/// leaf of the lease directory it names, less the prefix.
fn store_name(runtime: &CrunRuntime, run: &str) -> String {
    let lease = runtime.lease_dir(run).expect("a lease directory");
    let leaf = lease.file_name().and_then(|n| n.to_str()).expect("a name");
    leaf.strip_prefix(".lease-").expect("the prefix").to_owned()
}

/// Proves an upgrade leaves nothing in the store: a runtime that makes its leases beside the
/// runs removes, as it opens, the leases of its own agent that an agent from before left in the
/// store (`js-<tag>-<run>`, whatever the ledger says of them), and leaves another agent's
/// lease and the store's own directories. Catches a lease of the old layout that stays in the
/// store for ever, unswept and uncounted, and a sweep that takes another agent's lease.
#[test]
fn the_start_up_sweep_removes_the_own_leases_an_older_agent_left_in_the_store() {
    let node = Node::new("sweep-legacy");
    let runs = node.dir.join("leases-beside");
    std::fs::create_dir(&runs).expect("runs");
    let (mine, theirs) = {
        let probe = open_beside(&node, "node-1", &runs);
        let other = open_beside(&node, OTHER, &runs);
        (store_name(&probe, "old"), store_name(&other, "old"))
    };
    for name in [&mine, &theirs] {
        let dir = node.lease_of(name);
        std::fs::create_dir_all(dir.join("upper")).expect("a lease");
        std::fs::write(dir.join("upper/file"), b"left").expect("a file");
    }
    let _runtime = open_beside(&node, "node-1", &runs);
    assert!(!node.lease_of(&mine).exists(), "the own lease is removed");
    assert!(node.lease_of(&theirs).is_dir(), "another agent's stays");
    assert!(
        node.scratch.join("oci-images").is_dir(),
        "so does the store"
    );
}

/// Proves the removal of one lease takes the store's copy of it too: a lease of the old layout
/// that the ledger names is removed with its cgroup when the runtime removes it as an orphan.
/// Catches a lease whose sidecar is dropped while its directory stays in the store.
#[test]
fn removing_an_orphan_removes_its_lease_in_the_store() {
    let node = Node::new("sweep-legacy-orphan");
    let runs = node.dir.join("leases-beside");
    std::fs::create_dir(&runs).expect("runs");
    let mut runtime = open_beside(&node, "node-1", &runs);
    // The old layout: the lease is made in the store, with its sidecar.
    let mut old = node.open();
    let id = staged(&node, &mut old, "r1-exit");
    drop(old);
    assert!(node.lease_of(&id.0).is_dir(), "the lease is in the store");
    assert!(runtime.remove_orphan("r1-exit").expect("removed"));
    assert!(
        left_of(&node, &id.0).is_empty(),
        "{:?}",
        left_of(&node, &id.0)
    );
}

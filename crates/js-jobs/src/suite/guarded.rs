//! The guarded view of a job store ([`crate::JobStore::guarded`]) and [`crate::JobStore::settle_node`]:
//! writes conditional on a host's node row staying at a version, and the row's state written at
//! that version.

use js_agent_wire::Phase;
use js_state::conformance::Backend;
use js_state::{Store, Version};
use js_types::HostId;

use super::{host, job_id, rig, share};
use crate::{
    JobState, JobStore, JobStoreError, LoadReading, NewJob, NodeHello, NodeRow, NodeState,
    Placement, SessionDigest, keys,
};

/// What a hello announces in these checks, from an agent process `proc-1`.
fn hello() -> NodeHello {
    NodeHello {
        session: Some(SessionDigest::new([3; 32])),
        pool: "pool-g".to_owned(),
        claim: None,
        shape: None,
        labels: std::collections::BTreeMap::new(),
        phase: Phase::Serving,
        agent_process: Some("proc-1".to_owned()),
    }
}

/// A job to queue under `text`.
fn new_job(text: &str) -> NewJob {
    NewJob {
        id: Some(job_id(text)),
        spec: super::full_spec(),
        priority: 0,
        client: None,
    }
}

/// The version `host`'s node row stands at.
fn version_of<S: Store, I: crate::IdSource>(jobs: &JobStore<S, I>, host: &HostId) -> Version {
    jobs.read_node(host).expect("reads").expect("a row").version
}

/// A guarded view writes while the node row is at its version, and a write after the row moved
/// is [`JobStoreError::NodeMoved`] that writes nothing: the job it would insert is absent, and
/// the store's rows are as they were. The job store the view was made from is not guarded: it
/// writes with the row at any version, and with no row at all. Catches a guard that is not
/// checked (a stale beat's listing still writes), a guard that refuses at the right version,
/// and a guard that leaks into the store the view was made from.
pub fn a_guarded_write_commits_at_the_node_version_and_fails_after_it_moved<B: Backend>(
    backend: &B,
) {
    let name = "a_guarded_write_commits_at_the_node_version_and_fails_after_it_moved";
    let rig = rig(backend);
    let node = host("host-g");
    let said = rig.jobs.hello_node(&node, &hello()).expect("a hello");
    let guarded = rig.jobs.guarded(&node, said.version);

    let inserted = guarded.insert_queued(new_job("g-one")).expect("commits");
    assert!(
        inserted.created,
        "{name}: a write at the row's version was not made"
    );

    rig.tick();
    rig.jobs
        .beat_node(&node, said.record.epoch, Some(1), |row| row.state.clone())
        .expect("a beat");
    let before = rig.snapshot();
    assert_eq!(
        guarded.insert_queued(new_job("g-two")).map(drop),
        Err(JobStoreError::NodeMoved { host: node.clone() }),
        "{name}: a write after the row moved was not refused"
    );
    assert_eq!(
        rig.snapshot().0,
        before.0,
        "{name}: a refused write changed the store"
    );
    assert!(
        rig.raw(&keys::job(&job_id("g-two"))).is_none(),
        "{name}: the refused job was recorded"
    );

    assert!(
        rig.jobs
            .insert_queued(new_job("g-three"))
            .expect("commits")
            .created,
        "{name}: the unguarded store was held to the row"
    );
    let forgotten = rig.jobs.forget_node(&node, version_of(&rig.jobs, &node));
    assert_eq!(forgotten, Ok(true), "{name}: the row was not forgotten");
    assert_eq!(
        guarded.insert_queued(new_job("g-four")).map(drop),
        Err(JobStoreError::NodeMoved { host: node }),
        "{name}: a write with the row gone was not refused"
    );
    assert!(
        rig.jobs
            .insert_queued(new_job("g-five"))
            .expect("commits")
            .created,
        "{name}: an unguarded write needed a node row"
    );
}

/// A guarded view whose write conflicts on another row (the job it inserts was recorded by
/// another server between its read and its commit) answers as the job store does, from that
/// row: the view's guard is only for the node row. Catches a guarded store that answers every
/// conflict as the node row moving.
pub fn a_conflict_on_another_row_is_not_the_node_moving<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_conflict_on_another_row_is_not_the_node_moving";
    let rig = rig(backend);
    let node = host("host-g");
    let said = rig.jobs.hello_node(&node, &hello()).expect("a hello");
    let guarded = rig.jobs.guarded(&node, said.version);
    let other = rig.other();
    rig.store.before_next(move || {
        other.insert_queued(new_job("g-race")).expect("the other's");
    });
    let answer = guarded.insert_queued(new_job("g-race")).expect("answers");
    assert!(
        !answer.created,
        "{name}: the job the other recorded was recorded again"
    );
}

/// A guarded view shares its store's write count: a write through either moves the one count
/// both read ([`crate::JobStore::written`]), a refused guarded write counts too, and the two
/// readings carry one instance number. Catches a view that counts its writes apart (a reader
/// told the tables did not change when the view changed them).
pub fn a_guarded_view_shares_the_write_count_of_its_store<B: Backend>(backend: &B) {
    let name = "a_guarded_view_shares_the_write_count_of_its_store";
    let rig = rig(backend);
    let node = host("host-g");
    let said = rig.jobs.hello_node(&node, &hello()).expect("a hello");
    let guarded = rig.jobs.guarded(&node, said.version);
    let start = rig.jobs.written().expect("reads");
    assert_eq!(
        guarded.written().expect("reads"),
        start,
        "{name}: the view reads another count"
    );
    guarded.insert_queued(new_job("w-one")).expect("commits");
    let after_view = rig.jobs.written().expect("reads");
    assert_ne!(
        after_view, start,
        "{name}: a write of the view did not move its store's reading"
    );
    assert_eq!(
        guarded.written().expect("reads"),
        after_view,
        "{name}: the two readings differ"
    );
    rig.jobs.insert_queued(new_job("w-two")).expect("commits");
    let after_store = rig.jobs.written().expect("reads");
    assert_eq!(
        guarded.written().expect("reads"),
        after_store,
        "{name}: the store's write was not counted by the view"
    );
    rig.tick();
    rig.jobs
        .beat_node(&node, said.record.epoch, Some(1), |row| row.state.clone())
        .expect("a beat");
    let before_refused = guarded.written().expect("reads");
    assert!(guarded.insert_queued(new_job("w-three")).is_err());
    assert_ne!(
        guarded.written().expect("reads"),
        before_refused,
        "{name}: a refused guarded write was not counted"
    );
}

/// A guarded view's [`crate::JobStore::place_claimed`] carries the guard in its own commit: at
/// the row's version it places the job; after the row moved it is [`JobStoreError::NodeMoved`],
/// the job stays queued and nothing is booked; and an error of the placement itself (a job
/// that is not queued) is still that error, not a moved row. Catches a claimed placement that
/// bypasses the guard, a conflict on the job's row answered as the row moving, and a refused
/// placement that wrote.
pub fn a_guarded_claimed_placement_honours_the_node_guard<B: Backend>(backend: &B) {
    let name = "a_guarded_claimed_placement_honours_the_node_guard";
    let rig = rig(backend);
    let node = host("host-g");
    let said = rig.jobs.hello_node(&node, &hello()).expect("a hello");
    let guarded = rig.jobs.guarded(&node, said.version);
    let plain = Placement::default();
    let first = rig.queue("p-first");
    let second = rig.queue("p-second");

    let (job, _) = guarded
        .place_claimed(&first, &node, share(), &plain, None)
        .expect("a placement at the row's version");
    assert_eq!(job.state, JobState::Placed, "{name}: not placed");

    rig.jobs
        .beat_node(&node, said.record.epoch, Some(1), |row| row.state.clone())
        .expect("a beat");
    let before = rig.snapshot().0;
    assert_eq!(
        guarded
            .place_claimed(&second, &node, share(), &plain, None)
            .map(drop),
        Err(JobStoreError::NodeMoved { host: node.clone() }),
        "{name}: a placement after the row moved was not refused"
    );
    assert_eq!(
        rig.snapshot().0,
        before,
        "{name}: a refused placement wrote"
    );
    assert_eq!(
        guarded
            .place_claimed(&first, &node, share(), &plain, None)
            .map(drop),
        Err(JobStoreError::UnexpectedState {
            id: first,
            expected: JobState::Queued,
            found: JobState::Placed
        }),
        "{name}: a placement's own error was answered as the row moving"
    );
}

/// [`crate::JobStore::settle_node`] writes the row it is given with the state it is given, at
/// the version of that row: the epoch, session, agent process, hello fields, last beat and
/// `heard_at` are kept, and the row written is the row read back, at a new version. A second
/// settle at the old version returns `None` and writes nothing, as does one after a beat
/// moved the row, and the guarded view made at the old version is refused after the settle.
/// Catches a settle that drops a field of the row (the agent process included), writes over a
/// row that moved, or leaves the version where it was.
pub fn settle_node_writes_at_the_version_and_keeps_the_other_fields<B: Backend>(backend: &B) {
    let name = "settle_node_writes_at_the_version_and_keeps_the_other_fields";
    let rig = rig(backend);
    let node = host("host-g");
    let said = rig.jobs.hello_node(&node, &hello()).expect("a hello");
    assert_eq!(
        said.record.agent_process.as_deref(),
        Some("proc-1"),
        "{name}: the hello's agent process"
    );
    rig.tick();
    rig.jobs
        .beat_node(&node, said.record.epoch, Some(4), |row| row.state.clone())
        .expect("a beat");
    let applied = rig.jobs.read_node(&node).expect("reads").expect("a row");
    let settled_state = NodeState {
        first_listing_pending: false,
        last_listing_clean: false,
        load: Some(LoadReading {
            seq: 9,
            ..LoadReading::default()
        }),
        missing_in_a_row: [(job_id("m-one"), 1)].into(),
        ..applied.record.state.clone()
    };
    rig.tick();
    let settled = rig
        .jobs
        .settle_node(&applied, settled_state.clone())
        .expect("settles")
        .expect("the row was at its version");
    assert_eq!(
        settled.record.state, settled_state,
        "{name}: the state was not written"
    );
    let mut kept = settled.record.clone();
    kept.state = applied.record.state.clone();
    assert_eq!(
        kept, applied.record,
        "{name}: a field other than the state changed"
    );
    assert!(
        settled.version > applied.version,
        "{name}: the version did not move on"
    );
    assert_eq!(
        rig.jobs.read_node(&node).expect("reads"),
        Some(settled.clone()),
        "{name}: the row written is not the row stored"
    );

    let before = rig.snapshot().0;
    assert_eq!(
        rig.jobs.settle_node(&applied, settled_state),
        Ok(None),
        "{name}: a settle at an old version wrote"
    );
    assert_eq!(rig.snapshot().0, before, "{name}: a refused settle wrote");

    let stale: NodeRow = applied;
    let guarded = rig.jobs.guarded(&node, stale.version);
    assert_eq!(
        guarded.insert_queued(new_job("s-one")).map(drop),
        Err(JobStoreError::NodeMoved { host: node.clone() }),
        "{name}: a view guarded at the old version still writes after the settle"
    );
    assert!(
        rig.raw(&keys::node(&node)).is_some(),
        "{name}: the row is gone"
    );
}

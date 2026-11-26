//! The scaler's scale-down marks as rows of the job store: a mark put through one server's
//! effects keeps another server's placement pass off the host, a lift opens it, a write that
//! loses a race is made again, and a write that fails is put right by the next tick's mirror.
//! Every clock is manual; nothing sleeps.

use std::sync::Arc;

use js_jobs::interfere::Interfere;
use js_jobs::{JobState, ScaleMark};
use js_registry::HostMark;
use js_state::fake::Fake;
use js_state::{ManualClock as StoreClock, Store};
use js_testkit::dir::TestDir;
use js_types::HostId;

use super::Live;
use super::tests::live;
use crate::agents::Agents;
use crate::job_store::{ServerIds, ServerJobs};
use crate::scaler::{Call, Effects};
use crate::sched::cordon_tests::{h1, jobs_of, queue, server, state, store_on};

/// Two servers over one store, with the scaler's effects over the first one's agents.
fn two(name: &str) -> Rig {
    let store = store_on(&Arc::new(StoreClock::default()));
    let (a, dir_a, _) = server(&format!("{name}-a"), Some(&store));
    let (b, dir_b, _) = server(&format!("{name}-b"), Some(&store));
    let (mut effects, dir) = live(name);
    effects.agents = Arc::clone(&a);
    (effects, a, b, store, [dir, dir_a, dir_b])
}

/// The effects over server A, both servers, their store and the directories they use.
type Rig = (
    Live,
    Arc<Agents>,
    Arc<Agents>,
    Arc<Interfere<Fake>>,
    [TestDir; 3],
);

/// A job table over the store beneath the interference, so that a write through it is not
/// counted as a commit by the hooks and the failures of the store.
fn raw(store: &Arc<Interfere<Fake>>) -> Arc<ServerJobs> {
    let dynamic: Arc<dyn Store> = store.inner();
    Arc::new(js_jobs::JobStore::new(Arc::new(dynamic), ServerIds::random()).unwrap())
}

/// The scale-down mark in the row of `h1`.
fn row(agents: &Agents) -> Option<ScaleMark> {
    let read = jobs_of(agents).read_host_mark(&h1()).unwrap();
    read.map(|(mark, _)| mark)
}

/// The mark the registry of `agents` holds on `h1`.
fn held(agents: &Agents) -> Option<HostMark> {
    let hosts = agents.registry.hosts();
    hosts.into_iter().find(|host| host.id == h1()).unwrap().mark
}

/// Asserts the mark through two servers: a drain by the scaler's effects over server A writes
/// the row, server B's pass places no job on `h1` while B's registry has not heard of it, and
/// B's tick puts the mark on B's registry; an undrain deletes the row, and after B's tick B's
/// pass places the job on `h1`. Catches a mark kept only in the registry of the server that
/// made it, which lets every other server place jobs on a host being scaled away.
#[tokio::test]
async fn a_scaler_drain_through_one_server_keeps_every_servers_pass_off_the_host() {
    let (effects, a, b, _store, _dirs) = two("marks-two");
    queue(jobs_of(&b), "job-a");
    effects.call(Call::Drain, "h1");
    assert_eq!(row(&a), Some(ScaleMark::Draining));
    assert_eq!(held(&b), None, "B's registry has not heard of it");
    b.place_jobs();
    assert_eq!(state(jobs_of(&b), "job-a"), JobState::Queued);
    b.mirror_host_marks().await;
    assert_eq!(held(&b), Some(HostMark::Draining));

    effects.call(Call::Undrain, "h1");
    assert_eq!(row(&a), None);
    b.mirror_host_marks().await;
    assert_eq!(held(&b), None);
    b.place_jobs();
    assert_eq!(state(jobs_of(&b), "job-a"), JobState::Placed);
}

/// Asserts each call's row: a retire writes draining, a repeated drain finds the row it
/// wrote and writes nothing more, a release writes releasing, a cordon writes no mark row,
/// a drain the registry refuses (the host is releasing) leaves the row, a remove deletes the
/// row of the host it removes, and a mark of ending for a host the registry does not hold
/// still writes releasing. Catches a call whose mark never reaches the store, and a remove
/// that leaves its row behind to refuse the name's next host.
#[test]
fn each_scale_down_call_writes_or_deletes_the_hosts_row() {
    let (effects, a, _b, _store, _dirs) = two("marks-calls");
    effects.call(Call::Retire, "h1");
    assert_eq!(row(&a), Some(ScaleMark::Draining));
    effects.call(Call::Drain, "h1");
    assert_eq!(row(&a), Some(ScaleMark::Draining));
    effects.call(Call::Cordon, "h1");
    assert_eq!(row(&a), Some(ScaleMark::Draining));
    effects.call(Call::ReleaseHost, "h1");
    assert_eq!(row(&a), Some(ScaleMark::Releasing));
    effects.call(Call::Drain, "h1");
    assert_eq!(row(&a), Some(ScaleMark::Releasing));
    assert_eq!(effects.remove("h1"), Ok(()));
    assert_eq!(row(&a), None);
    effects.call(Call::MarkEnding, "ghost");
    let ghost = HostId::new("ghost").unwrap();
    let read = jobs_of(&a).read_host_mark(&ghost).unwrap();
    assert_eq!(read.map(|(mark, _)| mark), Some(ScaleMark::Releasing));
}

/// A hello into the pool `burst`, as the node rows hold it.
fn hello_row() -> js_jobs::NodeHello {
    js_jobs::NodeHello {
        session: None,
        pool: "burst".to_owned(),
        claim: None,
        shape: None,
        labels: std::collections::BTreeMap::new(),
        phase: js_agent_wire::Phase::Serving,
        agent_process: None,
        agent_version: None,
    }
}

/// Asserts that removing a released host deletes its `node/` row as well as its mark: the
/// row is gone, and a new host is admitted into a pool whose cap the removed host's row had
/// filled. Catches a released machine that keeps counting against its pool's cap (and stays
/// offered to every server while its row reads live) after the registry let it go.
#[test]
fn a_removed_host_leaves_no_row_to_fill_its_pool() {
    let (effects, a, _b, _store, _dirs) = two("marks-removed-row");
    let jobs = jobs_of(&a);
    jobs.hello_node(&h1(), &hello_row()).unwrap();
    let next = HostId::new("h2").unwrap();
    assert!(matches!(
        jobs.hello_node_in_pool(&next, &hello_row(), 1),
        Err(js_jobs::JobStoreError::PoolFull { .. })
    ));
    effects.call(Call::ReleaseHost, "h1");
    assert_eq!(effects.remove("h1"), Ok(()));
    assert_eq!(jobs.read_node(&h1()).unwrap(), None);
    assert_eq!(row(&a), None);
    assert!(jobs.hello_node_in_pool(&next, &hello_row(), 1).is_ok());
}

/// Asserts that a removed host's row that a beat moves between the read and the delete is
/// read again and deleted, that one moved under both tries is left (to be forgotten once
/// silent), and that a row that cannot be read is left; the mark row goes either way. Catches
/// a delete that gives up a row it could still delete, and a retry without an end.
#[test]
fn a_removed_hosts_row_that_moves_is_read_again() {
    let (effects, a, _b, store, _dirs) = two("marks-removed-moved");
    let jobs = jobs_of(&a);
    let epoch = jobs.hello_node(&h1(), &hello_row()).unwrap().record.epoch;
    let beat = move |other: &ServerJobs| {
        other
            .beat_node(&h1(), epoch, None, |row| row.state.clone())
            .unwrap();
    };
    let other = raw(&store);
    let once = Arc::clone(&other);
    store.before_next(move || beat(&once));
    effects.call(Call::ReleaseHost, "h1");
    // The release's own mark write takes the first hook; arm the beat for the delete.
    let (first, again) = (Arc::clone(&other), Arc::clone(&store));
    store.before_next(move || {
        beat(&first);
        let second = Arc::clone(&first);
        again.before_next(move || beat(&second));
    });
    assert_eq!(effects.remove("h1"), Ok(()));
    assert!(
        jobs.read_node(&h1()).unwrap().is_some(),
        "moved under both tries"
    );
    assert_eq!(row(&a), None);

    jobs.hello_node(&HostId::new("h3").unwrap(), &hello_row())
        .unwrap();
    effects.call(Call::MarkEnding, "h1");
    store.fail_read_after(0);
    assert_eq!(effects.remove("h1"), Ok(()));
    assert!(
        jobs.read_node(&h1()).unwrap().is_some(),
        "an unread row is left"
    );
}

/// Asserts that a drain never lowers a releasing row another server wrote: the row stays
/// releasing and the registry keeps the draining mark it took until the tick's mirror makes it
/// releasing. Catches a draining write over a releasing row, which would reopen a host whose
/// machine is being given back.
#[tokio::test]
async fn a_drain_does_not_lower_a_releasing_row() {
    let (effects, a, _b, store, _dirs) = two("marks-lower");
    raw(&store)
        .mark_host(&h1(), ScaleMark::Releasing, None)
        .unwrap();
    effects.call(Call::Drain, "h1");
    assert_eq!(row(&a), Some(ScaleMark::Releasing));
    assert_eq!(held(&a), Some(HostMark::Draining));
    a.mirror_host_marks().await;
    assert_eq!(held(&a), Some(HostMark::Releasing));
}

/// Asserts that a mark row written between the read and the write makes the write read again
/// and succeed: an undrain finds the row rewritten by another server and still deletes it. A
/// drain whose row another server wrote first (absent when read) reads again, finds it
/// already draining and writes nothing. Catches a write that gives up at the first lost race,
/// which would leave the host marked on every server after the scaler undrained it.
#[test]
fn a_mark_written_in_between_makes_the_write_read_again() {
    let (effects, a, _b, store, _dirs) = two("marks-retry");
    effects.call(Call::Drain, "h1");
    let other = raw(&store);
    let hooked = Arc::clone(&other);
    store.before_next(move || {
        rewrite(&hooked);
    });
    effects.call(Call::Undrain, "h1");
    assert_eq!(row(&a), None, "deleted at the version read the second time");

    let hooked = Arc::clone(&other);
    store.before_next(move || {
        hooked.mark_host(&h1(), ScaleMark::Draining, None).unwrap();
    });
    effects.call(Call::Drain, "h1");
    assert_eq!(row(&a), Some(ScaleMark::Draining));
}

/// Asserts that a write that loses its race twice is given up: the row stays as the other
/// writer left it and nothing else changes. Catches a retry without an end.
#[test]
fn a_write_that_loses_twice_is_given_up() {
    let (effects, a, _b, store, _dirs) = two("marks-twice");
    effects.call(Call::Drain, "h1");
    let other = raw(&store);
    let (first, second) = (Arc::clone(&other), Arc::clone(&other));
    let after = Arc::clone(&store);
    store.before_next(move || {
        rewrite(&first);
        after.before_next(move || rewrite(&second));
    });
    effects.call(Call::Undrain, "h1");
    assert_eq!(row(&a), Some(ScaleMark::Draining));
}

/// Asserts that a mark whose row cannot be read is left to the next tick's mirror: the
/// registry holds the draining mark and no row is written. Catches a failed read taken for
/// an absent row, which would write the mark over a row this server could not read.
#[test]
fn a_mark_whose_row_cannot_be_read_writes_nothing() {
    let (effects, a, _b, store, _dirs) = two("marks-unread");
    store.fail_read_after(0);
    effects.call(Call::Drain, "h1");
    assert_eq!(row(&a), None);
    assert_eq!(held(&a), Some(HostMark::Draining));
}

/// Rewrites the row of `h1` as draining, a new version of it.
fn rewrite(jobs: &ServerJobs) {
    let (_, at) = jobs.read_host_mark(&h1()).unwrap().unwrap();
    assert!(
        jobs.mark_host(&h1(), ScaleMark::Draining, Some(at))
            .unwrap()
    );
}

/// Asserts that a mark whose row write fails is logged and left: the registry holds the
/// draining mark with no row, and the next tick's mirror puts the registry back to the rows
/// (no mark); a mirror whose read fails keeps what the registry holds and counts the error.
/// Catches a registry that keeps a mark no other server sees, forever.
#[tokio::test]
async fn a_failed_row_write_is_mirrored_back_at_the_next_tick() {
    let (effects, a, _b, store, _dirs) = two("marks-failed");
    store.fail_next();
    effects.call(Call::Drain, "h1");
    assert_eq!(row(&a), None);
    assert_eq!(held(&a), Some(HostMark::Draining));
    store.fail_read_after(0);
    a.mirror_host_marks().await;
    assert_eq!(
        held(&a),
        Some(HostMark::Draining),
        "kept when the read fails"
    );
    assert_eq!(a.errors(), 1);
    a.mirror_host_marks().await;
    assert_eq!(held(&a), None);
}

/// Asserts that a server without a job store takes the call on its registry (the host is
/// marked releasing there, with no store to write a row to), and that its tick's mirror leaves
/// the mark as it is. Catches a call that needs the store to mark the host, and a mirror that
/// clears a mark for want of a row it has no store to read.
#[tokio::test]
async fn without_a_job_store_the_registry_alone_holds_the_mark() {
    let (mut effects, _dir) = live("marks-none");
    let (agents, _agents_dir, _) = server("marks-none-a", None);
    effects.agents = agents;
    assert!(effects.agents.jobs.is_none());
    assert_eq!(held(&effects.agents), None);
    effects.call(Call::MarkEnding, "h1");
    assert_eq!(held(&effects.agents), Some(HostMark::Releasing));
    effects.agents.mirror_host_marks().await;
    assert_eq!(held(&effects.agents), Some(HostMark::Releasing));
}

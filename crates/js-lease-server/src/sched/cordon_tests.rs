//! A host's cordon kept by the job store (`agents_cordon`): two servers over one store, where a
//! cordon taken through one keeps the other's pass off the host; the holder rule through the
//! server, with and without the job service; a job store that fails; the scaler's cordon; the
//! tick's mirror; and the cordons put back at start. Every clock is manual; nothing sleeps.

use std::sync::Arc;
use std::time::Duration;

use js_jobs::interfere::Interfere;
use js_jobs::{JobId, JobSpec, JobState, Needs, NewJob};
use js_registry::Credential;
use js_state::fake::{Fake, Flaw};
use js_state::{ManualClock as StoreClock, Store};
use js_testkit::dir::TestDir;
use js_types::cordon::{Asker, Cordon};
use js_types::{CordonHolder, HostId, ManualClock, Resources, Timestamp};

use super::engine_rig::{GIB, T0, hello_of, pool};
use super::wiring_tests::{JOIN, config_of, scratch};
use crate::agent_api::job_rig::spec;
use crate::agents::{Agents, CordonFault, assemble_placed, restore_cordons};
use crate::clock::SharedClock;
use crate::job_store::{ServerIds, ServerJobs};
use crate::util::blocking;

/// The flag a refused start names.
const FLAG: &str = "--job-listen";

/// The one host of every rig.
fn h1() -> HostId {
    HostId::new("h1").unwrap()
}

fn holder(name: &str) -> CordonHolder {
    CordonHolder::new(name).unwrap()
}

/// A store that can fail, on a clock the test moves.
fn store_on(clock: &Arc<StoreClock>) -> Arc<Interfere<Fake>> {
    Arc::new(Interfere::new(Fake::with_flaw(
        Arc::clone(clock),
        Flaw::None,
    )))
}

/// Job tables of one server over `store`, as a second process opens them.
fn tables_over(store: &Arc<Interfere<Fake>>) -> ServerJobs {
    let dynamic: Arc<dyn Store> = store.clone();
    js_jobs::JobStore::new(Arc::new(dynamic), ServerIds::random()).unwrap()
}

/// The agent side of a server under `--placement js-sched` with `h1` registered (8 cores and
/// 16 GiB booked), holding job tables over `store` when there is one; its directory, and the
/// registry's clock.
fn server(name: &str, store: Option<&Arc<Interfere<Fake>>>) -> (Arc<Agents>, TestDir, ManualClock) {
    let dir = scratch(name);
    let config = config_of(dir.path(), &pool(1, 8000, ""), &["--placement", "js-sched"]);
    let clock = ManualClock::new(Timestamp::from_millis(T0));
    let shared = SharedClock::new(clock.clone());
    let assembled = assemble_placed(
        config.agents,
        config.usage,
        &shared,
        &config.state_file,
        None,
    );
    let agents = Arc::try_unwrap(assembled.unwrap().0).ok().unwrap();
    let agents = match store {
        Some(store) => agents.with_jobs(Arc::new(tables_over(store))),
        None => agents,
    };
    let hello = hello_of("h1", 16_000, 32 * GIB);
    agents
        .registry
        .hello(&hello, &Credential::of_token(JOIN))
        .unwrap();
    (Arc::new(agents), dir, clock)
}

/// [`Agents::cordon_now`], from the pool for blocking calls as the scaler calls it.
async fn now_on(agents: &Arc<Agents>, host: &HostId, lease: Duration) -> Result<(), CordonFault> {
    let (agents, host) = (Arc::clone(agents), host.clone());
    blocking(move || agents.cordon_now(&host, lease)).await
}

/// The job tables a rig holds.
fn jobs_of(agents: &Agents) -> &ServerJobs {
    agents.jobs.as_deref().unwrap()
}

/// Queues `id`, one core and one GiB.
fn queue(jobs: &ServerJobs, id: &str) {
    let spec = JobSpec {
        needs: Needs::Shape(Resources::new(1000, GIB, 0, None).unwrap()),
        ..spec()
    };
    let new = NewJob {
        id: Some(JobId::parse(id).unwrap()),
        spec,
        priority: 0,
        client: None,
    };
    jobs.insert_queued(new).unwrap();
}

/// The state of the job `id`.
fn state(jobs: &ServerJobs, id: &str) -> JobState {
    let id = JobId::parse(id).unwrap();
    jobs.get_job(&id).unwrap().unwrap().state
}

/// `left` in whole seconds, rounded up: a commit of the fake store moves its clock on by a
/// few milliseconds, so what a write leaves is just under what it asked for.
fn secs(left: Duration) -> u64 {
    left.as_millis().div_ceil(1000) as u64
}

/// The holder of `h1`'s cordon and its time left in whole seconds (rounded up), as the
/// registry of `agents` holds it.
fn cordon_on(agents: &Agents) -> Option<(Option<String>, u64)> {
    let own = agents.registry.own_host(&h1()).unwrap();
    let left = |until: Timestamp| secs(Duration::from_millis(until.millis() - own.now.millis()));
    let holder = own.cordon_holder.map(|holder| holder.to_string());
    own.cordon_until.map(|until| (holder, left(until)))
}

/// Asserts the incident's case with two servers over one store, where `h1` is open to both and
/// the cordon is taken through server A: server B's pass, whose registry has not heard of the
/// cordon, places no job on `h1` while it holds; a lift that names another holder leaves it.
/// B's tick puts the cordon, with its holder, on B's registry, and keeps `h1` out of B's
/// passes until B's next tick after the holder's own lift, which opens `h1` to B's pass, which
/// places the job there. It
/// catches a cordon that keeps only the server that took it from placing on the host, which is
/// how a cordoned host was given new jobs.
#[tokio::test]
async fn a_cordon_taken_through_one_server_keeps_every_servers_pass_off_the_host() {
    let clock = Arc::new(StoreClock::default());
    let store = store_on(&clock);
    let (a, _dir_a, _) = server("cordon-two-a", Some(&store));
    let (b, _dir_b, _) = server("cordon-two-b", Some(&store));
    let ops = holder("ops-1");
    let lease = Duration::from_secs(600);
    let placed = a.cordon(&h1(), lease, Asker::Holder(ops.clone())).await;
    let placed = placed.unwrap();
    assert!(placed.placed && !placed.held_by_other);
    assert_eq!(cordon_on(&b), None, "B's registry has not heard of it");

    queue(jobs_of(&b), "job-a");
    b.place_jobs();
    assert_eq!(state(jobs_of(&b), "job-a"), JobState::Queued);
    b.mirror_cordons().await;
    assert_eq!(cordon_on(&b), Some((Some("ops-1".to_owned()), 600)));

    let other = a
        .uncordon(&h1(), Some(holder("someone-else")))
        .await
        .unwrap();
    assert!(!other.lifted && other.held_by_other);
    b.place_jobs();
    assert_eq!(state(jobs_of(&b), "job-a"), JobState::Queued);

    let lifted = a.uncordon(&h1(), Some(ops)).await.unwrap();
    assert!(lifted.lifted && !lifted.held_by_other);
    b.place_jobs();
    assert_eq!(
        state(jobs_of(&b), "job-a"),
        JobState::Queued,
        "until B's tick"
    );
    b.mirror_cordons().await;
    assert_eq!(cordon_on(&b), None);
    b.place_jobs();
    assert_eq!(state(jobs_of(&b), "job-a"), JobState::Placed);
    let id = JobId::parse("job-a").unwrap();
    let allocation = jobs_of(&b).get_allocation(&id).unwrap().unwrap();
    assert_eq!(allocation.host, h1());
}

/// Asserts the holder rule through the server with the job service, on the store's clock: the
/// cordon of a requester that died is found by the next requester and left as it was (its
/// time left is not raised to the new request's lease), so it lapses at its own bound, after
/// which the next request places its own; a take-over then makes it the asker's. It catches a
/// requester that keeps a dead holder's cordon alive by asking again, which is what held the
/// incident's host cordoned for as long as a unit re-ran the verb.
#[tokio::test]
async fn a_dead_holders_cordon_is_not_extended_and_lapses_at_its_bound() {
    let clock = Arc::new(StoreClock::default());
    let store = store_on(&clock);
    let (a, _dir, registry) = server("cordon-dead-holder", Some(&store));
    let minute = Duration::from_secs(60);
    let first = a
        .cordon(&h1(), minute, Asker::Holder(holder("run-1")))
        .await;
    assert!(first.unwrap().placed);

    let half = Duration::from_secs(30);
    clock.advance(half);
    registry.advance(half);
    let five = Duration::from_secs(300);
    let next = a.cordon(&h1(), five, Asker::Holder(holder("run-2"))).await;
    let next = next.unwrap();
    assert!(!next.placed && next.held_by_other);
    let left = jobs_of(&a).cordons().unwrap()[&h1()].clone();
    assert_eq!(secs(left.until), 30);
    assert_eq!(left.holder, Some(holder("run-1")));
    assert_eq!(cordon_on(&a), Some((Some("run-1".to_owned()), 30)));

    clock.advance(half);
    registry.advance(half);
    assert!(jobs_of(&a).cordons().unwrap().is_empty(), "it lapsed");
    let own = a.cordon(&h1(), five, Asker::Holder(holder("run-2"))).await;
    let own = own.unwrap();
    assert!(own.placed && !own.held_by_other);
    assert_eq!(cordon_on(&a), Some((Some("run-2".to_owned()), 300)));

    let taken = a
        .cordon(&h1(), minute, Asker::TakeOver(holder("run-3")))
        .await;
    let taken = taken.unwrap();
    assert!(!taken.placed && !taken.held_by_other);
    assert_eq!(cordon_on(&a), Some((Some("run-3".to_owned()), 300)));
}

/// Asserts the holder rule through a server without the job service, which the registry
/// decides: a holder's cordon is placed; another holder finds it and leaves it; a lift by
/// another holder leaves it, the holder's own lift ends it, and a lift of nothing lifts
/// nothing; the tick's mirror changes nothing without the job store; the scaler's cordon is
/// unnamed. It catches a server without the job service that extends or lifts another
/// holder's cordon, or whose tick lifts the cordons it holds.
#[tokio::test]
async fn without_the_job_service_the_registry_decides_by_the_same_rule() {
    let (a, _dir, _) = server("cordon-no-jobs", None);
    let minute = Duration::from_secs(60);
    let first = a
        .cordon(&h1(), minute, Asker::Holder(holder("run-1")))
        .await;
    assert!(first.unwrap().placed);
    let five = Duration::from_secs(300);
    let next = a.cordon(&h1(), five, Asker::Holder(holder("run-2"))).await;
    assert!(next.unwrap().held_by_other);
    assert_eq!(cordon_on(&a), Some((Some("run-1".to_owned()), 60)));

    let other = a.uncordon(&h1(), Some(holder("run-2"))).await.unwrap();
    assert!(!other.lifted && other.held_by_other);
    a.mirror_cordons().await;
    assert_eq!(cordon_on(&a), Some((Some("run-1".to_owned()), 60)));
    let own = a.uncordon(&h1(), Some(holder("run-1"))).await.unwrap();
    assert!(own.lifted && !own.held_by_other);
    assert_eq!(cordon_on(&a), None);
    let nothing = a.uncordon(&h1(), None).await.unwrap();
    assert!(!nothing.lifted && !nothing.held_by_other);

    now_on(&a, &h1(), minute).await.unwrap();
    assert_eq!(cordon_on(&a), Some((None, 60)));
    let ghost = HostId::new("ghost").unwrap();
    assert_eq!(now_on(&a, &ghost, minute).await, Err(CordonFault::NotHeld));
}

/// Asserts what a job store that fails does to a cordon: a cordon whose write fails is
/// `Unsaved` and leaves the host uncordoned in the store and the registry; a lift whose write
/// fails leaves the cordon in both; a tick whose read of the cordons fails is counted as an
/// error and leaves the registry's cordon. It catches a cordon acknowledged without its row,
/// and a failed read that lifts every cordon of the registry.
#[tokio::test]
async fn a_job_store_that_fails_answers_unsaved_and_changes_nothing() {
    let clock = Arc::new(StoreClock::default());
    let store = store_on(&clock);
    let (a, _dir, _) = server("cordon-store-fails", Some(&store));
    let minute = Duration::from_secs(60);
    store.fail_next();
    let failed = a.cordon(&h1(), minute, Asker::Unnamed).await;
    assert_eq!(failed.err(), Some(CordonFault::Unsaved));
    assert_eq!(cordon_on(&a), None);
    assert!(jobs_of(&a).cordons().unwrap().is_empty());

    assert!(
        a.cordon(&h1(), minute, Asker::Unnamed)
            .await
            .unwrap()
            .placed
    );
    store.fail_next();
    let failed = a.uncordon(&h1(), None).await;
    assert_eq!(failed.err(), Some(CordonFault::Unsaved));
    assert_eq!(cordon_on(&a), Some((None, 60)));
    assert_eq!(jobs_of(&a).cordons().unwrap().len(), 1);

    let errors = a.errors();
    store.fail_read_after(0);
    a.mirror_cordons().await;
    assert_eq!(a.errors(), errors + 1);
    assert_eq!(cordon_on(&a), Some((None, 60)));
}

/// Asserts the scaler's cordon with the job service: it is written to the job store, unnamed,
/// so the next tick's mirror keeps it, and a host the registry does not hold is `NotHeld`
/// with no row written. It catches a scaler's cordon that lives in the registry alone and is
/// lifted by the next tick.
#[tokio::test]
async fn the_scalers_cordon_is_kept_by_the_job_store() {
    let clock = Arc::new(StoreClock::default());
    let store = store_on(&clock);
    let (a, _dir, _) = server("cordon-scaler", Some(&store));
    let minute = Duration::from_secs(60);
    now_on(&a, &h1(), minute).await.unwrap();
    let rows = jobs_of(&a).cordons().unwrap();
    let row = rows.get(&h1()).expect("a row");
    assert_eq!((secs(row.until), row.holder.as_ref()), (60, None));
    a.mirror_cordons().await;
    assert_eq!(cordon_on(&a), Some((None, 60)));
    let ghost = HostId::new("ghost").unwrap();
    assert_eq!(now_on(&a, &ghost, minute).await, Err(CordonFault::NotHeld));
    assert_eq!(jobs_of(&a).cordons().unwrap().len(), 1);
}

/// Asserts the cordons put back at start: a cordon the registry holds (from a binding an
/// earlier release wrote) with no row is written into the job store with its time left and
/// holder, a row the store holds already is left as it is, and a store that fails refuses
/// the start naming the flag. It catches a restart onto this release whose first tick lifts
/// every cordon an earlier release kept, and a restore that overwrites the store's record.
#[tokio::test]
async fn the_cordons_a_binding_brought_back_are_written_into_the_job_store() {
    let (a, _dir, _) = server("cordon-restore", None);
    let minute = Duration::from_secs(60);
    a.cordon(&h1(), minute, Asker::Holder(holder("old")))
        .await
        .unwrap();
    let clock = Arc::new(StoreClock::default());
    let store = store_on(&clock);
    let jobs = tables_over(&store);
    restore_cordons(&jobs, &a.registry, FLAG).unwrap();
    let written = jobs.cordons().unwrap()[&h1()].clone();
    assert_eq!(secs(written.until), 60);
    assert_eq!(written.holder, Some(holder("old")));

    let other = tables_over(&store_on(&clock));
    let kept = Cordon {
        until: Duration::from_secs(5),
        holder: Some(holder("newer")),
    };
    other.restore_cordon(&h1(), &kept).unwrap();
    restore_cordons(&other, &a.registry, FLAG).unwrap();
    let left = other.cordons().unwrap()[&h1()].clone();
    assert_eq!((secs(left.until), left.holder), (5, kept.holder));

    let failing = store_on(&clock);
    let broken = tables_over(&failing);
    failing.fail_next();
    let refused = restore_cordons(&broken, &a.registry, FLAG);
    let refused = refused.expect_err("refused");
    assert!(refused.to_string().contains("--job-listen"), "{refused}");
}

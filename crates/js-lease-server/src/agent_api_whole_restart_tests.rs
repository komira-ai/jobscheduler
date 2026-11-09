//! Whole-host jobs across a restart of the server, and what a stored mark is (design sections
//! 5.4, 5.5 and 6.7, and section 8, rows 13, 14 and 40): the marks of queued whole-host jobs
//! are rows of the job store, restored by the engine of a server started again, dropped when
//! their host never says hello, and written beside a store that has job and allocation rows
//! only. The servers are [`whole_rig`]s on one manual clock; nothing sleeps.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::{HeldState, Outcome};
use js_jobs::{
    AllocationStep, BOOT_GRACE, JobId, JobState, JobStore, MarkRecord, Needs, StartFailure, keys,
};
use js_state::{Store, StoreTime, Versioned};

use super::job_rig::{FakeAgent, Rig, START_MS, starts, whole_over, whole_rig};
use super::whole_tests::{
    SECOND, beat, now, pass_time, place_all, queue_share, queue_whole, rules, run_share, served,
};
use crate::job_pass::store_time;
use crate::job_store::ServerIds;

/// A server with `host-a` running the share job `job-run` and the host marked for the
/// whole-host job `job-w`, which may run 600 seconds; the agent, the whole-host job and its
/// mark.
async fn marked(name: &str) -> (Rig, FakeAgent, JobId, MarkRecord) {
    let (rig, mut agent) = served(name);
    run_share(&rig, &mut agent, "job-run", 7200).await;
    let whole = queue_whole(&rig, "job-w", 600);
    place_all(&rig);
    let mark = rig.jobs.get_mark(&whole).unwrap().unwrap();
    assert_eq!(mark.host.as_str(), "host-a");
    (rig, agent, whole, mark)
}

/// The server of `before` started again at `boot_ms` on its tables: a new registry and a new
/// engine, whose hosts have not said hello, and the job store's boot step
/// ([`js_jobs::JobStore::raise_at_boot`]) run as the server runs it when it opens the store.
fn restarted(name: &str, before: &Rig, boot_ms: u64) -> Rig {
    let again = whole_over(name, (rules(), None), Some(before), boot_ms);
    again.jobs.raise_at_boot(store_time(boot_ms)).unwrap();
    again
}

/// The agent of `host-a` after the server restarted: it still holds `job-run`, running.
fn holding(before: &Rig) -> FakeAgent {
    let run = JobId::parse("job-run").unwrap();
    let mut agent = FakeAgent::new("host-a");
    let allocation = before.allocation(&run).allocation_id.to_string();
    agent.hold("job-run", &allocation, HeldState::Running);
    agent
}

/// `at` in milliseconds.
fn millis(at: StoreTime) -> u64 {
    u64::try_from(at.as_duration().as_millis()).unwrap()
}

/// A server is started again while a host is marked for a whole-host job and the host says
/// hello again: the mark stands with the deadline, the time it was made and the tail it was
/// made with, as it was stored, and the pass does not write it. It catches a restart that
/// gives the drain a fresh deadline (a restart loop would then never give up), one that
/// rewrites an unchanged mark, and one that drops it.
#[tokio::test]
async fn a_marked_deadline_is_not_reset_by_a_restart() {
    let (rig, _, whole, mark) = marked("whole_restart_deadline").await;
    let boot_ms = START_MS + 100_000;
    let mut again = restarted("whole_restart_deadline_again", &rig, boot_ms);
    let floor = store_time(boot_ms).saturating_add(BOOT_GRACE);
    assert!(
        mark.deadline > floor,
        "the boot floor is below the deadline"
    );
    assert_eq!(again.jobs.get_mark(&whole).unwrap(), Some(mark.clone()));

    again.register("host-a", true);
    let agent = holding(&rig);
    again.beat(&agent).await.unwrap();
    let before = again.commits();
    place_all(&again);
    assert_eq!(again.commits(), before);
    assert_eq!(again.jobs.get_mark(&whole).unwrap(), Some(mark));
    assert_eq!(again.job(&whole).state, JobState::Queued);
}

/// A server is started again after the boot's floor (the boot plus 120 seconds) went above the
/// stored deadline of a mark: the store raises the deadline to the floor, and the engine takes
/// it as stored, adding no grace of its own. It catches a restart that gives the mark the
/// floor plus the `regain_grace`, one that leaves the stored deadline, and one that moves the
/// mark's other fields.
#[tokio::test]
async fn a_marked_deadline_before_boot_plus_120s_is_raised_to_it() {
    let (rig, _, whole, mark) = marked("whole_restart_floor").await;
    let boot_ms = millis(mark.deadline) - 60_000;
    let mut again = restarted("whole_restart_floor_again", &rig, boot_ms);
    let floor = store_time(boot_ms).saturating_add(BOOT_GRACE);
    let raised = again.jobs.get_mark(&whole).unwrap().unwrap();
    assert!(raised.deadline > mark.deadline);
    assert_eq!(raised.deadline, floor);

    again.register("host-a", true);
    let agent = holding(&rig);
    again.beat(&agent).await.unwrap();
    place_all(&again);
    let stood = again.jobs.get_mark(&whole).unwrap().unwrap();
    assert_eq!(stood, raised);
    assert_eq!(stood.made, mark.made);
}

/// A server is started again and the marked host never says hello. The stored mark stands
/// until the server's start plus `regain_grace` (the job is not queued in the engine meanwhile),
/// and is deleted at that instant; the job stays queued, and when the host does say hello the
/// job is marked again, on a mark made then. It catches a mark dropped at once, one kept for
/// ever, one dropped a tick late or early, a job lost with its mark, and a job not marked
/// again.
#[tokio::test]
async fn a_restored_mark_whose_host_never_says_hello_is_dropped_after_claim_by() {
    let (rig, _, whole, mark) = marked("whole_restart_claim").await;
    let boot_ms = START_MS + 10_000;
    let mut again = restarted("whole_restart_claim_again", &rig, boot_ms);
    place_all(&again);
    assert_eq!(again.jobs.get_mark(&whole).unwrap(), Some(mark.clone()));

    again.advance(14 * SECOND);
    place_all(&again);
    assert_eq!(again.jobs.get_mark(&whole).unwrap(), Some(mark.clone()));
    assert_eq!(again.job(&whole).state, JobState::Queued);

    again.advance(SECOND);
    place_all(&again);
    assert_eq!(again.jobs.get_mark(&whole).unwrap(), None);
    assert_eq!(again.job(&whole).state, JobState::Queued);

    again.register("host-a", true);
    let agent = holding(&rig);
    again.beat(&agent).await.unwrap();
    place_all(&again);
    let marked_again = again.jobs.get_mark(&whole).unwrap().unwrap();
    assert!(marked_again.made > mark.made);
    assert_eq!(marked_again.host.as_str(), "host-a");
}

/// A server is started again and the marked host says hello inside `regain_grace`: the mark
/// stands, unchanged, past the instant it would have been dropped. It catches a mark dropped
/// for a host that did say hello, and one rewritten when the host returns.
#[tokio::test]
async fn a_restored_mark_whose_host_says_hello_in_time_stands() {
    let (rig, _, whole, mark) = marked("whole_restart_hello").await;
    let boot_ms = START_MS + 10_000;
    let mut again = restarted("whole_restart_hello_again", &rig, boot_ms);
    again.advance(5 * SECOND);
    again.register("host-a", true);
    let mut agent = holding(&rig);
    again.beat(&agent).await.unwrap();
    place_all(&again);
    assert_eq!(again.jobs.get_mark(&whole).unwrap(), Some(mark.clone()));

    pass_time(&again, &mut agent, 30).await;
    place_all(&again);
    assert_eq!(again.jobs.get_mark(&whole).unwrap(), Some(mark));
    assert_eq!(again.job(&whole).state, JobState::Queued);
}

/// A whole-host job is a row of the job table like any other: it is queued without an
/// allocation, listed with the queued jobs, and nothing presents it (the registry's wait list
/// holds nothing for it); it waits for the pool's queue wait, with its host marked, and ends
/// `no-capacity` at it, its mark deleted in the same commit. It catches a whole-host job kept
/// out of the table, one that needs a presentation, one dropped before its queue wait, and an
/// end that leaves its mark behind.
#[tokio::test]
async fn a_queued_whole_host_job_is_an_ordinary_job_row() {
    let (rig, mut agent) = served("whole_ordinary_row");
    run_share(&rig, &mut agent, "job-run", 7200).await;
    let whole = queue_whole(&rig, "job-w", 7200);
    place_all(&rig);

    let row = rig.job(&whole);
    assert_eq!(
        (row.state, row.host, row.placed_at),
        (JobState::Queued, None, None)
    );
    assert_eq!(row.spec.needs, Needs::WholeHost);
    assert!(rig.jobs.queued().unwrap().iter().any(|job| job.id == whole));
    assert_eq!(rig.jobs.get_allocation(&whole).unwrap(), None);
    assert!(rig.jobs.get_mark(&whole).unwrap().is_some());

    pass_time(&rig, &mut agent, 3599).await;
    rig.tick();
    assert_eq!(rig.job(&whole).state, JobState::Queued);
    assert!(rig.jobs.get_mark(&whole).unwrap().is_some());

    rig.advance(SECOND);
    let before = rig.commits();
    rig.tick();
    assert_eq!(rig.commits(), before + 1);
    assert_eq!(
        rig.job(&whole).state,
        JobState::FailedToStart(StartFailure::NoCapacity)
    );
    assert_eq!(rig.jobs.get_mark(&whole).unwrap(), None);
}

/// Every row of `rig`'s store, by key.
fn rows(rig: &Rig) -> BTreeMap<String, Versioned> {
    rig.store.scan_prefix("").unwrap().into_iter().collect()
}

/// A store with job and allocation rows only (what a release before the marks wrote) is
/// opened and passed over by a server with whole-node settings and nothing is written; a job
/// that needs the whole host is queued and its host marked, and the only new keys are that
/// job's own rows and its mark; every older key keeps its value and its version, and the older
/// rows read with the new fields at their defaults. It catches a build that rewrites rows it
/// has no new data for, one that cannot read an older row, and a mark that touches another
/// job's rows.
#[tokio::test]
async fn marks_are_additive_to_an_existing_store() {
    let (rig, _) = served("whole_additive");
    let old = rig.place("job-old", "host-a");
    let older = rows(&rig);
    assert!(older.keys().all(|key| !key.starts_with(keys::MARK)));
    assert!(older.keys().any(|key| key.starts_with(keys::ALLOCATION)));

    let dynamic: Arc<dyn Store> = rig.store.clone();
    let opened = JobStore::new(Arc::new(dynamic), ServerIds::random()).unwrap();
    assert_eq!(opened.get_job(&old).unwrap(), Some(rig.job(&old)));
    place_all(&rig);
    assert_eq!(rows(&rig), older);
    assert_eq!(rig.job(&old).failed_prepares, 0);
    assert!(rig.job(&old).prepare_bars.is_empty());
    let allocation = rig.allocation(&old);
    assert_eq!(
        (allocation.prepare_deadline, allocation.wall_limit_secs),
        (None, None)
    );

    let whole = queue_whole(&rig, "job-w", 600);
    place_all(&rig);
    assert!(rig.jobs.get_mark(&whole).unwrap().is_some());
    let newer = rows(&rig);
    for (key, row) in &older {
        assert_eq!(newer.get(key), Some(row), "{key} changed");
    }
    let fresh = newer.keys().filter(|key| !older.contains_key(*key));
    let added: Vec<&String> = fresh.collect();
    let mark = keys::mark(&whole);
    let marks = added.iter().filter(|key| key.starts_with(keys::MARK));
    let marks: Vec<&str> = marks.map(|key| key.as_str()).collect();
    assert_eq!(marks, [mark.as_str()]);
}

/// A host marked for a whole-host job takes a job that ends before the mark's tail with the
/// margin to spare, and not one that would overrun it. The backfilled job's wall limit (the
/// allocation's own when the engine cut it) ends it at least the margin before the tail, its
/// start carries that limit, and the host is quiet for the whole-host job when the job it
/// waited for ends. It catches a backfilled job that can hold the marked host past the margin,
/// a start that carries the job's own longer limit, and a host not committed when its tail
/// ends.
#[tokio::test]
async fn a_backfilled_job_cannot_hold_the_marked_host_past_margin_plus_grace() {
    let backfill = Some(Duration::from_secs(24 * 3600));
    let mut rig = whole_rig("whole_backfill", rules(), backfill);
    rig.register("host-a", true);
    let mut agent = FakeAgent::new("host-a");
    run_share(&rig, &mut agent, "job-run", 1200).await;
    let whole = queue_whole(&rig, "job-w", 7200);
    place_all(&rig);
    let stored = rig.jobs.get_mark(&whole).unwrap().unwrap();
    let tail = stored.tail_end.unwrap();
    let margin = Duration::from_secs(60);

    let fill = queue_share(&rig, "job-fill", 300);
    place_all(&rig);
    assert_eq!(
        rig.states(&fill),
        (JobState::Placed, AllocationStep::Booked)
    );
    let wall = rig.allocation(&fill).wall_limit_secs.unwrap_or(300);
    assert!(wall <= 300);
    let placed_at = rig.job(&fill).placed_at.unwrap();
    let ends = placed_at.saturating_add(Duration::from_secs(wall));
    assert!(
        ends.saturating_add(margin) <= tail,
        "ends {ends:?}, tail {tail:?}"
    );

    let long = queue_share(&rig, "job-long", 1190);
    place_all(&rig);
    assert_eq!(rig.job(&long).state, JobState::Queued);

    let reply = beat(&rig, &mut agent).await;
    let sent = starts(&reply);
    assert_eq!(sent.len(), 1);
    assert_eq!(
        (sent[0].job_id.as_str(), sent[0].wall_ms),
        ("job-fill", wall * 1000)
    );
    agent.run("job-fill");
    pass_time(&rig, &mut agent, wall).await;
    agent.end("job-fill", Outcome::TimedOut, None);
    beat(&rig, &mut agent).await;
    beat(&rig, &mut agent).await;
    assert_eq!(rig.job(&fill).state, JobState::Failed);

    let left = tail.since(now(&rig));
    pass_time(&rig, &mut agent, left.as_secs()).await;
    agent.end("job-run", Outcome::Succeeded, Some(0));
    beat(&rig, &mut agent).await;
    beat(&rig, &mut agent).await;
    place_all(&rig);
    assert_eq!(rig.job(&whole).state, JobState::Placed);
    assert!(rig.job(&whole).placed_at.unwrap() <= tail.saturating_add(margin));
    assert_eq!(rig.job(&long).state, JobState::Queued);
}

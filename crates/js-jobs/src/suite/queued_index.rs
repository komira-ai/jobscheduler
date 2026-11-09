//! The queued entries ([`crate::keys::QUEUED`]) between this build and an earlier one, and the
//! count of the job store's writes ([`JobStore::written`]).

use std::sync::Arc;

use js_state::conformance::Backend;
use js_state::{Cond, Store, Txn};

use super::{exited, host, rig, share};
use crate::ids::{JobId, RandomIds};
use crate::record::{EndReason, JobState};
use crate::store::JobStore;

/// The ids of the queue `jobs` reads, in order.
fn queue_of<S: Store>(jobs: &JobStore<S>) -> Vec<JobId> {
    let queued = jobs.queued().expect("the queue reads");
    queued.into_iter().map(|job| job.id).collect()
}

/// A store an earlier build wrote has no queued entries, and one an earlier build wrote to
/// after this one may keep a queued entry of a job it placed or cancelled: a job store that
/// opens it reads its whole queue, and none of the jobs that left it. Before the open, the
/// queue is read through the entries alone, with each row judged: no queued job with no
/// entry is found, and the job an entry still names, placed since, is left out.
pub fn a_store_an_earlier_build_wrote_reads_its_whole_queue_after_an_open<B: Backend>(backend: &B) {
    let name = "a_store_an_earlier_build_wrote_reads_its_whole_queue_after_an_open";
    let rig = rig(backend);
    let first = rig.queue("eb-first");
    rig.tick();
    let second = rig.queue("eb-second");
    rig.tick();
    let left = rig.queue("eb-left");
    // As an earlier build leaves the tables: the queued entries are gone, and the job it
    // placed after this build queued it keeps its queued entry.
    let mut earlier = Txn::new();
    for id in [&first, &second] {
        earlier = earlier.delete(format!("idx/job/queued/{id}"), Cond::Any);
    }
    rig.store
        .inner()
        .txn(earlier)
        .expect("reachable")
        .expect("unconditional deletes commit");
    rig.jobs
        .place(&left, host("host-a"), share())
        .expect("a queued job is placed");
    rig.store
        .inner()
        .txn(Txn::new().put(format!("idx/job/queued/{left}"), Vec::new(), Cond::Any))
        .expect("reachable")
        .expect("an unconditional put commits");
    assert_eq!(
        queue_of(&rig.jobs),
        Vec::<JobId>::new(),
        "{name}: before an open, a job with no queued entry was found, or a placed one read"
    );
    let opened = JobStore::new(Arc::clone(&rig.store), RandomIds).expect("the job store opens");
    assert_eq!(
        queue_of(&opened),
        [first, second],
        "{name}: after an open, the queue is not every queued job in order"
    );
    assert_eq!(
        opened.index_entries().expect("reads"),
        opened.index_by_scan().expect("reads"),
        "{name}: the open left the entries unlike the rows"
    );
    assert!(
        rig.raw(&format!("idx/job/queued/{left}")).is_none(),
        "{name}: the open kept the queued entry of a placed job"
    );
}

/// An earlier build reads the queue through the active entries and leaves a space it does not
/// name as it is: a queued job this build writes is under the active space too, and each
/// write that moves it on (placed, ended, cancelled while queued) moves both of its entries.
pub fn a_queued_job_stays_under_the_active_space_an_earlier_build_reads<B: Backend>(backend: &B) {
    let name = "a_queued_job_stays_under_the_active_space_an_earlier_build_reads";
    let rig = rig(backend);
    let id = rig.queue("ab-job");
    let cancelled = rig.queue("ab-cancelled");
    let entries = |id: &JobId, label: &str| {
        let active = rig.raw(&format!("idx/job/active/{id}")).is_some();
        let queued = rig.raw(&format!("idx/job/queued/{id}")).is_some();
        assert_eq!(
            rig.jobs.index_entries().expect("reads"),
            rig.jobs.index_by_scan().expect("reads"),
            "{name}: {label}: the entries are not the rows'"
        );
        (active, queued)
    };
    assert_eq!(entries(&id, "queued"), (true, true), "{name}: a queued job");
    rig.jobs
        .place(&id, host("host-a"), share())
        .expect("placed");
    assert_eq!(
        entries(&id, "placed"),
        (true, false),
        "{name}: a placed job"
    );
    rig.jobs
        .transition(&id, JobState::Placed, JobState::Starting)
        .expect("starting");
    rig.jobs
        .transition(&id, JobState::Starting, JobState::Running)
        .expect("running");
    assert_eq!(
        entries(&id, "running"),
        (true, false),
        "{name}: a running job"
    );
    rig.jobs.end_job(&id, exited(0)).expect("ended");
    assert_eq!(
        entries(&id, "ended"),
        (false, false),
        "{name}: an ended job"
    );
    rig.jobs.request_cancel(&cancelled).expect("cancelled");
    let gone = entries(&cancelled, "cancelled");
    assert_eq!(gone, (false, false), "{name}: a job cancelled while queued");
}

/// Every write of the job store moves where its writes stand, whatever the store answered (a
/// commit, a conflict, a failure); no read does; and two job stores over the same store never
/// read the same, so one's reading cannot stand for the other's tables.
pub fn every_write_moves_the_written_mark_and_no_read_does<B: Backend>(backend: &B) {
    let name = "every_write_moves_the_written_mark_and_no_read_does";
    let rig = rig(backend);
    let mut last = rig.jobs.written();
    let mut moved = |label: &str| {
        let now = rig.jobs.written();
        assert_ne!(now, last, "{name}: {label} did not move the mark");
        last = now;
    };
    let id = rig.queue("wr-job");
    moved("an insert");
    rig.jobs
        .place(&id, host("host-a"), share())
        .expect("placed");
    moved("a placement");
    let stale = rig.jobs.transition(&id, JobState::Queued, JobState::Placed);
    assert!(stale.is_err(), "{name}: a move from a state the job left");
    let unmoved = rig.jobs.written();
    rig.jobs
        .transition(&id, JobState::Placed, JobState::Starting)
        .expect("starting");
    assert_ne!(rig.jobs.written(), unmoved, "{name}: a transition");
    let before = rig.jobs.written();
    rig.store.fail_next();
    let failed = rig
        .jobs
        .transition(&id, JobState::Starting, JobState::Running);
    assert!(failed.is_err(), "{name}: the commit was to fail");
    assert_ne!(rig.jobs.written(), before, "{name}: a failed commit");
    rig.jobs
        .transition(&id, JobState::Starting, JobState::Running)
        .expect("running");
    rig.jobs.end_job(&id, exited(0)).expect("ended");
    rig.jobs.free(&id, EndReason::Exited).expect("freed");
    let read = rig.jobs.written();
    let _ = rig.jobs.get_job(&id).expect("reads");
    let _ = rig.jobs.get_allocation(&id).expect("reads");
    let _ = rig.jobs.queued().expect("reads");
    let _ = rig.jobs.live_allocations().expect("reads");
    let _ = rig.jobs.marks().expect("reads");
    let _ = rig.jobs.booked_on(&host("host-a")).expect("reads");
    assert_eq!(rig.jobs.written(), read, "{name}: a read moved the mark");
    let other = JobStore::new(Arc::clone(&rig.store), RandomIds).expect("opens");
    let fresh = JobStore::new(Arc::clone(&rig.store), RandomIds).expect("opens");
    assert_ne!(
        other.written(),
        fresh.written(),
        "{name}: two job stores read the same"
    );
}

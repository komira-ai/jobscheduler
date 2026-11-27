//! What the job store tells the other servers of its store after its commits ([`crate::Wake`]):
//! the notices a submit, a placement, a cancel, a free and the backfill send, and that a write
//! that did not commit sends none.

use std::sync::Arc;

use js_state::fake::{Fake, Flaw};
use js_state::recording::RecordingStore;
use js_state::{Cond, ManualClock, Notice, Store, Txn};

use super::{JobStore, NewJob, Placement};
use crate::error::JobStoreError;
use crate::ids::{JobId, RandomIds};
use crate::interfere::Interfere;
use crate::keys;
use crate::record::{EndReason, JobState};
use crate::suite::{exited, full_spec, host, job_id, share};
use crate::wake::Wake;

/// A job store over a store that records every notice sent, with the interference of the
/// suite's rig around the recording.
struct Told {
    recorded: Arc<RecordingStore>,
    store: Arc<Interfere<RecordingStore>>,
    jobs: JobStore<Interfere<RecordingStore>>,
}

impl Told {
    fn new() -> Told {
        let fake: Arc<dyn Store> = Arc::new(Fake::with_flaw(
            Arc::new(ManualClock::default()),
            Flaw::None,
        ));
        let store = Arc::new(Interfere::new(RecordingStore::new(fake)));
        let recorded = store.inner();
        let jobs = JobStore::new(Arc::clone(&store), RandomIds).expect("the job store opens");
        Told {
            recorded,
            store,
            jobs,
        }
    }

    /// The wakes the notices sent since the last call asked for, in the order sent.
    fn wakes(&self) -> Vec<Wake> {
        let notices = self.recorded.notices();
        self.recorded.clear();
        notices
            .iter()
            .map(|n| Wake::of(n).expect("a job store's notice"))
            .collect()
    }

    fn submit(&self, text: &str) -> Result<bool, JobStoreError> {
        let job = NewJob {
            id: Some(job_id(text)),
            spec: full_spec(),
            priority: 0,
            client: None,
        };
        self.jobs
            .insert_queued(job)
            .map(|inserted| inserted.created)
    }

    fn placed(&self, text: &str) -> JobId {
        assert_eq!(self.submit(text), Ok(true));
        let id = job_id(text);
        self.jobs
            .place(&id, host("host-a"), share())
            .expect("placed");
        id
    }

    fn running(&self, text: &str) -> JobId {
        let id = self.placed(text);
        for (from, to) in [
            (JobState::Placed, JobState::Starting),
            (JobState::Starting, JobState::Running),
        ] {
            self.jobs.transition(&id, from, to).expect("moves");
        }
        self.wakes();
        id
    }
}

/// A submit wakes the queue, once its job is recorded; a resubmit of the same job, which
/// writes nothing, and a submit whose commit fails send nothing. It catches a queue that is
/// woken for a write that did not happen, and one never woken by a submit.
#[test]
fn a_submit_wakes_the_queue_and_a_write_that_did_not_happen_wakes_nothing() {
    let told = Told::new();
    assert_eq!(told.submit("job-1"), Ok(true));
    assert_eq!(told.wakes(), [Wake::Queue]);
    assert_eq!(told.submit("job-1"), Ok(false), "the same job again");
    told.store.fail_after(0);
    assert!(told.submit("job-2").is_err(), "the commit fails");
    assert_eq!(told.wakes(), []);
}

/// A submit that another writer's submit of the same id beat to the commit tells nothing of its
/// own: the winner told. It catches a conflicted stamped write that sends its notices anyway.
#[test]
fn a_submit_that_loses_its_race_tells_nothing_of_its_own() {
    let told = Told::new();
    let other = JobStore::new(told.store.inner(), RandomIds).expect("a second writer");
    told.store.before_next(move || {
        let job = NewJob {
            id: Some(job_id("job-1")),
            spec: full_spec(),
            priority: 0,
            client: None,
        };
        assert!(other.insert_queued(job).expect("recorded").created);
    });
    assert_eq!(told.submit("job-1"), Ok(false), "the other writer's job");
    assert_eq!(told.wakes(), [Wake::Queue], "the winner's notice alone");
}

/// A placement, by a plain commit or a claim, tells the host it was placed on, and does not wake
/// the queue; a claim that is refused tells nothing. It catches a placement whose start reaches
/// a poll held on another server only at its recheck.
#[test]
fn a_placement_tells_its_host_and_a_refused_claim_tells_nothing() {
    let told = Told::new();
    told.placed("job-1");
    assert_eq!(told.wakes(), [Wake::Queue, Wake::Host(host("host-a"))]);
    assert_eq!(told.submit("job-2"), Ok(true));
    told.wakes();
    let plain = Placement::default();
    let (b, job) = (host("host-b"), job_id("job-2"));
    told.jobs
        .place_claimed(&job, &b, share(), &plain, None)
        .expect("claimed");
    assert_eq!(told.wakes(), [Wake::Host(b.clone())]);
    let again = told.jobs.place_claimed(&job, &b, share(), &plain, None);
    assert!(again.is_err(), "the job is placed already");
    assert_eq!(told.wakes(), []);
}

/// A cancel wakes the queue whatever the job's state: a running job's also tells its host,
/// whose stop it wrote, first; a second cancel, which writes nothing, tells nothing. It catches
/// a cancel whose stop waits for the host's recheck, and a queue not woken by a cancel.
#[test]
fn a_cancel_wakes_the_queue_and_tells_the_host_of_its_stop() {
    let told = Told::new();
    let running = told.running("job-1");
    told.jobs.request_cancel(&running).expect("cancelled");
    assert_eq!(told.wakes(), [Wake::Host(host("host-a")), Wake::Queue]);
    told.jobs.request_cancel(&running).expect("asked already");
    assert_eq!(told.wakes(), []);
    assert_eq!(told.submit("job-2"), Ok(true));
    told.wakes();
    told.jobs
        .request_cancel(&job_id("job-2"))
        .expect("cancelled");
    assert_eq!(told.wakes(), [Wake::Queue]);
}

/// The end of a job its host reported tells no one (the host stopped it, and its allocation is
/// still releasing), and the free of that allocation wakes the queue: the room is back. It
/// catches a free that waits for another server's tick to be placed into.
#[test]
fn a_free_wakes_the_queue() {
    let told = Told::new();
    let id = told.running("job-1");
    told.jobs.end_job(&id, exited(0)).expect("ended");
    assert_eq!(told.wakes(), []);
    told.jobs.free(&id, EndReason::Exited).expect("freed");
    assert_eq!(told.wakes(), [Wake::Queue]);
}

/// The backfill that indexes a queued job the index does not hold wakes the queue in its
/// batch: the job is placeable now. It catches a batch of commits that tells nothing.
#[test]
fn the_backfill_of_a_queued_entry_wakes_the_queue() {
    let told = Told::new();
    assert_eq!(told.submit("job-1"), Ok(true));
    let entry = format!("{}job-1", keys::QUEUED);
    told.recorded
        .txn(Txn::new().delete(entry.as_str(), Cond::Any))
        .expect("the store")
        .expect("the entry is gone");
    told.wakes();
    JobStore::new(Arc::clone(&told.store), RandomIds).expect("opens and backfills");
    assert_eq!(told.wakes(), [Wake::Queue]);
    assert!(told.recorded.get(&entry).expect("the store").is_some());
}

/// The notices are the wakes' own: what the store is asked to send reads back as the wake. It
/// catches a notice of the job store that a server's listener cannot read.
#[test]
fn every_notice_sent_is_a_wake() {
    let told = Told::new();
    told.placed("job-1");
    let notices: Vec<Notice> = told.recorded.notices();
    assert_eq!(
        notices,
        [Wake::Queue.notice(), Wake::Host(host("host-a")).notice()]
    );
}

//! The job store suite: every behaviour of [`crate::JobStore`] stated as a function generic
//! over a `js_state::conformance::Backend`, so the fake, SQLite and Postgres run the same
//! checks. A backend's tests run them with [`crate::job_store_tests!`] (one test each) or
//! [`run_job_store_suite`]; the fake's tests also run a check against a fake with a
//! deliberate defect and require it to panic.
//!
//! Every assertion message starts with the name of its check, which is what a
//! `should_panic(expected = ...)` test matches. Time is the `ManualClock` the backend is
//! opened with; nothing reads the wall clock or sleeps, and a race is arranged by a hook that
//! commits just before the operation's own commit ([`Interfere::before_next`]).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use js_state::conformance::Backend;
use js_state::{CommitStats, Cond, ManualClock, Store, Txn, Versioned};
use js_types::{ExecMode, HostId, ImageDigestRef, Labels, ReservedKey, Resources};

use crate::interfere::Interfere;
use crate::{
    Booked, Constraints, DeclaredPort, JobId, JobSpec, JobState, JobStore, Needs, NetworkPolicy,
    NewJob, RandomIds,
};

mod idempotency;
mod lifecycle;
mod placement;
mod rows;

pub use idempotency::{
    a_concurrent_insert_answers_from_the_job_it_made, a_minted_id_collision_is_retried,
    a_priority_only_difference_is_the_same_job, minted_ids_do_not_repeat_across_instances,
    same_id_different_job_is_a_conflict, same_id_same_job_returns_the_existing_row,
};
pub use lifecycle::{
    a_freed_allocation_row_is_kept_as_history, an_ended_job_cannot_move_again,
    cancel_of_a_queued_job_is_immediate_and_of_a_placed_job_is_an_intent,
    cancel_twice_is_cancel_once,
    ending_a_placed_job_moves_its_allocation_to_releasing_in_the_same_commit,
    free_twice_keeps_the_first_reason, releasing_needs_an_end_or_a_cancel,
};
pub use placement::{
    a_failed_commit_changes_nothing, a_requeued_job_gets_a_new_allocation,
    booked_on_sums_the_live_allocations_of_a_host, place_on_a_job_that_changed_is_stale,
    place_refuses_a_job_that_is_not_queued_or_still_booked,
    placement_and_allocation_commit_together,
};
pub use rows::{
    a_row_that_does_not_parse_is_corrupt, a_row_with_a_higher_format_byte_is_unknown_not_missing,
    a_row_with_unknown_tags_is_read_and_rewritten_intact,
    a_row_without_the_newest_tag_reads_with_defaults,
    list_jobs_pages_in_key_order_and_filters_by_state,
};

/// Runs every check of the suite against `backend`, one after another.
pub fn run_job_store_suite<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    same_id_same_job_returns_the_existing_row(backend);
    same_id_different_job_is_a_conflict(backend);
    a_priority_only_difference_is_the_same_job(backend);
    a_concurrent_insert_answers_from_the_job_it_made(backend);
    minted_ids_do_not_repeat_across_instances(backend);
    a_minted_id_collision_is_retried(backend);
    placement_and_allocation_commit_together(backend);
    a_failed_commit_changes_nothing(backend);
    place_on_a_job_that_changed_is_stale(backend);
    place_refuses_a_job_that_is_not_queued_or_still_booked(backend);
    a_requeued_job_gets_a_new_allocation(backend);
    booked_on_sums_the_live_allocations_of_a_host(backend);
    a_freed_allocation_row_is_kept_as_history(backend);
    free_twice_keeps_the_first_reason(backend);
    an_ended_job_cannot_move_again(backend);
    ending_a_placed_job_moves_its_allocation_to_releasing_in_the_same_commit(backend);
    cancel_of_a_queued_job_is_immediate_and_of_a_placed_job_is_an_intent(backend);
    cancel_twice_is_cancel_once(backend);
    releasing_needs_an_end_or_a_cancel(backend);
    a_row_with_unknown_tags_is_read_and_rewritten_intact(backend);
    a_row_without_the_newest_tag_reads_with_defaults(backend);
    a_row_with_a_higher_format_byte_is_unknown_not_missing(backend);
    a_row_that_does_not_parse_is_corrupt(backend);
    list_jobs_pages_in_key_order_and_filters_by_state(backend);
}

/// One `#[test]` per check of the suite, named as the check is, each on a fresh backend from
/// `$backend()`: `$backend` is a function (or closure) that makes a
/// `js_state::conformance::Backend`.
#[macro_export]
macro_rules! job_store_tests {
    (@each $backend:expr; $($check:ident),+ $(,)?) => {
        $(
            #[test]
            fn $check() {
                $crate::suite::$check(&$backend());
            }
        )+
    };
    ($backend:expr) => {
        $crate::job_store_tests!(
            @each $backend;
            same_id_same_job_returns_the_existing_row,
            same_id_different_job_is_a_conflict,
            a_priority_only_difference_is_the_same_job,
            a_concurrent_insert_answers_from_the_job_it_made,
            minted_ids_do_not_repeat_across_instances,
            a_minted_id_collision_is_retried,
            placement_and_allocation_commit_together,
            a_failed_commit_changes_nothing,
            place_on_a_job_that_changed_is_stale,
            place_refuses_a_job_that_is_not_queued_or_still_booked,
            a_requeued_job_gets_a_new_allocation,
            booked_on_sums_the_live_allocations_of_a_host,
            a_freed_allocation_row_is_kept_as_history,
            free_twice_keeps_the_first_reason,
            an_ended_job_cannot_move_again,
            ending_a_placed_job_moves_its_allocation_to_releasing_in_the_same_commit,
            cancel_of_a_queued_job_is_immediate_and_of_a_placed_job_is_an_intent,
            cancel_twice_is_cancel_once,
            releasing_needs_an_end_or_a_cancel,
            a_row_with_unknown_tags_is_read_and_rewritten_intact,
            a_row_without_the_newest_tag_reads_with_defaults,
            a_row_with_a_higher_format_byte_is_unknown_not_missing,
            a_row_that_does_not_parse_is_corrupt,
            list_jobs_pages_in_key_order_and_filters_by_state,
        );
    };
}

/// A store under test: its clock, the wrapper that interferes with its commits, and the job
/// store over the wrapper.
pub(crate) struct Rig<S> {
    /// The store's clock.
    pub(crate) clock: Arc<ManualClock>,
    /// The wrapped store.
    pub(crate) store: Arc<Interfere<S>>,
    /// The job store under test.
    pub(crate) jobs: JobStore<Interfere<S>>,
}

/// A fresh store of `backend` with a job store over it.
pub(crate) fn rig<B: Backend>(backend: &B) -> Rig<B::Store> {
    let clock = Arc::new(ManualClock::default());
    let store = Arc::new(Interfere::new(backend.open(Arc::clone(&clock))));
    let jobs = JobStore::new(Arc::clone(&store), RandomIds).expect("the job store opens");
    Rig { clock, store, jobs }
}

impl<S: Store> Rig<S> {
    /// Moves the clock on, so the next write's time differs from the last one's.
    pub(crate) fn tick(&self) {
        self.clock.advance(Duration::from_secs(1));
    }

    /// Records a queued job `text` with the full spec.
    pub(crate) fn queue(&self, text: &str) -> JobId {
        self.queue_spec(text, full_spec())
    }

    /// Records a queued job `text` with `spec`.
    pub(crate) fn queue_spec(&self, text: &str, spec: JobSpec) -> JobId {
        let inserted = self
            .jobs
            .insert_queued(NewJob {
                id: Some(job_id(text)),
                spec,
                priority: 0,
            })
            .expect("a fresh job is recorded");
        assert!(inserted.created, "a fresh job is created");
        inserted.record.id
    }

    /// Records the job `text` and places it on `host-a` with [`share`].
    pub(crate) fn placed(&self, text: &str) -> JobId {
        let id = self.queue(text);
        self.jobs
            .place(&id, host("host-a"), share())
            .expect("a queued job is placed");
        id
    }

    /// Records the job `text`, places it and moves it to running.
    pub(crate) fn running(&self, text: &str) -> JobId {
        let id = self.placed(text);
        self.jobs
            .transition(&id, JobState::Placed, JobState::Starting)
            .expect("placed to starting");
        self.jobs
            .transition(&id, JobState::Starting, JobState::Running)
            .expect("starting to running");
        id
    }

    /// The raw row `key`.
    pub(crate) fn raw(&self, key: &str) -> Option<Versioned> {
        self.store.get(key).expect("the store is reachable")
    }

    /// Writes `value` under `key` as it is, past the job store.
    pub(crate) fn put_raw(&self, key: &str, value: Vec<u8>) {
        self.store
            .inner()
            .txn(Txn::new().put(key, value, Cond::Any))
            .expect("the store is reachable")
            .expect("an unconditional put commits");
    }

    /// Every row and the commit counters.
    pub(crate) fn snapshot(&self) -> (Vec<(String, Versioned)>, CommitStats) {
        let rows = self.store.scan_prefix("").expect("the store is reachable");
        (rows, self.store.commit_stats())
    }

    /// How many job rows there are.
    pub(crate) fn job_rows(&self) -> usize {
        self.store
            .scan_prefix(crate::keys::JOB)
            .expect("the store is reachable")
            .len()
    }
}

/// The job id `text`.
pub(crate) fn job_id(text: &str) -> JobId {
    JobId::parse(text).expect("a valid job id")
}

/// The host id `text`.
pub(crate) fn host(text: &str) -> HostId {
    HostId::new(text).expect("a valid host id")
}

/// A digest of 64 hex digits that repeat `digit`.
pub(crate) fn digest(digit: char) -> String {
    format!("sha256:{}", digit.to_string().repeat(64))
}

/// A spec with every field set to something other than its default.
pub(crate) fn full_spec() -> JobSpec {
    let caller = BTreeMap::from([("team".to_owned(), "infra".to_owned())]);
    JobSpec {
        image: ImageDigestRef::parse(&format!("team/app:1@{}", digest('a'))).expect("an image"),
        command: vec!["run".to_owned()],
        args: vec!["--fast".to_owned(), "--seed=7".to_owned()],
        env: BTreeMap::from([
            ("MODE".to_owned(), "test".to_owned()),
            ("LEVEL".to_owned(), "2".to_owned()),
        ]),
        needs: Needs::Shape(
            Resources::new(2000, 1 << 30, 1, Some("accel-a".to_owned()))
                .expect("resources")
                .with_disk_bytes(1 << 32),
        ),
        constraints: Constraints {
            os: Some("linux".to_owned()),
            arch: Some("x86_64".to_owned()),
            exec: Some(ExecMode::Container),
            capabilities: BTreeMap::from([("driver".to_owned(), "550".to_owned())]),
        },
        wall_limit_secs: 600,
        network: NetworkPolicy::AllowList,
        ports: vec![DeclaredPort {
            host: 8080,
            container: 80,
        }],
        files: BTreeMap::from([("inputs/upload".to_owned(), b"upload-target".to_vec())]),
        labels: Labels::from_caller(&caller)
            .and_then(|labels| labels.with_reserved(ReservedKey::Run, "run-1"))
            .expect("labels"),
    }
}

/// A spec that needs the whole host.
pub(crate) fn whole_host_spec() -> JobSpec {
    JobSpec {
        needs: Needs::WholeHost,
        ..full_spec()
    }
}

/// The share of a host [`Rig::placed`] books.
pub(crate) fn share() -> Booked {
    Booked::Shape {
        cpu_millis: 2000,
        memory_bytes: 1 << 30,
        disk_bytes: 1 << 32,
        accelerators: 1,
    }
}

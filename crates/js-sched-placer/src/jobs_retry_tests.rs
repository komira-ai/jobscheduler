//! [`placeable`], the queued jobs a pass may place now: a job never retried passes as it is; a
//! retried job is held back until its backoff ends (the instant it ends is the first at which
//! it passes) and while its previous allocation is live (booked or releasing); the order of
//! the jobs is kept; each host that held an earlier attempt and does not report `start-fence`
//! (or is not open) is barred for life beside the bars the job holds. Then the passes of the
//! placer over job tables on the in-memory fake: a retried job is not placed during its
//! backoff or while its allocation is live, and its status is not written over; a host of an
//! earlier attempt is chosen only when it reports `start-fence`.

use std::time::Duration;

use js_jobs::{AttemptOutcome, AttemptRecord, Booked, EndReason, JobSpec, JobState, Needs, NewJob};
use js_registry::Placer;
use js_types::{ManualClock, QueueCode, Resources, RetryPolicy, Timestamp};

use super::*;
use crate::engine::Settings;
use crate::engine::jobs::{JobHosts, JobStarts, PassMetrics};
use crate::engine::rig::{Tables, at, candidate, host, hosts, spec_of, store};
use crate::place::SchedPlacer;

/// The wait, in milliseconds, a retry in these tests gives the next attempt.
const BACKOFF_MS: u64 = 10_000;

/// When the 15-minute bar of a retry placed at store time zero ends, in milliseconds.
const BAR_END_MS: u64 = 15 * 60 * 1000;

/// The id `name`.
fn id(name: &str) -> JobId {
    JobId::parse(name).unwrap()
}

/// What one core and 1 GiB book.
fn booking() -> Booked {
    Booked::Shape {
        cpu_millis: 1000,
        memory_bytes: 1 << 30,
        disk_bytes: 0,
        accelerators: 0,
    }
}

/// The spec of a share job that may run `max_attempts` attempts.
fn retrying(max_attempts: u32) -> JobSpec {
    let resources = Resources::new(1000, 1 << 30, 0, None).unwrap();
    let mut spec = spec_of(Needs::Shape(resources), 600);
    spec.retry = Some(RetryPolicy {
        max_attempts,
        initial_backoff: None,
        max_backoff: None,
    });
    spec
}

/// Records the queued job `name`, which may run three attempts.
fn queue(tables: &Tables, name: &str) -> JobRecord {
    let new = NewJob {
        id: Some(id(name)),
        spec: retrying(3),
        priority: 0,
        client: None,
    };
    tables.jobs.insert_queued(new).unwrap().record
}

/// The end of an attempt whose host was lost.
fn lost() -> AttemptRecord {
    AttemptRecord {
        outcome: AttemptOutcome::HostLost,
        exit_code: None,
        reason: None,
        start_failure: None,
        start_failure_detail: None,
    }
}

/// Places the queued job `name` on `on`, ends that attempt as lost and queues the next one for
/// `not_before_ms`: the job as the retry leaves it, its allocation releasing.
fn retry_on(tables: &Tables, name: &str, on: &str, not_before_ms: u64) -> JobRecord {
    tables.jobs.place(&id(name), host(on), booking()).unwrap();
    let end = lost();
    tables
        .jobs
        .retry_txn(&id(name), end, store(not_before_ms))
        .unwrap()
}

/// Frees the allocation of `name`, as its host's confirmation does.
fn free(tables: &Tables, name: &str) {
    tables.jobs.free(&id(name), EndReason::HostAbsent).unwrap();
}

/// The live allocations of the tables.
fn live(tables: &Tables) -> Vec<AllocationRecord> {
    tables.jobs.live_allocations().unwrap()
}

/// The open hosts named, none of them reporting `start-fence`.
fn open_plain(names: &[&str]) -> BTreeMap<HostId, OpenHost> {
    names
        .iter()
        .map(|name| (host(name), OpenHost::default()))
        .collect()
}

/// What [`placeable`] offers of `queued`, as a list of its own.
fn offered_of(
    queued: &[JobRecord],
    live: &[js_jobs::AllocationRecord],
    open: &BTreeMap<HostId, OpenHost>,
    now: FarmTime,
) -> Vec<JobRecord> {
    placeable(queued, live, open, now).into_owned()
}

/// The ids of `jobs`.
fn ids(jobs: &[JobRecord]) -> Vec<String> {
    jobs.iter().map(|job| job.id.to_string()).collect()
}

/// Asserts that a job never retried passes unchanged, with no bar added, and that a pass at
/// any time offers it. Catches a filter that drops or edits a first attempt.
#[test]
fn a_job_never_retried_passes_unchanged() {
    let tables = Tables::new();
    let job = queue(&tables, "fresh");
    assert_eq!(job.attempt.retry_not_before(), None);
    let open = open_plain(&["a"]);
    let queued = [job.clone()];
    assert!(
        matches!(placeable(&queued, &[], &open, at(0)), Cow::Borrowed(_)),
        "a queue with no retried job is offered as it is, not copied"
    );
    assert_eq!(offered_of(&queued, &[], &open, at(0)), vec![job]);
}

/// Asserts that a retried job is held back until its backoff ends: at one millisecond before
/// `retry_not_before` it is not offered, at that time it is, and after it too. Catches a
/// comparison that is off by one at the boundary, or one that ignores the backoff.
#[test]
fn a_retried_job_passes_from_the_time_its_backoff_ends() {
    let tables = Tables::new();
    queue(&tables, "r");
    let job = retry_on(&tables, "r", "a", BACKOFF_MS);
    free(&tables, "r");
    assert_eq!(live(&tables), []);
    let open = open_plain(&["b"]);
    let jobs = [job];
    assert_eq!(offered_of(&jobs, &[], &open, at(0)), []);
    assert_eq!(offered_of(&jobs, &[], &open, at(BACKOFF_MS - 1)), []);
    assert_eq!(
        ids(&offered_of(&jobs, &[], &open, at(BACKOFF_MS))),
        ["r"],
        "at the instant the backoff ends"
    );
    assert_eq!(
        ids(&offered_of(&jobs, &[], &open, at(BACKOFF_MS + 1))),
        ["r"]
    );
}

/// Asserts that a retried job whose backoff has ended is still held back while its previous
/// allocation is live (releasing, as a retry leaves it), and is offered once the allocation is
/// freed, so it is not in the live list. Catches a job offered while it holds an allocation,
/// which the store's placement refuses on every pass.
#[test]
fn a_retried_job_with_a_releasing_allocation_is_held_back_until_it_is_freed() {
    let tables = Tables::new();
    queue(&tables, "r");
    let job = retry_on(&tables, "r", "a", 0);
    let open = open_plain(&["b"]);
    let jobs = [job];
    let releasing = live(&tables);
    assert_eq!(releasing.len(), 1);
    assert_eq!(offered_of(&jobs, &releasing, &open, at(BACKOFF_MS)), []);
    free(&tables, "r");
    assert_eq!(
        ids(&offered_of(&jobs, &live(&tables), &open, at(BACKOFF_MS))),
        ["r"]
    );
}

/// Asserts that a queued job whose allocation is booked is held back, and that the allocation
/// of another job holds nothing back. Catches a hold keyed on anything but the job's id.
#[test]
fn a_job_with_a_booked_allocation_is_held_back_and_another_jobs_allocation_holds_nothing() {
    let tables = Tables::new();
    let job = queue(&tables, "j");
    let other = queue(&tables, "other");
    let (_, booked) = tables.jobs.place(&id("j"), host("a"), booking()).unwrap();
    let open = open_plain(&["b"]);
    assert_eq!(
        offered_of(
            std::slice::from_ref(&job),
            std::slice::from_ref(&booked),
            &open,
            at(0)
        ),
        []
    );
    assert_eq!(
        offered_of(std::slice::from_ref(&other), &[booked], &open, at(0)),
        vec![other]
    );
}

/// Asserts that the jobs offered keep the order they were queued in, with the held-back ones
/// left out of it. Catches a pass that reorders the queue or drops a job behind a held one.
#[test]
fn the_order_of_the_queue_is_kept_around_a_held_back_job() {
    let tables = Tables::new();
    let first = queue(&tables, "first");
    queue(&tables, "second");
    let third = queue(&tables, "third");
    let second = retry_on(&tables, "second", "a", BACKOFF_MS);
    free(&tables, "second");
    let open = open_plain(&["b"]);
    let jobs = [first, second, third];
    let rows = live(&tables);
    assert_eq!(
        ids(&offered_of(&jobs, &rows, &open, at(1))),
        ["first", "third"]
    );
    assert_eq!(
        ids(&offered_of(&jobs, &rows, &open, at(BACKOFF_MS))),
        ["first", "second", "third"]
    );
}

/// Asserts that a host of an earlier attempt that is not open, or is open and does not report
/// `start-fence`, is barred for life on the offered job in place of the 15-minute bar its row
/// holds, and that the job it was offered from is not changed. Catches a host whose agent may
/// drop the later attempt's start being left open to the job.
#[test]
fn a_host_of_an_earlier_attempt_without_start_fence_is_barred_for_life() {
    let tables = Tables::new();
    queue(&tables, "r");
    let job = retry_on(&tables, "r", "a", 0);
    free(&tables, "r");
    let until = retry_bar(&job.attempt.history()[0]);
    assert_eq!(job.prepare_bars.get(&host("a")), Some(&until));
    let barred: BTreeMap<HostId, Exclusion> =
        [(host("a"), Exclusion::ForLife)].into_iter().collect();
    for open in [open_plain(&[]), open_plain(&["a"]), open_plain(&["a", "b"])] {
        let offered = offered_of(std::slice::from_ref(&job), &[], &open, at(0));
        assert_eq!(offered.len(), 1);
        assert_eq!(offered[0].prepare_bars, barred, "open: {open:?}");
    }
    assert_eq!(job.prepare_bars.get(&host("a")), Some(&until));
}

/// Asserts that a host of an earlier attempt that reports `start-fence` is not barred for
/// life: the job keeps the 15-minute bar its row holds on it. Catches a fence ignored, which
/// would keep a job off its only capable host for ever.
#[test]
fn a_host_of_an_earlier_attempt_with_start_fence_keeps_only_its_own_bar() {
    let tables = Tables::new();
    queue(&tables, "r");
    let job = retry_on(&tables, "r", "a", 0);
    free(&tables, "r");
    let mut open = open_plain(&["a"]);
    open.get_mut(&host("a")).unwrap().start_fence = true;
    let offered = offered_of(std::slice::from_ref(&job), &[], &open, at(0));
    assert_eq!(offered, vec![job]);
}

/// Asserts that, for a job with attempts on two hosts, only the host that does not report
/// `start-fence` is barred for life, the other keeps its own bar, and a bar on a host that held
/// no attempt is kept as it was. Catches bars applied to every host or to the wrong ones, and
/// bars of the row that the offer drops.
#[test]
fn bars_are_made_per_host_and_the_bars_of_other_hosts_are_kept() {
    let tables = Tables::new();
    queue(&tables, "r");
    retry_on(&tables, "r", "a", 0);
    free(&tables, "r");
    let mut job = retry_on(&tables, "r", "b", 0);
    free(&tables, "r");
    assert_eq!(job.attempt.hosts(), &hosts(&["a", "b"]));
    let elsewhere = Exclusion::Until(store(5));
    job.prepare_bars.insert(host("z"), elsewhere);
    let mut open = open_plain(&["a", "b", "z"]);
    open.get_mut(&host("a")).unwrap().start_fence = true;
    let offered = offered_of(std::slice::from_ref(&job), &[], &open, at(0));
    let mut want = job.prepare_bars.clone();
    want.insert(host("b"), Exclusion::ForLife);
    assert_eq!(offered[0].prepare_bars, want);
    assert_eq!(want.get(&host("z")), Some(&elsewhere));
    assert_eq!(
        want.get(&host("a")),
        Some(&retry_bar(&job.attempt.history()[0]))
    );
}

/// The 15-minute bar the retry that ended `attempt` put on its host: from the retry, on the
/// store's clock (which the fake moves on with each commit, so it is read, not assumed).
fn retry_bar(attempt: &js_jobs::Attempt) -> Exclusion {
    Exclusion::Until(
        attempt
            .ended_at
            .saturating_add(Duration::from_millis(BAR_END_MS)),
    )
}

/// Counts nothing and starts nothing: the sinks of a pass.
struct Sink;

impl PassMetrics for Sink {
    fn job_placed(&self, _: Duration) {}

    fn store_write_failed(&self) {}
}

impl JobStarts for Sink {
    fn placed(&self, _: &JobId, _: &HostId) {}

    fn tainted(&self, _: &HostId, _: &JobId) {}
}

/// A placer that knows of the hosts `a` and `b`, over job tables, with both clocks the test's.
struct Farm {
    placer: SchedPlacer,
    clock: ManualClock,
    tables: Tables,
}

impl Farm {
    fn new() -> Farm {
        let clock = ManualClock::new(Timestamp::from_millis(0));
        let placer = SchedPlacer::new(Settings::default(), clock.clone());
        for name in ["a", "b"] {
            placer.host_up(&host(name), &candidate(name, &[]).shape);
        }
        Farm {
            placer,
            clock,
            tables: Tables::new(),
        }
    }

    /// A pass at `now_ms` on both clocks, over the hosts `open` (each of `fenced` reporting
    /// `start-fence`): the jobs it placed.
    fn pass(&self, now_ms: u64, open: &[&str], fenced: &[&str]) -> Vec<JobId> {
        self.clock.set(Timestamp::from_millis(now_ms));
        self.tables.set_ms(now_ms);
        let offered: BTreeMap<HostId, OpenHost> = open
            .iter()
            .map(|name| {
                let start_fence = fenced.contains(name);
                (
                    host(name),
                    OpenHost {
                        start_fence,
                        ..OpenHost::default()
                    },
                )
            })
            .collect();
        for name in open {
            self.placer.heard(&host(name), None);
        }
        let hello: BTreeSet<HostId> = offered.keys().cloned().collect();
        let tainted = BTreeSet::new();
        let hosts = JobHosts {
            open: &offered,
            hello: &hello,
            tainted: &tainted,
            whole: None,
        };
        let sinks = (&Sink as &dyn JobStarts, &Sink as &dyn PassMetrics);
        self.placer
            .place_jobs_with(&self.tables.jobs, &hosts, sinks)
            .unwrap()
    }

    /// The record of the job `name`.
    fn job(&self, name: &str) -> JobRecord {
        self.tables.jobs.get_job(&id(name)).unwrap().unwrap()
    }
}

/// Asserts that a retried job is not placed during its backoff nor while its previous
/// allocation is live, that the passes write no queue status over `WAITING_FOR_RETRY` for it,
/// and that it is placed, on a host other than the one of its earlier attempt, once both have
/// cleared. Catches a job placed early, a pass that fails on the store's refusal of a job with
/// an allocation, and a status overwritten while the job waits.
#[test]
fn a_retried_job_is_placed_only_after_its_backoff_and_its_allocation_clear() {
    let farm = Farm::new();
    queue(&farm.tables, "r");
    retry_on(&farm.tables, "r", "a", BACKOFF_MS);
    let waiting = farm.job("r");
    assert_eq!(waiting.queue.code(), QueueCode::WaitingForRetry);

    assert_eq!(farm.pass(5_000, &["a", "b"], &[]), []);
    assert_eq!(farm.job("r").queue, waiting.queue, "live and in backoff");

    free(&farm.tables, "r");
    assert_eq!(farm.pass(BACKOFF_MS - 1, &["a", "b"], &[]), []);
    assert_eq!(farm.job("r").queue, waiting.queue, "freed, in backoff");

    assert_eq!(farm.pass(BACKOFF_MS, &["a", "b"], &[]), [id("r")]);
    let placed = farm.job("r");
    assert_eq!(placed.state, JobState::Placed);
    assert_eq!(placed.host, Some(host("b")));
    assert_eq!(placed.attempt.number(), 2);
}

/// Asserts that a retried job whose allocation is live is not placed even once its backoff has
/// ended, and is placed after the allocation is freed. Catches a pass that offers the job to
/// the store while its allocation is releasing.
#[test]
fn a_retried_job_whose_backoff_has_ended_waits_for_its_live_allocation() {
    let farm = Farm::new();
    queue(&farm.tables, "r");
    retry_on(&farm.tables, "r", "a", BACKOFF_MS);
    let waiting = farm.job("r");
    assert_eq!(farm.pass(20_000, &["a", "b"], &[]), []);
    assert_eq!(farm.job("r").queue, waiting.queue);
    free(&farm.tables, "r");
    assert_eq!(farm.pass(20_000, &["a", "b"], &[]), [id("r")]);
}

/// Asserts that a host of an earlier attempt is not chosen once the 15-minute bar has lapsed
/// when it does not report `start-fence` (and the job's row keeps its bars), and is chosen
/// when it does; and that it is not chosen before the bar lapses even with the fence. Catches
/// the fence not read from the host's offer, a for-life bar that lapses, and a bar that is
/// written to the row.
#[test]
fn a_host_of_an_earlier_attempt_is_chosen_only_when_it_reports_start_fence() {
    let farm = Farm::new();
    queue(&farm.tables, "r");
    retry_on(&farm.tables, "r", "a", BACKOFF_MS);
    free(&farm.tables, "r");
    let before = farm.job("r").prepare_bars;

    assert_eq!(farm.pass(BACKOFF_MS, &["a"], &["a"]), [], "inside the bar");
    let late = BAR_END_MS + 100_000;
    assert_eq!(farm.pass(late, &["a"], &[]), [], "no fence, bar lapsed");
    assert_eq!(farm.job("r").prepare_bars, before, "the row is not written");
    assert_eq!(farm.pass(late, &["a"], &["a"]), [id("r")]);
    assert_eq!(farm.job("r").host, Some(host("a")));
}

//! The pass over the job tables ([`SchedPlacer::place_jobs_with`]) for the jobs that need the
//! whole host, and the wall limit of a job placed beside a whole-host job's mark: the job
//! tables on the in-memory fake, the placer on a clock the test moves by hand.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use js_elastic::RefusalCode;
use js_jobs::{Booked, Isolation, JobId, JobRecord, JobState, JobStoreError, Needs, NewJob};
use js_registry::{Placer, WholeNodeConfig};
use js_sched::Backfill;
use js_sched::failover::Refused;
use js_sched::queue_status::Outcome;
use js_state::{Store, StoreError};
use js_types::{HostId, Resources};
use js_types::{JobEventKind, ProviderId, QueueCode};
use js_types::{ManualClock, Timestamp};

use super::{JobHosts, JobStarts, OpenHost, PassMetrics};
use crate::engine::Settings;
use crate::engine::rig::{
    Tables, at, engine_with, every, host, hosts, mark, run, settings, spec_of, store,
};
use crate::place::SchedPlacer;

/// What a pass told its metrics and its starts.
#[derive(Default)]
struct Recorder {
    placed: Mutex<Vec<Duration>>,
    started: Mutex<Vec<(JobId, HostId)>>,
    tainted: Mutex<Vec<(HostId, JobId)>>,
    failed: Mutex<u32>,
}

impl PassMetrics for Recorder {
    fn job_placed(&self, waited: Duration) {
        self.placed.lock().unwrap().push(waited);
    }

    fn store_write_failed(&self) {
        *self.failed.lock().unwrap() += 1;
    }
}

impl JobStarts for Recorder {
    fn placed(&self, job: &JobId, host: &HostId) {
        self.started
            .lock()
            .unwrap()
            .push((job.clone(), host.clone()));
    }

    fn tainted(&self, host: &HostId, job: &JobId) {
        self.tainted
            .lock()
            .unwrap()
            .push((host.clone(), job.clone()));
    }
}

impl Recorder {
    /// How many writes failed.
    fn failures(&self) -> u32 {
        *self.failed.lock().unwrap()
    }
}

/// A placer over the hosts `a` and `b` and job tables, both on clocks the test moves.
struct Rig {
    placer: SchedPlacer,
    clock: ManualClock,
    tables: Tables,
}

impl Rig {
    /// A rig whose engine is built at zero over `hosts`, each busy with the allocations
    /// given: the jobs `held-<seq>` are placed on them in the tables and the engine takes the
    /// allocations up as the pass does.
    fn new(held: &[(&str, &[u64])]) -> Self {
        let clock = ManualClock::new(Timestamp::from_millis(0));
        let placer = SchedPlacer::new(Settings::default(), clock.clone());
        let tables = Tables::new();
        let hosts = every(held);
        for candidate in &hosts {
            placer
                .engine()
                .host_up(&candidate.host, &candidate.shape, at(0));
        }
        placer.engine().sync_jobs(&tables.occupy(&hosts));
        Self {
            placer,
            clock,
            tables,
        }
    }

    /// Both hosts busy with an allocation.
    fn busy() -> Self {
        Self::new(&[("a", &[1]), ("b", &[2])])
    }

    /// Both hosts quiet.
    fn quiet() -> Self {
        Self::new(&[("a", &[]), ("b", &[])])
    }

    /// Moves the placer's clock and the store's to `ms` milliseconds.
    fn at(&self, ms: u64) {
        self.clock.set(Timestamp::from_millis(ms));
        self.tables.set_ms(ms);
    }

    /// The hosts that may take a job, each with the default ceiling and running host jobs.
    fn open(&self) -> BTreeMap<HostId, OpenHost> {
        let mut open = BTreeMap::new();
        for name in ["a", "b"] {
            let offer = OpenHost {
                host_jobs: true,
                ..OpenHost::default()
            };
            open.insert(host(name), offer);
        }
        open
    }

    /// Records the queued job `id` that asks for `needs` and runs on the host itself.
    fn host_job(&self, id: &str, needs: Needs) -> JobRecord {
        let mut spec = spec_of(needs, 600);
        spec.constraints.isolation = Isolation::Host;
        let new = NewJob {
            id: Some(JobId::parse(id).unwrap()),
            spec,
            priority: 0,
            client: None,
        };
        self.tables.jobs.insert_queued(new).unwrap().record
    }

    /// A pass with the pool's settings after both hosts said hello: the jobs it placed.
    fn go(&self, recorder: &Recorder) -> Vec<JobId> {
        let whole = settings();
        self.pass(Some(&whole), &["a", "b"], recorder).unwrap()
    }

    /// A pass with the pool settings `whole`, the hosts `hello` having said hello, after both
    /// hosts' heartbeats.
    fn pass(
        &self,
        whole: Option<&WholeNodeConfig>,
        hello: &[&str],
        recorder: &Recorder,
    ) -> Result<Vec<JobId>, JobStoreError> {
        for name in ["a", "b"] {
            self.placer.heard(&host(name), None);
        }
        let open = self.open();
        let hello = hosts(hello);
        let on = JobHosts {
            open: &open,
            hello: &hello,
            whole,
        };
        let jobs = &self.tables.jobs;
        self.placer.place_jobs_with(jobs, &on, (recorder, recorder))
    }
}

/// Catches: a whole-host job granted a quiet host and never committed by the pass, a commit
/// that does not book the whole host, set no prepare deadline or leaves the mark, a commit
/// the metrics and the starts are not told of, and an engine that does not hold what the
/// store holds. `j`, submitted at 400 ms, is placed on `a` at 1 s: placed with the whole host
/// booked, a prepare deadline of the pool's `prepare_timeout` after the commit and no wall
/// cut; the metrics hear it waited 600 ms; the engine holds the allocation and queues no job.
#[test]
fn a_whole_host_job_granted_a_quiet_host_is_committed_in_the_pass() {
    let rig = Rig::quiet();
    rig.at(400);
    let j = rig.tables.whole("j");
    rig.at(1_000);
    let recorder = Recorder::default();
    assert_eq!(rig.go(&recorder), std::slice::from_ref(&j.id));
    let job = rig.tables.jobs.get_job(&j.id).unwrap().unwrap();
    assert_eq!(job.state, JobState::Placed);
    assert_eq!(job.host, Some(host("a")));
    let allocation = rig.tables.jobs.get_allocation(&j.id).unwrap().unwrap();
    assert_eq!(allocation.booked, Booked::WholeHost);
    assert_eq!(allocation.prepare_deadline, Some(store(601_000)));
    assert_eq!(allocation.wall_limit_secs, None);
    assert_eq!(rig.tables.jobs.get_mark(&j.id).unwrap(), None);
    let waited = recorder.placed.lock().unwrap().clone();
    assert_eq!(waited, [Duration::from_millis(600)]);
    let started = recorder.started.lock().unwrap().clone();
    assert_eq!(started, [(j.id.clone(), host("a"))]);
    assert_eq!(recorder.failures(), 0);
    let engine = rig.placer.engine();
    assert!(engine.holds_jobs_on(&host("a")));
    assert!(engine.whole_jobs.is_empty());
}

/// Catches: a mark the engine made and the pass never wrote, a mark written again at every
/// pass (a commit each time), and a mark given up or moved at its deadline. On busy hosts
/// `j` marks `a` at 20 s: the mark is in the table with its times; a second pass at the same
/// time commits nothing; at the deadline and past it the mark on `a` stands as written and
/// nothing is committed.
#[test]
fn the_marks_of_the_engine_are_written_to_the_table_once() {
    let rig = Rig::busy();
    let j = rig.tables.whole("j");
    let recorder = Recorder::default();
    rig.at(20_000);
    assert!(rig.go(&recorder).is_empty());
    let marked = rig.tables.jobs.get_mark(&j.id).unwrap().unwrap();
    assert_eq!(marked.host, host("a"));
    assert_eq!(marked.made, store(20_000));
    assert_eq!(marked.deadline, store(80_000));
    let before = rig.tables.store.commit_stats().txns;
    rig.go(&recorder);
    assert_eq!(rig.tables.store.commit_stats().txns, before);
    for at in [80_000, 200_000] {
        rig.at(at);
        rig.go(&recorder);
        assert_eq!(rig.tables.store.commit_stats().txns, before, "at {at}");
        let kept = rig.tables.jobs.get_mark(&j.id).unwrap().unwrap();
        assert_eq!(kept, marked, "at {at}");
    }
}

/// Catches: a stored mark dropped at a pass before `claim_by` (the server's start plus the
/// pool's `regain_grace`), kept past it when its host never says hello, or replaced when
/// its host said hello in time. A mark stored on `a` before the server started is kept at
/// 10 s with no hello (and no job queued for it); at 15 s it is replaced by a fresh mark; and
/// on another server, with `a` said hello, it is kept as stored and the job is queued.
#[test]
fn a_stored_mark_waits_for_its_hello_until_the_server_start_plus_the_grace() {
    let rig = Rig::busy();
    let j = rig.tables.whole("j");
    let held = mark(&j, "a", (5_000, 65_000), None);
    let stored = rig.tables.jobs.put_mark(held).unwrap();
    let whole = settings();
    let recorder = Recorder::default();
    rig.at(10_000);
    rig.pass(Some(&whole), &[], &recorder).unwrap();
    let now = rig.tables.jobs.get_mark(&j.id).unwrap();
    assert_eq!(now, Some(stored));
    assert!(rig.placer.engine().whole_jobs.is_empty());
    rig.at(15_000);
    rig.pass(Some(&whole), &[], &recorder).unwrap();
    let remade = rig.tables.jobs.get_mark(&j.id).unwrap().unwrap();
    assert_eq!(remade.made, store(15_000));
    let heard = Rig::busy();
    let k = heard.tables.whole("k");
    let held = mark(&k, "a", (5_000, 65_000), None);
    let kept = heard.tables.jobs.put_mark(held).unwrap();
    // Past `drain_after` from the submit (the fake store's clock moves with each commit), and
    // before `claim_by`.
    heard.at(12_000);
    heard.pass(Some(&whole), &["a"], &recorder).unwrap();
    assert_eq!(heard.tables.jobs.get_mark(&k.id).unwrap(), Some(kept));
    assert_eq!(heard.placer.engine().whole_jobs.len(), 1);
}

/// Catches: a failed delete of a mark that takes the pass with it, goes uncounted, or stops
/// the writes after it. `j`'s stored mark on `a`, whose host never says hello, is dropped at
/// `claim_by`; the delete fails: the pass ends well, counts one failed write, and goes on to
/// write the job's fresh mark, which the table holds in place of the stored one.
#[test]
fn a_mark_that_could_not_be_deleted_is_counted_and_the_pass_goes_on() {
    let rig = Rig::busy();
    let j = rig.tables.whole("j");
    let held = mark(&j, "a", (5_000, 65_000), None);
    rig.tables.jobs.put_mark(held).unwrap();
    let whole = settings();
    let recorder = Recorder::default();
    rig.at(15_000);
    rig.tables.store.fail_next();
    rig.pass(Some(&whole), &[], &recorder).unwrap();
    assert_eq!(recorder.failures(), 1);
    let remade = rig.tables.jobs.get_mark(&j.id).unwrap().unwrap();
    assert_eq!(remade.made, store(15_000));
}

/// Catches: a commit that fails and leaves the engine holding the host (the job is queued in
/// the table and the host is held in the engine for ever), a failure that is not counted, and
/// a job that is not placed by the next pass. The first commit fails: the job stays queued,
/// the engine holds nothing and counts a failed write; the next pass places it.
#[test]
fn a_commit_that_fails_leaves_the_job_queued_and_the_next_pass_places_it() {
    let rig = Rig::quiet();
    let j = rig.tables.whole("j");
    rig.at(1_000);
    let recorder = Recorder::default();
    rig.tables.store.fail_next();
    assert!(rig.go(&recorder).is_empty());
    assert_eq!(recorder.failures(), 1);
    let job = rig.tables.jobs.get_job(&j.id).unwrap().unwrap();
    assert_eq!(job.state, JobState::Queued);
    assert!(!rig.placer.engine().holds_jobs_on(&host("a")));
    assert_eq!(rig.placer.engine().whole_jobs.len(), 1);
    rig.at(2_000);
    assert_eq!(rig.go(&recorder), std::slice::from_ref(&j.id));
}

/// Catches: a store with a newer writer that the pass goes on writing to, and a failed write
/// that is not counted. A commit answered fenced ends the pass with the error and counts it.
#[test]
fn a_fenced_commit_ends_the_pass_with_the_error() {
    let rig = Rig::quiet();
    rig.tables.whole("j");
    rig.at(1_000);
    let recorder = Recorder::default();
    rig.tables.store.fail_with(StoreError::Fenced);
    rig.tables.store.fail_next();
    let whole = settings();
    let result = rig.pass(Some(&whole), &["a", "b"], &recorder);
    assert!(matches!(
        result,
        Err(JobStoreError::Store(StoreError::Fenced))
    ));
    assert_eq!(recorder.failures(), 1);
}

/// Catches: a mark write that fails and takes the pass with it (the other jobs would not be
/// placed), one that is not counted, or one that is not tried again at the next pass. The
/// commit of the mark fails: the pass ends well, counts it, and the next pass writes it.
#[test]
fn a_mark_that_could_not_be_written_is_counted_and_written_by_the_next_pass() {
    let rig = Rig::busy();
    let j = rig.tables.whole("j");
    rig.at(20_000);
    let recorder = Recorder::default();
    rig.tables.store.fail_next();
    assert!(rig.go(&recorder).is_empty());
    assert_eq!(recorder.failures(), 1);
    assert_eq!(rig.tables.jobs.get_mark(&j.id).unwrap(), None);
    rig.go(&recorder);
    assert!(rig.tables.jobs.get_mark(&j.id).unwrap().is_some());
}

/// Catches: `place_jobs`, which knows no whole-node settings, marking or placing a job that
/// needs the whole host, or touching the stored mark of a queued whole-host job (only a mark
/// of a job that is not queued is deleted, with or without settings). With a stored mark and a
/// quiet host, the pass places nothing, queues nothing in the engine and leaves the mark as
/// stored.
#[test]
fn a_pass_without_whole_node_settings_places_no_whole_host_job_and_leaves_its_marks() {
    let rig = Rig::quiet();
    let j = rig.tables.whole("j");
    let held = mark(&j, "a", (5_000, 65_000), None);
    let stored = rig.tables.jobs.put_mark(held).unwrap();
    rig.at(20_000);
    for name in ["a", "b"] {
        rig.placer.heard(&host(name), None);
    }
    let recorder = Recorder::default();
    let (jobs, open) = (&rig.tables.jobs, rig.open());
    let placed = rig.placer.place_jobs(jobs, &open, (&recorder, &recorder));
    assert!(placed.unwrap().is_empty());
    assert_eq!(rig.tables.jobs.get_mark(&j.id).unwrap(), Some(stored));
    assert!(rig.placer.engine().whole_jobs.is_empty());
    let job = rig.tables.jobs.get_job(&j.id).unwrap().unwrap();
    assert_eq!(job.state, JobState::Queued);
}

/// Catches: an ordinary job given a prepare deadline or a wall cut it does not have, an
/// ordinary placement that the new commit does not make, and a job no host has room for
/// placed all the same. A job that asks for a share is placed on a quiet host with a booked
/// share, no prepare deadline and no wall cut, and its start is handed on; a job that asks
/// for more memory than a host has stays queued.
#[test]
fn an_ordinary_job_is_still_placed_with_no_prepare_deadline_and_no_wall_cut() {
    let rig = Rig::quiet();
    let s = rig.tables.share("s");
    let too_big = Resources::new(1000, 64 << 30, 0, None).unwrap();
    let big = rig.tables.submit_with("big", Needs::Shape(too_big), 600);
    rig.at(1_000);
    let recorder = Recorder::default();
    assert_eq!(rig.go(&recorder), std::slice::from_ref(&s.id));
    let allocation = rig.tables.jobs.get_allocation(&s.id).unwrap().unwrap();
    assert!(matches!(allocation.booked, Booked::Shape { .. }));
    assert_eq!(allocation.prepare_deadline, None);
    assert_eq!(allocation.wall_limit_secs, None);
    assert_eq!(recorder.started.lock().unwrap().len(), 1);
    let left = rig.tables.jobs.get_job(&big.id).unwrap().unwrap();
    assert_eq!(left.state, JobState::Queued);
}

/// Catches: a committed host-isolation placement that is not told as a taint of its host, and
/// a whole-host job with host isolation marked or committed by the whole-host pass, which
/// does not hold a job to the hosts that run host jobs. On quiet hosts that run host jobs, `h`
/// (a share) is placed and told both as a start and as the taint of its host; `w`, which needs
/// the whole host and runs on the host itself, stays queued with no mark.
#[test]
fn a_host_isolation_job_is_told_as_a_taint_and_a_whole_host_one_waits() {
    let rig = Rig::quiet();
    let w = rig.host_job("w", Needs::WholeHost);
    let resources = Resources::new(1000, 1 << 30, 0, None).unwrap();
    let h = rig.host_job("h", Needs::Shape(resources));
    rig.at(1_000);
    let recorder = Recorder::default();
    assert_eq!(rig.go(&recorder), std::slice::from_ref(&h.id));
    let placed = rig.tables.jobs.get_job(&h.id).unwrap().unwrap();
    let on = placed.host.unwrap();
    assert_eq!(
        *recorder.tainted.lock().unwrap(),
        [(on.clone(), h.id.clone())]
    );
    assert_eq!(*recorder.started.lock().unwrap(), [(h.id.clone(), on)]);
    let waiting = rig.tables.jobs.get_job(&w.id).unwrap().unwrap();
    assert_eq!(waiting.state, JobState::Queued);
    assert_eq!(rig.tables.jobs.get_mark(&w.id).unwrap(), None);
}

/// Catches: an ordinary placement whose commit fails left booked in the engine (the job is
/// queued in the table and its host room is held for ever), or not counted. The commit fails:
/// the job stays queued, the engine holds nothing and one write is counted; the next pass
/// places it.
#[test]
fn an_ordinary_commit_that_fails_leaves_the_job_queued() {
    let rig = Rig::quiet();
    let s = rig.tables.share("s");
    rig.at(1_000);
    let recorder = Recorder::default();
    rig.tables.store.fail_next();
    assert!(rig.go(&recorder).is_empty());
    assert_eq!(recorder.failures(), 1);
    assert!(!rig.placer.engine().holds_jobs_on(&host("a")));
    rig.at(2_000);
    assert_eq!(rig.go(&recorder), std::slice::from_ref(&s.id));
}

/// What `walk_jobs` does with a share job of `wall_secs` offered `b`, which a whole-host job
/// marks with a tail ending at ten minutes: `a` is busy with a job of an hour and `b` with one
/// of ten minutes, both placed at the start, so the mark is made on `b`. The job's placement
/// on `b` with the wall limit it is given there, or `None` when it waits.
fn backfilled(wall_secs: u64) -> Option<Option<u64>> {
    let tables = Tables::new();
    let whole = tables.whole("j");
    let resources = Resources::new(1000, 1 << 30, 0, None).unwrap();
    let share = tables.submit_with("bf", Needs::Shape(resources.clone()), wall_secs);
    let backfill = Backfill::enforced(Duration::MAX);
    let settings = Settings {
        backfill,
        ..Settings::default()
    };
    let mut e = engine_with(settings, &every(&[("a", &[]), ("b", &[])]));
    for (name, wall) in [("a", 3_600), ("b", 600)] {
        let id = format!("busy-{name}");
        let busy = tables.submit_with(&id, Needs::Shape(resources.clone()), wall);
        let open = BTreeMap::from([(host(name), OpenHost::default())]);
        let (decided, _) = e.walk_jobs(&[busy], &open, at(0));
        assert_eq!(decided.len(), 1);
    }
    let steps = run(&mut e, (&[whole], &[]), 20_000);
    assert_eq!(steps.len(), 1);
    let open = BTreeMap::from([(host("b"), OpenHost::default())]);
    let (decided, waiting) = e.walk_jobs(&[share], &open, at(20_000));
    match decided.first() {
        Some(placed) => {
            assert!(waiting.is_empty());
            assert_eq!(placed.1, host("b"));
            Some(placed.3.wall_limit_secs)
        }
        None => {
            assert_eq!(waiting.len(), 1);
            None
        }
    }
}

/// Catches: a job backfilled on a marked host that outlives the mark's tail (the marked host
/// would wait for it), or a job that ends within the tail held back or given another wall
/// limit than its own. The mark's tail ends at 600 s and the pass is at 20 s: a 100 s job is
/// placed on the marked host with its own wall limit; a one-hour job is not placed there and
/// waits.
#[test]
fn a_job_is_backfilled_on_a_marked_host_only_when_it_ends_within_the_tail() {
    assert_eq!(backfilled(100), Some(None));
    assert_eq!(backfilled(3_600), None);
}

// ---- queue status ----------------------------------------------------------------------

/// Both hosts full: eight one-core allocations on each of their eight cores.
fn full() -> Rig {
    let a: Vec<u64> = (1..=8).collect();
    let b: Vec<u64> = (9..=16).collect();
    Rig::new(&[("a", &a), ("b", &b)])
}

/// The job `id` as the tables hold it.
fn read(rig: &Rig, id: &JobId) -> JobRecord {
    rig.tables.jobs.get_job(id).unwrap().unwrap()
}

/// The kinds and times of a job's events, in milliseconds on the store's clock.
fn events(job: &JobRecord) -> Vec<(JobEventKind, u64, Option<HostId>)> {
    let ms = |at: js_state::StoreTime| u64::try_from(at.as_duration().as_millis()).unwrap();
    let each = job.queue.events().iter();
    each.map(|event| (event.kind, ms(event.at), event.host.clone()))
        .collect()
}

/// A refusal pass 2 recorded for the job's request, for `code`.
fn refusal(code: RefusalCode, zone: &str) -> Outcome {
    Outcome::Refused(Refused {
        provider: ProviderId::new("cloud").unwrap(),
        offer: "big".to_owned(),
        zone: Some(zone.to_owned()),
        code,
        passed_over: None,
        request: None,
    })
}

/// Catches: a job placed nowhere that shows no reason, a reason written again at every pass,
/// and a time that is not the store's at the change. On full hosts a one-core job waits for
/// capacity from the pass at 5 s, with one event then and no attempt; a second pass at 9 s
/// writes nothing and the time stays 5 s.
#[test]
fn a_job_with_no_room_waits_for_capacity_from_the_pass_that_found_none() {
    let rig = full();
    let j = rig.tables.share("j");
    rig.at(5_000);
    let recorder = Recorder::default();
    assert!(rig.go(&recorder).is_empty());
    let job = read(&rig, &j.id);
    assert_eq!(job.queue.code(), QueueCode::WaitingForCapacity);
    assert_eq!(
        job.queue.detail(),
        "no open host that can run the job has room for it now"
    );
    assert_eq!(job.queue.since(), Some(store(5_000)));
    assert_eq!(job.queue.attempts(), 0);
    assert_eq!(
        events(&job),
        [(JobEventKind::WaitingForCapacity, 5_000, None)]
    );
    let before = rig.tables.store.commit_stats().txns;
    rig.at(9_000);
    rig.go(&recorder);
    assert_eq!(rig.tables.store.commit_stats().txns, before);
    assert_eq!(read(&rig, &j.id), job);
}

/// Catches: a job no open host can run told it waits for room. A job of 64 cores, more than
/// any host has, waits for capacity with the detail that no open host can run it.
#[test]
fn a_job_no_open_host_can_run_says_so() {
    let rig = Rig::quiet();
    let resources = Resources::new(64_000, 1 << 30, 0, None).unwrap();
    let j = rig.tables.submit_with("huge", Needs::Shape(resources), 600);
    rig.at(2_000);
    rig.go(&Recorder::default());
    let job = read(&rig, &j.id);
    assert_eq!(job.queue.code(), QueueCode::WaitingForCapacity);
    assert_eq!(job.queue.detail(), "no open host can run the job");
}

/// Catches: a whole-host job told it waits for room, a detail that does not name the host
/// being emptied, and a change of detail that moves the time or appends an event. On busy
/// hosts `w` waits for a whole host from 1 s with none being emptied; at 20 s it marks `a`:
/// the detail names `a`, the time stays 1 s and there is still one event.
#[test]
fn a_whole_host_job_waits_for_a_whole_host_and_then_names_its_mark() {
    let rig = Rig::busy();
    let w = rig.tables.whole("w");
    let recorder = Recorder::default();
    rig.at(1_000);
    rig.go(&recorder);
    let job = read(&rig, &w.id);
    assert_eq!(job.queue.code(), QueueCode::WaitingForWholeHost);
    assert_eq!(job.queue.detail(), "no whole host is free");
    assert_eq!(job.queue.since(), Some(store(1_000)));
    rig.at(20_000);
    rig.go(&recorder);
    let job = read(&rig, &w.id);
    assert_eq!(job.queue.detail(), "host a is being emptied for the job");
    assert_eq!(job.queue.since(), Some(store(1_000)));
    assert_eq!(
        events(&job),
        [(JobEventKind::WaitingForWholeHost, 1_000, None)]
    );
}

/// Catches: a quota refusal pass 2 told that never reaches the job, one that is not counted
/// as an attempt, and pass 1's "no room" taking its place. On full hosts `j` waits for
/// capacity at 1 s; pass 2 tells a quota refusal; the pass at 3 s shows quota backoff from
/// 3 s, one attempt, its event after the wait's, and the next pass keeps it.
#[test]
fn a_quota_refusal_backs_the_job_off_and_outlives_no_room() {
    let rig = full();
    let j = rig.tables.share("j");
    let recorder = Recorder::default();
    rig.at(1_000);
    rig.go(&recorder);
    rig.placer
        .note_outcomes([(j.id.clone(), refusal(RefusalCode::Quota, "z1"))]);
    rig.at(3_000);
    rig.go(&recorder);
    let job = read(&rig, &j.id);
    assert_eq!(job.queue.code(), QueueCode::QuotaBackoff);
    assert_eq!(job.queue.since(), Some(store(3_000)));
    assert_eq!(job.queue.attempts(), 1);
    assert_eq!(
        events(&job),
        [
            (JobEventKind::WaitingForCapacity, 1_000, None),
            (JobEventKind::QuotaBackoff, 3_000, None)
        ]
    );
    rig.at(4_000);
    rig.go(&recorder);
    assert_eq!(read(&rig, &j.id), job);
}

/// Catches: refusals that fail over counted once, a launch that leaves the job failing over
/// or that pass 1's "no room" takes the place of, and outcomes taken out of order. Pass 2
/// tells two capacity refusals and then a launch; the pass at 2 s, on full hosts, shows the
/// job waiting for the machine, from 2 s, with two attempts and the events in the order they
/// came.
#[test]
fn a_failover_then_a_launch_is_recorded_in_order() {
    let rig = full();
    let j = rig.tables.share("j");
    let launched = Outcome::Launched {
        machine: "m-1".to_owned(),
    };
    rig.placer.note_outcomes([
        (j.id.clone(), refusal(RefusalCode::Capacity, "z1")),
        (j.id.clone(), refusal(RefusalCode::Capacity, "z2")),
        (j.id.clone(), launched),
    ]);
    rig.at(2_000);
    rig.go(&Recorder::default());
    let job = read(&rig, &j.id);
    assert_eq!(job.queue.code(), QueueCode::WaitingForMachine);
    assert_eq!(
        job.queue.detail(),
        "machine m-1 was launched for the job; it waits for the machine to join"
    );
    assert_eq!(job.queue.since(), Some(store(2_000)));
    assert_eq!(job.queue.attempts(), 2);
    let kinds: Vec<JobEventKind> = events(&job).into_iter().map(|e| e.0).collect();
    assert_eq!(
        kinds,
        [
            JobEventKind::FailingOver,
            JobEventKind::FailingOver,
            JobEventKind::WaitingForMachine
        ]
    );
}

/// Catches: a placement without its status, a placement not counted as an attempt, and what
/// pass 2 told before it lost. Pass 2 tells a capacity refusal for `j`, and the pass at 6 s
/// places it on `a`: it shows placed from 6 s, two attempts, the refusal's event and then the
/// placement's naming `a`, all in the placement's own commit.
#[test]
fn a_placement_records_placed_with_what_pass_2_told_before_it() {
    let rig = Rig::quiet();
    let j = rig.tables.share("j");
    rig.placer
        .note_outcomes([(j.id.clone(), refusal(RefusalCode::Capacity, "z1"))]);
    rig.at(6_000);
    let before = rig.tables.store.commit_stats().txns;
    assert_eq!(rig.go(&Recorder::default()), std::slice::from_ref(&j.id));
    assert_eq!(rig.tables.store.commit_stats().txns, before + 1);
    let job = read(&rig, &j.id);
    assert_eq!(job.state, JobState::Placed);
    assert_eq!(job.queue.code(), QueueCode::Placed);
    assert_eq!(job.queue.since(), Some(store(6_000)));
    assert_eq!(job.queue.attempts(), 2);
    assert_eq!(
        events(&job),
        [
            (JobEventKind::FailingOver, 6_000, None),
            (JobEventKind::Placed, 6_000, Some(host("a")))
        ]
    );
}

/// Catches: a whole-host commit without its placed status. A whole-host job committed on a
/// quiet host at 1 s shows placed on `a` from 1 s with one attempt.
#[test]
fn a_whole_host_commit_records_placed() {
    let rig = Rig::quiet();
    let w = rig.tables.whole("w");
    rig.at(1_000);
    rig.go(&Recorder::default());
    let job = read(&rig, &w.id);
    assert_eq!(job.queue.code(), QueueCode::Placed);
    assert_eq!(job.queue.attempts(), 1);
    assert_eq!(
        events(&job),
        [(JobEventKind::Placed, 1_000, Some(host("a")))]
    );
}

/// Catches: a status write that fails and takes the pass with it, goes uncounted, or is never
/// tried again; and what pass 2 told of a job no longer queued kept for ever. The first status
/// write of the pass fails: the pass ends well and counts it, and the next pass writes it. An
/// outcome told for a job that is not queued is dropped by the pass.
#[test]
fn a_status_write_that_fails_is_counted_and_written_at_the_next_pass() {
    let rig = full();
    let j = rig.tables.share("j");
    let recorder = Recorder::default();
    rig.at(1_000);
    rig.tables.store.fail_next();
    assert_eq!(rig.go(&recorder), Vec::<JobId>::new());
    assert_eq!(recorder.failures(), 1);
    assert_eq!(read(&rig, &j.id).queue.code(), QueueCode::Unspecified);
    let gone = JobId::parse("gone").unwrap();
    rig.placer
        .note_outcomes([(gone, refusal(RefusalCode::Capacity, "z1"))]);
    rig.at(2_000);
    rig.go(&recorder);
    assert_eq!(read(&rig, &j.id).queue.since(), Some(store(2_000)));
    assert!(rig.placer.take_outcomes().is_empty());
}

/// Catches: a status write the store answers as fenced that does not end the pass. The
/// status write is answered with a newer writer: the pass answers the error.
#[test]
fn a_fenced_status_write_ends_the_pass() {
    let rig = full();
    rig.tables.share("j");
    rig.at(1_000);
    rig.tables.store.fail_with(StoreError::Fenced);
    rig.tables.store.fail_next();
    let whole = settings();
    let answered = rig.pass(Some(&whole), &["a", "b"], &Recorder::default());
    assert_eq!(answered, Err(JobStoreError::Store(StoreError::Fenced)));
}

/// Catches: what pass 2 told of a job lost when the write that carried it failed, and given
/// back after what pass 2 told since. A capacity refusal is told; the status write at 1 s
/// fails; a launch is told; the pass at 2 s records the refusal, then the launch, with one
/// attempt.
#[test]
fn what_pass_2_told_survives_a_status_write_that_failed() {
    let rig = full();
    let j = rig.tables.share("j");
    let recorder = Recorder::default();
    rig.placer
        .note_outcomes([(j.id.clone(), refusal(RefusalCode::Capacity, "z1"))]);
    rig.at(1_000);
    rig.tables.store.fail_next();
    rig.go(&recorder);
    assert_eq!(recorder.failures(), 1);
    assert_eq!(read(&rig, &j.id).queue.code(), QueueCode::Unspecified);
    let launched = Outcome::Launched {
        machine: "m-1".to_owned(),
    };
    rig.placer.note_outcomes([(j.id.clone(), launched)]);
    rig.at(2_000);
    rig.go(&recorder);
    let job = read(&rig, &j.id);
    assert_eq!(job.queue.code(), QueueCode::WaitingForMachine);
    assert_eq!(job.queue.attempts(), 1);
    assert_eq!(
        events(&job),
        [
            (JobEventKind::FailingOver, 2_000, None),
            (JobEventKind::WaitingForMachine, 2_000, None)
        ]
    );
}

/// Catches: what pass 2 told of a job lost with a placement whose commit failed. A capacity
/// refusal is told and the placement's commit fails: the same pass writes the refusal on the
/// job, which stays queued, failing over with one attempt.
#[test]
fn what_pass_2_told_survives_a_placement_that_failed() {
    let rig = Rig::quiet();
    let j = rig.tables.share("j");
    rig.placer
        .note_outcomes([(j.id.clone(), refusal(RefusalCode::Capacity, "z1"))]);
    rig.at(1_000);
    rig.tables.store.fail_next();
    let recorder = Recorder::default();
    assert!(rig.go(&recorder).is_empty());
    assert_eq!(recorder.failures(), 1);
    let job = read(&rig, &j.id);
    assert_eq!(job.state, JobState::Queued);
    assert_eq!(job.queue.code(), QueueCode::FailingOver);
    assert_eq!(job.queue.attempts(), 1);
    assert!(rig.placer.take_outcomes().is_empty());
}

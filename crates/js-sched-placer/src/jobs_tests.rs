//! The pass over the job tables ([`SchedPlacer::place_jobs_with`]) for the jobs that need the
//! whole host, and the wall limit of a job placed beside a whole-host job's mark: the job
//! tables on the in-memory fake, the placer on a clock the test moves by hand.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use js_agent_wire::NetworkCeiling;
use js_jobs::{Booked, JobId, JobState, JobStoreError, Needs};
use js_lease::{ManualClock, Timestamp};
use js_registry::{Placer, WholeNodeConfig};
use js_sched::Backfill;
use js_state::{Store, StoreError};
use js_types::sched::WorkKey;
use js_types::{HostId, LeaseId, Resources};

use super::{JobHosts, JobStarts, PassMetrics};
use crate::engine::rig::{Tables, at, every, host, hosts, mark, run, settings, store};
use crate::engine::{Engine, Settings};
use crate::place::SchedPlacer;

/// What a pass told its metrics and its starts.
#[derive(Default)]
struct Recorder {
    placed: Mutex<Vec<Duration>>,
    started: Mutex<Vec<(JobId, HostId)>>,
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
    /// A rig whose engine is built at zero over `hosts`, each with the registry leases given.
    fn new(held: &[(&str, &[u64])]) -> Self {
        let clock = ManualClock::new(Timestamp::from_millis(0));
        let placer = SchedPlacer::new(Settings::default(), clock.clone());
        *placer.engine() = Engine::build(Settings::default(), &every(held), at(0));
        Self {
            placer,
            clock,
            tables: Tables::new(),
        }
    }

    /// Both hosts busy with a lease of the registry.
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

    /// The hosts that may take a job, each with the default ceiling.
    fn open(&self) -> BTreeMap<HostId, NetworkCeiling> {
        let mut open = BTreeMap::new();
        for name in ["a", "b"] {
            open.insert(host(name), NetworkCeiling::default());
        }
        open
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
    assert_eq!(rig.go(&recorder), [j.id.clone()]);
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
/// pass (a commit each time), and a give-up that is not written. On busy hosts `j` marks
/// `a` at 20 s: the mark is in the table with its times; a second pass at the same time
/// commits nothing; at the deadline the job gives `a` up and the mark on `b` replaces it.
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
    rig.at(80_000);
    rig.go(&recorder);
    let moved = rig.tables.jobs.get_mark(&j.id).unwrap().unwrap();
    assert_eq!(moved.host, host("b"));
    assert_eq!(moved.made, store(80_000));
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
    heard.at(10_000);
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
    assert_eq!(rig.go(&recorder), [j.id.clone()]);
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
    assert_eq!(rig.go(&recorder), [s.id.clone()]);
    let allocation = rig.tables.jobs.get_allocation(&s.id).unwrap().unwrap();
    assert!(matches!(allocation.booked, Booked::Shape { .. }));
    assert_eq!(allocation.prepare_deadline, None);
    assert_eq!(allocation.wall_limit_secs, None);
    assert_eq!(recorder.started.lock().unwrap().len(), 1);
    let left = rig.tables.jobs.get_job(&big.id).unwrap().unwrap();
    assert_eq!(left.state, JobState::Queued);
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
    assert_eq!(rig.go(&recorder), [s.id.clone()]);
}

/// The wall limit `walk_jobs` gives a share job of `wall_secs` placed on `b`, which a
/// whole-host job marks with a tail ending at ten minutes: the job's start is expected a
/// minute, so it fits the tail whatever its wall limit.
fn backfilled(wall_secs: u64) -> Option<u64> {
    let tables = Tables::new();
    let whole = tables.whole("j");
    let resources = Resources::new(1000, 1 << 30, 0, None).unwrap();
    let share = tables.submit_with("bf", Needs::Shape(resources), wall_secs);
    let backfill = Backfill::enforced(Duration::MAX);
    let settings = Settings {
        backfill,
        ..Settings::default()
    };
    let busy = every(&[("a", &[1]), ("b", &[2])]);
    let mut e = Engine::build(settings, &busy, at(0));
    let ends = WorkKey::new("held", LeaseId::new(1, 2).to_string());
    let lives = BTreeMap::from([(ends, 10), (WorkKey::new("job", "bf"), 1)]);
    e.lives.set(lives);
    let steps = run(&mut e, (&[whole], &[]), 20_000);
    assert_eq!(steps.len(), 1);
    let open = BTreeMap::from([(host("b"), NetworkCeiling::default())]);
    let decided = e.walk_jobs(&[share], &open, at(20_000));
    assert_eq!(decided.len(), 1);
    assert_eq!(decided[0].1, host("b"));
    decided[0].3.wall_limit_secs
}

/// Catches: a job backfilled on a marked host given its own wall limit past the mark's tail
/// (the marked host would wait for it), or cut when it already ends sooner. The mark's tail
/// ends at 600 s and the pass is at 20 s: a one-hour job is cut to 520 s, the time left
/// less the 60 s margin; a 100 s job is not cut.
#[test]
fn a_job_backfilled_on_a_marked_host_has_its_wall_limit_cut_to_the_tail() {
    assert_eq!(backfilled(3_600), Some(520));
    assert_eq!(backfilled(100), None);
}

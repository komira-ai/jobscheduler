//! The pass after a heartbeat, which runs only when something it depends on changed
//! ([`SchedPlacer::place_jobs_if_changed`]), the count of the changes the registry makes to the
//! engine ([`SchedPlacer::engine_changes`]), and the job tables a pass reads again only after a
//! write of the job store ([`SchedPlacer::tables`]). The job tables are on the in-memory fake,
//! the placer on a clock that stands still.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::time::Duration;

use js_jobs::interfere::Read;
use js_jobs::{JobId, JobStoreError};
use js_registry::{Placer, WholeNodeConfig};
use js_types::sched::usage::NodeLoad;
use js_types::{HostId, ManualClock, Timestamp};

use super::super::jobs::{JobHosts, JobStarts, OpenHost, PassMetrics};
use super::super::rig::{Tables, candidate, host, settings};
use crate::engine::Settings;
use crate::place::SchedPlacer;

/// Counts the writes of a pass that failed; hands no start on.
#[derive(Default)]
struct Failures(Mutex<u32>);

impl PassMetrics for Failures {
    fn job_placed(&self, _: Duration) {}

    fn store_write_failed(&self) {
        *self.0.lock().unwrap() += 1;
    }
}

impl JobStarts for Failures {
    fn placed(&self, _: &JobId, _: &HostId) {}

    fn tainted(&self, _: &HostId, _: &JobId) {}
}

/// The hosts a pass is offered, the hosts that said hello and the pool's whole-node settings.
#[derive(Clone)]
struct Offered {
    open: BTreeMap<HostId, OpenHost>,
    hello: BTreeSet<HostId>,
    whole: Option<WholeNodeConfig>,
}

impl Offered {
    /// The hosts `a` and `b`, open and having said hello, with no whole-node settings.
    fn both() -> Offered {
        let names = [host("a"), host("b")];
        Offered {
            open: names
                .iter()
                .map(|h| (h.clone(), OpenHost::default()))
                .collect(),
            hello: names.into_iter().collect(),
            whole: None,
        }
    }

    fn hosts(&self) -> JobHosts<'_> {
        JobHosts {
            open: &self.open,
            hello: &self.hello,
            whole: self.whole.as_ref(),
        }
    }
}

/// A placer whose registry told it of the hosts `a` and `b` (8 cores each), and job tables.
struct Rig {
    placer: SchedPlacer,
    tables: Tables,
    failures: Failures,
}

impl Rig {
    fn new() -> Rig {
        let clock = ManualClock::new(Timestamp::from_millis(0));
        let placer = SchedPlacer::new(Settings::default(), clock);
        for name in ["a", "b"] {
            placer.host_up(&host(name), &candidate(name, &[]).shape);
        }
        Rig {
            placer,
            tables: Tables::new(),
            failures: Failures::default(),
        }
    }

    /// A pass after a heartbeat over `offered`: the jobs it placed, or `None` when it was
    /// skipped.
    fn beat(&self, offered: &Offered) -> Result<Option<Vec<JobId>>, JobStoreError> {
        let sinks = (
            &self.failures as &dyn JobStarts,
            &self.failures as &dyn PassMetrics,
        );
        let jobs = &self.tables.jobs;
        self.placer
            .place_jobs_if_changed(jobs, &offered.hosts(), sinks)
    }

    /// [`Rig::beat`], which ran and placed `placed`.
    #[track_caller]
    fn ran(&self, offered: &Offered, placed: &[&str]) {
        let ids: Vec<JobId> = placed.iter().map(|id| JobId::parse(id).unwrap()).collect();
        assert_eq!(self.beat(offered), Ok(Some(ids)), "the pass did not run");
    }

    /// [`Rig::beat`], which was skipped and read nothing of the store.
    #[track_caller]
    fn skipped(&self, offered: &Offered) {
        let _ = self.tables.store.take_reads();
        assert_eq!(self.beat(offered), Ok(None), "the pass ran");
        assert_eq!(
            self.tables.store.take_reads(),
            [],
            "a skipped pass read the store"
        );
    }

    /// Records the queued job `id` asking for every core of a host.
    fn queue_whole_cores(&self, id: &str) {
        let cores = js_types::Resources::new(8000, 1 << 30, 0, None).unwrap();
        self.tables
            .submit_with(id, js_jobs::Needs::Shape(cores), 600);
    }
}

/// A load reading of a host, the `seq`-th of its agent.
fn reading(seq: u64) -> NodeLoad {
    NodeLoad {
        seq,
        mono_usec: seq * 1_000_000,
        ..NodeLoad::default()
    }
}

/// Asserts that the first pass after a heartbeat runs, that one after it runs again when the
/// first wrote (its placements), and that the next, with nothing changed, is skipped and reads
/// nothing of the store; and that the count of skipped passes says so.
#[test]
fn a_pass_with_nothing_changed_since_the_last_is_skipped_and_reads_nothing() {
    let rig = Rig::new();
    let both = Offered::both();
    for id in ["one", "two", "three"] {
        rig.queue_whole_cores(id);
    }
    rig.ran(&both, &["one", "two"]);
    rig.ran(&both, &[]);
    rig.skipped(&both);
    rig.skipped(&both);
    assert_eq!(rig.placer.passes_skipped(), 2);
}

/// Asserts that each input of a pass brings the next pass after a heartbeat back, and that
/// with that input as it was again after it, the one after is skipped: a write of the job
/// store, a load reading, the hosts open to jobs or what one of them offers, the hosts that
/// said hello, the pool's whole-node settings, and what pass 2 told. A heartbeat with no
/// reading changes nothing.
#[test]
fn each_input_of_a_pass_brings_the_next_one_back() {
    let rig = Rig::new();
    let both = Offered::both();
    rig.queue_whole_cores("one");
    rig.queue_whole_cores("two");
    rig.queue_whole_cores("three");
    rig.ran(&both, &["one", "two"]);
    rig.ran(&both, &[]);
    rig.skipped(&both);

    // The pass that reads the new job writes its queue status, a write of its own: the next
    // pass runs once more, finds it written, and the one after is skipped.
    rig.queue_whole_cores("four");
    rig.ran(&both, &[]);
    rig.ran(&both, &[]);
    rig.skipped(&both);

    rig.placer.heard(&host("a"), None);
    rig.skipped(&both);
    rig.placer.heard(&host("a"), Some(&reading(1)));
    rig.ran(&both, &[]);
    rig.skipped(&both);

    let mut changed = both.clone();
    changed.open.remove(&host("b"));
    rig.ran(&changed, &[]);
    rig.skipped(&changed);
    let mut offered = changed.clone();
    let ceiling = js_agent_wire::NetworkCeiling {
        level: js_types::JobNetwork::Open,
        ..js_agent_wire::NetworkCeiling::default()
    };
    offered.open.insert(
        host("b"),
        OpenHost {
            ceiling,
            host_jobs: true,
        },
    );
    rig.ran(&offered, &[]);
    rig.ran(&both, &[]);
    rig.skipped(&both);

    let mut hello = both.clone();
    hello.hello.remove(&host("a"));
    rig.ran(&hello, &[]);
    let mut whole = both.clone();
    whole.whole = Some(settings());
    rig.ran(&whole, &[]);
    rig.ran(&both, &[]);
    rig.skipped(&both);

    let three = JobId::parse("three").unwrap();
    let told = js_sched::queue_status::Outcome::Launched {
        machine: "m-1".to_owned(),
    };
    rig.placer.note_outcomes([(three, told)]);
    rig.ran(&both, &[]);
    rig.ran(&both, &[]);
    rig.skipped(&both);
}

/// Asserts that a pass whose read of the tables failed, or whose placement did not commit, is
/// not taken as what the next pass after a heartbeat may skip on: the next one runs, and
/// places the job whose commit failed.
#[test]
fn a_pass_that_failed_a_read_or_a_write_is_run_again() {
    let rig = Rig::new();
    let both = Offered::both();
    rig.ran(&both, &[]);
    rig.skipped(&both);
    rig.queue_whole_cores("one");
    rig.tables.store.fail_read_after(0);
    assert!(rig.beat(&both).is_err(), "the read was to fail");
    rig.tables.store.fail_next();
    rig.ran(&both, &[]);
    assert_eq!(
        *rig.failures.0.lock().unwrap(),
        1,
        "the failed commit is counted"
    );
    rig.ran(&both, &["one"]);
    rig.ran(&both, &[]);
    rig.skipped(&both);
}

/// Asserts that a pass whose read failed is not kept as what the next pass may skip on: a job
/// is submitted, the pass that sees the write fails its read, and the next pass, which finds
/// the job store, the engine and the hosts as the failed pass found them (a failed read writes
/// nothing), runs and places the job. Were the failed pass kept, the next would be skipped.
#[test]
fn a_pass_whose_read_failed_is_run_again_with_nothing_else_changed() {
    let rig = Rig::new();
    let both = Offered::both();
    rig.ran(&both, &[]);
    rig.skipped(&both);
    rig.queue_whole_cores("one");
    rig.tables.store.fail_read_after(0);
    let written = rig.tables.jobs.written();
    assert!(rig.beat(&both).is_err(), "the read was to fail");
    assert_eq!(
        rig.tables.jobs.written(),
        written,
        "a failed read writes nothing"
    );
    rig.ran(&both, &["one"]);
    rig.ran(&both, &[]);
    rig.skipped(&both);
}

/// Asserts that a pass reads the job tables again only after a write of the job store: a
/// second pass with none between reads nothing, whether the tick's (which always runs) or one
/// after a heartbeat with a reading; and that the rows read from one job store never stand
/// for another's, though both stores are fresh and neither has written since it opened.
#[test]
fn the_tables_are_read_again_only_after_a_write_and_never_for_another_job_store() {
    let rig = Rig::new();
    let both = Offered::both();
    let sinks = (
        &rig.failures as &dyn JobStarts,
        &rig.failures as &dyn PassMetrics,
    );
    let read = |tables: &Tables| {
        let placed = rig
            .placer
            .place_jobs_with(&tables.jobs, &both.hosts(), sinks);
        (placed.unwrap(), tables.store.take_reads())
    };
    let (_, first) = read(&rig.tables);
    assert!(
        first.contains(&Read::Scan("idx/job/queued/".to_owned())),
        "{first:?}"
    );
    assert_eq!(read(&rig.tables), (Vec::new(), Vec::new()));
    rig.placer.heard(&host("a"), Some(&reading(1)));
    assert_eq!(rig.beat(&both), Ok(Some(Vec::new())));
    assert_eq!(rig.tables.store.take_reads(), []);
    let other = Tables::new();
    other.submit_with("elsewhere", js_jobs::Needs::WholeHost, 600);
    let cores = js_types::Resources::new(1000, 1 << 30, 0, None).unwrap();
    other.submit_with("other", js_jobs::Needs::Shape(cores), 600);
    let (placed, reads) = read(&other);
    assert_eq!(placed, [JobId::parse("other").unwrap()]);
    assert!(
        reads.contains(&Read::Scan("idx/job/queued/".to_owned())),
        "{reads:?}"
    );
}

/// Asserts which calls of the registry count as changes of the engine: a hello, a reading and
/// a removed host do; a heartbeat with no reading does not.
#[test]
fn only_a_call_that_changes_the_engine_counts() {
    let rig = Rig::new();
    let counted = |label: &str, by: u64, call: &dyn Fn()| {
        let before = rig.placer.engine_changes();
        call();
        let after = rig.placer.engine_changes();
        assert_eq!(after - before, by, "{label}");
    };
    counted("a heartbeat with no reading", 0, &|| {
        rig.placer.heard(&host("a"), None);
    });
    counted("a reading", 1, &|| {
        rig.placer.heard(&host("a"), Some(&reading(1)));
    });
    counted("a hello", 1, &|| {
        rig.placer.host_up(&host("c"), &candidate("c", &[]).shape);
    });
    counted("a removed host", 1, &|| rig.placer.host_gone(&host("c")));
}

//! A job a submit of this server just recorded, added to the job tables the last pass read
//! ([`SchedPlacer::note_inserted`]) when its commit is the only write of the job store since:
//! the next pass then reads none of the four tables. The reads are counted on the in-memory
//! fake ([`Interfere::take_reads`]), and where each pass took the tables from is what the pass
//! tells its metrics ([`TablesFrom`]).
//!
//! [`SchedPlacer::note_inserted`]: crate::place::SchedPlacer::note_inserted
//! [`Interfere::take_reads`]: js_jobs::interfere::Interfere::take_reads

use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_jobs::interfere::{Interfere, Read};
use js_jobs::{Inserted, JobId, JobStore, Needs, NewJob, RandomIds};
use js_state::fake::Fake;
use js_types::HostId;

use super::super::super::jobs::{JobStarts, PassMetrics};
use super::super::super::rig::spec_of;
use super::super::{Kept, Tables, TablesFrom};
use super::{Offered, Rig};

/// The prefix of the one scan of each job table a pass takes: the queued jobs, the live
/// allocations, the marks and the cordons.
const TABLE_SCANS: [&str; 4] = ["idx/job/queued/", "idx/alloc/host/", "mark/", "cordon/"];

/// Where each pass took the job tables from, in order; hands no start on.
#[derive(Default)]
struct Told(Mutex<Vec<TablesFrom>>);

impl Told {
    /// What the passes told since the last call, taken.
    fn take(&self) -> Vec<TablesFrom> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

impl PassMetrics for Told {
    fn job_placed(&self, _: Duration) {}

    fn store_write_failed(&self) {}

    fn job_tables(&self, from: TablesFrom) {
        self.0.lock().unwrap().push(from);
    }
}

impl JobStarts for Told {
    fn placed(&self, _: &JobId, _: &HostId) {}

    fn tainted(&self, _: &HostId, _: &JobId) {}
}

/// The scans of the job tables among `reads`.
fn table_scans(reads: &[Read]) -> Vec<&Read> {
    let of_a_table = |read: &&Read| match read {
        Read::Scan(prefix) => TABLE_SCANS.contains(&prefix.as_str()),
        Read::Get(_) => false,
    };
    reads.iter().filter(of_a_table).collect()
}

fn id(text: &str) -> JobId {
    JobId::parse(text).unwrap()
}

impl Rig {
    /// A pass of the tick (which always runs) over `offered`, telling `told`: the jobs it
    /// placed.
    fn tick(&self, offered: &Offered, told: &Told) -> Vec<JobId> {
        let sinks = (told as &dyn JobStarts, told as &dyn PassMetrics);
        let jobs = &self.tables.jobs;
        let placed = self.placer.place_jobs_with(jobs, &offered.hosts(), sinks);
        placed.unwrap()
    }

    /// Records the queued job `name` at `priority`, asking for `millis` thousandths of a core,
    /// on `jobs`: what the store answered.
    fn submit_on(
        jobs: &JobStore<Interfere<Fake>>,
        name: &str,
        millis: u64,
        priority: i32,
    ) -> Inserted {
        let cores = js_types::Resources::new(millis, 1 << 30, 0, None).unwrap();
        let new = NewJob {
            id: Some(id(name)),
            spec: spec_of(Needs::Shape(cores), 600),
            priority,
            client: None,
        };
        jobs.insert_queued(new).unwrap()
    }

    /// [`Rig::submit_on`] this server's job store.
    fn submit(&self, name: &str, millis: u64, priority: i32) -> Inserted {
        Rig::submit_on(&self.tables.jobs, name, millis, priority)
    }

    /// The job tables of a read now, as the placer keeps them (no pass runs).
    fn kept(&self, told: &Told) -> Arc<Tables> {
        self.placer.tables(&self.tables.jobs, told).unwrap()
    }

    /// The reads of the job tables since the last call.
    fn table_reads(&self) -> Vec<Read> {
        let reads = self.tables.store.take_reads();
        table_scans(&reads).into_iter().cloned().collect()
    }
}

/// Asserts the change itself: a pass has read the tables with an empty queue and written
/// nothing; a submit records a job, which is added to the tables kept; the next pass places
/// it and reads none of the four tables, telling its metrics it took them as kept with a
/// submit's job added. A pass after the placement, the pass's own write, reads them again.
/// It catches a submit whose job is not added (the pass reads all four tables again) and a
/// pass that misses the job added (it places nothing).
#[test]
fn a_submit_into_an_empty_queue_is_placed_with_no_read_of_the_tables() {
    let rig = Rig::new();
    let both = Offered::both();
    let told = Told::default();
    assert_eq!(rig.tick(&both, &told), Vec::<JobId>::new());
    assert_eq!(told.take(), [TablesFrom::Store]);
    let fresh = rig.submit("fresh", 1000, 0);
    let _ = rig.table_reads();
    assert!(rig.placer.note_inserted(&fresh), "the job was not added");
    assert_eq!(rig.tick(&both, &told), [id("fresh")]);
    assert_eq!(rig.table_reads(), [], "the pass read the tables again");
    assert_eq!(told.take(), [TablesFrom::Submitted]);
    assert_eq!(rig.tick(&both, &told), Vec::<JobId>::new());
    assert_eq!(
        told.take(),
        [TablesFrom::Store],
        "after the placement's write"
    );
    assert_eq!(rig.tick(&both, &told), Vec::<JobId>::new());
    assert_eq!(told.take(), [TablesFrom::Kept], "with no write since");
}

/// Asserts that the tables with a submit's job added are exactly those a read would give: the
/// job in queue order (after a job of a higher priority, after one of its priority submitted
/// at the same time with a lower id, before one of a lower priority), and the record as the
/// store decodes it. It catches a job appended at the end of the queue, and a record that
/// differs from the stored row.
#[test]
fn the_tables_with_a_job_added_are_those_a_read_gives() {
    let rig = Rig::new();
    let told = Told::default();
    let never = 64_000;
    rig.submit("first", never, 1);
    rig.submit("alpha", never, 0);
    rig.submit("last", never, -1);
    rig.kept(&told);
    let middle = rig.submit("middle", never, 0);
    assert!(rig.placer.note_inserted(&middle));
    let added = rig.kept(&told);
    let read = rig.tables.jobs.queued().unwrap();
    assert_eq!(added.queued, read);
    let order: Vec<String> = added.queued.iter().map(|job| job.id.to_string()).collect();
    assert_eq!(order, ["first", "alpha", "middle", "last"]);
    assert_eq!(told.take(), [TablesFrom::Store, TablesFrom::Submitted]);
}

/// Asserts that a write of the job store between the read of the tables and a submit's commit
/// keeps the job from being added, whether it is a write of this server (another job queued)
/// or of another server over the same store: the next pass reads the four tables again and
/// places both jobs. It catches tables kept past a write that is not the submit's, which would
/// leave the other job out of the pass (and, for a placement, its booking).
#[test]
fn a_write_between_the_read_and_the_submit_brings_the_read_back() {
    for elsewhere in [false, true] {
        let rig = Rig::new();
        let both = Offered::both();
        let told = Told::default();
        assert_eq!(rig.tick(&both, &told), Vec::<JobId>::new());
        let other = JobStore::new(Arc::clone(&rig.tables.store), RandomIds).unwrap();
        let writer = if elsewhere { &other } else { &rig.tables.jobs };
        Rig::submit_on(writer, "between", 1000, 0);
        let fresh = rig.submit("fresh", 1000, 0);
        assert!(!rig.placer.note_inserted(&fresh), "elsewhere: {elsewhere}");
        let _ = rig.table_reads();
        assert_eq!(rig.tick(&both, &told), [id("between"), id("fresh")]);
        assert_eq!(rig.table_reads().len(), TABLE_SCANS.len(), "{elsewhere}");
        assert_eq!(told.take(), [TablesFrom::Store, TablesFrom::Store]);
    }
}

/// Asserts that a write after a submit's job was added brings the read back too: the kept
/// tables, at the reading just after the submit's commit, no longer match, so the pass reads
/// the four tables and places both jobs. It catches kept tables that stop judging later writes
/// once a job was added to them.
#[test]
fn a_write_after_a_job_was_added_brings_the_read_back() {
    let rig = Rig::new();
    let both = Offered::both();
    let told = Told::default();
    assert_eq!(rig.tick(&both, &told), Vec::<JobId>::new());
    let fresh = rig.submit("fresh", 1000, 0);
    assert!(rig.placer.note_inserted(&fresh));
    rig.submit("later", 1000, 0);
    let _ = rig.table_reads();
    assert_eq!(rig.tick(&both, &told), [id("fresh"), id("later")]);
    assert_eq!(rig.table_reads().len(), TABLE_SCANS.len());
    assert_eq!(told.take(), [TablesFrom::Store, TablesFrom::Store]);
}

/// Asserts that two submits in a row are both added, the second to the tables the first left,
/// and the pass places both reading no table. It catches a second job judged against the
/// reading before the first, which is never added.
#[test]
fn two_submits_in_a_row_are_both_added() {
    let rig = Rig::new();
    let both = Offered::both();
    let told = Told::default();
    assert_eq!(rig.tick(&both, &told), Vec::<JobId>::new());
    for name in ["one", "two"] {
        let inserted = rig.submit(name, 1000, 0);
        assert!(rig.placer.note_inserted(&inserted), "{name}");
    }
    let _ = rig.table_reads();
    assert_eq!(rig.tick(&both, &told), [id("one"), id("two")]);
    assert_eq!(rig.table_reads(), []);
    assert_eq!(told.take(), [TablesFrom::Store, TablesFrom::Submitted]);
}

/// Asserts the boundaries where nothing is added: a submit before any pass read the tables, and
/// a retry that recorded nothing (the job was already queued). Neither changes the kept
/// tables: the pass after the retry takes them as kept, with no job added twice.
#[test]
fn a_submit_before_any_read_or_a_retry_adds_nothing() {
    let rig = Rig::new();
    let told = Told::default();
    let never = 64_000;
    let first = rig.submit("queued", never, 0);
    assert!(!rig.placer.note_inserted(&first), "no tables were read yet");
    rig.kept(&told);
    let again = rig.submit("queued", never, 0);
    assert!(!again.created);
    assert!(
        !rig.placer.note_inserted(&again),
        "a retry recorded nothing"
    );
    let kept = rig.kept(&told);
    assert_eq!(kept.queued, rig.tables.jobs.queued().unwrap());
    assert_eq!(told.take(), [TablesFrom::Store, TablesFrom::Kept]);
}

/// Asserts that a job the tables already hold is held once: the pass read where the writes
/// stood before the submit's commit and the tables after it (the reads are not one snapshot),
/// so the job is in them when the submit adds it. It catches a job added twice, which a pass
/// would try to place twice.
#[test]
fn a_job_the_read_already_holds_is_held_once() {
    let rig = Rig::new();
    let told = Told::default();
    let at = rig.tables.jobs.written().unwrap();
    let fresh = rig.submit("fresh", 64_000, 0);
    let tables = Tables {
        queued: rig.tables.jobs.queued().unwrap(),
        ..Tables::default()
    };
    let kept = Kept {
        at,
        tables: Arc::new(tables),
        added: false,
    };
    *rig.placer.tables.lock().unwrap() = Some(kept);
    assert!(rig.placer.note_inserted(&fresh));
    let added = rig.kept(&told);
    assert_eq!(added.queued, rig.tables.jobs.queued().unwrap());
    assert_eq!(added.queued.len(), 1);
}

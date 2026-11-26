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
use js_jobs::{Booked, Inserted, JobId, JobState, JobStore, MarkRecord, Needs, NewJob, RandomIds};
use js_state::fake::Fake;
use js_state::{Cond, Store, Txn};
use js_types::HostId;
use js_types::cordon::Asker;

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
        self.placer.tables(&self.tables.jobs, told).unwrap().0
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
/// submit's job added. A pass after the placement, the pass's own write, brings them up to
/// date from the store's feed.
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
        [TablesFrom::Changed],
        "after the placement's write"
    );
    assert_eq!(rig.table_reads(), [], "the feed told the placement");
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
/// or of another server over the same store: the next pass brings the tables up to date from
/// the store's feed, reading none of the four, and places both jobs. It catches tables kept
/// past a write that is not the submit's, which would leave the other job out of the pass
/// (and, for a placement, its booking).
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
        assert_eq!(rig.table_reads(), [], "{elsewhere}");
        assert_eq!(told.take(), [TablesFrom::Store, TablesFrom::Changed]);
    }
}

/// Asserts that a write after a submit's job was added brings the tables up to date too: the
/// kept tables, at the reading just after the submit's commit, no longer match, so the pass
/// follows the store's feed (which tells the added job again, and the later one) and places
/// both jobs, each once. It catches kept tables that stop judging later writes once a job was
/// added to them.
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
    assert_eq!(rig.table_reads(), []);
    assert_eq!(told.take(), [TablesFrom::Store, TablesFrom::Changed]);
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
/// would try to place twice. Tables kept with no head of the store to follow are read in full
/// after the next write.
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
        head: None,
        tables: Arc::new(tables),
        added: false,
    };
    *rig.placer.tables.lock().unwrap() = Some(kept);
    assert!(rig.placer.note_inserted(&fresh));
    let added = rig.kept(&told);
    assert_eq!(added.queued, rig.tables.jobs.queued().unwrap());
    assert_eq!(added.queued.len(), 1);
    // Those tables follow no head of the store: after the next write they are read in full.
    rig.submit("unfollowed", 64_000, 0);
    let read = rig.kept(&told);
    assert_eq!(read.queued, rig.tables.jobs.queued().unwrap());
    assert_eq!(told.take(), [TablesFrom::Submitted, TablesFrom::Store]);
}

/// A share of one core of a host, as a placement books it.
fn one_core() -> Booked {
    Booked::Shape {
        cpu_millis: 1000,
        memory_bytes: 1 << 30,
        disk_bytes: 0,
        accelerators: 0,
    }
}

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

impl Rig {
    /// The job tables taken now, with the count of the changes of what they hold.
    fn taken(&self, told: &Told) -> (Arc<Tables>, u64) {
        self.placer.tables(&self.tables.jobs, told).unwrap()
    }

    /// Asserts that `tables` hold what a read of the four tables gives now: the queued jobs,
    /// the live allocations and the marks as read, and a cordon for each host the store holds
    /// one for.
    #[track_caller]
    fn holds_what_a_read_gives(&self, tables: &Tables) {
        let jobs = &self.tables.jobs;
        assert_eq!(tables.queued, jobs.queued().unwrap(), "the queued jobs");
        assert_eq!(
            tables.live,
            jobs.live_allocations().unwrap(),
            "the live allocations"
        );
        assert_eq!(tables.marks, jobs.marks().unwrap(), "the marks");
        let cordoned: Vec<HostId> = jobs.cordons().unwrap().into_keys().collect();
        let held: Vec<HostId> = tables.cordons.keys().cloned().collect();
        assert_eq!(held, cordoned, "the cordons");
    }
}

/// Asserts the change itself: tables read once and then brought up to date from the store's
/// feed, through submits at three priorities, a placement and the free of another, a cancel, a
/// whole-host job's mark and a cordon, hold what a read of the four tables gives, and only the
/// marks and the cordons were read again, once each, after a mark or a cordon changed. It
/// catches tables followed from the feed that miss a job that entered or left the queue, keep a
/// freed allocation's room, or keep a mark or a cordon past its change.
#[test]
fn the_tables_followed_from_the_feed_are_those_a_read_gives() {
    let rig = Rig::new();
    let told = Told::default();
    let never = 64_000;
    rig.submit("placed", never, 0);
    rig.submit("freed", never, 0);
    rig.tables
        .jobs
        .place(&id("freed"), host("a"), one_core())
        .unwrap();
    let _ = rig.taken(&told);
    assert_eq!(told.take(), [TablesFrom::Store]);
    let _ = rig.table_reads();

    rig.submit("low", never, -1);
    rig.submit("high", never, 2);
    rig.submit("cancelled", never, 0);
    rig.tables.jobs.request_cancel(&id("cancelled")).unwrap();
    rig.tables
        .jobs
        .place(&id("placed"), host("b"), one_core())
        .unwrap();
    rig.tables
        .jobs
        .free(&id("freed"), js_jobs::EndReason::Exited)
        .unwrap();
    let (followed, _) = rig.taken(&told);
    assert_eq!(told.take(), [TablesFrom::Changed]);
    assert_eq!(rig.table_reads(), [], "no table was read again");
    rig.holds_what_a_read_gives(&followed);
    let order: Vec<String> = followed
        .queued
        .iter()
        .map(|job| job.id.to_string())
        .collect();
    assert_eq!(order, ["high", "low"]);

    let whole = rig.tables.submit_with("whole", Needs::WholeHost, 600);
    let at = rig.tables.jobs.now().unwrap();
    let mark = MarkRecord::new(whole.id, host("a"), at, at);
    rig.tables.jobs.put_mark(mark).unwrap();
    let lease = Duration::from_secs(600);
    rig.tables
        .jobs
        .cordon_host(&host("b"), lease, &Asker::Unnamed)
        .unwrap();
    let _ = rig.table_reads();
    let (followed, _) = rig.taken(&told);
    assert_eq!(told.take(), [TablesFrom::Changed]);
    let reads = rig.table_reads();
    let scans = [
        Read::Scan("mark/".to_owned()),
        Read::Scan("cordon/".to_owned()),
    ];
    assert_eq!(reads, scans, "the marks and the cordons, read again");
    rig.holds_what_a_read_gives(&followed);
}

/// Asserts what counts as a change of the tables, which brings back a pass after a heartbeat:
/// a commit that changes no row the tables hold (a host's node row, a duty's row, a placed
/// job's move to starting) is followed from the feed and counts none, and a queued job's write
/// counts one; the tables kept with no commit since count none either. It catches a pass after
/// every heartbeat of a busy farm, whose running jobs' rows change all the time, and a pass
/// skipped after a queued job changed.
#[test]
fn a_commit_that_changes_no_row_the_tables_hold_counts_no_change() {
    let rig = Rig::new();
    let told = Told::default();
    rig.submit("running", 64_000, 0);
    let jobs = &rig.tables.jobs;
    jobs.place(&id("running"), host("a"), one_core()).unwrap();
    let (_, read) = rig.taken(&told);
    let node = Txn::new().put("node/a", b"row".to_vec(), Cond::Any);
    rig.tables.store.txn(node).unwrap().unwrap();
    jobs.claim_duty("job-pass", Duration::from_secs(5)).unwrap();
    jobs.transition(&id("running"), JobState::Placed, JobState::Starting)
        .unwrap();
    let (_, quiet) = rig.taken(&told);
    assert_eq!(quiet, read, "no row the tables hold changed");
    let (_, kept) = rig.taken(&told);
    assert_eq!(kept, read, "nothing was written");
    assert_eq!(
        told.take(),
        [TablesFrom::Store, TablesFrom::Changed, TablesFrom::Kept]
    );
    rig.submit("queued", 64_000, 0);
    let (_, changed) = rig.taken(&told);
    assert_eq!(changed, read + 1, "a queued job's write");
}

/// Asserts that where the store's feed cannot tell what changed (it was trimmed past the head
/// the tables follow), the four tables are read in full, and hold the job submitted since. It
/// catches tables followed over a gap in the feed, which would miss what the gap held.
#[test]
fn where_the_feed_cannot_tell_the_tables_are_read_in_full() {
    let rig = Rig::new();
    let both = Offered::both();
    let told = Told::default();
    assert_eq!(rig.tick(&both, &told), Vec::<JobId>::new());
    rig.submit("after-the-gap", 1000, 0);
    let head = rig.tables.jobs.feed_head().unwrap().unwrap();
    rig.tables.store.trim_feed(head).unwrap();
    let _ = rig.table_reads();
    assert_eq!(rig.tick(&both, &told), [id("after-the-gap")]);
    assert_eq!(rig.table_reads().len(), TABLE_SCANS.len());
    assert_eq!(told.take(), [TablesFrom::Store, TablesFrom::Store]);
}

/// Asserts that a pass whose follow of the store's feed fails a read (of the cordons, after a
/// cordon changed) keeps nothing: the error is answered, and the next take reads the four
/// tables in full. It catches tables kept half brought up to date.
#[test]
fn a_failed_read_while_following_the_feed_keeps_nothing() {
    let rig = Rig::new();
    let told = Told::default();
    let _ = rig.taken(&told);
    let lease = Duration::from_secs(600);
    rig.tables
        .jobs
        .cordon_host(&host("c"), lease, &Asker::Unnamed)
        .unwrap();
    rig.tables.store.fail_read_after(0);
    assert!(rig.placer.tables(&rig.tables.jobs, &told).is_err());
    assert!(rig.placer.tables.lock().unwrap().is_none(), "kept nothing");
    let _ = rig.table_reads();
    let _ = rig.taken(&told);
    assert_eq!(rig.table_reads().len(), TABLE_SCANS.len());
    assert_eq!(told.take(), [TablesFrom::Store, TablesFrom::Store]);
}

/// Asserts that tables kept while only the hosts' node rows are written follow the store's head
/// as it moves: after more node rows than one bringing up to date may read, taken a few hundred
/// at a time with the tables taken between (as heartbeats come between passes), a submit is
/// still taken from the feed, not by reading the tables in full. It catches kept tables whose
/// head stays where they were read, which after a quiet spell of heartbeats read the whole
/// queue again for the next change.
#[test]
fn kept_tables_follow_the_head_past_the_hosts_node_rows() {
    let rig = Rig::new();
    let told = Told::default();
    let _ = rig.taken(&told);
    let batches = js_jobs::MOST_TABLE_CHANGES / 400 + 1;
    for batch in 0..batches {
        let txn = (0..400).fold(Txn::new(), |txn, n| {
            txn.put(format!("node/host-{batch}-{n}"), b"row".to_vec(), Cond::Any)
        });
        rig.tables.store.txn(txn).unwrap().unwrap();
        let _ = rig.taken(&told);
    }
    rig.submit("after-the-beats", 1000, 0);
    let _ = rig.table_reads();
    let (followed, _) = rig.taken(&told);
    assert_eq!(rig.table_reads(), [], "the tables were read in full");
    assert_eq!(followed.queued.len(), 1);
    let mut from = vec![TablesFrom::Store];
    from.extend((0..batches).map(|_| TablesFrom::Kept));
    from.push(TablesFrom::Changed);
    assert_eq!(told.take(), from);
}

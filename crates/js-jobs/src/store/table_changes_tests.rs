//! The changes of the tables a pass over the queued jobs reads, read from the store's feed
//! ([`JobStore::table_changes`]): applied to the tables as they were read at a revision, they
//! give the tables a read gives now; and the boundaries where the feed cannot tell, which
//! answer `None`, and the rows that cannot be read, which fail as a read of the tables fails.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use js_state::fake::{Fake, Flaw};
use js_state::sqlite::{Options, SqliteStore};
use js_state::{Clock, Cond, ManualClock, Revision, Store, Txn};
use js_testkit::dir::TestDir;
use js_types::HostId;
use js_types::cordon::Asker;

use super::{MOST_TABLE_CHANGES, TableChanges};
use crate::codec::{FORMAT, LATER_FORMAT, encode_job};
use crate::ids::{JobId, RandomIds};
use crate::keys;
use crate::record::{AllocationRecord, EndReason, JobRecord, MarkRecord};
use crate::store::{JobStore, NewJob};

type Jobs = JobStore<Fake, RandomIds>;

/// A job store over the in-memory fake, which tells its head, with the fake and its clock.
fn opened() -> (Arc<Fake>, Jobs, Arc<ManualClock>) {
    let clock = Arc::new(ManualClock::default());
    let store = Arc::new(Fake::with_flaw(Arc::clone(&clock), Flaw::None));
    let jobs = JobStore::new(Arc::clone(&store), RandomIds).expect("opens");
    (store, jobs, clock)
}

fn id(text: &str) -> JobId {
    JobId::parse(text).expect("an id")
}

fn host(name: &str) -> HostId {
    HostId::new(name).expect("a host id")
}

/// Records the queued job `name`.
fn queue(jobs: &Jobs, name: &str) -> JobId {
    queue_spec(jobs, name, crate::suite::full_spec())
}

/// Records the queued job `name` with `spec`.
fn queue_spec(jobs: &Jobs, name: &str, spec: crate::JobSpec) -> JobId {
    let new = NewJob {
        id: Some(id(name)),
        spec,
        priority: 0,
        client: None,
    };
    jobs.insert_queued(new).expect("records").record.id
}

/// The head of `jobs`'s store, which the fake always tells.
fn head(jobs: &Jobs) -> Revision {
    jobs.feed_head()
        .expect("reads")
        .expect("the fake tells its head")
}

/// The changes after `after`, which the feed can tell.
fn told(jobs: &Jobs, after: Revision) -> TableChanges {
    let changes = jobs.table_changes(after).expect("reads");
    changes.expect("the feed tells the changes")
}

/// Commits `txn` on `store`.
fn commit(store: &Fake, txn: Txn) {
    store.txn(txn).expect("the store").expect("no conflict");
}

/// The queued jobs and the live allocations as read at one time.
#[derive(Debug, PartialEq)]
struct Read {
    queued: Vec<JobRecord>,
    live: Vec<AllocationRecord>,
}

impl Read {
    fn now(jobs: &Jobs) -> Read {
        Read {
            queued: jobs.queued().expect("reads"),
            live: jobs.live_allocations().expect("reads"),
        }
    }

    /// This read with `changes` applied, as a reader of the tables applies them: each job and
    /// allocation told replaces its earlier record, or takes it out.
    fn with(self, changes: &TableChanges) -> Read {
        let mut queued: BTreeMap<JobId, JobRecord> = self
            .queued
            .into_iter()
            .map(|job| (job.id.clone(), job))
            .collect();
        for (job, record) in &changes.queued {
            queued.remove(job);
            queued.extend(record.clone().map(|record| (job.clone(), record)));
        }
        let mut live: BTreeMap<JobId, AllocationRecord> = self
            .live
            .into_iter()
            .map(|allocation| (allocation.job.clone(), allocation))
            .collect();
        for (job, allocation) in &changes.live {
            live.remove(job);
            live.extend(
                allocation
                    .clone()
                    .map(|allocation| (job.clone(), allocation)),
            );
        }
        let mut queued: Vec<JobRecord> = queued.into_values().collect();
        queued.sort_by(crate::queue_order);
        Read {
            queued,
            live: live.into_values().collect(),
        }
    }
}

/// Asserts the change itself: tables read just after a head was taken, brought up to date with
/// the changes after it, are the tables a read gives now, through submits, a placement, a
/// cancel, a free and a deleted job row; and only the jobs and allocations whose rows changed
/// are told, the queued job no write touched among none of them. It catches changes that miss
/// a job that left the queue (a pass would place it again), one that entered it, or a freed
/// allocation (a pass would hold its room).
#[test]
fn the_changes_after_a_read_bring_it_to_the_tables_as_they_stand() {
    let (store, jobs, _) = opened();
    let still = queue(&jobs, "still");
    let placed = queue(&jobs, "placed");
    let freed = queue(&jobs, "freed");
    jobs.place(&freed, host("host-a"), crate::suite::share())
        .expect("placed");
    let after = head(&jobs);
    let read = Read::now(&jobs);

    let fresh = queue(&jobs, "fresh");
    let cancelled = queue(&jobs, "cancelled");
    jobs.request_cancel(&cancelled).expect("cancels");
    jobs.place(&placed, host("host-b"), crate::suite::share())
        .expect("placed");
    jobs.free(&freed, EndReason::Exited).expect("frees");
    let gone = queue(&jobs, "gone");
    commit(&store, Txn::new().delete(keys::job(&gone), Cond::Any));

    let changes = told(&jobs, after);
    assert_eq!(read.with(&changes), Read::now(&jobs));
    assert_eq!(changes.head, head(&jobs), "the feed was read to the head");
    for job in [&cancelled, &fresh, &gone, &placed] {
        assert!(changes.queued.contains_key(job), "{job} is not told");
    }
    assert!(!changes.queued.contains_key(&still), "no write touched it");
    let queued_now: Vec<&JobId> = changes
        .queued
        .iter()
        .filter_map(|(job, record)| record.as_ref().map(|_| job))
        .collect();
    assert_eq!(queued_now, [&fresh], "only the fresh job is queued");
    assert_eq!(changes.live.get(&freed), Some(&None), "the free is told");
    assert!(
        changes.live.get(&placed).is_some_and(Option::is_some),
        "the placement is told live"
    );
    assert_eq!((changes.marks, changes.cordons), (false, false));
}

/// Asserts that a mark written or a cordon placed is told, each by itself, and that one that
/// lapses with the store's clock is told too; and that rows of no table a pass reads (a node
/// row, a duty's row) are passed over: the answer tells nothing changed, read to the head. It
/// catches a pass that keeps a mark or a cordon past its change, and one that reads the tables
/// again for every heartbeat's node row.
#[test]
fn marks_and_cordons_are_told_and_other_rows_are_passed_over() {
    let (store, jobs, clock) = opened();
    let after = head(&jobs);
    commit(
        &store,
        Txn::new().put(keys::node(&host("host-a")), b"row".to_vec(), Cond::Any),
    );
    jobs.claim_duty("job-pass", Duration::from_secs(5))
        .expect("claims");
    let quiet = told(&jobs, after);
    assert_eq!(
        quiet,
        TableChanges {
            head: head(&jobs),
            queued: BTreeMap::new(),
            live: BTreeMap::new(),
            marks: false,
            cordons: false,
        }
    );
    assert_eq!(
        told(&jobs, quiet.head),
        quiet,
        "nothing after the head itself"
    );

    let whole = queue_spec(&jobs, "whole", crate::suite::whole_host_spec());
    let after = head(&jobs);
    let at = jobs.now().expect("the clock");
    let mark = MarkRecord::new(whole, host("host-a"), at, at);
    jobs.put_mark(mark).expect("marks");
    let marked = told(&jobs, after);
    assert_eq!((marked.marks, marked.cordons), (true, false));

    let after = head(&jobs);
    let lease = Duration::from_secs(60);
    jobs.cordon_host(&host("host-b"), lease, &Asker::Unnamed)
        .expect("cordons");
    let cordoned = told(&jobs, after);
    assert_eq!((cordoned.marks, cordoned.cordons), (false, true));

    let after = head(&jobs);
    clock.advance(lease + Duration::from_secs(1));
    store.reap_expired().expect("reaps");
    let lapsed = told(&jobs, after);
    assert_eq!((lapsed.marks, lapsed.cordons), (false, true), "{lapsed:?}");
}

/// Asserts the boundaries where the feed cannot tell, each answering `None`, so the tables are
/// read in full: a store that tells no head; a feed trimmed past the revision asked after; more
/// than [`MOST_TABLE_CHANGES`] rows since; an entry of the rows this build cannot read; and a
/// job or allocation key that names no job. It catches changes that pass over what they cannot
/// judge, leaving tables that differ from the store's.
#[test]
fn the_changes_are_not_told_where_the_feed_cannot_tell_them() {
    let dir = TestDir::under(std::env::temp_dir(), "js-jobs-table-changes");
    let clock: Arc<dyn Clock> = Arc::new(ManualClock::default());
    let sqlite = SqliteStore::open(&dir.join("state.db"), clock, Options::default());
    let headless = JobStore::new(Arc::new(sqlite.expect("opens")), RandomIds).expect("opens");
    assert_eq!(headless.feed_head(), Ok(None));
    assert_eq!(headless.table_changes(Revision(0)), Ok(None), "no head");

    let (store, jobs, _) = opened();
    queue(&jobs, "early");
    let early = head(&jobs);
    queue(&jobs, "late");
    store.trim_feed(head(&jobs)).expect("trims");
    assert_eq!(jobs.table_changes(early), Ok(None), "trimmed past it");

    let after = head(&jobs);
    let many = MOST_TABLE_CHANGES + 1;
    let txn = (0..many).fold(Txn::new(), |txn, n| {
        txn.put(format!("test/{n}"), b"1".to_vec(), Cond::Any)
    });
    commit(&store, txn);
    assert_eq!(jobs.table_changes(after), Ok(None), "too many rows");

    for key in [
        format!("{}x", keys::UNREAD_JOB),
        format!("{}x", keys::UNREAD_ALLOCATION),
        format!("{}not/an-id", keys::JOB),
        format!("{}not/an-id", keys::ALLOCATION),
    ] {
        let after = head(&jobs);
        commit(
            &store,
            Txn::new().put(key.as_str(), b"1".to_vec(), Cond::Any),
        );
        assert_eq!(jobs.table_changes(after), Ok(None), "{key}");
    }
}

/// Asserts that a feed that stops short of the store's head (a commit below it still to land)
/// is read as far as it goes, and that the next call, after the head the first answered, reads
/// on and tells what came after. It catches a call that answers the store's head while a row
/// below it is still to come, which would never be told.
#[test]
fn a_feed_short_of_the_head_is_read_on_by_the_next_call() {
    let (store, jobs, _) = opened();
    let after = head(&jobs);
    let pending = store.prepare(Txn::new().put("test/pending", b"1".to_vec(), Cond::Any));
    let later = queue(&jobs, "later");
    let short = told(&jobs, after);
    assert_eq!(
        short.head, after,
        "nothing above the pending commit is read"
    );
    assert!(short.queued.is_empty(), "{short:?}");
    store.finish(pending).expect("commits");
    let on = told(&jobs, short.head);
    assert!(on.queued.get(&later).is_some_and(Option::is_some), "{on:?}");
    assert_eq!(on.head, head(&jobs));
}

/// Asserts that the rows the feed gives are judged as a read of the tables judges them, or not
/// at all: a job row a later build wrote is told as not queued (a read leaves it out), and a
/// job row that does not parse or that holds another job, or an allocation row a later build
/// wrote or that does not parse, makes the answer `None`, so the tables are read in full, which
/// fails only if it reads that row; a deleted allocation row is told as not live. It catches
/// changes that place a job a read would leave out, that pass over an allocation that cannot be
/// counted (a pass would take its room as free), and a pass failed by a row no read of the
/// tables would read.
#[test]
fn the_rows_are_judged_as_a_read_of_the_tables_judges_them() {
    let (store, jobs, _) = opened();
    let put = |key: &str, value: Vec<u8>| {
        let after = head(&jobs);
        commit(&store, Txn::new().put(key, value, Cond::Any));
        jobs.table_changes(after)
    };
    let later = id("later");
    let newer = put(&keys::job(&later), vec![LATER_FORMAT]).expect("reads");
    let newer = newer.expect("the feed tells it");
    assert_eq!(newer.queued.get(&later), Some(&None), "a later build's row");

    let other = jobs
        .get_job(&queue(&jobs, "other"))
        .expect("reads")
        .expect("recorded");
    let allocation = keys::allocation(&id("allocated"));
    let unreadable = [
        (keys::job(&id("broken")), vec![FORMAT, 0]),
        (
            keys::job(&id("elsewhere")),
            encode_job(&other).expect("encodes"),
        ),
        (allocation.clone(), vec![LATER_FORMAT]),
        (allocation.clone(), vec![FORMAT, 0]),
    ];
    for (key, value) in unreadable {
        assert_eq!(put(&key, value), Ok(None), "{key}");
    }

    let after = head(&jobs);
    commit(&store, Txn::new().delete(allocation.as_str(), Cond::Any));
    let deleted = told(&jobs, after);
    assert_eq!(deleted.live.get(&id("allocated")), Some(&None));
}

//! The index spaces (`idx/`): every write keeps them equal to what a full scan of the tables
//! calls for, so the indexed reads answer what the scans answered; the backfill a job store's
//! open runs makes them so for rows written past the job store, and changes nothing the second
//! time; a read judges the row an entry points at, not the entry.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use js_state::conformance::Backend;
use js_state::{Cond, Store, Txn};

use super::{Rig, cancelled, exited, full_spec, host, job_id, rig, whole_host_spec};
use crate::codec::{FORMAT, decode_allocation, decode_job, encode_allocation, encode_job};
use crate::store::queue_order;
use crate::{
    AllocationRecord, AllocationStep, Backfill, Booked, EndReason, HostBooking, JobId, JobRecord,
    JobState, JobStore, JobStoreError, NewJob, RandomIds, Retention,
};

/// The hosts the random walk places on.
const HOSTS: [&str; 3] = ["host-a", "host-b", "host-c"];

/// A small seeded generator (splitmix64), so a walk is the same on every run and backend.
struct Seeded(u64);

impl Seeded {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A number below `n`.
    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % n as u64).unwrap_or(0)
    }
}

/// Every job of the table, decoded, by a full scan.
fn scanned_jobs<S: Store>(rig: &Rig<S>) -> Vec<JobRecord> {
    rig.store
        .scan_prefix(crate::keys::JOB)
        .expect("reads")
        .into_iter()
        .filter_map(|(_, row)| decode_job(&row.value).ok())
        .collect()
}

/// Every allocation of the table, decoded, by a full scan.
fn scanned_allocations<S: Store>(rig: &Rig<S>) -> Vec<AllocationRecord> {
    rig.store
        .scan_prefix(crate::keys::ALLOCATION)
        .expect("reads")
        .into_iter()
        .filter_map(|(_, row)| decode_allocation(&row.value).ok())
        .collect()
}

/// That the index spaces hold exactly the entries a scan of the tables calls for, and that
/// every indexed read answers what the same read over a full scan answers: the queue, the live
/// allocations, and each host's live allocations and bookings.
fn assert_index_is_scan<S: Store>(rig: &Rig<S>, name: &str, step: &str) {
    assert_eq!(
        rig.jobs.index_entries().expect("reads"),
        rig.jobs.index_by_scan().expect("reads"),
        "{name}: after {step} the index spaces differ from the scan"
    );
    let mut queued: Vec<JobRecord> = scanned_jobs(rig)
        .into_iter()
        .filter(|job| job.state == JobState::Queued)
        .collect();
    queued.sort_by(queue_order);
    assert_eq!(
        rig.jobs.queued().expect("reads"),
        queued,
        "{name}: after {step} the queue differs from the scan's"
    );
    let live: Vec<AllocationRecord> = scanned_allocations(rig)
        .into_iter()
        .filter(|allocation| allocation.step != AllocationStep::Freed)
        .collect();
    assert_eq!(
        rig.jobs.live_allocations().expect("reads"),
        live,
        "{name}: after {step} the live allocations differ from the scan's"
    );
    for text in HOSTS {
        let on: Vec<&AllocationRecord> = live.iter().filter(|a| a.host == host(text)).collect();
        let listed = rig.jobs.live_on(&host(text)).expect("reads");
        let listed: Vec<&AllocationRecord> = listed.iter().map(|(a, _)| a).collect();
        assert_eq!(
            listed, on,
            "{name}: after {step} the allocations on {text} differ from the scan's"
        );
        let summed = on
            .iter()
            .try_fold(HostBooking::default(), |total, a| total.plus(&a.booked));
        assert_eq!(
            rig.jobs.booked_on(&host(text)).ok(),
            summed,
            "{name}: after {step} the bookings on {text} differ from the scan's"
        );
    }
}

/// A seeded walk of every write the job store makes (insert, place, start, run, end, cancel,
/// release, free, requeue, end-and-free, the tick's sweep and collection, the boot raise) over
/// a few jobs and hosts. After each step the index spaces equal what a full scan calls for, and
/// the indexed reads answer as the scans did: a write path that forgets an entry, or leaves one
/// behind, fails here.
pub fn every_write_keeps_the_indexes_equal_to_a_scan<B: Backend>(backend: &B) {
    let name = "every_write_keeps_the_indexes_equal_to_a_scan";
    let rig = rig(backend);
    let mut seeded = Seeded(0x6a6f_6273);
    let ids: Vec<JobId> = (0..8).map(|n| job_id(&format!("walk-{n}"))).collect();
    assert_index_is_scan(&rig, name, "the open");
    for step in 0..400 {
        let id = &ids[seeded.below(ids.len())];
        let at = host(HOSTS[seeded.below(HOSTS.len())]);
        let what = seeded.below(13);
        let jobs = &rig.jobs;
        // Each operation may be refused in the job's present state; a refusal writes nothing,
        // which the check after it holds it to as well.
        let label = match what {
            0 | 1 => {
                let spec = if seeded.below(3) == 0 {
                    whole_host_spec()
                } else {
                    full_spec()
                };
                let job = NewJob {
                    id: Some(id.clone()),
                    spec,
                    priority: i32::try_from(seeded.below(3)).unwrap_or(0),
                    client: None,
                };
                let _ = jobs.insert_queued(job);
                "an insert"
            }
            2 | 3 => {
                let whole = jobs
                    .get_job(id)
                    .ok()
                    .flatten()
                    .is_some_and(|job| job.spec.needs == crate::Needs::WholeHost);
                let booked = if whole {
                    Booked::WholeHost
                } else {
                    super::share()
                };
                let _ = jobs.place(id, at, booked);
                "a placement"
            }
            4 => {
                let _ = jobs.transition(id, JobState::Placed, JobState::Starting);
                let _ = jobs.transition(id, JobState::Starting, JobState::Running);
                "a start"
            }
            5 => {
                let _ = jobs.end_job(id, exited(i32::try_from(seeded.below(2)).unwrap_or(0)));
                "an end"
            }
            6 => {
                let _ = jobs.request_cancel(id);
                "a cancel"
            }
            7 => {
                let _ = jobs.mark_releasing(id);
                "a release"
            }
            8 => {
                let _ = jobs.free(id, EndReason::Exited);
                "a free"
            }
            9 => {
                let _ = jobs.requeue_failed_prepare(id);
                "a requeue"
            }
            10 => {
                let _ = jobs.end_and_free(id, cancelled(), EndReason::Cancelled);
                "an end and free"
            }
            11 => {
                rig.clock.advance(Duration::from_secs(
                    u64::try_from(seeded.below(4)).unwrap_or(0) * 3 * 24 * 3600,
                ));
                jobs.tick().expect("the tick runs");
                "a tick"
            }
            _ => {
                jobs.raise_at_boot(rig.store.now()).expect("the raise runs");
                "a boot raise"
            }
        };
        assert_index_is_scan(&rig, name, &format!("step {step}, {label} of {id}"));
    }
    let states: BTreeSet<String> = scanned_jobs(&rig)
        .iter()
        .map(|job| format!("{:?}", job.state))
        .collect();
    assert!(
        states.len() >= 3,
        "{name}: the walk reached too few states to mean anything: {states:?}"
    );
}

/// The backfill indexes rows written past the job store, whatever entries were there: a missing
/// entry is put, an entry no row calls for (its row gone, or moved on) is deleted, a row this
/// build cannot read gets its `unread` entry, and an entry of a space this build does not name is
/// left. A second backfill changes nothing and commits nothing.
pub fn the_backfill_makes_the_indexes_agree_and_a_second_changes_nothing<B: Backend>(backend: &B) {
    let name = "the_backfill_makes_the_indexes_agree_and_a_second_changes_nothing";
    let rig = rig(backend);
    let queued = rig.queue("bf-queued");
    rig.placed("bf-placed");
    let ended = rig.running("bf-ended");
    rig.jobs.end_job(&ended, exited(0)).expect("ends");
    rig.jobs.free(&ended, EndReason::Exited).expect("frees");
    let entries = rig.jobs.index_entries().expect("reads");
    assert_eq!(
        entries.len(),
        5,
        "{name}: the writes did not index: {entries:?}"
    );

    let mut wipe = Txn::new();
    for entry in &entries {
        wipe = wipe.delete(entry.as_str(), Cond::Any);
    }
    let later = rig.raw("job/bf-queued").expect("written").value;
    let mut newer = later.clone();
    newer[0] = FORMAT + 1;
    wipe = wipe
        .put("job/bf-later", newer, Cond::Any)
        .put("idx/job/active/bf-gone", Vec::new(), Cond::Any)
        .put(
            "idx/job/due/00000000000000000001/bf-queued",
            Vec::new(),
            Cond::Any,
        )
        .put("idx/later/space/x", Vec::new(), Cond::Any);
    rig.store
        .inner()
        .txn(wipe)
        .expect("reachable")
        .expect("unconditional writes commit");
    assert_eq!(
        rig.jobs.queued().expect("reads"),
        Vec::new(),
        "{name}: a read found a row with no entry"
    );

    let done = rig.jobs.backfill_indexes().expect("the backfill runs");
    assert_eq!(
        done,
        Backfill {
            added: 6,
            removed: 2
        },
        "{name}: the backfill did not count what it put and deleted"
    );
    assert_eq!(
        rig.jobs.index_entries().expect("reads"),
        rig.jobs.index_by_scan().expect("reads"),
        "{name}: the backfill left the indexes unlike the scan"
    );
    assert!(
        rig.raw("idx/job/unread/bf-later").is_some(),
        "{name}: a row this build cannot read has no unread entry"
    );
    assert!(
        rig.raw("idx/later/space/x").is_some(),
        "{name}: an entry of a later build's space was deleted"
    );
    assert_eq!(
        rig.jobs
            .queued()
            .expect("reads")
            .into_iter()
            .map(|job| job.id)
            .collect::<Vec<_>>(),
        [queued],
        "{name}: the queue is not the scan's after the backfill"
    );

    let before = rig.snapshot();
    assert_eq!(
        rig.jobs.backfill_indexes(),
        Ok(Backfill::default()),
        "{name}: a second backfill changed something"
    );
    assert_eq!(
        rig.snapshot(),
        before,
        "{name}: a second backfill wrote or committed"
    );
}

/// An indexed read judges the row an entry points at, never the entry: with every kind of
/// entry left stale by rows rewritten past the job store (an active job that ended, a due job
/// whose end moved later, a live allocation that moved host or was freed, a freed one booked again (which no
/// on-host entry names until a backfill) or turned unreadable, and rows deleted outright), the
/// queue, a host's allocations and bookings, the
/// live allocations, the tick's sweep and collection and both boot raises answer from the rows.
pub fn an_indexed_read_judges_the_row_not_the_entry<B: Backend>(backend: &B) {
    let name = "an_indexed_read_judges_the_row_not_the_entry";
    let rig = rig(backend);
    let jobs = JobStore::new(Arc::clone(&rig.store), RandomIds)
        .expect("opens")
        .with_allocation_retention(Retention::new(Duration::from_secs(3600)).expect("a window"));
    let active = rig.queue("st-active");
    let ended = rig.running("st-ended");
    rig.jobs.end_job(&ended, exited(0)).expect("ends");
    let moved = rig.placed("st-moved");
    let dropped = rig.placed("st-dropped");
    let rebooked = rig.placed("st-rebooked");
    rig.jobs.free(&rebooked, EndReason::Exited).expect("frees");
    let unread = rig.placed("st-unread");
    rig.jobs.free(&unread, EndReason::Exited).expect("frees");
    let gone = rig.placed("st-gone");
    let entries = rig.jobs.index_entries().expect("reads");

    let job = |id: &JobId| rig.jobs.get_job(id).expect("reads").expect("kept");
    let allocation = |id: &JobId| rig.jobs.get_allocation(id).expect("reads").expect("kept");
    let far = rig
        .store
        .now()
        .saturating_add(Duration::from_secs(10 * 7 * 24 * 3600));
    let active_now = JobRecord {
        state: JobState::Cancelled,
        retention_ends: Some(far),
        ..job(&active)
    };
    let ended_now = JobRecord {
        retention_ends: Some(far),
        ..job(&ended)
    };
    let moved_now = AllocationRecord {
        host: host("host-b"),
        ..allocation(&moved)
    };
    let dropped_now = AllocationRecord {
        step: AllocationStep::Freed,
        freed_at: Some(rig.store.now()),
        end_reason: Some(EndReason::Exited),
        ..allocation(&dropped)
    };
    let rebooked_now = AllocationRecord {
        step: AllocationStep::Booked,
        freed_at: None,
        end_reason: None,
        ..allocation(&rebooked)
    };
    let mut unread_now = encode_allocation(&allocation(&unread)).expect("encodes");
    unread_now[0] = FORMAT + 1;
    let txn = Txn::new()
        .put(
            crate::keys::job(&active),
            encode_job(&active_now).expect("encodes"),
            Cond::Any,
        )
        .put(
            crate::keys::job(&ended),
            encode_job(&ended_now).expect("encodes"),
            Cond::Any,
        )
        .put(
            crate::keys::allocation(&moved),
            encode_allocation(&moved_now).expect("encodes"),
            Cond::Any,
        )
        .put(
            crate::keys::allocation(&dropped),
            encode_allocation(&dropped_now).expect("encodes"),
            Cond::Any,
        )
        .put(
            crate::keys::allocation(&rebooked),
            encode_allocation(&rebooked_now).expect("encodes"),
            Cond::Any,
        )
        .put(crate::keys::allocation(&unread), unread_now, Cond::Any)
        .delete(crate::keys::allocation(&gone), Cond::Any)
        .delete(crate::keys::job(&gone), Cond::Any);
    rig.store
        .inner()
        .txn(txn)
        .expect("reachable")
        .expect("unconditional writes commit");
    assert_eq!(
        rig.jobs.index_entries().expect("reads"),
        entries,
        "{name}: the rewrite moved an entry"
    );

    assert_eq!(
        rig.jobs.queued().expect("reads"),
        Vec::new(),
        "{name}: the queue trusted an active entry over its ended row"
    );
    let on_a: Vec<JobId> = rig
        .jobs
        .live_on(&host("host-a"))
        .expect("reads")
        .into_iter()
        .map(|(allocation, _)| allocation.job)
        .collect();
    assert_eq!(
        on_a,
        std::slice::from_ref(&ended),
        "{name}: a host's allocations trusted an entry over its row"
    );
    assert_eq!(
        rig.jobs.booked_on(&host("host-a")).map(|b| b.allocations),
        Ok(1),
        "{name}: a host's bookings trusted an entry over its row"
    );
    let live: Vec<JobId> = rig
        .jobs
        .live_allocations()
        .expect("reads")
        .into_iter()
        .map(|allocation| allocation.job)
        .collect();
    assert_eq!(
        live,
        [ended.clone(), moved],
        "{name}: the live allocations trusted an entry over its row"
    );

    rig.clock.advance(Duration::from_secs(8 * 24 * 3600));
    let before = rig.snapshot().0;
    assert_eq!(
        jobs.tick(),
        Ok(Vec::new()),
        "{name}: the tick deleted a job its due entry named but its row keeps"
    );
    assert_eq!(
        rig.snapshot().0,
        before,
        "{name}: the tick's collection deleted a booked or unreadable row"
    );
    let boot = rig.store.now();
    assert_eq!(
        rig.jobs
            .raise_deadlines_at_boot(Txn::new(), boot)
            .map(|txn| txn.ops().len()),
        Ok(0),
        "{name}: the raise moved an end its row already has later"
    );
    assert_eq!(
        rig.jobs.raise_at_boot(boot),
        Ok(4),
        "{name}: the boot raise did not floor exactly the waiting jobs whose rows wait"
    );
    assert_eq!(
        (job(&active), job(&ended)),
        (active_now, ended_now),
        "{name}: the boot raise wrote a job whose row neither waits nor ends early"
    );
}

/// An indexed read whose space holds more entries than one page of a scan reads every page.
pub fn an_indexed_read_reads_every_page_of_entries<B: Backend>(backend: &B) {
    let name = "an_indexed_read_reads_every_page_of_entries";
    let rig = rig(backend);
    let template = rig
        .jobs
        .get_job(&rig.queue("pg-template"))
        .expect("reads")
        .expect("kept");
    let count = crate::SWEEP_BATCH.get() + 1;
    let mut bulk = Txn::new();
    for n in 0..count {
        let record = JobRecord {
            id: job_id(&format!("pg-{n:04}")),
            ..template.clone()
        };
        bulk = bulk.put(
            crate::keys::job(&record.id),
            encode_job(&record).expect("encodes"),
            Cond::Any,
        );
    }
    rig.store
        .inner()
        .txn(bulk)
        .expect("reachable")
        .expect("unconditional puts commit");
    rig.reindex();
    assert_eq!(
        rig.jobs.queued().map(|queued| queued.len()),
        Ok(count + 1),
        "{name}: the queue stopped at a page of entries"
    );
}

/// A backfill that loses a row to another writer between its read and its commit is stale,
/// naming the job; one on a row whose key names no valid job id is that row's corrupt key. The
/// next backfill finishes the work.
pub fn a_backfill_that_loses_its_row_is_stale<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_backfill_that_loses_its_row_is_stale";
    let rig = rig(backend);
    for (key, error) in [
        (
            "job/bs-raced",
            JobStoreError::Stale {
                id: job_id("bs-raced"),
            },
        ),
        (
            "alloc/bs-raced",
            JobStoreError::Stale {
                id: job_id("bs-raced"),
            },
        ),
        (
            "job/bs*bad",
            JobStoreError::Corrupt {
                key: "job/bs*bad".to_owned(),
                why: crate::DecodeError::WrongKey,
            },
        ),
    ] {
        let value = vec![FORMAT + 1];
        rig.store
            .inner()
            .txn(Txn::new().put(key, value.clone(), Cond::Any))
            .expect("reachable")
            .expect("commits");
        let inner = rig.store.inner();
        let rewrite = key.to_owned();
        rig.store.before_next(move || {
            inner
                .txn(Txn::new().put(rewrite, value, Cond::Any))
                .expect("reachable")
                .expect("the other writer commits");
        });
        assert_eq!(
            rig.jobs.backfill_indexes(),
            Err(error),
            "{name}: a backfill over a changed {key} was not refused by name"
        );
        assert_eq!(
            rig.jobs.backfill_indexes().map(|done| done.added),
            Ok(1),
            "{name}: the next backfill did not index {key}"
        );
    }
}

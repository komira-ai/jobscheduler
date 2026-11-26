//! The attempts a job's record keeps and the row of each that ran.

use std::collections::BTreeSet;
use std::time::Duration;

use js_state::conformance::Backend;
use js_state::{Cond, Store, StoreError, StoreTime, Txn};

use super::whole::{history, placed_whole, requeue_with};
use super::{Rig, attempt_end, exited, full_spec, host, job_id, policy, rig, share};
use crate::codec::{Carried, DecodeError, FORMAT, LATER_FORMAT, encode_attempt_row};
use crate::interfere::INJECTED;
use crate::{
    Attempt, AttemptOutcome, AttemptRecord, AttemptRow, EndReason, FailedPrepare, JobEnd, JobId,
    JobState, JobStoreError, MAX_ATTEMPT_HISTORY, StartFailure,
};

/// The end of an attempt whose host was lost.
fn lost() -> AttemptRecord {
    attempt_end(AttemptOutcome::HostLost, None, None, None)
}

/// The key of the row of attempt `number` of `id`, spelled out.
fn row_key(id: &JobId, number: u32) -> String {
    format!("attempt/{id}/{number:010}")
}

/// The error an operation answers when its read or write is made to fail.
fn injected() -> JobStoreError {
    JobStoreError::Store(StoreError::Failed(INJECTED.to_owned()))
}

/// Places the queued job `id` on `at`, retries its attempt as a lost host, frees the
/// allocation the retry released, and moves the clock on; answers when the attempt was placed
/// and when it ended.
fn retried_on<S: Store>(rig: &Rig<S>, id: &JobId, at: &str) -> (StoreTime, StoreTime) {
    let (placed, _) = rig
        .jobs
        .place(id, host(at), share())
        .expect("a queued job is placed");
    rig.tick();
    let ended = rig.store.now().expect("the store's clock");
    rig.jobs
        .retry_txn(id, lost(), ended)
        .expect("a placed job with attempts left retries");
    rig.jobs
        .free(id, EndReason::HostAbsent)
        .expect("the released allocation is freed");
    rig.tick();
    (placed.placed_at.expect("a placed job has a time"), ended)
}

/// A job retried more than the history bound allows over three hosts in turn still names all
/// three in its hosts: the attempts that named the first host fall out of the history, but
/// the hosts that held an attempt are kept whole, and they are what the history held before
/// it was bounded.
///
/// Catches a hosts set rebuilt from the bounded history, which forgets a host once its
/// attempts are dropped and lets a later attempt go back to it.
pub fn attempt_hosts_outlives_the_history_bound<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "attempt_hosts_outlives_the_history_bound";
    let rig = rig(backend);
    let id = rig.queue_spec(
        "ho-job",
        crate::JobSpec {
            retry: Some(policy(u32::MAX)),
            ..full_spec()
        },
    );
    let hosts = ["host-a", "host-b", "host-c"];
    let total = MAX_ATTEMPT_HISTORY + 4;
    for n in 0..total {
        retried_on(&rig, &id, hosts[n % 3]);
    }
    let job = rig.jobs.get_job(&id).expect("reads").expect("the job");
    assert_eq!(
        job.attempt.history().len(),
        MAX_ATTEMPT_HISTORY,
        "{name}: the history is not at its bound"
    );
    assert!(
        job.attempt
            .history()
            .iter()
            .all(|a| a.host != host("host-a") || a.number > 4),
        "{name}: the earliest attempts of the first host are still in the history"
    );
    let expected: BTreeSet<_> = hosts.iter().map(|h| host(h)).collect();
    assert_eq!(
        job.attempt.hosts(),
        &expected,
        "{name}: a host fell out of the hosts that held an attempt"
    );
    let first_attempts: Vec<u32> = job
        .attempt
        .history()
        .iter()
        .map(|a| a.number)
        .take(1)
        .collect();
    assert_eq!(
        first_attempts,
        [5],
        "{name}: the history does not start after the dropped attempts"
    );
}

/// After 44 attempts more than the bound, the history holds the newest 256 in order (numbers
/// 45 to 300), the 44 older are counted in dropped, every one of the 300 attempts still has
/// its row (dropped ones included), and the job's row stops growing with the attempts: its
/// encoded length after 300 attempts is its length after 270.
///
/// Catches a history that grows without a bound, a dropped count that is not kept, an attempt
/// row deleted when its history entry drops, and a job row that gains bytes per attempt past
/// the bound.
pub fn attempt_history_keeps_its_bound_and_counts_the_dropped<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "attempt_history_keeps_its_bound_and_counts_the_dropped";
    let rig = rig(backend);
    let id = rig.queue_spec(
        "hb-job",
        crate::JobSpec {
            retry: Some(policy(u32::MAX)),
            ..full_spec()
        },
    );
    let total = MAX_ATTEMPT_HISTORY + 44;
    let mut times = Vec::new();
    let mut length_at_270 = 0;
    for n in 1..=total {
        times.push(retried_on(&rig, &id, "host-a"));
        if n == 270 {
            length_at_270 = rig
                .raw(&format!("job/{id}"))
                .expect("the job row")
                .value
                .len();
        }
    }
    let length = rig
        .raw(&format!("job/{id}"))
        .expect("the job row")
        .value
        .len();
    assert_eq!(
        length, length_at_270,
        "{name}: the job's row grew with attempts past the bound"
    );
    let job = rig.jobs.get_job(&id).expect("reads").expect("the job");
    assert_eq!(
        (
            job.attempt.number(),
            job.attempt.dropped(),
            job.attempt.history().len()
        ),
        (300, 44, MAX_ATTEMPT_HISTORY),
        "{name}: the number, the dropped count or the length of the history"
    );
    let numbers: Vec<u32> = job.attempt.history().iter().map(|a| a.number).collect();
    assert_eq!(
        numbers,
        (45..=300).collect::<Vec<u32>>(),
        "{name}: the history is not the newest attempts in order"
    );
    for (n, (placed_at, ended_at)) in (1..=300u32).zip(times) {
        assert_eq!(
            rig.jobs.attempt_row(&id, n),
            Ok(Some(AttemptRow {
                host: host("host-a"),
                placed_at,
                ended_at,
            })),
            "{name}: the row of attempt {n}"
        );
    }
}

/// That the job `id` has the one row `attempt/<id>/<number>` of all the rows under its prefix,
/// holding `row`, and that the indexes equal a scan.
fn assert_one_row<S: Store>(rig: &Rig<S>, name: &str, id: &JobId, number: u32, row: AttemptRow) {
    let rows = rig
        .store
        .scan_prefix(&crate::keys::attempt_prefix(id))
        .expect("reads");
    let keys: Vec<String> = rows.into_iter().map(|(key, _)| key).collect();
    assert_eq!(
        keys,
        [row_key(id, number)],
        "{name}: {id}: the rows under the job's attempt prefix"
    );
    assert_eq!(
        rig.jobs.attempt_row(id, number),
        Ok(Some(row)),
        "{name}: {id}: the row of attempt {number}"
    );
    assert_eq!(
        rig.jobs.index_entries(),
        rig.jobs.index_by_scan(),
        "{name}: {id}: the indexes differ from the scan"
    );
}

/// Every end of an attempt that held a host writes its row in the commit of the end, and only
/// that commit: a retry, an end from running, an end from placed (a start timeout), the end of
/// a placed job a cancel asked for, and the terminal failed prepare each raise the commit count
/// by one and leave the one row `attempt/<id>/<n>` with the attempt's host, placed time and end
/// time; the second attempt of a retried job is the row 2; a commit that fails writes neither
/// the job nor the row; a job that ends while queued (a cancel, no capacity) writes no row.
///
/// Catches an end that forgets the row, writes it in a commit of its own, names it by the wrong
/// number, writes it for a job that ran nowhere, or writes it when the commit fails.
pub fn each_attempt_writes_its_row_in_its_commit<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "each_attempt_writes_its_row_in_its_commit";
    let rig = rig(backend);
    let commits = || rig.store.commit_stats().txns;
    let placed_at = |id: &JobId| {
        rig.jobs
            .get_job(id)
            .expect("reads")
            .expect("the job")
            .placed_at
            .expect("placed")
    };
    let at_a = |placed_at, ended_at| AttemptRow {
        host: host("host-a"),
        placed_at,
        ended_at,
    };

    // A retry, then the end of the second attempt.
    let retry = rig.running("ar-retry");
    rig.tick();
    let placed = placed_at(&retry);
    let before = commits();
    let queued = rig
        .jobs
        .retry_txn(&retry, lost(), rig.store.now().expect("clock"))
        .expect("a running job retries");
    let ended = queued.attempt.history()[0].ended_at;
    assert_eq!(
        commits(),
        before + 1,
        "{name}: the retry took other than one commit"
    );
    assert_one_row(&rig, name, &retry, 1, at_a(placed, ended));
    rig.jobs.free(&retry, EndReason::HostAbsent).expect("freed");
    rig.tick();
    let (second, _) = rig
        .jobs
        .place(&retry, host("host-b"), share())
        .expect("placed again");
    rig.jobs
        .transition(&retry, JobState::Placed, JobState::Starting)
        .expect("starting");
    rig.jobs
        .transition(&retry, JobState::Starting, JobState::Running)
        .expect("running");
    rig.tick();
    let before = commits();
    let second_end = rig
        .jobs
        .end_job(&retry, exited(1))
        .expect("ends")
        .ended_at
        .expect("ended");
    assert_eq!(
        commits(),
        before + 1,
        "{name}: the end of attempt 2 took other than one commit"
    );
    let rows = rig
        .store
        .scan_prefix(&crate::keys::attempt_prefix(&retry))
        .expect("reads");
    assert_eq!(rows.len(), 2, "{name}: the rows of the retried job");
    assert_eq!(
        rig.jobs.attempt_row(&retry, 2),
        Ok(Some(AttemptRow {
            host: host("host-b"),
            placed_at: second.placed_at.expect("placed"),
            ended_at: second_end,
        })),
        "{name}: the row of attempt 2"
    );
    assert_eq!(
        rig.jobs.attempt_row(&retry, 1),
        Ok(Some(at_a(placed, ended))),
        "{name}: the row of attempt 1 after attempt 2"
    );

    // The end from running.
    let running = rig.running("ar-running");
    rig.tick();
    let placed = placed_at(&running);
    let before = commits();
    let end = rig
        .jobs
        .end_job(&running, exited(0))
        .expect("ends")
        .ended_at
        .expect("ended");
    assert_one_row(&rig, name, &running, 1, at_a(placed, end));
    assert_eq!(
        commits(),
        before + 1,
        "{name}: the end took other than one commit"
    );

    // The end from placed, with a start timeout.
    let timed_out = rig.placed("ar-timeout");
    rig.tick();
    let placed = placed_at(&timed_out);
    let before = commits();
    let end = rig
        .jobs
        .end_job(
            &timed_out,
            JobEnd::FailedToStart(StartFailure::StartTimeout),
        )
        .expect("ends")
        .ended_at
        .expect("ended");
    assert_one_row(&rig, name, &timed_out, 1, at_a(placed, end));
    assert_eq!(
        commits(),
        before + 1,
        "{name}: the timeout took other than one commit"
    );

    // The cancel of a placed job, which ends when the host stops it.
    let cancel = rig.placed("ar-cancel");
    rig.tick();
    let placed = placed_at(&cancel);
    rig.jobs.request_cancel(&cancel).expect("asked");
    rig.tick();
    let before = commits();
    let end = rig
        .jobs
        .end_job(&cancel, super::cancelled())
        .expect("ends")
        .ended_at
        .expect("ended");
    assert_one_row(&rig, name, &cancel, 1, at_a(placed, end));
    assert_eq!(
        commits(),
        before + 1,
        "{name}: the cancel's end took other than one commit"
    );

    // The terminal failed prepare.
    let (prepare, placed_job, _) = placed_whole(&rig, "ar-prepare");
    rig.tick();
    let failed = FailedPrepare {
        terminal: true,
        ..requeue_with(&rig, 3)
    };
    let before = commits();
    let (ended_job, _) = rig
        .jobs
        .fail_prepare_with(&prepare, failed, &history)
        .expect("ends");
    assert_one_row(
        &rig,
        name,
        &prepare,
        1,
        at_a(
            placed_job.placed_at.expect("placed"),
            ended_job.ended_at.expect("ended"),
        ),
    );
    assert_eq!(
        commits(),
        before + 1,
        "{name}: the last prepare took other than one commit"
    );

    // A commit that fails writes neither the job nor the row.
    for (label, text) in [("retry", "ar-fail-retry"), ("end", "ar-fail-end")] {
        let failing = rig.running(text);
        rig.tick();
        let rows = rig.store.scan_prefix("").expect("reads");
        rig.store.fail_after(0);
        let result = if label == "retry" {
            rig.jobs
                .retry_txn(&failing, lost(), rig.store.now().expect("clock"))
                .map(drop)
        } else {
            rig.jobs.end_job(&failing, exited(1)).map(drop)
        };
        assert_eq!(result, Err(injected()), "{name}: a failed {label} commit");
        assert_eq!(
            rig.store.scan_prefix("").expect("reads"),
            rows,
            "{name}: a failed {label} commit wrote a row"
        );
        assert_eq!(
            rig.jobs.attempt_row(&failing, 1),
            Ok(None),
            "{name}: {label}"
        );
    }

    // A job that ends while it waits for a host ran nowhere.
    let queued = rig.queue("ar-queued");
    let capacity = rig.queue("ar-capacity");
    rig.tick();
    rig.jobs
        .request_cancel(&queued)
        .expect("a queued job's cancel");
    rig.jobs
        .end_job(&capacity, JobEnd::FailedToStart(StartFailure::NoCapacity))
        .expect("a queued job out of capacity ends");
    for id in [&queued, &capacity] {
        let rows = rig
            .store
            .scan_prefix(&crate::keys::attempt_prefix(id))
            .expect("reads");
        assert!(
            rows.is_empty(),
            "{name}: {id}: a job that ran nowhere wrote a row"
        );
        let job = rig.jobs.get_job(id).expect("reads").expect("the job");
        assert!(job.state.is_ended(), "{name}: {id} did not end");
        assert!(
            job.attempt.history().is_empty() && job.attempt.hosts().is_empty(),
            "{name}: {id}: a job that ran nowhere has a history"
        );
        assert_eq!(rig.jobs.attempt_row(id, 1), Ok(None), "{name}: {id}");
    }
    assert_eq!(
        rig.jobs.index_entries(),
        rig.jobs.index_by_scan(),
        "{name}: the indexes differ from the scan"
    );
}

/// Writes `value` under `key` past the job store, with no reindex.
fn put_row<S: Store>(rig: &Rig<S>, key: &str, value: Vec<u8>) {
    rig.store
        .inner()
        .txn(Txn::new().put(key, value, Cond::Any))
        .expect("the store is reachable")
        .expect("an unconditional put commits");
}

/// An attempt that ran on `at` from `placed_at` to `ended_at`, for a row.
fn attempt_of(number: u32, at: &str, placed_at: StoreTime, ended_at: StoreTime) -> Attempt {
    Attempt {
        number,
        host: host(at),
        queued_at: placed_at,
        placed_at,
        started_at: None,
        ended_at,
        end: lost(),
        retried: true,
        carried: Carried::default(),
    }
}

/// An attempt row of a job is read only for an ended attempt of that job: attempt 0, one past
/// the job's current attempt and the current attempt while it runs are none; an ended attempt
/// is its row; a row an earlier job of the same id left (placed before this job was
/// submitted) is none, and one placed exactly when it was submitted is the job's; a row under
/// the key that names another job or number is corrupt with a wrong key; a row of a later
/// format is an unknown row; a row that does not parse is corrupt; a job that is absent is not
/// found; and a read that fails is the store's error, whether the job's or the row's.
///
/// Catches a row served for an attempt that has not ended, a leftover row of an earlier job
/// served as this job's, a row under another job's name served, and a row that cannot be read
/// answered as absent.
pub fn an_attempt_row_is_read_only_for_an_ended_attempt_of_this_job<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "an_attempt_row_is_read_only_for_an_ended_attempt_of_this_job";
    let rig = rig(backend);
    rig.clock.advance(Duration::from_secs(100));
    let id = rig.queue("rr-job");
    rig.tick();
    let (_, ended) = retried_on(&rig, &id, "host-a");
    let submitted = rig
        .jobs
        .get_job(&id)
        .expect("reads")
        .expect("the job")
        .submitted_at;

    // The job waits for its second attempt: attempt 1 ended, 0 and 2 are none.
    let row = rig
        .jobs
        .attempt_row(&id, 1)
        .expect("reads")
        .expect("attempt 1 ended");
    assert_eq!(
        (row.host.clone(), row.ended_at),
        (host("host-a"), ended),
        "{name}: the ended attempt's row"
    );
    for number in [0, 2, u32::MAX] {
        assert_eq!(
            rig.jobs.attempt_row(&id, number),
            Ok(None),
            "{name}: attempt {number}"
        );
    }
    // The second attempt runs: its row is not there, and a leftover under its key is not read.
    rig.jobs
        .place(&id, host("host-b"), share())
        .expect("placed again");
    assert_eq!(
        rig.jobs.attempt_row(&id, 2),
        Ok(None),
        "{name}: the current attempt while it runs"
    );
    let before = StoreTime::from_duration(Duration::from_secs(50));
    let stale = |number| {
        encode_attempt_row(&id, &attempt_of(number, "host-z", before, before)).expect("encodes")
    };
    put_row(&rig, &row_key(&id, 2), stale(2));
    assert_eq!(
        rig.jobs.attempt_row(&id, 2),
        Ok(None),
        "{name}: a row an earlier job left under the current attempt's key"
    );
    // A leftover under an ended attempt's key, placed before the job was submitted.
    put_row(&rig, &row_key(&id, 1), stale(1));
    assert_eq!(
        rig.jobs.attempt_row(&id, 1),
        Ok(None),
        "{name}: a row placed before the job was submitted"
    );
    let at_submit = attempt_of(1, "host-y", submitted, submitted);
    put_row(
        &rig,
        &row_key(&id, 1),
        encode_attempt_row(&id, &at_submit).expect("encodes"),
    );
    assert_eq!(
        rig.jobs.attempt_row(&id, 1),
        Ok(Some(AttemptRow {
            host: host("host-y"),
            placed_at: submitted,
            ended_at: submitted,
        })),
        "{name}: a row placed when the job was submitted"
    );

    // A row that names another job, another number, or cannot be read.
    let key = row_key(&id, 1);
    let wrong = JobStoreError::Corrupt {
        key: key.clone(),
        why: DecodeError::WrongKey,
    };
    let other = job_id("rr-other");
    put_row(
        &rig,
        &key,
        encode_attempt_row(&other, &attempt_of(1, "host-a", submitted, submitted))
            .expect("encodes"),
    );
    assert_eq!(
        rig.jobs.attempt_row(&id, 1),
        Err(wrong.clone()),
        "{name}: another job's row"
    );
    put_row(
        &rig,
        &key,
        encode_attempt_row(&id, &attempt_of(2, "host-a", submitted, submitted)).expect("encodes"),
    );
    assert_eq!(
        rig.jobs.attempt_row(&id, 1),
        Err(wrong),
        "{name}: another number's row"
    );
    put_row(&rig, &key, vec![LATER_FORMAT, 0, 1, 0, 0, 0, 0]);
    assert_eq!(
        rig.jobs.attempt_row(&id, 1),
        Err(JobStoreError::UnknownRow { key: key.clone() }),
        "{name}: a row of a later format"
    );
    put_row(&rig, &key, vec![FORMAT, 0]);
    assert_eq!(
        rig.jobs.attempt_row(&id, 1),
        Err(JobStoreError::Corrupt {
            key,
            why: DecodeError::Truncated,
        }),
        "{name}: a row that does not parse"
    );

    // An absent job, and reads that fail.
    let absent = job_id("rr-absent");
    assert_eq!(
        rig.jobs.attempt_row(&absent, 1),
        Err(JobStoreError::NotFound { id: absent }),
        "{name}: an absent job"
    );
    rig.store.fail_read_after(0);
    assert_eq!(
        rig.jobs.attempt_row(&id, 1),
        Err(injected()),
        "{name}: the job's read fails"
    );
    rig.store.fail_read_after(1);
    assert_eq!(
        rig.jobs.attempt_row(&id, 1),
        Err(injected()),
        "{name}: the row's read fails"
    );
}

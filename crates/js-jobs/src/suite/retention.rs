//! Results and retention: a result is written with the end that makes it, read back whole
//! (after a restart too) until its retention end and not after, deleted with its job alone,
//! and kept past an outage by the raise at boot. Time is the backend's `ManualClock`.

use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::limits::MAX_LOG_STREAM_LEN;
use js_state::conformance::Backend;
use js_state::{Cond, Store, StoreError, StoreTime, Txn};

use super::{Rig, cancelled, exited, job_id, output, rig};
use crate::codec::{FORMAT, encode_job};
use crate::interfere::INJECTED;
use crate::{
    AllocationStep, BOOT_GRACE, EndReason, JobEnd, JobRecord, JobResult, JobState, JobStore,
    JobStoreError, JobTiming, Output, RandomIds, ResourceUsage, ResultReason, Retention,
    SWEEP_BATCH, StartFailure, sweep,
};

/// Seven days, the window of design question 14.
const WEEK: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// One second.
const SECOND: Duration = Duration::from_secs(1);

/// A job row of format 1 as the build before results wrote it for a job that succeeded: no
/// result fields (tags 31 to 40) and no retention end (tag 30). Its fields are listed in
/// `tests/golden/README.md`.
const BEFORE_RESULTS: &[u8] = include_bytes!("../../tests/golden/job-before-results.bin");

/// The job `id` as stored; it must be present.
fn stored<S: Store>(jobs: &JobStore<S>, id: &crate::JobId) -> JobRecord {
    jobs.get_job(id).expect("reads").expect("present")
}

/// The store's error for a read or commit the wrapper failed.
fn injected() -> JobStoreError {
    JobStoreError::Store(StoreError::Failed(INJECTED.to_owned()))
}

/// The store's clock now.
fn now<S: Store>(rig: &Rig<S>) -> StoreTime {
    rig.store.now()
}

/// Ends a job at T with a retention of seven days: at T + 7 d - 1 s the store reads the whole
/// result and nothing is past its end; at T + 7 d the sweep lists the job, and once deleted
/// the store reads none (design section 8, row 30).
pub fn a_result_is_readable_until_the_retention_end_and_not_after<B: Backend>(backend: &B) {
    let name = "a_result_is_readable_until_the_retention_end_and_not_after";
    let rig = rig(backend);
    let jobs = JobStore::new(Arc::clone(&rig.store), RandomIds)
        .expect("opens")
        .with_retention(Retention::new(WEEK).expect("a window"));
    assert_eq!(jobs.retention().window(), WEEK, "{name}: the window set");
    let id = rig.running("keep");
    let running = stored(&jobs, &id);
    rig.tick();
    let ended_at = now(&rig);
    let ended = jobs.end_job(&id, exited(0)).expect("ends");
    let want = JobResult {
        exit_code: Some(0),
        reason: ResultReason::Exit,
        stdout_tail: "done\n".to_owned(),
        stderr_tail: "warning: slow disk\n".to_owned(),
        truncated: false,
        timing: JobTiming {
            queued_at: running.submitted_at,
            placed_at: running.placed_at,
            started_at: running.started_at,
            ended_at,
        },
        usage: Some(ResourceUsage {
            cpu_time: Duration::from_millis(2500),
            peak_memory_bytes: 64 << 20,
        }),
    };
    assert_eq!(
        (ended.state, ended.result.clone(), ended.retention_ends),
        (
            JobState::Succeeded,
            Some(want.clone()),
            Some(ended_at.saturating_add(WEEK))
        ),
        "{name}: the end is not stored with its result and retention end"
    );

    rig.clock.advance(WEEK - SECOND);
    let read = stored(&jobs, &id);
    assert_eq!(read, ended, "{name}: the record changed within the window");
    assert_eq!(
        read.result,
        Some(want),
        "{name}: the result is not readable within the window"
    );
    assert_eq!(
        sweep([&read], now(&rig)),
        Vec::new(),
        "{name}: the sweep listed the job a second before its end"
    );
    assert_eq!(
        jobs.tick(),
        Ok(Vec::new()),
        "{name}: the tick deleted the job a second before its end"
    );

    rig.clock.advance(SECOND);
    assert_eq!(
        sweep([&read], now(&rig)),
        vec![id.clone()],
        "{name}: the sweep did not list the job at its end"
    );
    assert_eq!(
        jobs.delete_swept(std::slice::from_ref(&id)),
        Ok(vec![id.clone()]),
        "{name}: the swept job was not deleted"
    );
    assert_eq!(
        jobs.get_job(&id),
        Ok(None),
        "{name}: the job is readable after its retention end"
    );
}

/// Three jobs end (a clean exit, a wall limit, a cancel) with tails, the store restarts, and
/// every record (result, tails, truncation, timing, usage, retention end) reads as written
/// (design section 8, row 3).
pub fn restart_keeps_results<B: Backend>(backend: &B) {
    let name = "restart_keeps_results";
    let rig = rig(backend);
    let clean = rig.running("r-clean");
    let limit = rig.running("r-limit");
    let cancel = rig.running("r-cancel");
    rig.jobs.request_cancel(&cancel).expect("an intent");
    rig.tick();
    let mut written = Vec::new();
    written.push(rig.jobs.end_job(&clean, exited(0)).expect("ends"));
    rig.tick();
    let cut = Output {
        stdout: "x".repeat(MAX_LOG_STREAM_LEN + 10),
        truncated: true,
        ..output()
    };
    let wall_limit = JobEnd::WallLimit {
        exit_code: Some(137),
        output: cut,
    };
    written.push(rig.jobs.end_job(&limit, wall_limit).expect("ends"));
    rig.tick();
    let stopped = JobEnd::Cancelled {
        exit_code: Some(143),
        output: Output {
            usage: None,
            ..output()
        },
    };
    written.push(rig.jobs.end_job(&cancel, stopped).expect("ends"));
    let reasons: Vec<Option<ResultReason>> = written
        .iter()
        .map(|job| job.result.as_ref().map(|result| result.reason))
        .collect();
    assert_eq!(
        reasons,
        [
            Some(ResultReason::Exit),
            Some(ResultReason::WallLimit),
            Some(ResultReason::Cancelled)
        ],
        "{name}: the ends did not store their results"
    );

    let Rig { store, jobs, .. } = rig;
    drop(jobs);
    let store = Arc::into_inner(store)
        .and_then(|wrapped| wrapped.into_inner())
        .expect("nothing else holds the store");
    let jobs = JobStore::new(Arc::new(backend.restart(store)), RandomIds).expect("reopens");
    for job in written {
        assert_eq!(
            jobs.get_job(&job.id).expect("reads"),
            Some(job.clone()),
            "{name}: {} did not survive a restart as written",
            job.id
        );
    }
}

/// The result of a job outlives its allocation's end, and the allocation outlives the job's
/// deletion: freeing the allocation leaves the result, and sweeping the job leaves the
/// allocation's row (design section 8, row 30; question 14).
pub fn a_result_survives_the_end_of_its_allocation<B: Backend>(backend: &B) {
    let name = "a_result_survives_the_end_of_its_allocation";
    let rig = rig(backend);
    let id = rig.running("alloc-end");
    let ended = rig.jobs.end_job(&id, exited(2)).expect("ends");
    assert_eq!(
        ended.state,
        JobState::Failed,
        "{name}: a nonzero exit did not fail"
    );
    rig.tick();
    let freed = rig.jobs.free(&id, EndReason::Exited).expect("frees");
    assert_eq!(freed.step, AllocationStep::Freed, "{name}: not freed");
    assert_eq!(
        stored(&rig.jobs, &id),
        ended,
        "{name}: freeing the allocation changed the job or its result"
    );

    rig.clock.advance(WEEK);
    assert_eq!(
        rig.jobs.tick(),
        Ok(vec![id.clone()]),
        "{name}: the tick did not delete the job past its end"
    );
    assert_eq!(rig.jobs.get_job(&id), Ok(None), "{name}: the job is kept");
    assert_eq!(
        rig.jobs.get_allocation(&id),
        Ok(Some(freed)),
        "{name}: sweeping the job removed its allocation"
    );
}

/// An outage outlasts a retention end: at boot the end is raised to the boot plus
/// [`BOOT_GRACE`], so the result reads at boot, the sweep lists nothing then and lists the
/// job at the boot plus the grace. An end already later stays; a job with no end (queued)
/// and a row a later build wrote are not touched; and a second raise at the same boot adds
/// nothing (design section 6.7, step 7; section 8, row 36).
pub fn retention_ends_are_raised_at_boot_after_a_long_outage<B: Backend>(backend: &B) {
    let name = "retention_ends_are_raised_at_boot_after_a_long_outage";
    let rig = rig(backend);
    let early = rig.running("o-early");
    let early_ended = rig.jobs.end_job(&early, exited(0)).expect("ends");
    let queued = rig.queue("o-queued");
    rig.put_raw("job/o-zz-later", vec![FORMAT + 1]);

    // The outage: past the first end by an hour; then a job ends just before the boot.
    rig.clock.advance(WEEK + Duration::from_secs(3600));
    let late = rig.running("o-late");
    let late_ended = rig.jobs.end_job(&late, exited(0)).expect("ends");
    rig.clock.advance(Duration::from_secs(10));
    let boot = now(&rig);
    let queued_row = rig.raw("job/o-queued");
    let later_row = rig.raw("job/o-zz-later");

    let txn = rig
        .jobs
        .raise_deadlines_at_boot(Txn::new(), boot)
        .expect("reads every job");
    rig.store
        .txn(txn)
        .expect("reachable")
        .expect("nothing changed since the read");

    let raised = stored(&rig.jobs, &early);
    let floor = boot.saturating_add(BOOT_GRACE);
    assert_eq!(
        raised,
        JobRecord {
            retention_ends: Some(floor),
            ..early_ended
        },
        "{name}: the end was not raised to the boot plus the grace, or more than it changed"
    );
    assert_eq!(
        sweep([&raised], boot),
        Vec::new(),
        "{name}: the sweep lists the job at boot"
    );
    assert_eq!(
        rig.jobs.tick(),
        Ok(Vec::new()),
        "{name}: the first tick deleted a result"
    );
    assert_eq!(
        stored(&rig.jobs, &late),
        late_ended,
        "{name}: a later end was changed"
    );
    assert_eq!(
        (rig.raw("job/o-queued"), rig.raw("job/o-zz-later")),
        (queued_row, later_row),
        "{name}: a job with no end, or a later build's row, was written"
    );
    assert_eq!(
        stored(&rig.jobs, &queued).retention_ends,
        None,
        "{name}: a queued job was given an end"
    );
    let again = rig
        .jobs
        .raise_deadlines_at_boot(Txn::new(), boot)
        .expect("reads every job");
    assert_eq!(
        again.ops().len(),
        0,
        "{name}: a second raise at the same boot raised again"
    );

    rig.clock.advance(BOOT_GRACE);
    assert_eq!(
        sweep([&raised], now(&rig)),
        vec![early.clone()],
        "{name}: the sweep does not list the job at the boot plus the grace"
    );
    assert_eq!(
        rig.jobs.tick(),
        Ok(vec![early]),
        "{name}: the tick did not delete the job at the boot plus the grace"
    );
}

/// The same job ended in two stores whose clocks read far apart (one of them decades past
/// the process's real time) gets retention ends that are each store's clock plus the
/// window, exactly, so they differ by the clocks' difference: nothing reads the system clock.
pub fn retention_uses_the_injected_clock<B: Backend>(backend: &B) {
    let name = "retention_uses_the_injected_clock";
    let early = rig(backend);
    let late = rig(backend);
    early.clock.advance(Duration::from_secs(1_000));
    // About the year 2096 as seconds since the Unix epoch: decades past any real now.
    let far = Duration::from_secs(3_999_999_999);
    late.clock.advance(far);
    let mut ends = Vec::new();
    for rig in [&early, &late] {
        let id = rig.running("same");
        let at = now(rig);
        let ended = rig.jobs.end_job(&id, exited(0)).expect("ends");
        assert_eq!(
            ended.retention_ends,
            Some(at.saturating_add(WEEK)),
            "{name}: the end is not the store's clock plus the window"
        );
        ends.push(ended.retention_ends.map(StoreTime::as_duration));
    }
    assert_eq!(
        ends,
        [Some(Duration::from_secs(1_000) + WEEK), Some(far + WEEK)],
        "{name}: a retention end is not the exact instant of its store's clock"
    );
}

/// The result, the retention end and the end state are one commit: when it fails, or the
/// job changes before it, the job keeps its previous state with no result and no retention
/// end; when it succeeds, it is one commit.
pub fn the_result_and_the_state_change_commit_together<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "the_result_and_the_state_change_commit_together";
    let rig = rig(backend);
    let id = rig.running("together");
    let before = rig.snapshot();
    rig.store.fail_next();
    assert_eq!(
        rig.jobs.end_job(&id, exited(0)),
        Err(injected()),
        "{name}: a failed commit is not the store's error"
    );
    assert_eq!(rig.snapshot(), before, "{name}: a failed end wrote");
    let job = stored(&rig.jobs, &id);
    assert_eq!(
        (job.state, job.result, job.retention_ends),
        (JobState::Running, None, None),
        "{name}: the job ended without its result"
    );

    // A second writer records a cancel between the end's read and its commit.
    let other = rig.other();
    let target = id.clone();
    rig.store.before_next(move || {
        other
            .request_cancel(&target)
            .expect("the other writer cancels");
    });
    assert_eq!(
        rig.jobs.end_job(&id, exited(0)),
        Err(JobStoreError::Stale { id: id.clone() }),
        "{name}: an end over a changed job was not stale"
    );
    let job = stored(&rig.jobs, &id);
    assert_eq!(
        (
            job.state,
            job.cancel_requested,
            job.result,
            job.retention_ends
        ),
        (JobState::Running, true, None, None),
        "{name}: the stale end wrote part of the end"
    );

    let commits = rig.store.commit_stats().txns;
    let ended = rig.jobs.end_job(&id, cancelled()).expect("ends");
    assert_eq!(
        rig.store.commit_stats().txns,
        commits + 1,
        "{name}: the end took more than one commit"
    );
    assert!(
        ended.result.is_some() & ended.retention_ends.is_some(),
        "{name}: the end has no result or no retention end"
    );
    assert_eq!(stored(&rig.jobs, &id), ended, "{name}: not what was stored");
}

/// A standard output one byte over the cap, whose cut falls inside a character and which ends
/// in another, is stored as its last bytes from the next character on: within the cap, whole
/// characters, marked truncated; a standard error exactly at the cap is stored whole.
pub fn an_overlong_tail_is_cut_on_a_character_boundary<B: Backend>(backend: &B) {
    let name = "an_overlong_tail_is_cut_on_a_character_boundary";
    let rig = rig(backend);
    let id = rig.running("tails");
    let body = "a".repeat(MAX_LOG_STREAM_LEN - 4);
    let stdout = format!("é{body}€");
    assert_eq!(stdout.len(), MAX_LOG_STREAM_LEN + 1, "{name}: the setup");
    let stderr = "e".repeat(MAX_LOG_STREAM_LEN);
    let end = JobEnd::Exited {
        exit_code: 1,
        output: Output {
            stdout,
            stderr: stderr.clone(),
            truncated: false,
            usage: None,
        },
    };
    let ended = rig.jobs.end_job(&id, end).expect("ends");
    let result = stored(&rig.jobs, &id).result.expect("a result");
    assert_eq!(
        Some(&result),
        ended.result.as_ref(),
        "{name}: not what was stored"
    );
    assert_eq!(
        result.stdout_tail,
        format!("{body}€"),
        "{name}: the cut is not the last whole characters"
    );
    assert!(
        result.stdout_tail.len() <= MAX_LOG_STREAM_LEN,
        "{name}: the stored tail is over the cap"
    );
    assert!(
        result.truncated,
        "{name}: a cut tail is not marked truncated"
    );
    assert_eq!(
        result.stderr_tail, stderr,
        "{name}: a tail within the cap was not stored whole"
    );
}

/// A second end of an ended job, whatever it says, answers the first record and writes
/// nothing; an end the job's state does not allow, and an end of a job that does not exist,
/// are refused and write nothing; and `transition` makes no end.
pub fn ending_an_ended_job_keeps_the_first_result<B: Backend>(backend: &B) {
    let name = "ending_an_ended_job_keeps_the_first_result";
    let rig = rig(backend);
    let id = rig.running("twice");
    let first = rig.jobs.end_job(&id, exited(0)).expect("ends");
    let before = rig.snapshot();
    rig.tick();
    let seconds = [
        exited(3),
        cancelled(),
        JobEnd::Memory {
            exit_code: None,
            output: output(),
        },
        JobEnd::Lost,
    ];
    for second in seconds {
        assert_eq!(
            rig.jobs.end_job(&id, second),
            Ok(first.clone()),
            "{name}: a second end did not answer the first"
        );
    }
    assert_eq!(rig.snapshot(), before, "{name}: a second end wrote");

    let queued = rig.queue("twice-queued");
    let running = rig.running("twice-running");
    let before = rig.snapshot();
    assert_eq!(
        rig.jobs.end_job(&queued, exited(0)),
        Err(JobStoreError::IllegalTransition {
            id: queued.clone(),
            from: JobState::Queued,
            to: JobState::Succeeded,
        }),
        "{name}: a queued job exited"
    );
    assert_eq!(
        rig.jobs
            .end_job(&running, JobEnd::FailedToStart(StartFailure::NoCapacity)),
        Err(JobStoreError::IllegalTransition {
            id: running.clone(),
            from: JobState::Running,
            to: JobState::FailedToStart(StartFailure::NoCapacity),
        }),
        "{name}: a running job failed to start"
    );
    assert_eq!(
        rig.jobs
            .transition(&running, JobState::Running, JobState::Succeeded),
        Err(JobStoreError::IllegalTransition {
            id: running.clone(),
            from: JobState::Running,
            to: JobState::Succeeded,
        }),
        "{name}: a move ended a job without a result"
    );
    let missing = job_id("twice-missing");
    assert_eq!(
        rig.jobs.end_job(&missing, exited(0)),
        Err(JobStoreError::NotFound { id: missing }),
        "{name}: a missing job ended"
    );
    assert_eq!(rig.snapshot(), before, "{name}: a refused end wrote");
}

/// Rows written before results were stored read with no result and no retention end: an
/// ended job's (`job-before-results.bin`) and a queued job's (`job-before-network.bin`).
/// Neither is ever swept, and the ended one cannot end again.
pub fn a_job_without_a_result_decodes<B: Backend>(backend: &B) {
    let name = "a_job_without_a_result_decodes";
    let rig = rig(backend);
    rig.put_raw("job/golden-2", BEFORE_RESULTS.to_vec());
    rig.put_raw("job/golden-1", super::rows::BEFORE_NETWORK.to_vec());
    let ended = stored(&rig.jobs, &job_id("golden-2"));
    let at = |secs| Some(StoreTime::from_duration(Duration::from_secs(secs)));
    assert_eq!(
        (
            ended.state,
            ended.placed_at,
            ended.started_at,
            ended.ended_at,
            ended.host.as_ref().map(js_types::HostId::as_str),
            ended.result.clone(),
            ended.retention_ends,
        ),
        (
            JobState::Succeeded,
            at(6),
            at(7),
            at(8),
            Some("host-a"),
            None,
            None
        ),
        "{name}: an ended row written before results does not read as written"
    );
    let queued = stored(&rig.jobs, &job_id("golden-1"));
    assert_eq!(
        (queued.result.clone(), queued.retention_ends),
        (None, None),
        "{name}: a queued row written before results does not read without a result"
    );
    let far = StoreTime::from_duration(Duration::MAX);
    assert_eq!(
        sweep([&ended, &queued], far),
        Vec::new(),
        "{name}: a job with no retention end was swept"
    );
    assert_eq!(
        rig.jobs.end_job(&ended.id, exited(1)),
        Ok(ended.clone()),
        "{name}: an ended job written before results ended again"
    );
}

/// Jobs that have not ended (queued, placed, starting, running, running with a cancel asked
/// for) have no retention end: the sweep lists none of them however late it is asked, and a
/// tick centuries on deletes nothing.
pub fn sweep_never_lists_a_job_that_has_not_ended<B: Backend>(backend: &B) {
    let name = "sweep_never_lists_a_job_that_has_not_ended";
    let rig = rig(backend);
    let starting = rig.placed("live-starting");
    rig.jobs
        .transition(&starting, JobState::Placed, JobState::Starting)
        .expect("starting");
    let cancelling = rig.running("live-cancelling");
    rig.jobs.request_cancel(&cancelling).expect("an intent");
    let ids = [
        rig.queue("live-queued"),
        rig.placed("live-placed"),
        starting,
        rig.running("live-running"),
        cancelling,
    ];
    let live: Vec<JobRecord> = ids.iter().map(|id| stored(&rig.jobs, id)).collect();
    assert!(
        live.iter().all(|job| job.retention_ends.is_none()),
        "{name}: a live job has a retention end"
    );
    assert_eq!(
        sweep(&live, StoreTime::from_duration(Duration::MAX)),
        Vec::new(),
        "{name}: the sweep listed a job that has not ended"
    );
    rig.clock.advance(Duration::from_secs(1 << 40));
    assert_eq!(
        rig.jobs.tick(),
        Ok(Vec::new()),
        "{name}: the tick deleted a job that has not ended"
    );
    assert_eq!(rig.job_rows(), ids.len(), "{name}: a job row is gone");
}

/// A tick over more ended jobs than one batch deletes them all, one commit per batch of at
/// most [`SWEEP_BATCH`] rows; a job not yet at its end and a row a later build wrote are left
/// (the raise at boot leaves the later row too); and a row that does not parse stops the
/// tick and the raise with an error naming it.
pub fn the_tick_deletes_in_batches_and_leaves_what_it_cannot_read<B: Backend>(backend: &B) {
    let name = "the_tick_deletes_in_batches_and_leaves_what_it_cannot_read";
    let rig = rig(backend);
    let template = rig.running("b-template");
    let template = rig.jobs.end_job(&template, exited(0)).expect("ends");
    let batch = SWEEP_BATCH.get();
    let mut bulk = Txn::new();
    let mut want = Vec::new();
    for n in 0..=batch {
        let id = job_id(&format!("b-{n:04}"));
        let record = JobRecord {
            id: id.clone(),
            ..template.clone()
        };
        bulk = bulk.put(
            crate::keys::job(&id),
            encode_job(&record).expect("encodes"),
            Cond::Any,
        );
        want.push(id);
    }
    want.push(template.id.clone());
    rig.store
        .inner()
        .txn(bulk)
        .expect("reachable")
        .expect("unconditional puts commit");
    rig.put_raw("job/b-later", vec![FORMAT + 1]);
    rig.clock.advance(WEEK);
    let young = rig.running("b-young");
    rig.jobs.end_job(&young, exited(0)).expect("ends");

    let commits = rig.store.commit_stats().txns;
    assert_eq!(
        rig.jobs.tick(),
        Ok(want),
        "{name}: the tick did not delete exactly the jobs past their end, in key order"
    );
    assert_eq!(
        rig.store.commit_stats().txns,
        commits + 2,
        "{name}: the deletes were not one commit per batch"
    );
    let left: Vec<String> = rig
        .store
        .scan_prefix(crate::keys::JOB)
        .expect("reads")
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    assert_eq!(
        left,
        ["job/b-later", "job/b-young"],
        "{name}: the tick left the wrong rows"
    );

    rig.put_raw("job/b-corrupt", vec![0]);
    let corrupt = JobStoreError::Corrupt {
        key: "job/b-corrupt".to_owned(),
        why: crate::DecodeError::BadFormat,
    };
    assert_eq!(
        rig.jobs.tick(),
        Err(corrupt.clone()),
        "{name}: the tick read a corrupt row"
    );
    assert_eq!(
        rig.jobs
            .raise_deadlines_at_boot(Txn::new(), now(&rig))
            .map(drop),
        Err(corrupt),
        "{name}: the raise read a corrupt row"
    );
}

/// Each read and each commit of a sweep, a tick and a raise at boot failing in turn: the
/// operation answers the store's error and every row and the commit counters read as before.
pub fn a_failed_sweep_or_raise_changes_nothing<B: Backend>(backend: &B) {
    let name = "a_failed_sweep_or_raise_changes_nothing";
    let rig = rig(backend);
    let id = rig.running("fail-sweep");
    rig.jobs.end_job(&id, exited(0)).expect("ends");
    rig.clock.advance(WEEK);
    let ids = [id];

    type Operation<'a> = Box<dyn Fn() -> Result<(), JobStoreError> + 'a>;
    let jobs = &rig.jobs;
    let boot = now(&rig);
    let operations: [(&str, bool, Operation<'_>); 5] = [
        (
            "a sweep's read",
            true,
            Box::new(|| jobs.delete_swept(&ids).map(drop)),
        ),
        (
            "a sweep's commit",
            false,
            Box::new(|| jobs.delete_swept(&ids).map(drop)),
        ),
        ("a tick's read", true, Box::new(|| jobs.tick().map(drop))),
        ("a tick's commit", false, Box::new(|| jobs.tick().map(drop))),
        (
            "a raise's read",
            true,
            Box::new(|| jobs.raise_deadlines_at_boot(Txn::new(), boot).map(drop)),
        ),
    ];
    for (what, read, operation) in operations {
        let before = rig.snapshot();
        if read {
            rig.store.fail_read_after(0);
        } else {
            rig.store.fail_next();
        }
        assert_eq!(
            operation(),
            Err(injected()),
            "{name}: {what} answered otherwise"
        );
        assert_eq!(
            rig.snapshot(),
            before,
            "{name}: {what} failing changed the store"
        );
    }
}

/// A sweep whose job had its retention end raised between the read and the commit is stale
/// and deletes nothing; swept again, the raised job is not due and is left, as is a job that
/// is gone; a tick that loses the same race is stale too.
pub fn a_sweep_that_loses_to_a_raise_is_stale<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_sweep_that_loses_to_a_raise_is_stale";
    let rig = rig(backend);
    let id = rig.running("raced");
    rig.jobs.end_job(&id, exited(0)).expect("ends");
    rig.clock.advance(WEEK);
    let raise = |rig: &Rig<B::Store>| {
        let other = rig.other();
        let inner = rig.store.inner();
        let boot = now(rig);
        rig.store.before_next(move || {
            let txn = other
                .raise_deadlines_at_boot(Txn::new(), boot)
                .expect("the other writer reads");
            inner
                .txn(txn)
                .expect("reachable")
                .expect("the other writer raises");
        });
    };

    raise(&rig);
    assert_eq!(
        rig.jobs.delete_swept(std::slice::from_ref(&id)),
        Err(JobStoreError::Stale { id: id.clone() }),
        "{name}: a sweep over a raised end was not stale"
    );
    let raised = stored(&rig.jobs, &id);
    assert_eq!(
        raised.retention_ends,
        Some(now(&rig).saturating_add(BOOT_GRACE)),
        "{name}: the raise did not commit"
    );
    let before = rig.snapshot();
    let gone = job_id("raced-gone");
    assert_eq!(
        rig.jobs.delete_swept(&[id.clone(), gone]),
        Ok(Vec::new()),
        "{name}: a job no longer due, or one that is gone, was deleted"
    );
    assert_eq!(
        rig.snapshot(),
        before,
        "{name}: a sweep with nothing due wrote"
    );

    // Past the raised end, a tick loses the same race against a second raise.
    rig.clock.advance(BOOT_GRACE);
    raise(&rig);
    assert_eq!(
        rig.jobs.tick(),
        Err(JobStoreError::Stale { id: id.clone() }),
        "{name}: a tick over a raised end was not stale"
    );
    assert!(
        rig.jobs.get_job(&id).expect("reads").is_some(),
        "{name}: the stale tick deleted the job"
    );
}

//! The one way an allocation is freed, and the answer to a retired id.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use js_jobs::interfere::Interfere;
use js_jobs::{
    AllocationStep, AttemptOutcome, AttemptRecord, Booked, EndReason, JobEnd, JobId, JobState,
    JobStore, JobStoreError, NewJob, Output, ResourceUsage, ResultReason, StartFailure, keys,
};
use js_state::fake::{Fake, Flaw};
use js_state::{Cond, ManualClock, Store, StoreTime, Txn};
use js_types::{CpuSeries, HostId};
use js_usage::{
    AllocationId, Booked as UsageBooked, DrainOutcome, EndedHistory, EndedReason, EndedRow,
    ProfileKey, Used,
};

use super::{Freed, answer_retired, ended_reason, free_allocation, profile_of, reason_of, used_of};
use crate::agent_api::job_rig::{BOOKED, placed, spec, tables};
use crate::job_store::ServerJobs;

/// The reason a confirmation records is the job's end: its result's reason when it has one,
/// otherwise its state (a cancel asked for and not yet ended counts as a cancel). It catches
/// a free that records another reason than the job ended for.
#[test]
fn a_confirmation_frees_with_the_reason_of_the_jobs_end() {
    let (_store, jobs) = tables();
    let (job, _) = placed(&jobs, "job-a", "host-a", spec(), BOOKED);
    jobs.transition(&job.id, JobState::Placed, JobState::Starting)
        .unwrap();
    jobs.transition(&job.id, JobState::Starting, JobState::Running)
        .unwrap();
    let ended = jobs
        .end_job(
            &job.id,
            JobEnd::Exited {
                exit_code: 0,
                output: Output::default(),
            },
        )
        .unwrap();
    let with = |state: JobState, reason: Option<ResultReason>| {
        let mut record = ended.clone();
        record.state = state;
        record.result = reason.map(|reason| {
            let mut result = ended.result.clone().unwrap();
            result.reason = reason;
            result
        });
        reason_of(&record)
    };
    let failed = |reason| with(JobState::FailedToStart(reason), None);
    let cases = [
        (with(JobState::Cancelled, None), EndReason::Cancelled),
        (with(JobState::Lost, None), EndReason::HostAbsent),
        (failed(StartFailure::StartTimeout), EndReason::StartTimeout),
        (
            failed(StartFailure::ExclusiveNotReady),
            EndReason::FailedPrepare,
        ),
        (failed(StartFailure::AgentRefused), EndReason::Refused),
        (failed(StartFailure::NoCapacity), EndReason::Refused),
        (failed(StartFailure::InputUnavailable), EndReason::Refused),
        (
            with(JobState::Failed, Some(ResultReason::InputFailed)),
            EndReason::Exited,
        ),
        (
            with(JobState::Succeeded, Some(ResultReason::Exit)),
            EndReason::Exited,
        ),
        (with(JobState::Failed, None), EndReason::Exited),
        (
            with(JobState::Failed, Some(ResultReason::WallLimit)),
            EndReason::WallLimit,
        ),
        (
            with(JobState::Failed, Some(ResultReason::Memory)),
            EndReason::Memory,
        ),
        (
            with(JobState::Failed, Some(ResultReason::Cancelled)),
            EndReason::Cancelled,
        ),
        (with(JobState::Queued, None), EndReason::Cancelled),
        (with(JobState::Placed, None), EndReason::Cancelled),
        (with(JobState::Starting, None), EndReason::Cancelled),
        (with(JobState::Running, None), EndReason::Cancelled),
    ];
    for (index, (found, expected)) in cases.into_iter().enumerate() {
        assert_eq!(found, expected, "case {index}");
    }
}

/// A job whose input failed while it ran ends failed with reason input-failed, and its
/// allocation is freed as exited: its process ran and stopped, the failure being the host's
/// input's, not a stop by the server. One whose input was unavailable at the start is freed as
/// refused, as any start the agent refused. It catches the two new ends of a job's file system
/// freed under another reason (a cancel, a wall limit) than the one they ended for.
#[test]
fn an_input_failure_frees_as_exited_and_an_unavailable_input_as_refused() {
    let (_store, jobs) = tables();
    let (job, _) = placed(&jobs, "job-a", "host-a", spec(), BOOKED);
    jobs.transition(&job.id, JobState::Placed, JobState::Starting)
        .unwrap();
    jobs.transition(&job.id, JobState::Starting, JobState::Running)
        .unwrap();
    let ended = jobs
        .end_job(
            &job.id,
            JobEnd::InputFailed {
                exit_code: Some(1),
                input_failure: "input-unreadable".to_owned(),
                output: Output::default(),
            },
        )
        .unwrap();
    assert_eq!(ended.state, JobState::Failed);
    assert_eq!(
        ended.result.as_ref().map(|result| result.reason),
        Some(ResultReason::InputFailed)
    );
    assert_eq!(reason_of(&ended), EndReason::Exited);
    let (refused, _) = placed(&jobs, "job-b", "host-a", spec(), BOOKED);
    let refused = jobs
        .end_job(
            &refused.id,
            JobEnd::Refused {
                failure: StartFailure::InputUnavailable,
                detail: "input-unavailable".to_owned(),
            },
        )
        .unwrap();
    assert_eq!(
        refused.state,
        JobState::FailedToStart(StartFailure::InputUnavailable)
    );
    assert_eq!(reason_of(&refused), EndReason::Refused);
}

/// A confirmation does not free the allocation of a job still meant to run (it is not ended
/// and no cancel was asked for), and writes nothing; a provider's absence frees whatever the
/// job's state; a job whose row was swept frees as exited. It catches a free on the host
/// merely not listing a job that should be running.
#[test]
fn a_confirmation_frees_only_a_job_that_ended_or_was_cancelled() {
    let (store, jobs) = tables();
    let (running, _) = placed(&jobs, "job-running", "host-a", spec(), BOOKED);
    let before = store.commit_stats().txns;
    assert_eq!(
        free_allocation(&jobs, &running.id, Freed::HostConfirmed),
        Err(JobStoreError::NotReleasable {
            id: running.id.clone()
        })
    );
    assert_eq!(store.commit_stats().txns, before);
    let absent = free_allocation(&jobs, &running.id, Freed::ProviderAbsent).unwrap();
    assert_eq!(
        (absent.step, absent.end_reason),
        (AllocationStep::Freed, Some(EndReason::HostAbsent))
    );

    let (swept, _) = placed(&jobs, "job-swept", "host-a", spec(), BOOKED);
    jobs.end_job(&swept.id, JobEnd::Lost).unwrap();
    store
        .txn(Txn::new().delete(keys::job(&swept.id), Cond::Any))
        .unwrap()
        .unwrap();
    let freed = free_allocation(&jobs, &swept.id, Freed::HostConfirmed).unwrap();
    assert_eq!(
        (freed.step, freed.end_reason),
        (AllocationStep::Freed, Some(EndReason::Exited))
    );
}

/// A retired id changes only a job whose live allocation is on the host that retired it: a
/// cancelled job ends cancelled and frees, an ended job frees; an unknown id, a freed
/// allocation, another host's job and a job neither ended nor asked to cancel are left as
/// they are. It catches an answer that revives, frees another host's booking, or ends a job
/// nobody cancelled.
#[test]
fn a_retired_answer_changes_only_a_job_held_on_that_host() {
    let (store, jobs) = tables();
    let host = HostId::new("host-a").unwrap();
    let cancel = |id: &str, on: &str| {
        let (job, _) = placed(&jobs, id, on, spec(), BOOKED);
        jobs.request_cancel(&job.id).unwrap();
        job.id
    };
    let elsewhere = cancel("job-elsewhere", "host-b");
    let freed = cancel("job-freed", "host-a");
    jobs.end_and_free(&freed, JobEnd::Lost, EndReason::HostAbsent)
        .unwrap();
    let (idle, _) = placed(&jobs, "job-idle", "host-a", spec(), BOOKED);
    let before = store.commit_stats().txns;
    for id in [&elsewhere, &freed, &idle.id] {
        answer_retired(&jobs, &host, id).unwrap();
    }
    let unknown = js_jobs::JobId::parse("job-unknown").unwrap();
    answer_retired(&jobs, &host, &unknown).unwrap();
    assert_eq!(store.commit_stats().txns, before);
    let step = |id| jobs.get_allocation(id).unwrap().unwrap().step;
    assert_eq!(step(&elsewhere), AllocationStep::Booked);
    assert_eq!(step(&idle.id), AllocationStep::Booked);

    let cancelled = cancel("job-cancelled", "host-a");
    answer_retired(&jobs, &host, &cancelled).unwrap();
    assert_eq!(
        jobs.get_job(&cancelled).unwrap().unwrap().state,
        JobState::Cancelled
    );
    assert_eq!(step(&cancelled), AllocationStep::Freed);

    let (timed_out, _) = placed(&jobs, "job-timed-out", "host-a", spec(), BOOKED);
    let failure = JobEnd::FailedToStart(StartFailure::StartTimeout);
    jobs.end_job(&timed_out.id, failure).unwrap();
    answer_retired(&jobs, &host, &timed_out.id).unwrap();
    let allocation = jobs.get_allocation(&timed_out.id).unwrap().unwrap();
    assert_eq!(
        (allocation.step, allocation.end_reason),
        (AllocationStep::Freed, Some(EndReason::StartTimeout))
    );
}

/// Job tables over a store whose clock the test moves.
fn timed() -> (Arc<ManualClock>, Arc<Interfere<Fake>>, ServerJobs) {
    let time = Arc::new(ManualClock::default());
    let store = Arc::new(Interfere::new(Fake::with_flaw(
        Arc::clone(&time),
        Flaw::None,
    )));
    let dynamic: Arc<dyn Store> = store.clone();
    let jobs = JobStore::new(Arc::new(dynamic), crate::job_store::ServerIds::random()).unwrap();
    (time, store, jobs)
}

/// Every usage history row `store` holds, oldest end first.
fn history_rows(store: &Interfere<Fake>) -> Vec<EndedRow> {
    let limit = NonZeroUsize::new(100).unwrap();
    EndedHistory::new(store)
        .range(0, u64::MAX, limit)
        .unwrap()
        .rows
}

/// Asserts that freeing a job's allocation, on its host's confirmation or on the provider's
/// absence, writes exactly one usage history row, in the commit that frees: keyed by the
/// free's time and the job id, naming the host, what it booked, its queue wait (submit to
/// placement, at least the 2 s the test waits) and its run time (start to end, at least 5 s),
/// and the reason; that freeing again writes
/// nothing; and that a failed commit leaves neither the free nor the row. It catches a row
/// written in a second transaction, twice, or not at all.
#[test]
fn a_freed_job_allocation_writes_one_usage_row() {
    let cases = [
        (Freed::HostConfirmed, EndedReason::Released),
        (Freed::ProviderAbsent, EndedReason::Lost),
    ];
    for (freed, reason) in cases {
        let (time, store, jobs) = timed();
        let second = Duration::from_secs(1);
        let id = JobId::parse("job-a").unwrap();
        let new = NewJob {
            id: Some(id.clone()),
            spec: spec(),
            priority: 0,
            client: None,
        };
        jobs.insert_queued(new).unwrap();
        time.advance(2 * second);
        jobs.place(&id, HostId::new("host-a").unwrap(), BOOKED)
            .unwrap();
        time.advance(second);
        jobs.transition(&id, JobState::Placed, JobState::Starting)
            .unwrap();
        time.advance(second);
        jobs.transition(&id, JobState::Starting, JobState::Running)
            .unwrap();
        time.advance(5 * second);
        let exited = JobEnd::Exited {
            exit_code: 0,
            output: Output::default(),
        };
        jobs.end_job(&id, exited).unwrap();
        time.advance(second);
        let usage_keys = || store.scan_prefix("usage/").unwrap().len();

        store.fail_next();
        assert!(free_allocation(&jobs, &id, freed).is_err(), "{freed:?}");
        let step = jobs.get_allocation(&id).unwrap().unwrap().step;
        assert_eq!((step, usage_keys()), (AllocationStep::Releasing, 0));

        let before = store.commit_stats().txns;
        let allocation = free_allocation(&jobs, &id, freed).unwrap();
        assert_eq!(store.commit_stats().txns, before + 1, "{freed:?}");
        // The fake's clock moves a little at each write, so the times are read back from
        // the records: the queue wait is submit to placement, the run start to end.
        let job = jobs.get_job(&id).unwrap().unwrap();
        let ms = |duration: Duration| u64::try_from(duration.as_millis()).unwrap();
        let queued = ms(allocation.created_at.since(job.submitted_at));
        let ran = ms(job.ended_at.unwrap().since(job.started_at.unwrap()));
        let ended = ms(allocation.freed_at.unwrap().as_duration());
        assert!(
            (queued >= 2_000) & (ran >= 5_000),
            "{freed:?}: {queued} {ran}"
        );
        let row = EndedRow {
            id: AllocationId::parse("job-a").unwrap(),
            profile: profile_of(&job),
            host: Some(HostId::new("host-a").unwrap()),
            booked: Some(UsageBooked {
                whole_host: false,
                cpu_milli: 1500,
                memory_bytes: 1 << 30,
            }),
            queued_ms: Some(queued),
            run_ms: Some(ran),
            ended_reason: reason,
            drain_outcome: DrainOutcome::None,
            give_up_outcome: None,
            estimated_end: None,
            actual_end: ended,
            error_ms: None,
            used: None,
            cpu_series: None,
            class: Some(js_sched_placer::sizes::class_name(&job)),
            attempt: None,
        };
        assert_eq!(history_rows(&store), [row], "{freed:?}");
        assert_eq!(usage_keys(), 2, "{freed:?}: the row and its index entry");

        let before = store.commit_stats().txns;
        time.advance(second);
        free_allocation(&jobs, &id, freed).unwrap();
        assert_eq!(
            (store.commit_stats().txns, usage_keys()),
            (before, 2),
            "{freed:?}: a second free wrote"
        );
    }
}

/// Asserts that the history row of a job whose id the history's ids do not take (an
/// upper-case letter) is named by the allocation's id, that a whole-host allocation is
/// recorded as booking the whole host, that a cancel answered by a retired id records a
/// job that never ran (zero run time) for the reason it was freed, that a job whose row was
/// swept is recorded with no queue wait or run time, and the history's reason for every
/// reason an allocation is freed for. It catches a row lost for an id, a wrong booking, and
/// a reason recorded as another.
#[test]
fn the_history_row_of_a_free_names_its_allocation_its_booking_and_its_reason() {
    let (time, store, jobs) = timed();
    let host = HostId::new("host-a").unwrap();
    let (upper, allocation) = placed(&jobs, "Job-Upper", "host-a", spec(), Booked::WholeHost);
    time.advance(Duration::from_secs(1));
    free_allocation(&jobs, &upper.id, Freed::ProviderAbsent).unwrap();
    let hex = allocation.allocation_id.to_string();
    let rows = history_rows(&store);
    assert_eq!(rows[0].id.as_str(), hex);
    let whole = UsageBooked {
        whole_host: true,
        cpu_milli: 0,
        memory_bytes: 0,
    };
    assert_eq!(
        (rows[0].booked, rows[0].run_ms, rows[0].ended_reason),
        (Some(whole), Some(0), EndedReason::Lost)
    );

    let (cancelled, _) = placed(&jobs, "job-cancel", "host-a", spec(), BOOKED);
    jobs.request_cancel(&cancelled.id).unwrap();
    time.advance(Duration::from_secs(1));
    answer_retired(&jobs, &host, &cancelled.id).unwrap();
    let rows = history_rows(&store);
    assert_eq!(rows[1].id.as_str(), "job-cancel");
    assert_eq!(
        (rows[1].run_ms, rows[1].ended_reason),
        (Some(0), EndedReason::Released)
    );

    let (swept, _) = placed(&jobs, "job-swept", "host-a", spec(), BOOKED);
    jobs.end_job(&swept.id, JobEnd::Lost).unwrap();
    store
        .txn(Txn::new().delete(keys::job(&swept.id), Cond::Any))
        .unwrap()
        .unwrap();
    time.advance(Duration::from_secs(1));
    free_allocation(&jobs, &swept.id, Freed::HostConfirmed).unwrap();
    let rows = history_rows(&store);
    assert_eq!(
        (rows[2].id.as_str(), rows[2].queued_ms, rows[2].run_ms),
        ("job-swept", None, None)
    );

    let reasons = [
        (Some(EndReason::Exited), EndedReason::Released),
        (Some(EndReason::Memory), EndedReason::Released),
        (Some(EndReason::Cancelled), EndedReason::Released),
        (None, EndedReason::Released),
        (Some(EndReason::WallLimit), EndedReason::StoppedAtWallLimit),
        (Some(EndReason::HostAbsent), EndedReason::Lost),
        (Some(EndReason::FailedPrepare), EndedReason::FailedToStart),
        (Some(EndReason::StartTimeout), EndedReason::FailedToStart),
        (Some(EndReason::Refused), EndedReason::FailedToStart),
    ];
    for (reason, recorded) in reasons {
        assert_eq!(ended_reason(reason), recorded, "{reason:?}");
    }
}

/// Asserts what a job's usage history row says it used: its host's peak memory, and the CPU
/// time it reported per second of its run (from its start to its end) as both its highest and
/// its mean; nothing for a job whose host reported no usage or that has no result; and no CPU
/// for a job that never started. It catches a row that drops the job's measurement (every
/// job row read "nothing measured", so no report could set what a host used beside what it
/// booked) and a CPU divided by the wrong time.
#[test]
fn a_job_row_holds_the_peak_and_the_cpu_its_host_reported() {
    let (time, _store, jobs) = timed();
    let second = Duration::from_secs(1);
    let usage = ResourceUsage {
        cpu_time: Duration::from_secs(30),
        peak_memory_bytes: 3 << 30,
        cpu_throttle: None,
        cpu_pressure_some: None,
        cpu_peak: None,
    };
    let output = Output {
        usage: Some(usage),
        ..Output::default()
    };
    let (job, _) = placed(&jobs, "ran", "host-a", spec(), BOOKED);
    assert_eq!(used_of(&job), None, "no result");
    jobs.transition(&job.id, JobState::Placed, JobState::Starting)
        .unwrap();
    jobs.transition(&job.id, JobState::Starting, JobState::Running)
        .unwrap();
    time.advance(10 * second);
    let end = JobEnd::Exited {
        exit_code: 0,
        output: output.clone(),
    };
    let ended = jobs.end_job(&job.id, end).unwrap();
    let timing = ended.result.as_ref().unwrap().timing;
    let ran = timing
        .ended_at
        .since(timing.started_at.unwrap())
        .as_micros();
    assert!(ran >= 10_000_000, "{ran}");
    let cpu = u64::try_from(30_000_000_000 / ran).unwrap();
    let used = Used {
        cpu_milli: cpu,
        memory_bytes: 3 << 30,
        cpu_mean_milli: cpu,
    };
    assert_eq!(used_of(&ended), Some(used));

    let (quiet, _) = placed(&jobs, "quiet", "host-a", spec(), BOOKED);
    let cancelled = JobEnd::Cancelled {
        exit_code: None,
        output: Output::default(),
    };
    assert_eq!(
        used_of(&jobs.end_job(&quiet.id, cancelled).unwrap()),
        None,
        "no usage"
    );

    let (unstarted, _) = placed(&jobs, "unstarted", "host-a", spec(), BOOKED);
    let cancelled = JobEnd::Cancelled {
        exit_code: None,
        output,
    };
    let unstarted = jobs.end_job(&unstarted.id, cancelled).unwrap();
    let none = Used {
        cpu_milli: 0,
        memory_bytes: 3 << 30,
        cpu_mean_milli: 0,
    };
    assert_eq!(used_of(&unstarted), Some(none), "never ran: no CPU");
}

/// Asserts that a job's history row is kept under the name of what it builds, the same for a
/// repeat run under another job id: the `size-class` label as it is, else `run-` and the hash
/// of its image name and command (one name for two jobs that run the same command, whatever
/// their args, another for another command). Each row keeps what its own run used. A label that
/// is no profile key names none. Catches a row keyed by its job id, image digest or args, which
/// a repeat run never finds.
#[test]
fn a_repeat_run_of_one_build_writes_its_row_under_the_same_name() {
    let (time, store, jobs) = timed();
    let labels = |value: &str| {
        let map = std::collections::BTreeMap::from([("size-class".to_owned(), value.to_owned())]);
        js_types::Labels::from_caller(&map).unwrap()
    };
    let named = || {
        let mut spec = spec();
        spec.labels = labels("kbf.cargo-test.kbf-server");
        spec
    };
    for (id, peak) in [("build-1", 3u64 << 30), ("build-2", 5 << 30)] {
        let (job, _) = placed(&jobs, id, "host-a", named(), BOOKED);
        jobs.transition(&job.id, JobState::Placed, JobState::Starting)
            .unwrap();
        jobs.transition(&job.id, JobState::Starting, JobState::Running)
            .unwrap();
        time.advance(Duration::from_secs(10));
        let usage = ResourceUsage {
            cpu_time: Duration::from_secs(20),
            peak_memory_bytes: peak,
            cpu_throttle: None,
            cpu_pressure_some: None,
            cpu_peak: None,
        };
        let output = Output {
            usage: Some(usage),
            ..Output::default()
        };
        let end = JobEnd::Exited {
            exit_code: 0,
            output,
        };
        jobs.end_job(&job.id, end).unwrap();
        free_allocation(&jobs, &job.id, Freed::HostConfirmed).unwrap();
    }
    let name = ProfileKey::parse("kbf.cargo-test.kbf-server").unwrap();
    let rows: Vec<_> = history_rows(&store)
        .into_iter()
        .map(|row| {
            (
                row.id.to_string(),
                row.profile,
                row.used.map(|used| used.memory_bytes),
            )
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            ("build-1".to_owned(), Some(name.clone()), Some(3 << 30)),
            ("build-2".to_owned(), Some(name), Some(5 << 30)),
        ]
    );

    let (first, _) = placed(&jobs, "plain-1", "host-a", spec(), BOOKED);
    let (again, _) = placed(&jobs, "plain-2", "host-a", spec(), BOOKED);
    let mut args = spec();
    args.args = vec!["--lease".to_owned(), "plain-3".to_owned()];
    let (args, _) = placed(&jobs, "plain-3", "host-a", args, BOOKED);
    let mut other = spec();
    other.command = vec!["bench".to_owned()];
    let (other, _) = placed(&jobs, "plain-4", "host-a", other, BOOKED);
    let plain = profile_of(&first).unwrap();
    let digits = plain.as_str().strip_prefix("run-").unwrap();
    assert_eq!(digits.len(), 16, "{plain:?}");
    assert!(digits.bytes().all(|b| b.is_ascii_hexdigit()), "{plain:?}");
    assert_eq!(profile_of(&again), Some(plain.clone()), "same command");
    assert_eq!(profile_of(&args), Some(plain.clone()), "other args");
    assert_ne!(profile_of(&other), Some(plain), "another command");

    let mut odd = spec();
    odd.labels = labels("-odd");
    let (odd, _) = placed(&jobs, "odd", "host-a", odd, BOOKED);
    assert_eq!(profile_of(&odd), None, "no profile key");
}

/// Asserts that a job labelled with the class of a build target
/// ([`js_types::SizeClass::of_target`]) keeps its history row under exactly that name, for a
/// short target and for one long enough to be cut and hashed. Catches a target's class that is
/// no profile key, so that its rows would carry no name at all.
#[test]
fn a_build_targets_class_names_its_history_row() {
    let (_, _, jobs) = timed();
    let long = format!("root//{}:integration_test", "deep/".repeat(20));
    for (id, target) in [
        ("target-1", "root//crates/js-sched:js-sched".to_owned()),
        ("target-2", long),
    ] {
        let class = js_types::SizeClass::of_target(&target).unwrap();
        let mut spec = spec();
        let map = std::collections::BTreeMap::from([(
            js_types::SIZE_CLASS_LABEL.to_owned(),
            class.as_str().to_owned(),
        )]);
        spec.labels = js_types::Labels::from_caller(&map).unwrap();
        let (job, _) = placed(&jobs, id, "host-a", spec, BOOKED);
        let name = ProfileKey::parse(class.as_str()).unwrap();
        assert_eq!(profile_of(&job), Some(name), "{target}");
    }
}

/// Asserts that the history row of a job that sends a 200-byte history key (a build target's
/// full name with slashes, a colon, capitals and spaces) keeps that class byte for byte as its
/// `class`, with no profile (the name is no profile key), and that a job labelled with a short
/// class keeps it as both. Catches a class cut, lowered or dropped on its way to the usage
/// store, so a booking's row could not say which target it was.
#[test]
fn a_history_key_is_kept_byte_for_byte_in_the_history_row() {
    let (_, store, jobs) = timed();
    let long = format!("Komira//{}json:json mojo_compile build", "src/".repeat(40));
    let long = format!("{long}{}", " x".repeat((200 - long.len()) / 2));
    let long = format!("{long}{}", "y".repeat(200 - long.len()));
    assert_eq!(long.len(), 200);
    let mut full = spec();
    full.history_key = Some(js_types::HistoryKey::parse(&long).unwrap());
    let mut labelled = spec();
    let map = std::collections::BTreeMap::from([("size-class".to_owned(), "compile".to_owned())]);
    labelled.labels = js_types::Labels::from_caller(&map).unwrap();
    for (id, spec) in [("full-1", full), ("short-1", labelled)] {
        let (job, _) = placed(&jobs, id, "host-a", spec, BOOKED);
        jobs.transition(&job.id, JobState::Placed, JobState::Starting)
            .unwrap();
        jobs.transition(&job.id, JobState::Starting, JobState::Running)
            .unwrap();
        let end = JobEnd::Exited {
            exit_code: 0,
            output: Output::default(),
        };
        jobs.end_job(&job.id, end).unwrap();
        free_allocation(&jobs, &job.id, Freed::HostConfirmed).unwrap();
    }
    let rows: Vec<_> = history_rows(&store)
        .into_iter()
        .map(|row| (row.id.to_string(), row.class, row.profile))
        .collect();
    let compile = ProfileKey::parse("compile").unwrap();
    assert_eq!(
        rows,
        vec![
            ("full-1".to_owned(), Some(long), None),
            (
                "short-1".to_owned(),
                Some("compile".to_owned()),
                Some(compile)
            ),
        ]
    );
}

/// Asserts that the history row of a freed job carries the CPU series its result holds, as
/// stored and whatever else the result reports: with a usage, without one (the series is not
/// tied to it), and that a job whose result holds no series, and one that was lost and has no
/// result at all, are recorded with none. It catches a series lost between the job's result
/// and its usage row, one dropped for lack of a usage, and one made up for a job without.
#[test]
fn the_history_row_of_a_free_carries_the_jobs_cpu_series() {
    let series = CpuSeries::new(2, vec![0, 16, 40], 5_500_000, true).unwrap();
    let usage = ResourceUsage {
        cpu_time: Duration::from_secs(10),
        peak_memory_bytes: 1 << 20,
        cpu_throttle: None,
        cpu_pressure_some: None,
        cpu_peak: None,
    };
    let cases = [
        ("with-usage", Some(usage), Some(series.clone())),
        ("no-usage", None, Some(series.clone())),
        ("no-series", Some(usage), None),
    ];
    let (time, store, jobs) = timed();
    for (name, usage, cpu_series) in cases {
        let (job, _) = placed(&jobs, name, "host-a", spec(), BOOKED);
        jobs.transition(&job.id, JobState::Placed, JobState::Starting)
            .unwrap();
        jobs.transition(&job.id, JobState::Starting, JobState::Running)
            .unwrap();
        time.advance(Duration::from_secs(5));
        let output = Output {
            usage,
            cpu_series,
            ..Output::default()
        };
        let exited = JobEnd::Exited {
            exit_code: 0,
            output,
        };
        jobs.end_job(&job.id, exited).unwrap();
        free_allocation(&jobs, &job.id, Freed::HostConfirmed).unwrap();
    }
    let (lost, _) = placed(&jobs, "lost", "host-a", spec(), BOOKED);
    jobs.end_job(&lost.id, JobEnd::Lost).unwrap();
    free_allocation(&jobs, &lost.id, Freed::ProviderAbsent).unwrap();

    let rows = history_rows(&store);
    let by_name = |name: &str| rows.iter().find(|row| row.id.as_str() == name).unwrap();
    assert_eq!(by_name("with-usage").cpu_series, Some(series.clone()));
    assert!(by_name("with-usage").used.is_some());
    assert_eq!(by_name("no-usage").cpu_series, Some(series));
    assert_eq!(by_name("no-usage").used, None);
    assert_eq!(by_name("no-series").cpu_series, None);
    assert_eq!(by_name("lost").cpu_series, None);
}

/// Asserts that a job on its first attempt writes a usage row with no attempt, that after one
/// retry (its host lost) the job's second placement, end and free write a second row with
/// attempt 2, that the first attempt's row, written at its own free after the retry queued the
/// job again, still says none, and that both rows exist, one per attempt, under the job's name.
/// It catches an attempt left out of the row, one counted on the first attempt, and the second
/// attempt's row lost behind the first's.
#[test]
fn each_attempt_of_a_retried_job_writes_its_own_usage_row() {
    let name = "each_attempt_of_a_retried_job_writes_its_own_usage_row";
    let (time, store, jobs) = timed();
    let retrying = js_jobs::JobSpec {
        retry: Some(js_types::RetryPolicy {
            max_attempts: 3,
            initial_backoff: None,
            max_backoff: None,
        }),
        ..spec()
    };
    let (job, _) = placed(&jobs, "job-retry", "host-a", retrying, BOOKED);
    time.advance(Duration::from_secs(1));
    jobs.transition(&job.id, JobState::Placed, JobState::Starting)
        .unwrap();
    jobs.transition(&job.id, JobState::Starting, JobState::Running)
        .unwrap();
    time.advance(Duration::from_secs(2));
    let lost = AttemptRecord {
        outcome: AttemptOutcome::HostLost,
        exit_code: None,
        reason: None,
        start_failure: None,
        start_failure_detail: None,
    };
    let not_before = jobs.now().unwrap().saturating_add(Duration::from_secs(1));
    let queued = jobs.retry_txn(&job.id, lost, not_before).unwrap();
    assert_eq!(queued.attempt.number(), 1, "{name}: number after retry");
    free_allocation(&jobs, &job.id, Freed::ProviderAbsent).unwrap();
    let first = history_rows(&store);
    assert_eq!(first.len(), 1, "{name}: one row after the first free");
    assert_eq!(first[0].attempt, None, "{name}: first attempt's row");
    assert_eq!(first[0].id.as_str(), "job-retry", "{name}: first id");
    let entry = &queued.attempt.history()[0];
    let ms = |from: StoreTime, to: StoreTime| u64::try_from(to.since(from).as_millis()).unwrap();
    let started = entry.started_at.unwrap();
    assert_eq!(
        (first[0].queued_ms, first[0].run_ms),
        (
            Some(ms(entry.queued_at, entry.placed_at)),
            Some(ms(started, entry.ended_at))
        ),
        "{name}: the first attempt's wait and run, from its history entry"
    );
    assert!(
        first[0].run_ms >= Some(2000),
        "{name}: the first attempt ran its two seconds"
    );

    time.advance(Duration::from_secs(3));
    let host = HostId::new("host-a").unwrap();
    let (_, second) = jobs.place(&job.id, host, BOOKED).unwrap();
    assert_eq!(
        jobs.get_job(&job.id).unwrap().unwrap().attempt.number(),
        2,
        "{name}: number after the second placement"
    );
    time.advance(Duration::from_secs(1));
    jobs.end_job(&job.id, JobEnd::Lost).unwrap();
    free_allocation(&jobs, &job.id, Freed::ProviderAbsent).unwrap();
    let rows = history_rows(&store);
    assert_eq!(rows.len(), 2, "{name}: one row per attempt");
    assert_eq!(rows[0], first[0], "{name}: the first row is unchanged");
    assert_eq!(rows[1].attempt, Some(2), "{name}: second attempt's row");
    assert_eq!(rows[1].id.as_str(), "job-retry", "{name}: second id");
    assert_eq!(
        (rows[1].queued_ms, rows[1].run_ms),
        (Some(ms(not_before, second.created_at)), Some(0)),
        "{name}: the second attempt waited from its backoff's end and never started"
    );
}

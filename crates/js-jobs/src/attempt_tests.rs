//! Tests of the attempts of a job.

use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::record::JobState;
use crate::suite::{full_spec, host, job_id, output};

fn t(secs: u64) -> StoreTime {
    StoreTime::from_duration(Duration::from_secs(secs))
}

fn record(
    outcome: AttemptOutcome,
    exit_code: Option<i32>,
    reason: Option<ResultReason>,
    start_failure: Option<StartFailure>,
    detail: Option<&str>,
) -> AttemptRecord {
    AttemptRecord {
        outcome,
        exit_code,
        reason,
        start_failure,
        start_failure_detail: detail.map(str::to_string),
    }
}

fn job(host_id: Option<&str>, placed_at: Option<StoreTime>, trail: AttemptTrail) -> JobRecord {
    JobRecord {
        id: job_id("job-a"),
        spec: full_spec(),
        priority: 0,
        state: JobState::Running,
        submitted_at: t(10),
        placed_at,
        started_at: Some(t(25)),
        ended_at: None,
        host: host_id.map(host),
        cancel_requested: false,
        cancel_requested_at: None,
        result: None,
        start_failure_detail: None,
        retention_ends: None,
        deadline_floor: None,
        client: None,
        failed_prepares: 0,
        prepare_bars: BTreeMap::new(),
        held_refusals: Vec::new(),
        capacity_refusals: Vec::new(),
        queue: Default::default(),
        attempt: trail,
        carried: Carried::default(),
    }
}

fn entry(number: u32) -> Attempt {
    Attempt {
        number,
        host: host("h-old"),
        queued_at: t(1),
        placed_at: t(2),
        started_at: None,
        ended_at: t(3),
        end: record(AttemptOutcome::HostLost, None, None, None, None),
        retried: true,
        carried: Carried::default(),
    }
}

fn simple_end() -> AttemptRecord {
    record(
        AttemptOutcome::Failed,
        Some(1),
        Some(ResultReason::Exit),
        None,
        None,
    )
}

/// Asserts `of_end` of each variant with an exit code or none, carrying the reason.
///
/// Catches an exit code or reason taken from the wrong variant, or an exit code dropped.
#[test]
fn of_end_exited_and_stopped_variants() {
    let name = "of_end_exited_and_stopped_variants";
    let o = output();
    assert_eq!(
        AttemptRecord::of_end(&JobEnd::Exited {
            exit_code: 0,
            output: o.clone()
        }),
        record(
            AttemptOutcome::Succeeded,
            Some(0),
            Some(ResultReason::Exit),
            None,
            None
        ),
        "{name}: exit 0"
    );
    assert_eq!(
        AttemptRecord::of_end(&JobEnd::Exited {
            exit_code: 1,
            output: o.clone()
        }),
        record(
            AttemptOutcome::Failed,
            Some(1),
            Some(ResultReason::Exit),
            None,
            None
        ),
        "{name}: exit 1"
    );
    assert_eq!(
        AttemptRecord::of_end(&JobEnd::WallLimit {
            exit_code: Some(137),
            output: o.clone()
        }),
        record(
            AttemptOutcome::Failed,
            Some(137),
            Some(ResultReason::WallLimit),
            None,
            None
        ),
        "{name}: wall limit with exit"
    );
    assert_eq!(
        AttemptRecord::of_end(&JobEnd::WallLimit {
            exit_code: None,
            output: o.clone()
        }),
        record(
            AttemptOutcome::Failed,
            None,
            Some(ResultReason::WallLimit),
            None,
            None
        ),
        "{name}: wall limit without exit"
    );
    assert_eq!(
        AttemptRecord::of_end(&JobEnd::Memory {
            exit_code: Some(9),
            output: o.clone()
        }),
        record(
            AttemptOutcome::Failed,
            Some(9),
            Some(ResultReason::Memory),
            None,
            None
        ),
        "{name}: memory with exit"
    );
    assert_eq!(
        AttemptRecord::of_end(&JobEnd::Memory {
            exit_code: None,
            output: o.clone()
        }),
        record(
            AttemptOutcome::Failed,
            None,
            Some(ResultReason::Memory),
            None,
            None
        ),
        "{name}: memory without exit"
    );
    assert_eq!(
        AttemptRecord::of_end(&JobEnd::Cancelled {
            exit_code: Some(143),
            output: o.clone()
        }),
        record(
            AttemptOutcome::Cancelled,
            Some(143),
            Some(ResultReason::Cancelled),
            None,
            None
        ),
        "{name}: cancelled with exit"
    );
    assert_eq!(
        AttemptRecord::of_end(&JobEnd::Cancelled {
            exit_code: None,
            output: o.clone()
        }),
        record(
            AttemptOutcome::Cancelled,
            None,
            Some(ResultReason::Cancelled),
            None,
            None
        ),
        "{name}: cancelled without exit"
    );
    assert_eq!(
        AttemptRecord::of_end(&JobEnd::InputFailed {
            exit_code: Some(2),
            input_failure: "input-unavailable".to_string(),
            output: o.clone(),
        }),
        record(
            AttemptOutcome::Failed,
            Some(2),
            Some(ResultReason::InputFailed),
            None,
            None
        ),
        "{name}: input failed with exit"
    );
    assert_eq!(
        AttemptRecord::of_end(&JobEnd::InputFailed {
            exit_code: None,
            input_failure: "x".to_string(),
            output: o,
        }),
        record(
            AttemptOutcome::Failed,
            None,
            Some(ResultReason::InputFailed),
            None,
            None
        ),
        "{name}: input failed without exit"
    );
}

/// Asserts `of_end` of a lost job, every start failure and both refusals.
///
/// Catches a start failure or detail lost, a detail invented for a non-refusal, or a lost
/// host counted as a failure.
#[test]
fn of_end_lost_start_failures_and_refusals() {
    let name = "of_end_lost_start_failures_and_refusals";
    assert_eq!(
        AttemptRecord::of_end(&JobEnd::Lost),
        record(AttemptOutcome::HostLost, None, None, None, None),
        "{name}: lost"
    );
    for failure in [
        StartFailure::NoCapacity,
        StartFailure::StartTimeout,
        StartFailure::ExclusiveNotReady,
        StartFailure::AgentRefused,
        StartFailure::InputUnavailable,
    ] {
        assert_eq!(
            AttemptRecord::of_end(&JobEnd::FailedToStart(failure)),
            record(AttemptOutcome::StartFailed, None, None, Some(failure), None),
            "{name}: failed to start {failure:?}"
        );
    }
    assert_eq!(
        AttemptRecord::of_end(&JobEnd::Refused {
            failure: StartFailure::AgentRefused,
            detail: "host-tainted".to_string(),
        }),
        record(
            AttemptOutcome::StartFailed,
            None,
            None,
            Some(StartFailure::AgentRefused),
            Some("host-tainted")
        ),
        "{name}: agent refused"
    );
    assert_eq!(
        AttemptRecord::of_end(&JobEnd::Refused {
            failure: StartFailure::InputUnavailable,
            detail: "input-unavailable".to_string(),
        }),
        record(
            AttemptOutcome::StartFailed,
            None,
            None,
            Some(StartFailure::InputUnavailable),
            Some("input-unavailable")
        ),
        "{name}: input unavailable"
    );
}

/// Asserts the whole `told_by_host` table: only a succeeded or failed attempt, or a start the
/// agent refused or could not read an input for, was the host's own report.
///
/// Catches a server-decided end (start timeout, no capacity, lost host, cancel) treated as
/// told by the host, so its host is never stopped, or a host refusal treated as the server's.
#[test]
fn told_by_host_table() {
    let name = "told_by_host_table";
    for (outcome, expected) in [
        (AttemptOutcome::Succeeded, true),
        (AttemptOutcome::Failed, true),
        (AttemptOutcome::HostLost, false),
        (AttemptOutcome::Cancelled, false),
    ] {
        let attempt = record(outcome, None, None, None, None);
        assert_eq!(attempt.told_by_host(), expected, "{name}: {outcome:?}");
    }
    for (failure, expected) in [
        (None, false),
        (Some(StartFailure::NoCapacity), false),
        (Some(StartFailure::StartTimeout), false),
        (Some(StartFailure::ExclusiveNotReady), false),
        (Some(StartFailure::AgentRefused), true),
        (Some(StartFailure::InputUnavailable), true),
    ] {
        let attempt = record(AttemptOutcome::StartFailed, None, None, failure, None);
        assert_eq!(
            attempt.told_by_host(),
            expected,
            "{name}: start failed {failure:?}"
        );
    }
}

/// Asserts the default trail is the first attempt with nothing ended, and the accessors read
/// the parts `from_parts` was given.
///
/// Catches a default starting at attempt 0, or an accessor returning the wrong field.
#[test]
fn default_trail_and_accessors() {
    let name = "default_trail_and_accessors";
    let d = AttemptTrail::default();
    assert_eq!(d.number(), 1, "{name}: number");
    assert!(d.history().is_empty(), "{name}: history");
    assert_eq!(d.dropped(), 0, "{name}: dropped");
    assert_eq!(d.retry_not_before(), None, "{name}: retry_not_before");
    assert!(d.hosts().is_empty(), "{name}: hosts");
    assert_eq!(d.ended(), 0, "{name}: ended");

    let hosts: BTreeSet<HostId> = [host("h1"), host("h2")].into_iter().collect();
    let p = AttemptTrail::from_parts(4, vec![entry(3)], 2, Some(t(77)), hosts.clone());
    assert_eq!(p.number(), 4, "{name}: parts number");
    assert_eq!(p.history(), &[entry(3)][..], "{name}: parts history");
    assert_eq!(p.dropped(), 2, "{name}: parts dropped");
    assert_eq!(
        p.retry_not_before(),
        Some(t(77)),
        "{name}: parts retry_not_before"
    );
    assert_eq!(p.hosts(), &hosts, "{name}: parts hosts");
    assert_eq!(p.ended(), 3, "{name}: dropped plus kept");
}

/// Asserts `placed` leaves a fresh trail alone, bumps the number once the current attempt
/// ended, leaves it when only an earlier attempt ended, and saturates at `u32::MAX`.
///
/// Catches a first placement counted as attempt 2, a retry placement that keeps the old
/// number, a requeue within an attempt that bumps it, and an overflow at the top.
#[test]
fn placed_counts_next_attempt_only_after_end() {
    let name = "placed_counts_next_attempt_only_after_end";
    assert_eq!(
        AttemptTrail::default().placed(),
        AttemptTrail::default(),
        "{name}: fresh"
    );

    let ended_once = AttemptTrail::from_parts(1, vec![entry(1)], 0, Some(t(5)), BTreeSet::new());
    let expected = AttemptTrail::from_parts(2, vec![entry(1)], 0, Some(t(5)), BTreeSet::new());
    assert_eq!(ended_once.placed(), expected, "{name}: after retry");

    let inside = AttemptTrail::from_parts(2, vec![entry(1)], 0, Some(t(5)), BTreeSet::new());
    assert_eq!(inside.clone().placed(), inside, "{name}: inside attempt 2");

    let dropped = AttemptTrail::from_parts(5, vec![entry(3), entry(4)], 3, None, BTreeSet::new());
    assert_eq!(dropped.placed().number(), 6, "{name}: dropped counted");

    let top = AttemptTrail::from_parts(u32::MAX, Vec::new(), u32::MAX, None, BTreeSet::new());
    assert_eq!(top.ended(), u64::from(u32::MAX), "{name}: top ended");
    assert_eq!(top.placed().number(), u32::MAX, "{name}: saturates");
}

/// Asserts `closed` on a first attempt appends the entry with the job's submit as queued_at,
/// every field exact, and adds the host.
///
/// Catches a wrong queued_at for attempt 1, a lost started_at, a host not added to the hosts,
/// or the retried flag dropped.
#[test]
fn closed_first_attempt_entry() {
    let name = "closed_first_attempt_entry";
    let j = job(Some("h-1"), Some(t(20)), AttemptTrail::default());
    let closed = AttemptTrail::closed(&j, simple_end(), t(40), false);
    let want = Attempt {
        number: 1,
        host: host("h-1"),
        queued_at: t(10),
        placed_at: t(20),
        started_at: Some(t(25)),
        ended_at: t(40),
        end: simple_end(),
        retried: false,
        carried: Carried::default(),
    };
    let hosts: BTreeSet<HostId> = [host("h-1")].into_iter().collect();
    assert_eq!(
        closed,
        AttemptTrail::from_parts(1, vec![want], 0, None, hosts),
        "{name}"
    );
}

/// Asserts `closed` of a retried second attempt uses `retry_not_before` as queued_at, never
/// ran (no started_at), keeps earlier hosts, and flags the entry retried.
///
/// Catches queued_at taken from the submit after a retry, hosts replaced rather than added to,
/// or the number taken from the wrong place.
#[test]
fn closed_later_attempt_entry() {
    let name = "closed_later_attempt_entry";
    let first = entry(1);
    let hosts: BTreeSet<HostId> = [host("h-old")].into_iter().collect();
    let before = AttemptTrail::from_parts(2, vec![first.clone()], 0, Some(t(30)), hosts);
    let mut j = job(Some("h-2"), Some(t(35)), before);
    j.started_at = None;
    let closed = AttemptTrail::closed(&j, simple_end(), t(50), true);
    let second = Attempt {
        number: 2,
        host: host("h-2"),
        queued_at: t(30),
        placed_at: t(35),
        started_at: None,
        ended_at: t(50),
        end: simple_end(),
        retried: true,
        carried: Carried::default(),
    };
    let both: BTreeSet<HostId> = [host("h-old"), host("h-2")].into_iter().collect();
    assert_eq!(
        closed,
        AttemptTrail::from_parts(2, vec![first, second], 0, Some(t(30)), both),
        "{name}"
    );
}

/// Asserts `closed` returns the trail unchanged when the job holds no host, or holds a host
/// but no placement time.
///
/// Catches a queued job's end appending an attempt that ran nowhere, on either missing half.
#[test]
fn closed_without_host_or_placement_changes_nothing() {
    let name = "closed_without_host_or_placement_changes_nothing";
    let base = AttemptTrail::from_parts(
        2,
        vec![entry(1)],
        1,
        Some(t(9)),
        [host("h-old")].into_iter().collect(),
    );
    let queued = job(None, None, base.clone());
    assert_eq!(
        AttemptTrail::closed(&queued, simple_end(), t(40), false),
        base,
        "{name}: no host"
    );
    let no_time = job(Some("h-1"), None, base.clone());
    assert_eq!(
        AttemptTrail::closed(&no_time, simple_end(), t(40), false),
        base,
        "{name}: no time"
    );
    let no_host = job(None, Some(t(20)), base.clone());
    assert_eq!(
        AttemptTrail::closed(&no_host, simple_end(), t(40), false),
        base,
        "{name}: no host, time"
    );
}

/// Asserts `closed` applies the history bound: with 256 kept, one more appended drops the
/// oldest and counts it.
///
/// Catches a history growing past the bound, or a dropped attempt not counted.
#[test]
fn closed_applies_the_bound() {
    let name = "closed_applies_the_bound";
    let kept: Vec<Attempt> = (1..=256).map(entry).collect();
    let before = AttemptTrail::from_parts(257, kept, 4, Some(t(30)), BTreeSet::new());
    let j = job(Some("h-1"), Some(t(35)), before);
    let closed = AttemptTrail::closed(&j, simple_end(), t(50), true);
    assert_eq!(
        closed.history().len(),
        MAX_ATTEMPT_HISTORY,
        "{name}: length"
    );
    assert_eq!(closed.history()[0].number, 2, "{name}: oldest");
    assert_eq!(closed.history()[255].number, 257, "{name}: newest");
    assert_eq!(closed.dropped(), 5, "{name}: dropped");
    assert_eq!(closed.ended(), 261, "{name}: ended");
}

/// Asserts `waiting` sets only `retry_not_before`.
///
/// Catches a waiting trail that resets the history, number or hosts, or sets no time.
#[test]
fn waiting_sets_only_not_before() {
    let name = "waiting_sets_only_not_before";
    let hosts: BTreeSet<HostId> = [host("h-old")].into_iter().collect();
    let base = AttemptTrail::from_parts(1, vec![entry(1)], 2, Some(t(1)), hosts.clone());
    let waiting = base.waiting(t(99));
    assert_eq!(
        waiting,
        AttemptTrail::from_parts(1, vec![entry(1)], 2, Some(t(99)), hosts),
        "{name}"
    );
}

/// Asserts `from_parts` keeps the newest 256 of 300 entries and counts the 44 over, adding to
/// a dropped count it was given; an exactly full history drops nothing.
///
/// Catches keeping the oldest rather than the newest, an off-by-one at the bound, or a given
/// dropped count overwritten.
#[test]
fn from_parts_bounds_the_history() {
    let name = "from_parts_bounds_the_history";
    let many: Vec<Attempt> = (1..=300).map(entry).collect();
    let bounded = AttemptTrail::from_parts(301, many, 6, None, BTreeSet::new());
    assert_eq!(bounded.history().len(), 256, "{name}: kept");
    assert_eq!(bounded.history()[0].number, 45, "{name}: oldest kept");
    assert_eq!(bounded.history()[255].number, 300, "{name}: newest kept");
    assert_eq!(bounded.dropped(), 50, "{name}: dropped");
    assert_eq!(bounded.ended(), 306, "{name}: ended");

    let full: Vec<Attempt> = (1..=256).map(entry).collect();
    let exact = AttemptTrail::from_parts(257, full.clone(), 0, None, BTreeSet::new());
    assert_eq!(exact.history(), &full[..], "{name}: exact");
    assert_eq!(exact.dropped(), 0, "{name}: exact dropped");
}

/// Asserts the dropped count saturates at `u32::MAX` rather than wrapping.
///
/// Catches a wrapping or panicking add when a trail already at the top drops more.
#[test]
fn dropped_saturates() {
    let name = "dropped_saturates";
    let many: Vec<Attempt> = (1..=257).map(entry).collect();
    let trail = AttemptTrail::from_parts(258, many, u32::MAX, None, BTreeSet::new());
    assert_eq!(trail.dropped(), u32::MAX, "{name}");
    assert_eq!(trail.history().len(), 256, "{name}: kept");
}

/// Asserts the bar a retry puts on its host against each bar the host may already have: none,
/// a back-off and an earlier end give way to the retry's end; a bar for life, an end as late
/// and a later end stay; other hosts' bars are kept. Catches a retry that shortens a bar the
/// job already had on its host to the retry's 15 minutes.
#[test]
fn a_retry_bar_never_shortens_a_longer_one() {
    let host = HostId::new("host-a").expect("a host");
    let other = HostId::new("host-b").expect("a host");
    let until = StoreTime::from_duration(Duration::from_secs(1000));
    let earlier = StoreTime::from_duration(Duration::from_secs(999));
    let later = StoreTime::from_duration(Duration::from_secs(1001));
    let cases = [
        (None, Exclusion::Until(until)),
        (Some(Exclusion::Backoff(later)), Exclusion::Until(until)),
        (Some(Exclusion::Until(earlier)), Exclusion::Until(until)),
        (Some(Exclusion::Until(until)), Exclusion::Until(until)),
        (Some(Exclusion::Until(later)), Exclusion::Until(later)),
        (Some(Exclusion::ForLife), Exclusion::ForLife),
    ];
    for (had, want) in cases {
        let mut bars = BTreeMap::from([(other.clone(), Exclusion::ForLife)]);
        bars.extend(had.map(|bar| (host.clone(), bar)));
        let barred = retry_barred(bars, &host, until);
        assert_eq!(
            barred,
            BTreeMap::from([(other.clone(), Exclusion::ForLife), (host.clone(), want)]),
            "{had:?}"
        );
    }
}

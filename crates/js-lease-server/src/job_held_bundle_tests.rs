//! A start a host refused while it fetches the bundle the job names (`bundle-pending`), served
//! through the per-job half of a heartbeat on the job tables alone: the job goes back to the
//! queue with no failure counted and the host barred for [`BUNDLE_PENDING_BAR`], however many
//! times it is refused and whatever other refusals counted; a cancelled job's refusal, and each
//! bundle refusal that asking again cannot change, end the job as before.

use super::*;
use crate::job_held::BUNDLE_PENDING_BAR;

/// The codes of a bundle start that end the job as before: the fetch failed or its bundle is
/// not one the host may run, or the host cannot run a bundle at all. Each is a refusal a later
/// start of the job on the same host is refused again, or one the agent answers once for a
/// fetch that ended.
const HARD_BUNDLE_CODES: [&str; 11] = [
    "bundle-fetch-failed",
    "bundle-not-allowed",
    "bundle-denied",
    "bundle-registry-not-allowed",
    "bundle-not-held",
    "bundle-no-space",
    "bundle-os-too-old",
    "bundle-unsupported",
    "bundle-index",
    "invalid-bundle",
    "no-full-disk-access",
];

/// A start refused `bundle-pending`, of a job placed on a share of the host, of the same job
/// once it is starting, and of a whole-host job once its host was ready, is answered `Accepted`
/// and sends the job back to the queue in one commit: no host and no placement time, the host in
/// neither its held nor its capacity refusals and barred for [`BUNDLE_PENDING_BAR`] from the
/// store's time, a `REQUEUED` event naming the host and the job waiting again as it waited before
/// it was placed, with the fetch in its detail; its allocation is freed with `Refused`. No start
/// failure is told, no failed prepare counted and no start failure detail stored, the refused
/// start is told once with its host and code, and the reply sends the host neither a stop nor a
/// start of the job. Catches a bundle-pending refusal that fails the job (as before), that counts
/// it as a failure, that leaves its allocation booking the host, that does not keep the job off
/// the host and that is written as a held or a capacity refusal.
#[test]
fn a_start_refused_bundle_pending_sends_the_job_back_to_the_queue_and_counts_no_failure() {
    let cases = [
        (
            "share-placed",
            spec(),
            BOOKED,
            false,
            QueueCode::WaitingForCapacity,
        ),
        (
            "share-starting",
            spec(),
            BOOKED,
            true,
            QueueCode::WaitingForCapacity,
        ),
        (
            "whole-starting",
            whole_spec(),
            Booked::WholeHost,
            true,
            QueueCode::WaitingForWholeHost,
        ),
    ];
    for (name, spec, booked, starting, waits) in cases {
        let (store, jobs) = tables();
        let (placed_job, booking) = placed(&jobs, name, "host-a", spec, booked);
        if starting {
            jobs.transition(&placed_job.id, JobState::Placed, JobState::Starting)
                .unwrap();
        }
        let now = jobs.now().unwrap();
        let failures = Failures::default();
        let commits = store.commit_stats().txns;
        let listed = refusal(&booking, "bundle-pending");
        let reply = beat(&jobs, &booking, listed, &failures).unwrap();
        assert_eq!(
            store.commit_stats().txns,
            commits + 1,
            "{name}: the requeue took other than one commit"
        );
        assert_eq!(reply.job_acks[0].ack, Ack::Accepted, "{name}");
        assert_eq!(
            reply.commands,
            Vec::<Command>::new(),
            "{name}: a stop or a start"
        );
        let requeued = job(&jobs, &placed_job.id);
        assert_eq!(
            (requeued.state, requeued.host.clone(), requeued.placed_at),
            (JobState::Queued, None, None),
            "{name}: the job did not go back to the queue"
        );
        assert_eq!(
            (
                requeued.held_refusals.clone(),
                requeued.capacity_refusals.clone()
            ),
            (Vec::new(), Vec::new()),
            "{name}: the refusal was written as held or for capacity"
        );
        assert_eq!(
            requeued.prepare_bars.get(&host("host-a")),
            Some(&Exclusion::Until(now.saturating_add(BUNDLE_PENDING_BAR))),
            "{name}: the host is not barred for the bundle-pending bar"
        );
        assert_eq!(
            (
                requeued.failed_prepares,
                requeued.start_failure_detail.clone()
            ),
            (0, None),
            "{name}: a failure was counted or a detail stored"
        );
        let event = requeued.queue.events().last().cloned().unwrap();
        assert_eq!(
            (event.kind, event.host, requeued.queue.code()),
            (JobEventKind::Requeued, Some(host("host-a")), waits),
            "{name}: the queue status does not say why the job waits again"
        );
        assert_eq!(
            requeued.queue.detail(),
            "host host-a is still fetching the job's bundle and refused the start; the job is queued again",
            "{name}"
        );
        let freed = allocation(&jobs, &placed_job.id);
        assert_eq!(
            (freed.step, freed.end_reason),
            (AllocationStep::Freed, Some(EndReason::Refused)),
            "{name}: the allocation was not freed as refused"
        );
        assert_eq!(failures.told(), [], "{name}: a start failure was told");
        assert_eq!(
            failures.refused_told(),
            [("host-a".to_owned(), "bundle-pending".to_owned())],
            "{name}: the refused start was not told once with its host and code"
        );
    }
}

/// Bundle-pending refusals are not counted: a job refused `bundle-pending` on the same host more
/// times than the held and the capacity bounds together goes back to the queue every time, and
/// a job that used up its held and its capacity requeues is still sent back by a bundle-pending
/// refusal, while the next held refusal still ends it naming the hosts that refused it as held.
/// Catches a bound on bundle-pending requeues (a fetch longer than a few bars failing the job), a
/// bundle-pending refusal counted against the held or the capacity bound, and one that lifts
/// those bounds.
#[test]
fn bundle_pending_refusals_are_not_counted_against_any_bound() {
    let failures = Failures::default();

    let (_store, jobs) = tables();
    let (pending_job, mut booking) = placed(&jobs, "pending-often", "host-a", spec(), BOOKED);
    for _ in 0..MAX_HELD_REQUEUES + MAX_CAPACITY_REQUEUES + 1 {
        booking = refused_then_placed(&jobs, &booking, "bundle-pending", "host-a", &failures);
    }
    let still = job(&jobs, &pending_job.id);
    assert_eq!(still.state, JobState::Placed, "the job was ended");
    assert_eq!(
        (still.held_refusals, still.capacity_refusals),
        (Vec::new(), Vec::new())
    );

    let (_store, jobs) = tables();
    let (used_up, mut booking) = placed(&jobs, "bounds-used", "host-a", spec(), BOOKED);
    for _ in 0..MAX_CAPACITY_REQUEUES {
        booking = refused_then_placed(&jobs, &booking, "no-capacity", "host-a", &failures);
    }
    for next in ["host-b", "host-c", "host-d"] {
        booking = refused_then_placed(&jobs, &booking, "host-held", next, &failures);
    }
    booking = refused_then_placed(&jobs, &booking, "bundle-pending", "host-e", &failures);
    assert_eq!(
        refused(&jobs, &booking, "host-held", &failures),
        Ack::Accepted
    );
    let ended = job(&jobs, &used_up.id);
    assert_eq!(
        (ended.state, ended.start_failure_detail.as_deref()),
        (
            JobState::FailedToStart(StartFailure::AgentRefused),
            Some("host-held: host-a, host-b, host-c, host-e")
        ),
        "a bundle-pending refusal lifted the held bound or was named as held"
    );
    assert_eq!(failures.told(), [StartFailure::AgentRefused]);
}

/// A start refused `bundle-pending` of a job whose cancel was asked for ends it as before
/// (failed to start, with the code, told once) and bars no host. Catches a cancelled job sent
/// back to the queue.
#[test]
fn a_bundle_pending_refusal_of_a_cancelled_job_ends_it_as_before() {
    let (_store, jobs) = tables();
    let failures = Failures::default();
    let (cancelled, booking) = placed(&jobs, "cancelled", "host-a", spec(), BOOKED);
    jobs.request_cancel(&cancelled.id).unwrap();
    assert_eq!(
        refused(&jobs, &booking, "bundle-pending", &failures),
        Ack::Accepted
    );
    let ended = job(&jobs, &cancelled.id);
    assert_eq!(
        (ended.state, ended.start_failure_detail.as_deref()),
        (
            JobState::FailedToStart(StartFailure::AgentRefused),
            Some("bundle-pending")
        )
    );
    assert_eq!(ended.prepare_bars.get(&host("host-a")), None);
    assert_eq!(failures.told(), [StartFailure::AgentRefused]);
}

/// A start refused with each bundle code that asking again cannot change (the fetch failed, the
/// bundle is not one the host may run, the host has no bundle store or no Full Disk Access)
/// still ends the job at once, failed to start as the agent's refusal with the agent's code as
/// its detail, told once as a start failure and as a refused start: it is not requeued and bars
/// no host. Catches a requeue that takes in every bundle refusal (a job that cannot run kept
/// waiting until its queue wait ends), and a code lost from the detail.
#[test]
fn a_start_refused_for_a_hard_bundle_reason_still_fails_the_job() {
    for code in HARD_BUNDLE_CODES {
        let (_store, jobs) = tables();
        let failures = Failures::default();
        let (refused_job, booking) = placed(&jobs, code, "host-a", spec(), BOOKED);
        assert_eq!(refused(&jobs, &booking, code, &failures), Ack::Accepted);
        let ended = job(&jobs, &refused_job.id);
        assert_eq!(
            (ended.state, ended.start_failure_detail.as_deref()),
            (
                JobState::FailedToStart(StartFailure::AgentRefused),
                Some(code)
            ),
            "{code}: the job did not fail with the agent's code"
        );
        assert_eq!(
            ended.prepare_bars.get(&host("host-a")),
            None,
            "{code}: a host was barred"
        );
        assert_eq!(failures.told(), [StartFailure::AgentRefused], "{code}");
        assert_eq!(
            failures.refused_told(),
            [("host-a".to_owned(), code.to_owned())],
            "{code}"
        );
    }
}

//! The dispatch queue ([`crate::JobStore::claim_dispatch`], [`crate::JobStore::ack_dispatch`]):
//! two servers never hand out one message at once, a claim that lapses is handed out again by
//! the store's clock, an acknowledgement deletes what was handed out and nothing else, the
//! placement that books a host queues its start in its own transaction, and an end leaves a stop
//! exactly where the start may have reached the host and no start behind.

use std::collections::BTreeSet;
use std::sync::{Arc, Barrier, Mutex};
use std::time::Duration;

use js_state::conformance::Backend;
use js_state::{Clock, Store, StoreError};
use js_types::cordon::Asker;

use super::whole::{engine_placement, placed_whole, requeue_with};
use super::{Rig, cancelled, exited, host, output, rig, share, start_body};
use crate::codec::dispatch_fields::encode_dispatch;
use crate::codec::{LATER_FORMAT, field_of, set_field};
use crate::interfere::INJECTED;
use crate::keys;
use crate::{
    AllocationId, Booked, Capacity, DispatchKind, DispatchMessage, EndReason, FailedPrepare,
    JobEnd, JobId, JobState, JobStoreError, Placement, StartFailure,
};

/// How long a claim holds a message in these checks.
const LEASE: Duration = Duration::from_secs(10);

/// The messages queued for `on`, as the store reads them.
fn queue<S: Store>(rig: &Rig<S>, on: &str) -> Vec<DispatchMessage> {
    rig.jobs.dispatch_queue(&host(on)).expect("reads the queue")
}

/// The messages queued on `host-a` for the job `id`: each one's kind and whether it was handed
/// out.
fn of_job<S: Store>(rig: &Rig<S>, id: &JobId) -> Vec<(DispatchKind, bool)> {
    let own = queue(rig, "host-a").into_iter().filter(|m| m.job == *id);
    own.map(|m| (m.kind, m.claimed_by.is_some())).collect()
}

/// The allocations of `messages`.
fn allocations(messages: &[DispatchMessage]) -> BTreeSet<AllocationId> {
    messages.iter().map(|m| m.allocation_id).collect()
}

/// Places the job `text` on `on` with a share and returns its id and allocation id.
fn placed_on<S: Store>(rig: &Rig<S>, text: &str, on: &str) -> (JobId, AllocationId) {
    let id = rig.queue(text);
    let (_, allocation) = rig
        .jobs
        .place(&id, host(on), share())
        .expect("a queued job is placed");
    (id, allocation.allocation_id)
}

/// Hands out every due message of `host-a` as `server-1`.
fn claim_all<S: Store>(rig: &Rig<S>) -> Vec<DispatchMessage> {
    let a = host("host-a");
    rig.jobs
        .claim_dispatch(&a, "server-1", LEASE)
        .expect("claims")
}

/// Two servers that claim one host's messages at the same moment (two threads let go by one
/// barrier) hand out each message to exactly one of them, and every message to one; and a claim
/// whose messages another server takes between its read of the queue and its own claims hands
/// out none of them, since each claim reads its message again on the locked row. Catches a
/// message handed out twice at once, and a claim decided on the queue as first read.
pub fn two_servers_claiming_one_host_at_once_hand_out_each_message_once<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "two_servers_claiming_one_host_at_once_hand_out_each_message_once";
    let rig = rig(backend);
    let placed: BTreeSet<AllocationId> = (0..6)
        .map(|n| placed_on(&rig, &format!("race-{n}"), "host-a").1)
        .collect();
    let other = rig.other();
    let barrier = Barrier::new(2);
    let a = host("host-a");
    let (first, second) = std::thread::scope(|scope| {
        let one = scope.spawn(|| {
            barrier.wait();
            rig.jobs.claim_dispatch(&a, "server-1", LEASE)
        });
        let two = scope.spawn(|| {
            barrier.wait();
            other.claim_dispatch(&a, "server-2", LEASE)
        });
        let joined = (one.join(), two.join());
        (
            joined.0.expect("joins").expect("claims"),
            joined.1.expect("joins").expect("claims"),
        )
    });
    let (mine, theirs) = (allocations(&first), allocations(&second));
    assert_eq!(
        first.len() + second.len(),
        placed.len(),
        "{name}: {first:?} {second:?}"
    );
    assert!(mine.is_disjoint(&theirs), "{name}: a message went to both");
    let both: BTreeSet<AllocationId> = mine.union(&theirs).copied().collect();
    assert_eq!(both, placed, "{name}: a message went to neither");
    for (claimed, by) in [(&first, "server-1"), (&second, "server-2")] {
        for message in claimed {
            assert_eq!(message.claimed_by.as_deref(), Some(by), "{name}");
        }
    }
    let stored = queue(&rig, "host-a");
    let held = |by: &str| {
        stored
            .iter()
            .filter(|m| m.claimed_by.as_deref() == Some(by))
            .count()
    };
    assert_eq!(
        (held("server-1"), held("server-2")),
        (first.len(), second.len()),
        "{name}"
    );

    let fresh: BTreeSet<AllocationId> = (0..2)
        .map(|n| placed_on(&rig, &format!("late-{n}"), "host-a").1)
        .collect();
    let taken = Arc::new(Mutex::new(Vec::new()));
    let (by_hook, sniper) = (Arc::clone(&taken), rig.other());
    rig.store.before_next(move || {
        let claimed = sniper.claim_dispatch(&host("host-a"), "server-2", LEASE);
        *by_hook.lock().expect("unpoisoned") = claimed.expect("claims");
    });
    let late = rig
        .jobs
        .claim_dispatch(&a, "server-1", LEASE)
        .expect("claims");
    assert_eq!(
        late,
        Vec::new(),
        "{name}: messages another server took were handed out again"
    );
    let taken = taken.lock().expect("unpoisoned").clone();
    assert_eq!(
        allocations(&taken),
        fresh,
        "{name}: the other server took the new messages"
    );
}

/// A message a server claimed and never acknowledged (it died) is not handed out again while the
/// store's clock reads at or before its `claim_until`, and is handed out to another server once
/// the clock passes it, with that server's name and a new hold. Catches a claim that ignores the
/// hold, one that never lets it lapse, and a lapse judged on another clock.
pub fn a_message_whose_claim_lapses_is_handed_out_again_by_the_store_clock<B: Backend>(
    backend: &B,
) {
    let name = "a_message_whose_claim_lapses_is_handed_out_again_by_the_store_clock";
    let rig = rig(backend);
    let a = host("host-a");
    let (_, allocation) = placed_on(&rig, "lapse", "host-a");
    let at = rig.clock.now();
    let first = claim_all(&rig);
    assert_eq!(allocations(&first), BTreeSet::from([allocation]), "{name}");
    assert_eq!(
        first[0].claim_until,
        Some(at.saturating_add(LEASE)),
        "{name}"
    );
    let other = rig.other();
    let again = |by: &str| other.claim_dispatch(&a, by, LEASE).expect("claims");
    assert_eq!(
        again("server-2"),
        Vec::new(),
        "{name}: handed out while held"
    );
    let until = first[0].claim_until.expect("held");
    rig.clock.advance(until.since(rig.clock.now()));
    assert_eq!(rig.clock.now(), until, "{name}");
    assert_eq!(
        again("server-2"),
        Vec::new(),
        "{name}: handed out at its claim_until"
    );
    rig.clock.advance(Duration::from_nanos(1));
    let now = rig.clock.now();
    let retaken = again("server-2");
    assert_eq!(
        retaken.len(),
        1,
        "{name}: a lapsed claim was not handed out again"
    );
    assert_eq!(retaken[0].claimed_by.as_deref(), Some("server-2"), "{name}");
    assert_eq!(
        retaken[0].claim_until,
        Some(now.saturating_add(LEASE)),
        "{name}"
    );
    assert_eq!(queue(&rig, "host-a"), retaken, "{name}: the stored message");
    assert_eq!(
        claim_all(&rig),
        Vec::new(),
        "{name}: the new hold is not kept"
    );
}

/// An acknowledgement deletes the named messages (allocation and kind) that were handed out, in
/// one commit: a second acknowledgement, one of an unknown allocation, one on another host and an
/// empty one change nothing and write nothing; a message never handed out is kept, and so is a
/// stop queued after its start was handed out, whether or not a server has handed the stop out,
/// when the agent acknowledges that start. Catches an acknowledgement that deletes what the agent
/// never saw, and one that is not idempotent.
pub fn an_ack_deletes_only_what_was_handed_out_and_a_repeat_changes_nothing<B: Backend>(
    backend: &B,
) {
    let name = "an_ack_deletes_only_what_was_handed_out_and_a_repeat_changes_nothing";
    let rig = rig(backend);
    let a = host("host-a");
    let start = |allocation| (allocation, DispatchKind::Start);
    let (_, one) = placed_on(&rig, "ack-a", "host-a");
    let (_, two) = placed_on(&rig, "ack-b", "host-a");
    assert_eq!(claim_all(&rig).len(), 2, "{name}");
    let commits = rig.store.commit_stats().txns;
    assert_eq!(
        rig.jobs.ack_dispatch(&a, &[start(one), start(one)]),
        Ok(1),
        "{name}"
    );
    assert_eq!(
        rig.store.commit_stats().txns,
        commits + 1,
        "{name}: one commit"
    );
    assert_eq!(
        allocations(&queue(&rig, "host-a")),
        BTreeSet::from([two]),
        "{name}"
    );

    let (_, three) = placed_on(&rig, "ack-c", "host-a");
    let before = rig.snapshot();
    let unknown = AllocationId::from_bits([9; 16]);
    for (on, named) in [
        (&a, vec![start(one)]),
        (&a, vec![start(unknown)]),
        (&a, vec![start(three)]),
        (&a, vec![(two, DispatchKind::Stop)]),
        (&host("host-b"), vec![start(two)]),
        (&a, Vec::new()),
    ] {
        assert_eq!(
            rig.jobs.ack_dispatch(on, &named),
            Ok(0),
            "{name}: {named:?}"
        );
    }
    assert_eq!(rig.snapshot(), before, "{name}: an ack of nothing wrote");
    assert_eq!(
        rig.jobs.ack_dispatch(&a, &[start(two), start(three)]),
        Ok(1),
        "{name}"
    );
    assert_eq!(
        allocations(&queue(&rig, "host-a")),
        BTreeSet::from([three]),
        "{name}"
    );

    let (id, four) = placed_on(&rig, "ack-d", "host-a");
    claim_all(&rig);
    rig.jobs.request_cancel(&id).expect("asks for the cancel");
    let stop = (four, DispatchKind::Stop);
    assert_eq!(
        rig.jobs.ack_dispatch(&a, &[start(four), stop]),
        Ok(0),
        "{name}: a stop not yet sent"
    );
    assert_eq!(of_job(&rig, &id), [(DispatchKind::Stop, false)], "{name}");
    assert_eq!(claim_all(&rig).len(), 1, "{name}: the stop is handed out");
    assert_eq!(
        rig.jobs.ack_dispatch(&a, &[start(four)]),
        Ok(0),
        "{name}: the start's ack deleted a stop handed out since"
    );
    assert_eq!(rig.jobs.ack_dispatch(&a, &[stop]), Ok(1), "{name}");
    assert_eq!(of_job(&rig, &id), [], "{name}");
}

/// The capacity of one [`share`].
fn one_share() -> Capacity {
    Capacity {
        cpu_millis: 2000,
        memory_bytes: 1 << 30,
        disk_bytes: 1 << 32,
        accelerators: 1,
    }
}

/// Every placement that books a host for a job that starts on its placement queues one start in
/// its own commit: for its job and allocation, at the placement's time, carrying the start body
/// it was given with the placement's own job id, allocation id and time; a whole-host placement
/// queues none. A placement that commits nothing (no room, a cordon, a job another server placed
/// first) queues nothing. Catches a start written outside the placement's transaction, one a
/// refused placement leaves, and a body that names another placement.
pub fn every_placement_that_books_a_host_queues_its_start_and_a_refused_one_none<B: Backend>(
    backend: &B,
) where
    B::Store: 'static,
{
    let name = "every_placement_that_books_a_host_queues_its_start_and_a_refused_one_none";
    let rig = rig(backend);
    let (id, allocation) = placed_on(&rig, "q-plain", "host-a");
    let job = rig.jobs.get_job(&id).expect("reads").expect("a job");
    let queued = queue(&rig, "host-a");
    let expected = DispatchMessage {
        kind: DispatchKind::Start,
        job: id.clone(),
        allocation_id: allocation,
        host: host("host-a"),
        queued_at: job.placed_at.expect("placed"),
        start: None,
        claimed_by: None,
        claim_until: None,
        carried: Default::default(),
    };
    assert_eq!(queued, [expected], "{name}");

    let given = Placement {
        start: Some(start_body("someone-else", "not-this-one")),
        ..Placement::default()
    };
    let with = rig.queue("q-with");
    let (placed, made) = rig
        .jobs
        .place_with(&with, host("host-b"), share(), given.clone())
        .expect("placed");
    let claimed = rig.queue("q-claimed");
    rig.tick();
    let (claimed_job, claimed_made) = rig
        .jobs
        .place_claimed(&claimed, &host("host-b"), share(), &given, None)
        .expect("placed");
    let on_b = queue(&rig, "host-b");
    assert_eq!(on_b.len(), 2, "{name}: {on_b:?}");
    for (message, (job, allocation)) in on_b
        .iter()
        .zip([(placed, made), (claimed_job, claimed_made)])
    {
        let at = job.placed_at.expect("placed");
        let millis = u64::try_from(at.as_duration().as_millis()).expect("small");
        let body = start_body(job.id.as_str(), &allocation.allocation_id.to_string());
        let body = js_agent_wire::JobStartSpec {
            placed_at_ms: Some(millis),
            ..body
        };
        assert_eq!((&message.job, message.queued_at), (&job.id, at), "{name}");
        assert_eq!(message.start, Some(body), "{name}: the start body");
    }

    let whole = rig.queue_spec("q-whole", super::whole_host_spec());
    rig.jobs
        .place_claimed(&whole, &host("host-c"), Booked::WholeHost, &given, None)
        .expect("placed");
    assert_eq!(
        queue(&rig, "host-c"),
        [],
        "{name}: a whole-host placement queued a start"
    );

    let d = host("host-d");
    let fits = rig.queue("q-fits");
    let full = rig.queue("q-full");
    let room = Some(one_share());
    rig.jobs
        .place_claimed(&fits, &d, share(), &given, room.as_ref())
        .expect("placed");
    let refused = rig
        .jobs
        .place_claimed(&full, &d, share(), &given, room.as_ref());
    assert!(
        matches!(refused, Err(JobStoreError::HostFull { .. })),
        "{name}: {refused:?}"
    );
    let on_d: Vec<JobId> = queue(&rig, "host-d").into_iter().map(|m| m.job).collect();
    assert_eq!(
        on_d,
        [fits],
        "{name}: a placement with no room queued a start"
    );

    let e = host("host-e");
    rig.jobs
        .cordon_host(&e, Duration::from_secs(600), &Asker::Unnamed)
        .expect("cordons");
    let fenced = rig.queue("q-fenced");
    let refused = rig.jobs.place_claimed(&fenced, &e, share(), &given, None);
    assert!(
        matches!(refused, Err(JobStoreError::HostCordoned { .. })),
        "{name}"
    );
    let refused = rig.jobs.place(&fenced, e.clone(), share());
    assert!(
        matches!(refused, Err(JobStoreError::Stale { .. })),
        "{name}: {refused:?}"
    );
    assert_eq!(
        queue(&rig, "host-e"),
        [],
        "{name}: a cordoned host was queued a start"
    );

    let raced = rig.queue("q-raced");
    let (other, first) = (rig.other(), raced.clone());
    rig.store.before_next(move || {
        other
            .place_claimed(
                &first,
                &host("host-f"),
                share(),
                &Placement::default(),
                None,
            )
            .expect("the other server places the job first");
    });
    let lost = rig
        .jobs
        .place_claimed(&raced, &host("host-g"), share(), &given, None);
    assert!(
        matches!(lost, Err(JobStoreError::UnexpectedState { .. })),
        "{name}"
    );
    assert_eq!(
        queue(&rig, "host-g"),
        [],
        "{name}: the loser of a race queued a start"
    );
    assert_eq!(queue(&rig, "host-f").len(), 1, "{name}: the winner's start");
}

/// A whole-host job's start is queued by the move from placed to starting, in that commit, once:
/// none at its placement, one after the move, still one after the move to running; a job of a
/// share is not queued a second start by its moves. Catches a whole-host start sent before its
/// host is ready, and one never sent.
pub fn a_whole_host_job_queues_its_start_when_it_moves_to_starting<B: Backend>(backend: &B) {
    let name = "a_whole_host_job_queues_its_start_when_it_moves_to_starting";
    let rig = rig(backend);
    let (id, _, allocation) = placed_whole(&rig, "w-start");
    assert_eq!(
        of_job(&rig, &id),
        [],
        "{name}: queued before its host is ready"
    );
    rig.tick();
    let (commits, at) = (rig.store.commit_stats().txns, rig.clock.now());
    rig.jobs
        .transition(&id, JobState::Placed, JobState::Starting)
        .expect("starts");
    assert_eq!(
        rig.store.commit_stats().txns,
        commits + 1,
        "{name}: one commit"
    );
    let queued = queue(&rig, "host-a");
    assert_eq!(queued.len(), 1, "{name}: {queued:?}");
    assert_eq!(queued[0].allocation_id, allocation.allocation_id, "{name}");
    assert_eq!(queued[0].queued_at, at, "{name}: queued at the move");
    rig.jobs
        .transition(&id, JobState::Starting, JobState::Running)
        .expect("runs");
    assert_eq!(of_job(&rig, &id), [(DispatchKind::Start, false)], "{name}");

    let shared = rig.running("w-share");
    assert_eq!(
        of_job(&rig, &shared),
        [(DispatchKind::Start, false)],
        "{name}"
    );
}

/// Each way a job ends, or its cancel is asked for, leaves its host's queue as the start's fate
/// says: a start never handed out is deleted and nothing is sent; a start handed out, one of a job
/// the host reports running (the heartbeat's reply carried it), or one acknowledged (or never
/// queued, by an earlier release) for a job whose start was due, is replaced by one stop; an end the host reported sends none; a cancel already decided sends no
/// second stop; a whole-host job not yet starting sends none. Catches an orphan start, a stop
/// for a job that never reached its host or that the host already ended, and two stops.
pub fn an_end_leaves_a_stop_only_where_the_start_may_have_reached_the_host<B: Backend>(
    backend: &B,
) {
    let name = "an_end_leaves_a_stop_only_where_the_start_may_have_reached_the_host";
    let rig = rig(backend);
    let a = host("host-a");
    let stop = [(DispatchKind::Stop, false)];
    let ack = |allocation| {
        let start = (allocation, DispatchKind::Start);
        rig.jobs.ack_dispatch(&a, &[start]).expect("acks")
    };

    let (never, _) = placed_on(&rig, "e-never", "host-a");
    rig.jobs.request_cancel(&never).expect("asks");
    assert_eq!(
        of_job(&rig, &never),
        [],
        "{name}: cancel of a start never sent"
    );
    rig.jobs
        .end_and_free(&never, cancelled(), EndReason::Cancelled)
        .expect("ends");
    assert_eq!(
        of_job(&rig, &never),
        [],
        "{name}: the end after a decided cancel"
    );

    let (sent, _) = placed_on(&rig, "e-sent", "host-a");
    claim_all(&rig);
    rig.jobs.request_cancel(&sent).expect("asks");
    assert_eq!(
        of_job(&rig, &sent),
        stop,
        "{name}: cancel of a start handed out"
    );
    let reported = JobEnd::Cancelled {
        exit_code: Some(143),
        output: output(),
    };
    rig.jobs.end_job(&sent, reported).expect("ends");
    assert_eq!(of_job(&rig, &sent), stop, "{name}: a second stop");

    let (acked, allocation) = placed_on(&rig, "e-acked", "host-a");
    claim_all(&rig);
    ack(allocation);
    rig.jobs.request_cancel(&acked).expect("asks");
    assert_eq!(
        of_job(&rig, &acked),
        stop,
        "{name}: cancel of a start acknowledged"
    );

    let (late, _) = placed_on(&rig, "e-late", "host-a");
    claim_all(&rig);
    let timeout = JobEnd::FailedToStart(StartFailure::StartTimeout);
    rig.jobs.end_job(&late, timeout).expect("ends");
    assert_eq!(of_job(&rig, &late), stop, "{name}: a start timeout");

    let (waited, _) = placed_on(&rig, "e-waited", "host-a");
    let timeout = JobEnd::FailedToStart(StartFailure::StartTimeout);
    rig.jobs.end_job(&waited, timeout).expect("ends");
    assert_eq!(
        of_job(&rig, &waited),
        [],
        "{name}: a timeout of a start never sent"
    );

    let reported = rig.running("e-reported");
    rig.jobs.request_cancel(&reported).expect("asks");
    assert_eq!(
        of_job(&rig, &reported),
        stop,
        "{name}: cancel of a running job whose start went by the heartbeat"
    );
    rig.jobs
        .end_and_free(&reported, JobEnd::Lost, EndReason::HostAbsent)
        .expect("loses");
    assert_eq!(
        of_job(&rig, &reported),
        stop,
        "{name}: a second stop at its loss"
    );

    let ran = rig.running("e-ran");
    claim_all(&rig);
    rig.jobs.end_job(&ran, exited(0)).expect("ends");
    assert_eq!(of_job(&rig, &ran), [], "{name}: an exit the host reported");

    let (lost, allocation) = placed_on(&rig, "e-lost", "host-a");
    claim_all(&rig);
    ack(allocation);
    rig.jobs
        .end_and_free(&lost, JobEnd::Lost, EndReason::HostAbsent)
        .expect("loses");
    assert_eq!(of_job(&rig, &lost), stop, "{name}: a loss");

    let earlier = rig.running("e-earlier");
    for message in queue(&rig, "host-a")
        .into_iter()
        .filter(|m| m.job == earlier)
    {
        let key = keys::dispatch(
            &message.host,
            message.queued_at,
            message.allocation_id,
            message.kind,
        );
        rig.delete_raw(&key);
    }
    rig.jobs.request_cancel(&earlier).expect("asks");
    assert_eq!(
        of_job(&rig, &earlier),
        stop,
        "{name}: a job an earlier release placed"
    );

    let (whole, _, _) = placed_whole(&rig, "e-whole");
    rig.jobs.request_cancel(&whole).expect("asks");
    assert_eq!(
        of_job(&rig, &whole),
        [],
        "{name}: a whole-host job not yet starting"
    );
    let (failed, _, _) = placed_whole(&rig, "e-failed");
    let last = FailedPrepare {
        terminal: true,
        ..requeue_with(&rig, 3)
    };
    rig.jobs
        .fail_prepare_with(&failed, last, &|_| Vec::new())
        .expect("fails");
    assert_eq!(of_job(&rig, &failed), [], "{name}: a failed prepare");

    let (requeued, _, _) = placed_whole(&rig, "e-requeued");
    rig.jobs
        .fail_prepare_with(&requeued, requeue_with(&rig, 1), &|_| Vec::new())
        .expect("requeues");
    rig.jobs
        .place_with(
            &requeued,
            host("host-a"),
            Booked::WholeHost,
            engine_placement(),
        )
        .expect("placed again");
    assert_eq!(
        of_job(&rig, &requeued),
        [],
        "{name}: a requeued whole-host job"
    );
}

/// An end that read its job's start as never handed out loses to a server that hands it out
/// before the end commits: the end is stale and writes nothing, so the start is not deleted
/// without a stop; asked again, it sends the stop. Catches a start deleted at no precondition.
pub fn an_end_that_loses_to_a_claim_of_its_start_is_stale<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "an_end_that_loses_to_a_claim_of_its_start_is_stale";
    let rig = rig(backend);
    let (id, _) = placed_on(&rig, "s-race", "host-a");
    let other = rig.other();
    rig.store.before_next(move || {
        let taken = other.claim_dispatch(&host("host-a"), "server-2", LEASE);
        assert_eq!(taken.expect("claims").len(), 1);
    });
    let raced = rig.jobs.request_cancel(&id);
    assert_eq!(
        raced,
        Err(JobStoreError::Stale { id: id.clone() }),
        "{name}"
    );
    let job = rig.jobs.get_job(&id).expect("reads").expect("a job");
    assert!(
        !job.cancel_requested,
        "{name}: the stale cancel was written"
    );
    assert_eq!(of_job(&rig, &id), [(DispatchKind::Start, true)], "{name}");
    rig.jobs.request_cancel(&id).expect("asks again");
    assert_eq!(of_job(&rig, &id), [(DispatchKind::Stop, false)], "{name}");
}

/// The retention sweep deletes a job's messages, handed out or not, in the commit that deletes
/// its row, and leaves another job's. Catches a stop no agent ever acknowledged kept forever.
pub fn the_sweep_deletes_a_jobs_messages_with_its_row<B: Backend>(backend: &B) {
    let name = "the_sweep_deletes_a_jobs_messages_with_its_row";
    let rig = rig(backend);
    let (gone, _) = placed_on(&rig, "r-gone", "host-a");
    claim_all(&rig);
    rig.jobs.request_cancel(&gone).expect("asks");
    rig.jobs.end_job(&gone, cancelled()).expect("ends");
    let (kept, _) = placed_on(&rig, "r-kept", "host-a");
    assert_eq!(of_job(&rig, &gone), [(DispatchKind::Stop, false)], "{name}");
    rig.clock.advance(rig.jobs.retention().window());
    assert_eq!(rig.jobs.tick(), Ok(vec![gone.clone()]), "{name}");
    assert_eq!(
        of_job(&rig, &gone),
        [],
        "{name}: a swept job's stop was kept"
    );
    assert_eq!(
        of_job(&rig, &kept),
        [(DispatchKind::Start, false)],
        "{name}"
    );
}

/// A message an earlier build wrote, without the optional fields, is handed out; one with a
/// field a later build adds is handed out and keeps the field; one a later build wrote in a
/// format this build cannot read, and one under another message's key, are left as they are by
/// every claim, acknowledgement and end. Catches a release that cannot deliver what the release
/// before queued, and one that drops or deletes what the release after wrote.
pub fn a_dispatch_row_of_another_build_is_read_or_left<B: Backend>(backend: &B) {
    let name = "a_dispatch_row_of_another_build_is_read_or_left";
    let rig = rig(backend);
    let a = host("host-a");
    let (id, allocation) = placed_on(&rig, "c-old", "host-a");
    let stored = queue(&rig, "host-a").remove(0);
    let key = keys::dispatch(&a, stored.queued_at, allocation, DispatchKind::Start);
    let bare = encode_dispatch(&stored).expect("encodes");
    assert_eq!(
        field_of(&bare, 6),
        None,
        "{name}: the earlier build's row has no start body"
    );
    let later = set_field(&bare, 77, b"later").expect("parses");
    rig.put_raw(&key, later);
    let claimed = claim_all(&rig);
    assert_eq!(
        allocations(&claimed),
        BTreeSet::from([allocation]),
        "{name}"
    );
    let row = rig.raw(&key).expect("kept").value;
    assert_eq!(
        field_of(&row, 77),
        Some(b"later".to_vec()),
        "{name}: the later field"
    );

    let unreadable = format!(
        "{}{}",
        keys::dispatch_prefix(&a),
        "99999999999999999999-x-start"
    );
    rig.put_raw(&unreadable, vec![LATER_FORMAT]);
    let elsewhere = format!(
        "{}{}",
        keys::dispatch_prefix(&a),
        "00000000000000000000-y-stop"
    );
    rig.put_raw(&elsewhere, bare.clone());
    rig.clock.advance(LEASE + LEASE);
    assert_eq!(
        allocations(&claim_all(&rig)),
        BTreeSet::from([allocation]),
        "{name}"
    );
    rig.jobs
        .ack_dispatch(&a, &[(allocation, DispatchKind::Start)])
        .expect("acks");
    rig.jobs.request_cancel(&id).expect("asks");
    assert_eq!(
        rig.raw(&unreadable).map(|row| row.value),
        Some(vec![LATER_FORMAT]),
        "{name}"
    );
    assert_eq!(
        rig.raw(&elsewhere).map(|row| row.value),
        Some(bare),
        "{name}"
    );
    assert_eq!(of_job(&rig, &id), [(DispatchKind::Stop, false)], "{name}");
}

/// The error every injected failure answers.
fn injected<T>() -> Result<T, JobStoreError> {
    Err(JobStoreError::Store(StoreError::Failed(
        INJECTED.to_owned(),
    )))
}

/// A claim whose read of its message fails inside its transaction, or whose store clock cannot
/// be read, answers the failure and hands out nothing; an acknowledgement whose commit fails
/// deletes nothing. Catches a failure taken for an empty queue, and a message lost to a failed
/// write.
pub fn a_failed_claim_or_ack_changes_nothing_and_says_so<B: Backend>(backend: &B) {
    let name = "a_failed_claim_or_ack_changes_nothing_and_says_so";
    let rig = rig(backend);
    let a = host("host-a");
    let (_, allocation) = placed_on(&rig, "f-one", "host-a");
    let before = rig.snapshot().0;
    rig.store.fail_next_decision_get();
    let failed = rig.jobs.claim_dispatch(&a, "server-1", LEASE);
    assert_eq!(failed, injected(), "{name}: a failed read inside the claim");
    assert_eq!(rig.snapshot().0, before, "{name}: a failed claim wrote");
    rig.store.fail_clock(true);
    let failed = rig.jobs.claim_dispatch(&a, "server-1", LEASE);
    rig.store.fail_clock(false);
    assert_eq!(
        failed,
        injected(),
        "{name}: a claim without the store's clock"
    );
    assert_eq!(
        rig.snapshot().0,
        before,
        "{name}: a claim without the clock wrote"
    );

    assert_eq!(
        claim_all(&rig).len(),
        1,
        "{name}: the claim after the failures"
    );
    let held = rig.snapshot().0;
    rig.store.fail_next();
    let failed = rig
        .jobs
        .ack_dispatch(&a, &[(allocation, DispatchKind::Start)]);
    assert_eq!(failed, injected(), "{name}: a failed acknowledgement");
    assert_eq!(
        rig.snapshot().0,
        held,
        "{name}: a failed acknowledgement deleted"
    );
    let acked = rig
        .jobs
        .ack_dispatch(&a, &[(allocation, DispatchKind::Start)]);
    assert_eq!(acked, Ok(1), "{name}");
}

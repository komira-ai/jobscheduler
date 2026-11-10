//! The whole-host invariants over hand-built histories: each has negative cases (in the
//! parent's `NEGATIVE_CASES` too) and correct siblings, and the boundaries of what each
//! reads (which host, which lease, which time, which source). Uses the helpers of the
//! parent file.

use js_jobs::{AllocationId, AllocationStep, EndReason, JobId, JobState};
use js_sim_cell::{AllocationRow, JobReply, StartSeen, StopCommand, StopSeen};

use super::*;

/// The negative cases of the whole-host invariants, each with the one invariant it must
/// trip, as the parent's `NEGATIVE_CASES`.
pub(crate) const NEGATIVE_CASES: &[NegativeCase] = &[
    (Invariant::WholeHostIsExclusive, two_whole_items_on_one_host),
    (
        Invariant::WholeHostIsExclusive,
        whole_host_booked_beside_another_lease,
    ),
    (Invariant::HeldMeansReady, granted_before_ready),
    (Invariant::DrainKillsNothing, cancel_on_a_draining_host),
    (
        Invariant::MarkedHostsServeNoOrdinaryStart,
        ordinary_start_on_a_marked_host,
    ),
    (
        Invariant::HeldMeansReady,
        whole_host_job_started_before_ready,
    ),
    (
        Invariant::DrainKillsNothing,
        stop_of_a_job_on_a_draining_host,
    ),
];

/// Whole-host items 0 and 1 both placed on host 0, each after host 0 reported ready for
/// its lease; host 0 runs nothing.
pub fn two_whole_items_on_one_host() -> Observations {
    let mut observations = Observations::quiet().at(ms(500));
    for n in [0, 1] {
        let item = ItemId(u32::try_from(n).expect("item"));
        observations = observations
            .with_item(item, whole())
            .with_event(item, ms(100), ClientEvent::Submitted)
            .with_event(item, ms(200), queued(n))
            .with_ready(host(0), &lease(n).to_string(), ms(220))
            .with_event(item, ms(250), placed(0));
    }
    observations
}

/// Whole-host item 0 placed on host 0, whose listing books its lease 0 and lease 1.
pub fn whole_host_booked_beside_another_lease() -> Observations {
    whole_item_queued()
        .with_event(ItemId(0), ms(250), placed(0))
        .with_booking(host(0), lease(0))
        .with_booking(host(0), lease(1))
}

/// Whole-host item 0 granted lease 0 on host 0 at 250; host 0 reported ready for it only
/// at 300.
pub fn granted_before_ready() -> Observations {
    Observations::quiet()
        .at(ms(500))
        .with_item(ItemId(0), whole())
        .with_event(ItemId(0), ms(100), ClientEvent::Submitted)
        .with_event(ItemId(0), ms(200), queued(0))
        .with_event(ItemId(0), ms(250), placed(0))
        .with_ready(host(0), "1-0", ms(300))
}

/// Ordinary item 0, its run 0 of lease 0 placed and running on host 0.
fn ordinary_running() -> Observations {
    Observations::quiet()
        .at(ms(500))
        .with_item(ItemId(0), share(1_000))
        .with_event(ItemId(0), ms(100), ClientEvent::Submitted)
        .with_event(ItemId(0), ms(200), queued(0))
        .with_event(ItemId(0), ms(250), placed(0))
        .with_agent(host(0), runs_lease_zero())
}

/// A `command` of run 0 of lease 0 on host 0 at 400, whose wall limit ends at 10 000.
fn stop_at_400(command: StopCommand) -> StopSeen {
    StopSeen {
        host: host(0),
        run: run(0),
        lease: lease(0),
        at: ms(400),
        command,
        wall_ends_at: ms(10_000),
    }
}

/// Host 0, set aside for request `r1`, is sent a cancel of ordinary item 0's running run
/// at 400 that the client never asked for.
pub fn cancel_on_a_draining_host() -> Observations {
    ordinary_running()
        .with_reservation(host(0), "r1", None)
        .with_stop(stop_at_400(StopCommand::Cancel))
}

/// A start of run 0 of lease 0 on host 0 at 400, naming a heartbeat sent at 300, with a
/// wall limit of 10 s.
fn start_at_400() -> StartSeen {
    StartSeen {
        host: host(0),
        run: run(0),
        lease: lease(0),
        at: ms(400),
        asked_at: ms(300),
        wall: Duration::from_secs(10),
    }
}

/// Host 0, set aside for request `r1` with its tail ending at 5 000, is sent a start of
/// ordinary item 0's lease that may run until 10 300.
pub fn ordinary_start_on_a_marked_host() -> Observations {
    Observations::quiet()
        .at(ms(500))
        .with_item(ItemId(0), share(1_000))
        .with_event(ItemId(0), ms(100), ClientEvent::Submitted)
        .with_event(ItemId(0), ms(200), queued(0))
        .with_reservation(host(0), "r1", Some(ms(5_000)))
        .with_start(start_at_400())
}

/// Asserts that a host held by two whole-host items at once, and a host whose listing
/// books another lease beside the whole-host item holding it, are each flagged as
/// `WholeHostIsExclusive` with a line naming the host, the items and the leases; and the
/// correct siblings: the second item placed only after the first sent its release, the
/// listing booking only the item's own lease, a listing not read at the step, and a host
/// whose holder released before a step that touched it. Catches a check that misses a
/// second holder or a second booking, one that flags the holder's own booking, and one
/// whose hold outlives the release.
#[test]
fn two_leases_on_one_whole_host_are_flagged() {
    assert_eq!(
        lines(&step_all(&two_whole_items_on_one_host())),
        [
            "whole-host-is-exclusive at 500: host host-0 is held by whole-host item0 with \
             lease 1.0 and by whole-host item1 with lease 1.1"
        ]
    );
    let beside = whole_host_booked_beside_another_lease();
    assert_eq!(
        lines(&step_all(&beside)),
        [
            "whole-host-is-exclusive at 500: host host-0 books lease 1.1 while whole-host \
             item0 holds lease 1.0 there"
        ]
    );
    let unread = Touched::nothing().item(ItemId(0)).agent(host(0));
    none(&Checker::new().step(&beside, &unread));
    let own = whole_item_queued()
        .with_event(ItemId(0), ms(250), placed(0))
        .with_booking(host(0), lease(0));
    none(&step_all(&own));
    let in_turn = Observations::quiet()
        .at(ms(500))
        .with_item(ItemId(0), whole())
        .with_event(ItemId(0), ms(200), queued(0))
        .with_ready(host(0), "1-0", ms(220))
        .with_event(ItemId(0), ms(250), placed(0))
        .with_event(ItemId(0), ms(300), ClientEvent::ReleaseSent)
        .with_item(ItemId(1), whole())
        .with_event(ItemId(1), ms(350), queued(1))
        .with_ready(host(0), "1-1", ms(360))
        .with_event(ItemId(1), ms(400), placed(0));
    none(&step_all(&in_turn));

    let mut checker = Checker::new();
    let held = whole_item_queued().with_event(ItemId(0), ms(250), placed(0));
    none(&checker.step(&held, &Touched::everything_in(&held)));
    let released = held.with_event(ItemId(0), ms(300), ClientEvent::ReleaseSent);
    none(&checker.step(&released, &Touched::nothing().item(ItemId(0))));
    let booked = released.with_booking(host(0), lease(1));
    let touched = Touched::nothing().agent(host(0)).server();
    none(&checker.step(&booked, &touched));
}

/// Asserts that a whole-host item granted its lease before its host reported ready for
/// it is flagged as `HeldMeansReady`, with a line naming the item, the lease, the host
/// and the time; that a report at the very time of the grant is enough; that a report
/// from another host, or for another lease, is not; and that the grant of a share item,
/// and a placement heard before any lease, are not checked. Catches a check that reads a
/// report after the grant, one that takes any report of the host or of the lease, and
/// one that applies to items that did not ask for a whole host.
#[test]
fn a_grant_before_the_ready_report_is_flagged() {
    let expected = "held-means-ready at 500: whole-host item0 was granted lease 1.0 on host \
                    host-0 at 250 before the host's agent reported ready for that lease";
    assert_eq!(lines(&step_all(&granted_before_ready())), [expected]);
    let grant = || {
        Observations::quiet()
            .at(ms(500))
            .with_item(ItemId(0), whole())
            .with_event(ItemId(0), ms(200), queued(0))
            .with_event(ItemId(0), ms(250), placed(0))
    };
    none(&step_all(&grant().with_ready(host(0), "1-0", ms(250))));
    let elsewhere = grant().with_ready(host(1), "1-0", ms(100));
    assert_eq!(lines(&step_all(&elsewhere)), [expected]);
    let other_lease = grant().with_ready(host(0), "1-1", ms(100));
    assert_eq!(lines(&step_all(&other_lease)), [expected]);
    let mut share_item = grant();
    share_item.items.get_mut(&ItemId(0)).expect("item 0").ask = share(1_000);
    none(&step_all(&share_item));
    let no_lease = Observations::quiet()
        .at(ms(500))
        .with_item(ItemId(0), whole())
        .with_event(ItemId(0), ms(250), placed(0));
    none(&step_all(&no_lease));
}

/// Asserts that a cancel, or a stop, reaching a running run on a host set aside for a
/// request is flagged as `DrainKillsNothing` unless the run's client asked first, with a
/// line naming the host, the request, the command, the run and the lease; that a client
/// cancel or release sent before it, a run past its own wall limit, a host not set aside
/// and a host not listed are not; and that a cancel asked only after the command, or a
/// run no item knows, are. Catches a drain that cuts work unflagged, a check that
/// excuses a kill by a later client request, and one that flags the client's own stop.
#[test]
fn a_cancel_sent_to_a_draining_host_is_flagged() {
    let line = |command: &str| {
        format!(
            "drain-kills-nothing at 500: host host-0, set aside for request r1, was sent a \
             {command} of run \"run-0\" of lease 1.0 at 400 that its client did not ask for"
        )
    };
    assert_eq!(
        lines(&step_all(&cancel_on_a_draining_host())),
        [line("cancel")]
    );
    let stopped = ordinary_running()
        .with_reservation(host(0), "r1", None)
        .with_stop(stop_at_400(StopCommand::Stop));
    assert_eq!(lines(&step_all(&stopped)), [line("stop")]);
    none(&step_all(
        &cancel_on_a_draining_host().with_cancel(ItemId(0), ms(400)),
    ));
    let late = cancel_on_a_draining_host().with_cancel(ItemId(0), ms(401));
    assert_eq!(lines(&step_all(&late)), [line("cancel")]);
    let released =
        cancel_on_a_draining_host().with_event(ItemId(0), ms(350), ClientEvent::ReleaseSent);
    none(&step_all(&released));
    let past_wall = StopSeen {
        wall_ends_at: ms(400),
        ..stop_at_400(StopCommand::Cancel)
    };
    let lapsed = ordinary_running()
        .with_reservation(host(0), "r1", None)
        .with_stop(past_wall);
    none(&step_all(&lapsed));
    let not_aside = ordinary_running()
        .with_booking(host(0), lease(0))
        .with_stop(stop_at_400(StopCommand::Cancel));
    none(&step_all(&not_aside));
    let unlisted = ordinary_running().with_stop(stop_at_400(StopCommand::Cancel));
    none(&step_all(&unlisted));
    let mut no_item = cancel_on_a_draining_host();
    no_item.items.clear();
    assert_eq!(lines(&step_all(&no_item)), [line("cancel")]);
}

/// Asserts that a start of an ordinary item's lease on a host set aside for a request is
/// flagged as `MarkedHostsServeNoOrdinaryStart` when it may run past the host's tail,
/// counted from the heartbeat the start names, and when the tail is unknown, with a line
/// naming the host, the request, the tail, the run, the lease and both times; that
/// backfill ending exactly at the tail is not; and that a whole-host item's start, a
/// start of a lease no item asked for, and a start on a host not set aside or not
/// listed are not checked. Catches a mark that ordinary work ignores, a tail counted
/// from the start's arrival (a correct backfill would read late), and a check that
/// trusts the server's word for which lease is ordinary.
#[test]
fn an_ordinary_start_on_a_marked_host_is_flagged() {
    let line = |tail: &str| {
        format!(
            "marked-hosts-serve-no-ordinary-start at 500: host host-0, set aside for request \
             r1 with its tail ending at {tail}, was sent a start of run \"run-0\" of ordinary \
             lease 1.0 at 400 that may run until 10300"
        )
    };
    let marked = ordinary_start_on_a_marked_host();
    assert_eq!(lines(&step_all(&marked)), [line("5000")]);
    let unknown = marked.clone().with_reservation(host(0), "r1", None);
    assert_eq!(lines(&step_all(&unknown)), [line("unknown")]);
    let inside = marked
        .clone()
        .with_reservation(host(0), "r1", Some(ms(10_300)));
    none(&step_all(&inside));
    let mut whole_item = marked.clone();
    whole_item.items.get_mut(&ItemId(0)).expect("item 0").ask = whole();
    none(&step_all(&whole_item));
    let mut no_item = marked.clone();
    no_item.items.clear();
    none(&step_all(&no_item));
    let mut not_aside = marked.clone();
    not_aside
        .server
        .hosts
        .insert(host(0), ServerHost::default());
    none(&step_all(&not_aside));
    let mut unlisted = marked;
    unlisted.server.hosts.clear();
    none(&step_all(&unlisted));
}

/// Asserts that ready reports, starts and stops are each read once, at the step that
/// appended them: a checker that already read a flagged start or stop does not flag it
/// again at a later step, and a ready report read at an earlier step still counts for a
/// grant heard later. Catches a check that re-reads every fact at every step (the cost
/// rule) and one that forgets what it read.
#[test]
fn whole_host_facts_are_read_once_at_the_step_that_appends_them() {
    let mut checker = Checker::new();
    let marked = ordinary_start_on_a_marked_host().with_stop(stop_at_400(StopCommand::Cancel));
    let marked = marked.with_agent(host(0), runs_lease_zero());
    let everything = Touched::everything_in(&marked);
    let found = checker.step(&marked, &everything);
    let names: Vec<Invariant> = found.iter().map(|v| v.invariant).collect();
    let wanted = [
        Invariant::MarkedHostsServeNoOrdinaryStart,
        Invariant::DrainKillsNothing,
    ];
    assert_eq!(names, wanted);
    none(&checker.step(&marked, &everything));

    let mut checker = Checker::new();
    let reported = Observations::quiet()
        .at(ms(100))
        .with_ready(host(0), "1-0", ms(100));
    none(&checker.step(&reported, &Touched::nothing()));
    let granted = reported
        .with_item(ItemId(0), whole())
        .with_event(ItemId(0), ms(200), queued(0))
        .with_event(ItemId(0), ms(250), placed(0))
        .at(ms(300));
    none(&checker.step(&granted, &Touched::nothing().item(ItemId(0))));
}

// ---- The invariants over jobs. ----

fn job(n: u64) -> JobId {
    JobId::parse(&format!("sim-job-{n}")).expect("a job id")
}

fn allocation(n: u8) -> AllocationId {
    AllocationId::from_bits([n; 16])
}

/// Allocation `a` of job `j` on host `h`, booked, committed at 100.
fn booked_row(j: u64, a: u8, h: u64) -> AllocationRow {
    AllocationRow {
        job: job(j),
        allocation: allocation(a),
        host: host(h),
        step: AllocationStep::Booked,
        created_at: ms(100),
        end_reason: None,
    }
}

/// Allocation 1 of job 0 on host 0, freed for a failed prepare.
fn failed_row() -> AllocationRow {
    AllocationRow {
        step: AllocationStep::Freed,
        end_reason: Some(EndReason::FailedPrepare),
        ..booked_row(0, 1, 0)
    }
}

/// An answer host `h` read at `at` that carried a stop of each job of `stops`.
fn stop_reply(h: u64, at: u64, stops: &[JobId]) -> JobReply {
    JobReply {
        host: host(h),
        at: ms(at),
        beat_at: ms(at - 50),
        listed: stops.iter().cloned().collect(),
        starts: Vec::new(),
        stops: stops.iter().cloned().collect(),
        acks: Vec::new(),
    }
}

/// Whole-host item 0, whose job `sim-job-0` was read placed on host 0 under allocation 1 at
/// 200 and starting at 300; host 0's agent reported ready for the allocation only at 400.
pub fn whole_host_job_started_before_ready() -> Observations {
    Observations::quiet()
        .at(ms(500))
        .with_item(ItemId(0), whole())
        .with_allocation_read(booked_row(0, 1, 0), ms(200))
        .with_job_read(job(0), JobState::Placed, ms(200))
        .with_job_read(job(0), JobState::Starting, ms(300))
        .with_ready(host(0), &allocation(1).to_string(), ms(400))
}

/// Job `sim-job-0` of item 0, running on host 0, which the listing sets aside for request
/// `r1`, sent a stop at 400 that no client asked for.
pub fn stop_of_a_job_on_a_draining_host() -> Observations {
    Observations::quiet()
        .at(ms(500))
        .with_item(ItemId(0), share(1_000))
        .with_reservation(host(0), "r1", None)
        .with_job_reply(stop_reply(0, 400, &[job(0)]))
}

/// Asserts that a whole-host job read starting, or running, on an allocation before its
/// host's agent reported ready for exactly that allocation is flagged as `HeldMeansReady`,
/// with a line naming the job, the state, the allocation, the host and the time; that a
/// report at the very time of the read is enough; that a report from another host, or for
/// another allocation, is not; that a job read starting without any allocation read, a share
/// job, a job no item asked for and a job read placed are not checked; that a job is judged
/// once for each allocation it is read starting on (a second read, running, is not flagged
/// again; a second allocation without a report is); and that a read is judged with the
/// allocation the tables showed by then, not one read later. Catches a check that reads a
/// report after the read, one that takes any report of the host or of the job, one that
/// applies to jobs that did not ask for a whole host, and one that judges a read with the
/// next placement.
#[test]
fn a_whole_host_job_started_before_its_ready_report_is_flagged() {
    let allocation_1 = allocation(1).to_string();
    let line = |state: &str, at: u64| {
        format!(
            "held-means-ready at 500: whole-host job sim-job-0 was read {state} on allocation \
             {allocation_1} on host host-0 at {at} before the host's agent reported ready for \
             that allocation"
        )
    };
    assert_eq!(
        lines(&step_all(&whole_host_job_started_before_ready())),
        [line("starting", 300)]
    );
    let started = || {
        Observations::quiet()
            .at(ms(500))
            .with_item(ItemId(0), whole())
            .with_allocation_read(booked_row(0, 1, 0), ms(200))
            .with_job_read(job(0), JobState::Starting, ms(300))
    };
    none(&step_all(&started().with_ready(
        host(0),
        &allocation_1,
        ms(300),
    )));
    let elsewhere = started().with_ready(host(1), &allocation_1, ms(100));
    assert_eq!(lines(&step_all(&elsewhere)), [line("starting", 300)]);
    let other = started().with_ready(host(0), &allocation(2).to_string(), ms(100));
    assert_eq!(lines(&step_all(&other)), [line("starting", 300)]);
    let running = Observations::quiet()
        .at(ms(500))
        .with_item(ItemId(0), whole())
        .with_allocation_read(booked_row(0, 1, 0), ms(200))
        .with_job_read(job(0), JobState::Running, ms(300));
    assert_eq!(lines(&step_all(&running)), [line("running", 300)]);

    let placed_only = Observations::quiet()
        .at(ms(500))
        .with_item(ItemId(0), whole())
        .with_allocation_read(booked_row(0, 1, 0), ms(200))
        .with_job_read(job(0), JobState::Placed, ms(200));
    none(&step_all(&placed_only));
    let no_row = Observations::quiet()
        .at(ms(500))
        .with_item(ItemId(0), whole())
        .with_job_read(job(0), JobState::Starting, ms(300));
    none(&step_all(&no_row));
    let mut share_job = whole_host_job_started_before_ready();
    share_job.items.get_mut(&ItemId(0)).expect("item 0").ask = share(1_000);
    none(&step_all(&share_job));
    let mut no_item = whole_host_job_started_before_ready();
    no_item.items.clear();
    none(&step_all(&no_item));
    let other_job = JobId::parse("other-job").expect("a job id");
    let not_an_item = Observations::quiet()
        .at(ms(500))
        .with_item(ItemId(0), whole())
        .with_allocation_read(booked_row(0, 1, 0), ms(200))
        .with_job_read(other_job, JobState::Starting, ms(300));
    none(&step_all(&not_an_item));

    let mut checker = Checker::new();
    let first = whole_host_job_started_before_ready();
    assert_eq!(checker.step(&first, &Touched::nothing()).len(), 1);
    let again = first.with_job_read(job(0), JobState::Running, ms(450));
    none(&checker.step(&again, &Touched::nothing()));
    let moved = again
        .with_allocation_read(failed_row(), ms(455))
        .with_allocation_read(booked_row(0, 2, 1), ms(460))
        .with_job_read(job(0), JobState::Starting, ms(470));
    let found = lines(&checker.step(&moved, &Touched::nothing()));
    let second = "held-means-ready at 500: whole-host job sim-job-0 was read starting on \
                  allocation 02020202020202020202020202020202 on host host-1 at 470 before the \
                  host's agent reported ready for that allocation";
    assert_eq!(found, [second]);

    let next_placement = Observations::quiet()
        .at(ms(500))
        .with_item(ItemId(0), whole())
        .with_allocation_read(booked_row(0, 1, 0), ms(200))
        .with_job_read(job(0), JobState::Starting, ms(300))
        .with_allocation_read(failed_row(), ms(340))
        .with_allocation_read(booked_row(0, 2, 1), ms(350))
        .with_ready(host(0), &allocation_1, ms(250));
    none(&step_all(&next_placement));
}

/// Asserts that a stop of a job sent to a host the listing sets aside is flagged as
/// `DrainKillsNothing` unless the job's client asked to cancel it first, with a line naming
/// the host, the job and the time; that a cancel asked at the time of the answer or before
/// is enough and one asked after is not; that an answer that carried no stop, a host the
/// listing does not set aside and a host it does not list are not checked; that a job no
/// item asked for is flagged; and that an answer is read once. Catches a drain that cuts a
/// job unflagged, a check that excuses a kill by a later client request, one that flags the
/// client's own cancel, and one that reads an answer at every step.
#[test]
fn a_stop_sent_to_a_draining_host_is_flagged_for_a_job() {
    let line = |job: &str| {
        format!(
            "drain-kills-nothing at 500: host host-0, which the server sets aside, was sent a \
             stop of job {job} at 400 that its client did not ask for"
        )
    };
    let drained = stop_of_a_job_on_a_draining_host();
    assert_eq!(lines(&step_all(&drained)), [line("sim-job-0")]);
    none(&step_all(&drained.clone().with_cancel(ItemId(0), ms(400))));
    let late = drained.clone().with_cancel(ItemId(0), ms(401));
    assert_eq!(lines(&step_all(&late)), [line("sim-job-0")]);
    let quiet = Observations::quiet()
        .at(ms(500))
        .with_reservation(host(0), "r1", None)
        .with_job_reply(stop_reply(0, 400, &[]));
    none(&step_all(&quiet));
    let not_aside = Observations::quiet()
        .at(ms(500))
        .with_booking(host(0), lease(0))
        .with_job_reply(stop_reply(0, 400, &[job(0)]));
    none(&step_all(&not_aside));
    let unlisted = Observations::quiet()
        .at(ms(500))
        .with_job_reply(stop_reply(0, 400, &[job(0)]));
    none(&step_all(&unlisted));
    let other_job = JobId::parse("other-job").expect("a job id");
    let unknown = Observations::quiet()
        .at(ms(500))
        .with_reservation(host(0), "r1", None)
        .with_cancel(ItemId(0), ms(100))
        .with_job_reply(stop_reply(0, 400, &[other_job]));
    assert_eq!(lines(&step_all(&unknown)), [line("other-job")]);

    let mut checker = Checker::new();
    assert_eq!(checker.step(&drained, &Touched::nothing()).len(), 1);
    none(&checker.step(&drained, &Touched::nothing()));
}

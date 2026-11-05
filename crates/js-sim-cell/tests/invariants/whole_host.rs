//! The whole-host invariants over hand-built histories: each has negative cases (in the
//! parent's `NEGATIVE_CASES` too) and correct siblings, and the boundaries of what each
//! reads (which host, which lease, which time, which source). Uses the helpers of the
//! parent file.

use js_sim_cell::{StartSeen, StopCommand, StopSeen};

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

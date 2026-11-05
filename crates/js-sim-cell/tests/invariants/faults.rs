//! The invariants the fault family added, over hand-built observations:
//! `NoLeaseOnALostHost`, `NoResultLost`, and the two clauses of `NoDoubleBooking` that
//! read the server's listing whole (a lease on one host only, a host listed once). Each
//! has its negative cases (also rows of the parent's `NEGATIVE_CASES`) and the correct
//! siblings it must not flag. Uses the helpers of the parent file.

use js_sim_cell::ServerView;

use super::*;

/// Item 0 is placed on host 0 at 300 while the server lists host 0 lost.
pub(super) fn placed_on_a_lost_host() -> Observations {
    Observations::quiet()
        .at(ms(300))
        .with_item(ItemId(0), share(1_000))
        .with_event(ItemId(0), ms(100), ClientEvent::Submitted)
        .with_event(ItemId(0), ms(300), queued(0))
        .with_event(ItemId(0), ms(300), placed(0))
        .with_lost(host(0), true)
}

/// Item 0's run 0 completed on host 0 at 900; the client heard it lost at 1000 and
/// released its lease.
pub(super) fn result_never_reached_the_server() -> Observations {
    released(HowEnded::Lost).with_completion(run(0), host(0), ms(900))
}

/// Item 0's run 0 completed on host 0 at 900 and succeeded; its result was accepted on
/// host 0 at 950, and again on host 1 at 960.
pub(super) fn result_accepted_twice() -> Observations {
    succeeded_and_completed()
        .with_verdict(run(0), host(0), Verdict::Accepted, ms(950))
        .with_verdict(run(0), host(1), Verdict::Accepted, ms(960))
}

/// Item 0 succeeded and released, and no agent completed its run 0.
pub(super) fn succeeded_without_a_completion() -> Observations {
    released(HowEnded::Succeeded)
}

/// The server's listing books lease 0 on host 0 and on host 1.
pub(super) fn lease_on_two_hosts() -> Observations {
    Observations::quiet()
        .at(ms(300))
        .with_booking(host(0), lease(0))
        .with_booking(host(1), lease(0))
}

/// The server's listing names host 0 twice.
pub(super) fn host_listed_twice() -> Observations {
    let mut observations = Observations::quiet().at(ms(300));
    observations.server = ServerView::from_listing([
        (host(0), ServerHost::default()),
        (host(0), ServerHost::default()),
    ]);
    observations
}

/// Item 0 succeeded and released; its run 0 completed on host 0 at 900.
fn succeeded_and_completed() -> Observations {
    released(HowEnded::Succeeded).with_completion(run(0), host(0), ms(900))
}

/// The `NoResultLost` lines of the end check over `observations`.
fn result_lines(observations: &Observations) -> Vec<String> {
    let found = end(observations);
    let lost = found
        .iter()
        .filter(|v| v.invariant == Invariant::NoResultLost);
    lost.map(ToString::to_string).collect()
}

/// The line of a completion at 900 whose item 0 ended `how`.
fn lost_line(how: &str) -> String {
    format!("no-result-lost at 5000: run \"run-0\" completed on host host-0 at 900 and item0 {how}")
}

/// Asserts that a client told its item was placed on a host the server's listing, read
/// at that call, shows lost is flagged `NoLeaseOnALostHost` with the item, the host and
/// the time, and so is a lease the listing first shows booked on a host it shows lost
/// (but not one first seen while the host was live); that the same placement on a live
/// host, or on a host the listing does not
/// name, is not; and that a host that turns lost after the placement was heard does not
/// flag it at a later step that touches the item again. Catches a check that reads no
/// listing, one that reads the host's state at a later time than the call, and one that
/// judges an old placement again.
#[test]
fn a_lease_booked_on_a_lost_host_is_flagged() {
    assert_eq!(
        lines(&step_all(&placed_on_a_lost_host())),
        [
            "no-lease-on-a-lost-host at 300: item0 was placed on host host-0 at 300 while the \
          server lists it lost"
        ]
    );
    let live = placed_on_a_lost_host().with_lost(host(0), false);
    none(&step_all(&live));
    let booked = Observations::quiet()
        .at(ms(300))
        .with_booking(host(0), lease(0))
        .with_lost(host(0), true);
    assert_eq!(
        lines(&step_all(&booked)),
        [
            "no-lease-on-a-lost-host at 300: lease 1.0 is booked on host host-0 while the \
          server lists it lost"
        ]
    );
    let mut seen_first = Checker::new();
    let server = Touched::nothing().server();
    none(&seen_first.step(&booked.clone().with_lost(host(0), false), &server));
    none(&seen_first.step(&booked, &server));
    let mut unlisted = placed_on_a_lost_host();
    unlisted.server = ServerView::default();
    none(&step_all(&unlisted));
    let mut checker = Checker::new();
    let touched = Touched::nothing().item(ItemId(0)).server();
    none(&checker.step(&live, &touched));
    let later =
        placed_on_a_lost_host()
            .at(ms(400))
            .with_event(ItemId(0), ms(400), ClientEvent::Started);
    none(&checker.step(&later, &touched));
}

/// Asserts that a run an agent completed whose client then heard another end than
/// `succeeded` is flagged `NoResultLost` with the run, the host, the time and the item's
/// end, and that one whose client heard no end is flagged too; that it is not flagged
/// when the client had its end before the completion, when it succeeded, when no item
/// queued the run, or when the agent's process was killed after the completion before
/// it read a verdict on it; and that a kill before the completion, a kill after the
/// first verdict, or a kill of another host does not excuse it. Catches a check that
/// takes the server's word, one that excuses every lost result of a host that was ever
/// killed, and one that judges a client that had already given up.
#[test]
fn a_result_that_never_reached_the_server_is_flagged() {
    let flagged = result_never_reached_the_server();
    assert_eq!(result_lines(&flagged), [lost_line("ended lost")]);
    let unanswered = Observations::quiet()
        .at(ms(END))
        .with_item(ItemId(0), share(1_000))
        .with_event(ItemId(0), ms(200), queued(0))
        .with_completion(run(0), host(0), ms(900));
    assert_eq!(
        result_lines(&unanswered),
        [lost_line("has no terminal answer")]
    );
    let gave_up_first = released(HowEnded::GaveUp).with_completion(run(0), host(0), ms(1_001));
    none(&end(&gave_up_first));
    none(&end(&succeeded_and_completed()));
    let unqueued = released(HowEnded::Lost).with_completion(run(7), host(0), ms(900));
    none(&end(&unqueued));
    let killed = flagged.clone().with_kill(host(0), ms(950));
    none(&end(&killed));
    let killed_at_once = flagged.clone().with_kill(host(0), ms(900));
    none(&end(&killed_at_once));
    let judged = flagged
        .clone()
        .with_verdict(run(0), host(0), Verdict::Stale, ms(920));
    let killed_after_verdict = judged.clone().with_kill(host(0), ms(950));
    assert_eq!(
        result_lines(&killed_after_verdict),
        [lost_line("ended lost")]
    );
    let killed_before_verdict = judged.with_kill(host(0), ms(910));
    none(&end(&killed_before_verdict));
    let other_verdicts = flagged
        .clone()
        .with_verdict(run(1), host(0), Verdict::Stale, ms(905))
        .with_verdict(run(0), host(1), Verdict::Stale, ms(906))
        .with_verdict(run(0), host(0), Verdict::Stale, ms(850))
        .with_kill(host(0), ms(950));
    none(&end(&other_verdicts));
    let killed_before = flagged.clone().with_kill(host(0), ms(800));
    assert_eq!(result_lines(&killed_before), [lost_line("ended lost")]);
    let other_host = flagged.with_kill(host(1), ms(950));
    assert_eq!(result_lines(&other_host), [lost_line("ended lost")]);
}

/// Asserts that two `Accepted` verdicts on one run's result are flagged `NoResultLost`
/// with the run and both hosts and times, on one host or two, and that an `Accepted`
/// followed by `Duplicate`, or `Accepted` once for each of two runs, is not. Catches a
/// check that counts every verdict as a recording, and one that reads only the last
/// verdict of a run.
#[test]
fn a_result_recorded_twice_is_flagged() {
    assert_eq!(
        result_lines(&result_accepted_twice()),
        [
            "no-result-lost at 5000: run \"run-0\" was accepted twice: on host host-0 at 950 \
          and on host host-1 at 960"
        ]
    );
    let one_host = succeeded_and_completed()
        .with_verdict(run(0), host(0), Verdict::Accepted, ms(950))
        .with_verdict(run(0), host(0), Verdict::Accepted, ms(970));
    assert_eq!(result_lines(&one_host).len(), 1);
    let duplicate = succeeded_and_completed()
        .with_verdict(run(0), host(0), Verdict::Accepted, ms(950))
        .with_verdict(run(0), host(0), Verdict::Duplicate, ms(960));
    none(&end(&duplicate));
    let two_runs = succeeded_and_completed()
        .with_verdict(run(0), host(0), Verdict::Accepted, ms(950))
        .with_verdict(run(1), host(0), Verdict::Accepted, ms(960));
    none(&end(&two_runs));
}

/// Asserts that a client that heard `succeeded` for a run no agent completed is flagged
/// `NoResultLost`, and that a completion of the run on any host clears it. Catches a
/// check that believes the server's outcome over the agents' ground truth.
#[test]
fn a_success_no_agent_produced_is_flagged() {
    assert_eq!(
        result_lines(&succeeded_without_a_completion()),
        ["no-result-lost at 5000: item0 succeeded and no agent completed its run \"run-0\""]
    );
    let elsewhere = released(HowEnded::Succeeded).with_completion(run(0), host(3), ms(900));
    none(&end(&elsewhere));
}

/// Asserts that a lease the listing books on two hosts at once, or on a host other than
/// the one it was first booked on, is flagged `NoDoubleBooking` naming both hosts; that a
/// lease that stays on its host, or is dropped, is not; and that a host the listing names
/// twice is flagged. Catches a restart, or a forgotten host's return, that moves a lease
/// or seats a host twice without the cell seeing it.
#[test]
fn a_lease_on_two_hosts_or_a_host_listed_twice_is_flagged() {
    assert_eq!(
        lines(&step_all(&lease_on_two_hosts())),
        [
            "no-double-booking at 300: lease 1.0 is booked on host host-1, and was booked on \
          host host-0 first"
        ]
    );
    let mut checker = Checker::new();
    let server = Touched::nothing().server();
    let first = Observations::quiet()
        .at(ms(100))
        .with_booking(host(0), lease(0));
    none(&checker.step(&first, &server));
    none(&checker.step(&first.clone().at(ms(200)), &server));
    none(&checker.step(&Observations::quiet().at(ms(300)), &server));
    let moved = Observations::quiet()
        .at(ms(400))
        .with_booking(host(1), lease(0));
    assert_eq!(
        lines(&checker.step(&moved, &server)),
        [
            "no-double-booking at 400: lease 1.0 is booked on host host-1, and was booked on \
          host host-0 first"
        ]
    );
    assert_eq!(
        lines(&step_all(&host_listed_twice())),
        ["no-double-booking at 300: host host-0 is listed twice"]
    );
}

/// Asserts that the view of a listing keeps the first entry of each host and notes each
/// later one, in order, and that a listing of distinct hosts notes none. Catches a view
/// that hides a host listed twice by keeping one entry of a map.
#[test]
fn a_listing_that_names_a_host_again_notes_it() {
    let lost = ServerHost {
        lost: true,
        ..ServerHost::default()
    };
    let view = ServerView::from_listing([
        (host(1), ServerHost::default()),
        (host(0), ServerHost::default()),
        (host(1), lost.clone()),
        (host(1), lost.clone()),
    ]);
    assert_eq!(view.repeated, [host(1), host(1)]);
    assert_eq!(view.hosts.len(), 2);
    assert_eq!(view.hosts[&host(1)], ServerHost::default());
    let distinct = ServerView::from_listing([(host(0), lost.clone()), (host(1), lost)]);
    assert!(distinct.repeated.is_empty());
}

/// Asserts that a history of the faults this family makes passes the invariants it
/// added: a run whose result was accepted, its acknowledgement lost and the result
/// answered `duplicate`; a run on a host whose process was killed after it completed,
/// whose client heard it lost; a client that gave up before its run completed; and a
/// host that went lost after its item was placed, then live again. Catches an
/// over-strict invariant: one that flags a correct history of faults.
#[test]
fn a_history_of_faults_passes_the_invariants_of_the_family() {
    let mut observations = Observations::quiet().at(ms(END));
    for item in 0..3 {
        let ask = share(1_000);
        observations = observations
            .with_item(ItemId(item), ask)
            .with_event(ItemId(item), ms(100), ClientEvent::Submitted)
            .with_event(ItemId(item), ms(200), queued(u64::from(item)))
            .with_event(ItemId(item), ms(300), placed(u64::from(item)));
    }
    let ends = [HowEnded::Succeeded, HowEnded::Lost, HowEnded::GaveUp];
    for (item, how) in (0..).zip(ends) {
        observations = observations
            .with_event(ItemId(item), ms(1_000), ended(how))
            .with_event(ItemId(item), ms(1_050), ClientEvent::ReleaseSent)
            .with_event(ItemId(item), ms(1_100), ClientEvent::Released);
    }
    let observations = observations
        .with_completion(run(0), host(0), ms(900))
        .with_verdict(run(0), host(0), Verdict::Accepted, ms(910))
        .with_verdict(run(0), host(0), Verdict::Duplicate, ms(920))
        .with_completion(run(1), host(1), ms(900))
        .with_kill(host(1), ms(905))
        .with_completion(run(2), host(2), ms(1_200))
        .with_verdict(run(2), host(2), Verdict::Stale, ms(1_210))
        .with_lost(host(0), true)
        .with_lost(host(0), false);
    none(&step_all(&observations));
    none(&end(&observations));
}

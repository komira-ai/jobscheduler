//! The window of `js-agent cordon` over a fake lease server and a clock moved by hand:
//! which calls it makes and when, which lines it prints, when it lifts and when it must not.
//! Nothing waits: a wait is a number the test reads back.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use js_agent::cordon::{IDLE_POLL, Outcome, Plan, hold};
use js_agent::own_host::LeaseError;

use super::fakes::{Call, Effect, FakeLease, FakePace, HOST, Kind, Lines};

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// A plan for `HOST` with cordons of `lease_secs` held for `window` seconds.
fn plan(lease_secs: u64, window: u64) -> Plan<'static> {
    Plan {
        host: HOST,
        lease_secs,
        window: secs(window),
    }
}

/// Runs the window and returns how it ended.
fn run(lease: &FakeLease, pace: &mut FakePace<'_>, plan: &Plan<'_>, lines: &Lines) -> Outcome {
    hold(lease, pace, plan, &mut lines.clone())
}

const CORDONED: &str = "cordoned host=h-1 expires_in_ms=300000 leases=0";
const IDLE: &str = "idle host=h-1";
const LIFTED: &str = "lifted host=h-1";

/// Asserts the rhythm of a window on an idle host: one cordon, then a renewal every third of
/// `lease_secs` (100 s of 300) until the window of an hour has passed, no read of the host
/// (it read idle at once), one uncordon at the end, and the three lines `cordoned`, `idle`,
/// `lifted` and no more. It catches a renewal at the wrong step, a renewal at the very end of
/// the window, a poll that goes on after the host read idle, and a missing or repeated line.
#[test]
fn a_cordon_is_renewed_every_third_of_its_lease_until_the_window_ends_and_then_lifted() {
    let (lease, lines) = (FakeLease::new(), Lines::default());
    let mut pace = FakePace::new();
    let outcome = run(&lease, &mut pace, &plan(300, 3600), &lines);
    assert_eq!(outcome, Outcome::Done);
    assert_eq!(pace.waits, vec![secs(100); 36]);
    let calls = lease.calls();
    assert_eq!(calls.len(), 1 + 35 + 1, "{calls:?}");
    assert!(calls[..36].iter().all(|call| *call == Call::Cordon(300)));
    assert_eq!(calls[36], Call::Uncordon);
    assert_eq!(lines.all(), [CORDONED, IDLE, LIFTED]);
    assert_eq!(pace.elapsed(), secs(3600));
    assert!(!lease.cordoned());
}

/// Asserts the idle line: a host that holds a lease, then only runs to finish, then a mark,
/// is read every `IDLE_POLL` (not every renewal) and no line says `idle` until the read after
/// all three have ended; the line is printed once; after it the host is not read again and the
/// waits go back to the renewal step. It catches an idle line that waits for the next
/// renewal, one printed while `releasing` or a `mark` is still there, one printed twice, and
/// a poll that never stops.
#[test]
fn the_idle_line_comes_once_when_the_host_reads_idle() {
    let (lease, lines) = (FakeLease::new(), Lines::default());
    lease.set(|state| {
        state.lease = Some("1-1".to_owned());
        state.leases = 1;
    });
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut pace = FakePace::new().on_wait({
        let (lease, lines, seen) = (lease.clone(), lines.clone(), Rc::clone(&seen));
        move |waits, _| {
            seen.borrow_mut().push(lines.all());
            match waits {
                1 => lease.set(|state| {
                    state.lease = None;
                    state.leases = 0;
                    state.releasing = true;
                }),
                2 => lease.set(|state| {
                    state.releasing = false;
                    state.mark = Some("draining".to_owned());
                }),
                3 => lease.set(|state| state.mark = None),
                _ => {}
            }
        }
    });
    let outcome = run(&lease, &mut pace, &plan(300, 600), &lines);
    assert_eq!(outcome, Outcome::Done);

    let cordoned = "cordoned host=h-1 expires_in_ms=300000 leases=1";
    let seen = seen.borrow();
    for before in &seen[..3] {
        assert_eq!(before, &[cordoned], "no idle line while the host is busy");
    }
    assert_eq!(seen[3], [cordoned, IDLE]);
    assert_eq!(lines.all(), [cordoned, IDLE, LIFTED]);
    assert_eq!(&pace.waits[..3], &[IDLE_POLL; 3]);
    assert_eq!(pace.waits[3], secs(85), "back to the renewal step");
    assert_eq!(pace.waits[4], secs(100));
    let reads = lease.calls().iter().filter(|c| **c == Call::Host).count();
    assert_eq!(reads, 3, "the host is read no more once it read idle");
}

/// Asserts what idle means, one cause at a time: a lease, a `leases` count with no lease
/// named, a release in progress and each of the four marks keep the host from reading idle for
/// the whole window, so no idle line is printed, while the cordon is still renewed and lifted.
/// It catches an idle test that forgets one of the conditions of the operator's drain gate.
#[test]
fn a_lease_a_release_or_a_mark_keeps_the_host_from_reading_idle() {
    type Busy = (u64, Option<&'static str>, bool, Option<&'static str>);
    let cases: [Busy; 7] = [
        (1, Some("1-1"), false, None),
        (1, None, false, None),
        (0, None, true, None),
        (0, None, false, Some("reserved")),
        (0, None, false, Some("held")),
        (0, None, false, Some("draining")),
        (0, None, false, Some("releasing")),
    ];
    for (leases, named, releasing, mark) in cases {
        let (lease, lines) = (FakeLease::new(), Lines::default());
        lease.set(|state| {
            state.leases = leases;
            state.lease = named.map(str::to_owned);
            state.releasing = releasing;
            state.mark = mark.map(str::to_owned);
        });
        let outcome = run(&lease, &mut FakePace::new(), &plan(300, 60), &lines);
        let cordoned = format!("cordoned host=h-1 expires_in_ms=300000 leases={leases}");
        assert_eq!(outcome, Outcome::Done, "{named:?} {releasing} {mark:?}");
        assert_eq!(lines.all(), [cordoned.as_str(), LIFTED]);
        assert!(lease.calls().contains(&Call::Host));
    }
}

/// Asserts that a stop request lifts the cordon the verb placed, whether it comes after two
/// renewals or before the first wait: the last call is the uncordon, the lines end with
/// `lifted`, and no time passes after the stop. It catches a stop that leaves the cordon to
/// lapse and one that is only noticed at the next renewal.
#[test]
fn a_stop_request_lifts_the_cordon_it_placed() {
    let (lease, lines) = (FakeLease::new(), Lines::default());
    let mut pace = FakePace::new().stop_at(2);
    assert_eq!(
        run(&lease, &mut pace, &plan(300, 3600), &lines),
        Outcome::Done
    );
    assert_eq!(pace.elapsed(), secs(200));
    assert_eq!(
        lease.calls(),
        [
            Call::Cordon(300),
            Call::Cordon(300),
            Call::Cordon(300),
            Call::Uncordon
        ]
    );
    assert_eq!(lines.all(), [CORDONED, IDLE, LIFTED]);

    let (lease, lines) = (FakeLease::new(), Lines::default());
    let mut pace = FakePace::new().stop_at(0);
    run(&lease, &mut pace, &plan(300, 3600), &lines);
    assert_eq!(lease.calls(), [Call::Cordon(300), Call::Uncordon]);
    assert_eq!(pace.elapsed(), Duration::ZERO);
}

/// Asserts that a cordon found in force is another holder's: the verb makes the one call that
/// told it, prints `kept ... reason=already_cordoned`, does not wait, renew or lift, and the
/// cordon stays. It catches a verb that lifts a cordon it did not place.
#[test]
fn a_cordon_that_was_there_already_is_neither_renewed_nor_lifted() {
    let (lease, lines) = (FakeLease::new(), Lines::default());
    lease.set(|state| state.cordoned = true);
    let mut pace = FakePace::new();
    let outcome = run(&lease, &mut pace, &plan(300, 3600), &lines);
    assert_eq!(outcome, Outcome::OtherHolder);
    assert_eq!(lease.calls(), [Call::Cordon(300)]);
    assert_eq!(lines.all(), ["kept host=h-1 reason=already_cordoned"]);
    assert!(pace.waits.is_empty());
    assert!(lease.cordoned());
}

/// Asserts that a reply naming another host stops the verb and lifts nothing, whichever reply
/// it is: the first cordon (nothing is printed), a renewal, a read of the host, and the
/// uncordon itself (which was sent, and whose `lifted` is not printed). Each ends as a
/// failure whose text names both hosts. It catches an uncordon sent on the word of a server
/// that is talking about another host.
#[test]
fn a_reply_that_names_another_host_lifts_nothing() {
    let busy = |lease: &FakeLease| {
        lease.set(|state| {
            state.lease = Some("1-1".to_owned());
            state.leases = 1;
        });
    };
    // (the call that names h-2, whether the host is busy, calls before it ends, lines)
    type Case = (Kind, usize, bool, Vec<Call>, usize);
    let cases: [Case; 4] = [
        (Kind::Cordon, 0, false, vec![Call::Cordon(300)], 0),
        (
            Kind::Cordon,
            1,
            false,
            vec![Call::Cordon(300), Call::Cordon(300)],
            2,
        ),
        (Kind::Host, 0, true, vec![Call::Cordon(300), Call::Host], 1),
        (
            Kind::Uncordon,
            0,
            false,
            vec![Call::Cordon(300), Call::Cordon(300)],
            2,
        ),
    ];
    for (kind, nth, busy_host, mut calls, printed) in cases {
        let (lease, lines) = (FakeLease::new(), Lines::default());
        if busy_host {
            busy(&lease);
        }
        lease.on(kind, nth, Effect::Name("h-2"));
        // The uncordon case needs the window to end: 200 s with a renewal every 100 s.
        let window = if kind == Kind::Uncordon { 200 } else { 3600 };
        let outcome = run(&lease, &mut FakePace::new(), &plan(300, window), &lines);
        let Outcome::Failed(text) = outcome else {
            panic!("{kind:?} {nth}: {outcome:?}");
        };
        assert!(
            text.contains("\"h-2\"") && text.contains("\"h-1\""),
            "{text}"
        );
        assert!(text.starts_with("server error: "), "{text}");
        if kind == Kind::Uncordon {
            calls.push(Call::Uncordon);
        }
        assert_eq!(lease.calls(), calls, "{kind:?} {nth}");
        assert_eq!(lines.all().len(), printed, "{kind:?} {nth}");
        assert!(!lines.all().iter().any(|line| line.starts_with("lifted")));
    }
}

/// Asserts how a failing server is told: the failed call names itself and whose fault it is
/// (`server error` for an unreachable server, a refusal and an unknown host; `credential
/// error` for the codes that mean the credential was not accepted), a failure after the
/// cordon was placed still tries to lift it, and a failure of the first cordon sends nothing
/// more. It catches an error that blames the server for a bad credential, one that hides the
/// call, and a failure that leaves the cordon we placed in force without an attempt to lift
/// it.
#[test]
fn a_server_error_ends_the_window_and_says_which_call_and_whose_fault() {
    let unreachable = || LeaseError::Unreachable("boom".to_owned());
    let refused = |code: &str| LeaseError::Refused(code.to_owned());
    let busy = |lease: &FakeLease| lease.set(|state| state.leases = 1);

    // The first cordon fails: nothing was placed that is known, so nothing is lifted.
    let (lease, lines) = (FakeLease::new(), Lines::default());
    lease.on(Kind::Cordon, 0, Effect::Fail(unreachable()));
    let outcome = run(&lease, &mut FakePace::new(), &plan(300, 600), &lines);
    let failed = Outcome::Failed("server error: cannot cordon the host: boom".to_owned());
    assert_eq!(outcome, failed);
    assert_eq!(lease.calls(), [Call::Cordon(300)]);
    assert!(lines.all().is_empty());

    // A renewal fails: the cordon is lifted all the same.
    let (lease, lines) = (FakeLease::new(), Lines::default());
    lease.on(Kind::Cordon, 1, Effect::Fail(unreachable()));
    let outcome = run(&lease, &mut FakePace::new(), &plan(300, 600), &lines);
    let failed = Outcome::Failed("server error: cannot renew the cordon: boom".to_owned());
    assert_eq!(outcome, failed);
    assert_eq!(lease.calls().last(), Some(&Call::Uncordon));
    assert_eq!(lines.all().last().map(String::as_str), Some(LIFTED));

    // A read of the host fails.
    let (lease, lines) = (FakeLease::new(), Lines::default());
    busy(&lease);
    lease.on(Kind::Host, 0, Effect::Fail(refused("invalid-cordon")));
    let outcome = run(&lease, &mut FakePace::new(), &plan(300, 600), &lines);
    let text = "server error: cannot read the host: the server refused (invalid-cordon)";
    assert_eq!(outcome, Outcome::Failed(text.to_owned()));
    assert_eq!(lease.calls().last(), Some(&Call::Uncordon));

    // The uncordon fails: the window itself went well.
    let (lease, lines) = (FakeLease::new(), Lines::default());
    lease.on(Kind::Uncordon, 0, Effect::Fail(LeaseError::UnknownHost));
    let outcome = run(&lease, &mut FakePace::new(), &plan(300, 600), &lines);
    let text = "server error: cannot lift the cordon: the server holds no such host (it has not \
                said hello since the server started)";
    assert_eq!(outcome, Outcome::Failed(text.to_owned()));
    assert_eq!(lines.all(), [CORDONED, IDLE]);

    // The server does not accept the credential.
    for code in [
        "host-credential-required",
        "join-token-not-admitted",
        "join-token-expired",
        "http-401",
    ] {
        let (lease, lines) = (FakeLease::new(), Lines::default());
        lease.on(Kind::Cordon, 0, Effect::Fail(refused(code)));
        let outcome = run(&lease, &mut FakePace::new(), &plan(300, 600), &lines);
        let text = format!(
            "credential error: cannot cordon the host: the server refused the credential ({code})"
        );
        assert_eq!(outcome, Outcome::Failed(text));
    }
}

/// Asserts that a cordon which lapsed before the end of the window is reported as `lapsed`
/// and the verb still ends well: the uncordon found nothing to lift (`lifted: false`). It
/// catches a verb that claims to have lifted a cordon that was not there, and one that treats
/// the lapse as a failure.
#[test]
fn a_cordon_that_lapsed_by_the_end_is_reported_as_lapsed() {
    let (lease, lines) = (FakeLease::new(), Lines::default());
    let mut pace = FakePace::new().on_wait({
        let lease = lease.clone();
        move |_, _| lease.set(|state| state.cordoned = false)
    });
    // 100 s window, a renewal every 100 s: one wait, and the window is over before a renewal.
    let outcome = run(&lease, &mut pace, &plan(300, 100), &lines);
    assert_eq!(outcome, Outcome::Done);
    assert_eq!(lines.all(), [CORDONED, IDLE, "lapsed host=h-1"]);
}

/// Asserts that the window is measured on the clock and not by adding up the waits: with a
/// server that takes 50 s to answer each call, the window of an hour ends when the clock says
/// an hour has passed since the cordon was placed (and the uncordon's 50 s), within one call
/// of it. It catches a verb that counts only its sleeps, which holds the cordon for longer the
/// slower the server is.
#[test]
fn the_window_is_counted_on_the_clock_and_not_in_waits() {
    let (lease, lines) = (FakeLease::new(), Lines::default());
    let mut pace = FakePace::new();
    lease.slow(&pace.clock(), secs(50));
    let outcome = run(&lease, &mut pace, &plan(300, 3600), &lines);
    assert_eq!(outcome, Outcome::Done);
    // The clock when the first cordon answered, the window, and the uncordon's own 50 s.
    let elapsed = pace.elapsed().as_secs();
    assert!((3700..3750).contains(&elapsed), "{elapsed}");
}

/// Asserts that a cordon this verb lets lapse (the process was stopped for longer than
/// `lease_secs`, so it renewed nothing) and that another holder then placed is not lifted: the
/// renewal after the lapse finds the host cordoned, which cannot be the verb's own cordon any
/// more, so the verb says `kept` with the reason, makes no uncordon and ends as another holder's
/// cordon. It catches a verb that takes every renewal as its own and lifts another holder's
/// cordon at the end of its window.
#[test]
fn a_cordon_taken_by_another_holder_after_ours_lapsed_is_kept() {
    let (lease, lines) = (FakeLease::new(), Lines::default());
    let pace = FakePace::new();
    let clock = pace.clock();
    // During the first wait the process is stopped for 400 s more: the cordon of 300 s lapses
    // and another holder cordons the host (the server reads it cordoned).
    let mut pace = pace.on_wait(move |waits, _| {
        if waits == 1 {
            clock.set(clock.get() + secs(400));
        }
    });
    let outcome = run(&lease, &mut pace, &plan(300, 3600), &lines);
    assert_eq!(outcome, Outcome::OtherHolder);
    assert_eq!(lease.calls(), [Call::Cordon(300), Call::Cordon(300)]);
    assert_eq!(
        lines.all(),
        [CORDONED, IDLE, "kept host=h-1 reason=taken_after_lapse"]
    );
    assert!(lease.cordoned());
}

/// Asserts that a cordon this verb let lapse and that nobody took is placed again by the next
/// renewal (the reply says the host was not cordoned), said again with a `cordoned` line, held
/// for the rest of the window and lifted at its end. It catches a verb that gives up its window
/// after a lapse, or one that lifts nothing at the end because it no longer counts the cordon
/// as its own.
#[test]
fn a_cordon_that_lapsed_and_was_not_taken_is_placed_again_and_lifted() {
    let (lease, lines) = (FakeLease::new(), Lines::default());
    let pace = FakePace::new();
    let clock = pace.clock();
    let mut pace = pace.on_wait({
        let lease = lease.clone();
        move |waits, _| {
            if waits == 1 {
                clock.set(clock.get() + secs(400));
                lease.set(|state| state.cordoned = false);
            }
        }
    });
    let outcome = run(&lease, &mut pace, &plan(300, 3600), &lines);
    assert_eq!(outcome, Outcome::Done);
    assert_eq!(lease.calls().last(), Some(&Call::Uncordon));
    assert_eq!(lines.all(), [CORDONED, IDLE, CORDONED, LIFTED]);
    assert!(!lease.cordoned());
}

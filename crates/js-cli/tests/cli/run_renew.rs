//! `lease run` and time: the renewal period and the deadline read off the expiry the
//! server answers with, and renewals that are slow.

use std::fs;
use std::thread::scope;

use js_cli::ForwardedSignal;

use crate::procs::{fifo, path_str, secs};
use crate::servers::Gated;
use crate::support::{EndsRun, Harness, Manual, Scratch, T0, against, http, run_words, sequence};

/// Asserts that the renewal period is a third of the time the server's last answer left,
/// whatever ttl was asked for: with the server's default ttl of 60s (no `--ttl`) it is
/// 20s, and with `--ttl 10m`, which the server clamps to its maximum of 120s, it is 40s;
/// each renewal is done before the lease expires (the server lists the expiry it granted
/// from the time of the renewal), and the command, which waits for the test, ends with 0.
#[test]
fn the_period_follows_the_ttl_the_server_grants() {
    let cases: [(&[&str], u64, u64); 2] = [(&[], 60, 20), (&["--ttl", "10m"], 120, 40)];
    for (n, (flags, granted, period)) in cases.into_iter().enumerate() {
        let server = ["--default-ttl", "60s", "--max-ttl", "120s"];
        let h = Harness::build(&format!("renew_clamp_{n}"), &server, None);
        let waiter = Manual::new();
        let (services, signals) = h.js.services_with_signals(waiter.clone());
        let go = fifo(&h.scratch, "go");
        let words = run_words(flags, &["sh", "-c", r#"read line < "$0""#, path_str(&go)]);
        let (got, expiries) = scope(|s| {
            let run = s.spawn(|| h.js.with(&words, services));
            let _ends = EndsRun(signals.recv().unwrap(), ForwardedSignal::Terminate);
            let mut expiries = Vec::new();
            for _ in 0..2 {
                h.advance(secs(period));
                waiter.step();
                let listed = h.js.go(&["--json", "lease", "list"]).json();
                expiries.push(listed["leases"][0]["expires_at_ms"].as_u64().unwrap());
            }
            fs::write(&go, "done\n").unwrap();
            (run.join().unwrap(), expiries)
        });
        assert_eq!(got.code, 0, "{}", got.err);
        let granted_ms = granted * 1000;
        let now = T0 + period * 1000;
        assert_eq!(
            expiries,
            vec![now + granted_ms, now + period * 1000 + granted_ms]
        );
        assert_eq!(waiter.intervals(), vec![secs(period); 3]);
        assert!(h.listed().is_empty());
    }
}

/// Asserts that a server that cannot be reached until the deadline (one period short of
/// the expiry, here 400s of the 600s of the default ttl) loses the lease at the deadline
/// and not later: the one renewal that was tried failed and was reported, the wait that
/// followed was the time left to the deadline, and the command was stopped, `js` exiting
/// 7 with the injected clock (and the server's, which it is) still 200s before the lease
/// would be expired.
#[test]
fn a_lease_that_cannot_be_renewed_is_given_up_before_the_server_expires_it() {
    let mut h = Harness::new("renew_deadline");
    let waiter = Manual::new();
    let (services, signals) = h.js.services_with_signals(waiter.clone());
    let words = run_words(&[], &["sleep", "30"]);
    let js = h.js.clone();
    let got = scope(|s| {
        let run = s.spawn(|| js.with(&words, services));
        let _ends = EndsRun(signals.recv().unwrap(), ForwardedSignal::Terminate);
        waiter.running();
        h.stop();
        h.advance(secs(200));
        waiter.step();
        h.advance(secs(200));
        waiter.release();
        run.join().unwrap()
    });
    assert_eq!(got.code, 7, "{}", got.err);
    assert_eq!(
        got.err
            .matches("js: could not renew lease 1-1: cannot talk to")
            .count(),
        1,
        "{}",
        got.err
    );
    assert!(
        got.err.contains(
            "js: lease 1-1 could not be renewed before its deadline; stopped the command\n"
        ),
        "{}",
        got.err
    );
    // The period, the time left to the deadline, and then the grace period of the stop.
    assert_eq!(waiter.intervals(), vec![secs(200), secs(200), secs(30)]);
    let expires = T0 + 600_000;
    let at_loss = js_lease::Clock::now(&h.clock).millis();
    assert_eq!(at_loss, T0 + 400_000);
    assert!(at_loss < expires);
    assert!(
        got.err.contains(&format!("expires {expires}\n")),
        "{}",
        got.err
    );
}

/// Asserts that `js` does not wait for a renewal the server holds back: a signal sent to
/// it meanwhile reaches the command, which ends the run (143) while the renewal is still
/// unanswered (nothing is reported about it), and the lease is released.
#[test]
fn a_signal_is_forwarded_while_a_renewal_is_in_flight() {
    let scratch = Scratch::new("renew_blocked_signal");
    let gated = Gated::new(T0 + 600_000);
    let js = against(&gated.url, &scratch);
    let waiter = Manual::new();
    let (services, signals) = js.services_with_signals(waiter.clone());
    let ready = fifo(&scratch, "ready");
    let script = r#"echo $$ > "$0"; exec sleep 30"#;
    let words = run_words(&[], &["sh", "-c", script, path_str(&ready)]);
    let got = scope(|s| {
        let run = s.spawn(|| js.with(&words, services));
        let signaller = signals.recv().unwrap();
        fs::read_to_string(&ready).unwrap();
        waiter.release();
        gated.renewing.recv().unwrap();
        signaller.deliver(ForwardedSignal::Terminate);
        let got = run.join().unwrap();
        gated.open();
        got
    });
    assert_eq!(got.code, 143, "{}", got.err);
    assert!(!got.err.contains("could not renew"), "{}", got.err);
    assert_eq!(
        gated.requests(),
        vec![
            "POST /v1/leases",
            "POST /v1/leases/1-1/renew",
            "DELETE /v1/leases/1-1"
        ]
    );
}

/// Asserts that `js` does not wait for a renewal the server holds back when the command
/// ends on its own either: its status (0) is `js`'s, nothing is reported about the
/// renewal, and the lease is released.
#[test]
fn the_end_of_the_command_is_seen_while_a_renewal_is_in_flight() {
    let scratch = Scratch::new("renew_blocked_exit");
    let gated = Gated::new(T0 + 600_000);
    let js = against(&gated.url, &scratch);
    let waiter = Manual::new();
    let (services, _signals) = js.services_with_signals(waiter.clone());
    let (ready, go) = (fifo(&scratch, "ready"), fifo(&scratch, "go"));
    let script = r#"echo $$ > "$0"; read line < "$1""#;
    let words = run_words(&[], &["sh", "-c", script, path_str(&ready), path_str(&go)]);
    let got = scope(|s| {
        let run = s.spawn(|| js.with(&words, services));
        fs::read_to_string(&ready).unwrap();
        waiter.release();
        gated.renewing.recv().unwrap();
        fs::write(&go, "done\n").unwrap();
        let got = run.join().unwrap();
        gated.open();
        got
    });
    assert_eq!(got.code, 0, "{}", got.err);
    assert!(!got.err.contains("could not renew"), "{}", got.err);
    let requests = gated.requests();
    assert_eq!(requests.last().unwrap(), "DELETE /v1/leases/1-1");
}

/// Asserts that a lease the server grants with an expiry that is already past by the
/// injected clock (the clocks disagree, or the ttl is too short to hold) is released
/// without running the command, with a message that names the clocks.
#[test]
fn a_lease_that_is_already_expired_by_the_clock_is_not_run() {
    let scratch = Scratch::new("renew_skew");
    let (url, log) = sequence(vec![
        http(
            "201 Created",
            r#"{"id":"1-1","host":"gpu-01","expires_at_ms":5}"#,
        ),
        http("200 OK", "{}"),
    ]);
    let ran = scratch.path("ran");
    let script = r#"echo ran > "$0""#;
    let words = run_words(&[], &["sh", "-c", script, path_str(&ran)]);
    let got = against(&url, &scratch).waiting(&words, Manual::new());
    assert_eq!(got.code, 1, "{}", got.err);
    assert!(
        got.err.contains(
            "js: lease 1-1 expires at 5 ms, too soon after this machine's clock (1000000 ms) to hold it: are the clocks of this machine and the server in step?\n"
        ),
        "{}",
        got.err
    );
    assert!(!ran.exists());
    let seen = log.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert!(seen[1].starts_with("DELETE /v1/leases/1-1"), "{seen:?}");
}

/// Asserts that a renewal that fails after the deadline has passed (the injected clock
/// is 450s on, the deadline is at 400s) gives the lease up at once, without waiting for a
/// deadline report: the command is stopped and `js` exits 7.
#[test]
fn a_renewal_that_fails_after_the_deadline_gives_the_lease_up() {
    let mut h = Harness::new("renew_late");
    let waiter = Manual::new();
    let (services, signals) = h.js.services_with_signals(waiter.clone());
    let words = run_words(&[], &["sleep", "30"]);
    let js = h.js.clone();
    let got = scope(|s| {
        let run = s.spawn(|| js.with(&words, services));
        let _ends = EndsRun(signals.recv().unwrap(), ForwardedSignal::Terminate);
        waiter.running();
        h.stop();
        h.advance(secs(450));
        waiter.release();
        run.join().unwrap()
    });
    assert_eq!(got.code, 7, "{}", got.err);
    assert!(
        got.err.contains(
            "js: lease 1-1 could not be renewed before its deadline; stopped the command\n"
        ),
        "{}",
        got.err
    );
    assert_eq!(waiter.intervals(), vec![secs(200), secs(30)]);
}

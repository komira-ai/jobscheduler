//! `lease run`: the command's environment and status, renewals through the waiter,
//! the release afterwards, and the ways the run ends early.

use std::fs;
use std::io;
use std::sync::Arc;
use std::thread::scope;
use std::time::Duration;

use js_cli::{ForwardedSignal, Services, Waiter};
use js_provider::Call;

use crate::support::{
    Harness, Manual, Outcome, T0, acquire_words, run_words, services_with_signals,
};

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// Runs `lease run` with `extra` flags and `command`, with a waiter nobody steps.
fn run_idle(h: &Harness, extra: &[&str], command: &[&str]) -> Outcome {
    h.js.waiting(&run_words(extra, command), Manual::new())
}

/// Asserts that the command is a child of `js` with `JS_LEASE_ID` and `JS_LEASE_HOST` in
/// its environment, that its stdout is its own (`js` prints the held lease on stderr),
/// and that the lease is released when it ends.
#[test]
fn the_command_gets_the_lease_in_its_environment() {
    let h = Harness::new("run_env");
    let file = h.scratch.path("seen");
    let script = r#"echo "$JS_LEASE_ID $JS_LEASE_HOST" > "$0""#;
    let got = run_idle(&h, &[], &["sh", "-c", script, file.to_str().unwrap()]);
    assert_eq!(got.code, 0, "{}", got.err);
    assert_eq!(got.out, "");
    assert_eq!(
        got.err,
        format!("lease 1-1 host gpu-01 expires {}\n", T0 + 600_000)
    );
    assert_eq!(fs::read_to_string(file).unwrap(), "1-1 gpu-01\n");
    assert!(h.listed().is_empty());
}

/// Asserts that the held lease is printed as JSON on stderr with `--json`.
#[test]
fn the_held_lease_is_json_with_json() {
    let h = Harness::new("run_json");
    let got = run_idle(&h, &["--json"], &["true"]);
    assert_eq!(got.code, 0, "{}", got.err);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&got.err).unwrap()["host"],
        "gpu-01"
    );
}

/// Asserts that the status of the command is the status of `js`, whatever it is, and
/// that a command a signal ended gives 128 plus the signal; the lease is released each
/// time, so the next run gets the same host.
#[test]
fn the_exit_status_of_the_command_passes_through() {
    let h = Harness::new("run_status");
    let cases: [(&[&str], u8); 5] = [
        (&["true"], 0),
        (&["false"], 1),
        (&["sh", "-c", "exit 3"], 3),
        (&["sh", "-c", "exit 255"], 255),
        (&["sh", "-c", "kill -KILL $$"], 137),
    ];
    for (n, (command, code)) in cases.into_iter().enumerate() {
        let got = run_idle(&h, &[], command);
        assert_eq!(got.code, code, "{command:?}: {}", got.err);
        let held = format!("lease 1-{} host gpu-01", n + 1);
        assert!(got.err.starts_with(&held), "{}", got.err);
        assert!(h.listed().is_empty());
    }
}

/// Asserts that a command that cannot be started is exit 127 naming it, that the lease
/// taken for it is released, and that nothing else ran.
#[test]
fn a_command_that_cannot_start_exits_127_and_releases() {
    let h = Harness::new("run_no_command");
    let got = run_idle(&h, &[], &["/no/such/command", "arg"]);
    assert_eq!(got.code, 127, "{}", got.err);
    assert!(
        got.err.contains("js: cannot start /no/such/command: "),
        "{}",
        got.err
    );
    assert!(h.listed().is_empty());
}

/// Asserts that a start that is refused runs nothing and exits like `acquire` would.
#[test]
fn a_refused_start_runs_no_command() {
    let h = Harness::new("run_refused");
    assert_eq!(h.js.go(&acquire_words(&[])).code, 0);
    assert_eq!(h.js.go(&acquire_words(&[])).code, 0);
    let file = h.scratch.path("ran");
    let script = r#"echo ran > "$0""#;
    let got = run_idle(&h, &[], &["sh", "-c", script, file.to_str().unwrap()]);
    assert_eq!(
        (got.code, got.err.as_str()),
        (3, "js: refused: no-capacity\n")
    );
    assert!(!file.exists());
}

/// Asserts that renewals go out through the waiter at the period asked (a third of the
/// ttl), each extending the lease from the server's clock, and that SIGTERM sent to `js`
/// ends the command (status 143) and the lease is released.
#[test]
fn the_lease_is_renewed_every_period_until_a_signal_ends_the_run() {
    let h = Harness::new("run_renews");
    let waiter = Manual::new();
    let (services, signals) = services_with_signals(waiter.clone());
    let words = run_words(&["--ttl", "60s"], &["sleep", "600"]);
    let got = scope(|s| {
        let run = s.spawn(|| h.js.with(&words, services));
        let signaller = signals.recv().unwrap();
        h.advance(secs(50));
        waiter.step();
        let listed = h.js.go(&["--json", "lease", "list"]).json();
        assert_eq!(listed["leases"][0]["expires_at_ms"], T0 + 50_000 + 60_000);
        signaller.deliver(ForwardedSignal::Terminate);
        run.join().unwrap()
    });
    assert_eq!(got.code, 143, "{}", got.err);
    assert_eq!(waiter.intervals(), vec![secs(20), secs(20)]);
    assert!(h.listed().is_empty());
}

/// Asserts that SIGINT is forwarded too (status 130), that `--renew-every` sets the
/// period, and that a command which ignores nothing is the one stopped.
#[test]
fn sigint_is_forwarded_and_the_period_can_be_set() {
    let h = Harness::new("run_sigint");
    let waiter = Manual::new();
    let (services, signals) = services_with_signals(waiter.clone());
    let words = run_words(&["--ttl", "60s", "--renew-every", "7s"], &["sleep", "600"]);
    let got = scope(|s| {
        let run = s.spawn(|| h.js.with(&words, services));
        waiter.step();
        signals.recv().unwrap().deliver(ForwardedSignal::Interrupt);
        run.join().unwrap()
    });
    assert_eq!(got.code, 130, "{}", got.err);
    assert_eq!(waiter.intervals(), vec![secs(7), secs(7)]);
}

/// Asserts that a renewal that finds the lease gone (the server expired it) stops the
/// command, waits for it, exits 7 and does not try to release; without `--ttl` the
/// period is 200s, a third of the server's default ttl.
#[test]
fn a_lost_lease_stops_the_command_and_exits_seven() {
    let h = Harness::new("run_lost");
    let waiter = Manual::new();
    let (services, signals) = services_with_signals(waiter.clone());
    let words = run_words(&[], &["sleep", "600"]);
    let got = scope(|s| {
        let run = s.spawn(|| h.js.with(&words, services));
        let _lease_is_held = signals.recv().unwrap();
        h.advance(secs(601));
        h.tick();
        waiter.step();
        run.join().unwrap()
    });
    assert_eq!(got.code, 7, "{}", got.err);
    assert!(
        got.err
            .contains("js: lease 1-1 is gone from the server; stopped the command"),
        "{}",
        got.err
    );
    assert!(!got.err.contains("could not release"), "{}", got.err);
    assert_eq!(waiter.intervals()[0], secs(200));
    assert!(h.listed().is_empty());
}

/// Asserts that a renewal that fails for another reason (the server is gone) is
/// reported and tried again at the next period instead of stopping the command, and
/// that failing to release at the end is reported without changing the status.
#[test]
fn a_renewal_that_cannot_reach_the_server_is_retried() {
    let mut h = Harness::new("run_unreachable");
    let waiter = Manual::new();
    let (services, signals) = services_with_signals(waiter.clone());
    let words = run_words(&["--ttl", "60s"], &["sleep", "600"]);
    let js = h.js.clone();
    let got = scope(|s| {
        let run = s.spawn(|| js.with(&words, services));
        let signaller = signals.recv().unwrap();
        h.stop();
        waiter.step();
        waiter.step();
        signaller.deliver(ForwardedSignal::Terminate);
        run.join().unwrap()
    });
    assert_eq!(got.code, 143, "{}", got.err);
    assert_eq!(
        got.err
            .matches("js: could not renew lease 1-1: cannot talk to")
            .count(),
        2,
        "{}",
        got.err
    );
    assert!(
        got.err
            .contains("js: could not release lease 1-1: cannot talk to"),
        "{}",
        got.err
    );
}

/// Asserts that a signal source that cannot be installed ends the run before the
/// command starts, releases the lease and exits 1.
#[test]
fn a_signal_source_that_fails_ends_the_run() {
    let h = Harness::new("run_no_signals");
    let file = h.scratch.path("ran");
    let script = r#"echo ran > "$0""#;
    let words = run_words(&[], &["sh", "-c", script, file.to_str().unwrap()]);
    let failing = Services::new(
        Manual::new(),
        Box::new(|_| Err(io::Error::other("no handlers today"))),
    );
    let got = h.js.with(&words, failing);
    assert_eq!(got.code, 1, "{}", got.err);
    assert!(
        got.err
            .ends_with("js: cannot watch for signals: no handlers today\n"),
        "{}",
        got.err
    );
    assert!(!file.exists());
    assert!(h.listed().is_empty());
}

/// Asserts that a release the provider has not confirmed after the command is reported
/// as releasing, with the command's own status kept.
#[test]
fn a_release_that_is_not_confirmed_is_reported() {
    let h = Harness::scripted("run_releasing", vec![(Call::Stop, false)]);
    let got = run_idle(&h, &[], &["sh", "-c", "exit 4"]);
    assert_eq!(got.code, 4, "{}", got.err);
    assert!(
        got.err
            .contains("js: lease 1-1 is releasing; the server retries the stop\n"),
        "{}",
        got.err
    );
    h.tick();
    assert!(h.listed().is_empty());
}

/// Asserts that a pending start in `run` settles through the same waiter before the
/// command starts, and that the renewal clock starts after that.
#[test]
fn a_pending_start_settles_before_the_command_runs() {
    let h = Harness::scripted("run_pending", vec![(Call::Start, true)]);
    let ticking = h.ticking();
    let first = Arc::new(Settle {
        ticking: ticking.clone(),
        manual: Manual::new(),
    });
    let got = h.js.waiting(&run_words(&[], &["true"]), first);
    assert_eq!(got.code, 0, "{}", got.err);
    assert!(got.err.starts_with("lease 1-1 host h1 "), "{}", got.err);
    assert_eq!(ticking.asked(), vec![secs(2)]);
}

/// A waiter that ticks the server for the pending poll (an interval of two seconds) and
/// holds every other wait until the end.
struct Settle {
    ticking: Arc<crate::support::Driven>,
    manual: Arc<Manual>,
}

impl Waiter for Settle {
    fn wait(&self, interval: Duration) {
        match interval == secs(2) {
            true => self.ticking.wait(interval),
            false => self.manual.wait(interval),
        }
    }
}

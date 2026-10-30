//! `lease run`: the command's environment and status, renewals through the waiter,
//! the release afterwards, the signals at every stage, and the ways the run ends early.

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};
use std::thread::scope;
use std::time::Duration;

use js_cli::{ForwardedSignal, Services, Signaller, Waiter};
use js_provider::Call;

use crate::procs::{assert_reaped, fifo, path_str, secs};
#[cfg(target_os = "linux")]
use crate::support::TOKEN;
use crate::support::{
    EndsRun, FixedClock, Harness, Manual, Never, Outcome, Scratch, T0, acquire_words, against,
    http, run_words, sequence, services,
};

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
    let got = run_idle(&h, &[], &["sh", "-c", script, path_str(&file)]);
    assert_eq!(got.code, 0, "{}", got.err);
    assert_eq!(got.out, "");
    assert_eq!(
        got.err,
        format!("lease 1-1 host gpu-01 expires {}\n", T0 + 600_000)
    );
    assert_eq!(fs::read_to_string(file).unwrap(), "1-1 gpu-01\n");
    assert!(h.listed().is_empty());
}

/// Asserts that the token reaches the command neither in its environment nor as an open
/// file: the environment the command dumps does not contain the token string, and no
/// descriptor it holds (the links of `/proc/self/fd`) points at the token file.
#[test]
#[cfg(target_os = "linux")]
fn the_token_does_not_reach_the_command() {
    let h = Harness::new("run_token");
    let (env, fds) = (h.scratch.path("environment"), h.scratch.path("descriptors"));
    let script = r#"env > "$0"; ls -l /proc/self/fd > "$1""#;
    let command = ["sh", "-c", script, path_str(&env), path_str(&fds)];
    let got = run_idle(&h, &[], &command);
    assert_eq!(got.code, 0, "{}", got.err);
    let environment = fs::read_to_string(&env).unwrap();
    assert!(environment.contains("JS_LEASE_ID=1-1"), "{environment}");
    assert!(!environment.contains(TOKEN), "{environment}");
    let descriptors = fs::read_to_string(&fds).unwrap();
    assert!(descriptors.contains("->"), "{descriptors}");
    assert!(!descriptors.contains(TOKEN), "{descriptors}");
    assert!(
        !descriptors.contains(path_str(&h.js.token_file)),
        "{descriptors}"
    );
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
    let got = run_idle(&h, &[], &["sh", "-c", script, path_str(&file)]);
    assert_eq!(
        (got.code, got.err.as_str()),
        (3, "js: refused: no-capacity\n")
    );
    assert!(!file.exists());
}

/// Asserts that renewals go out through the waiter at the period asked (a third of the
/// ttl), each extending the lease from the server's clock, and that SIGTERM sent to `js`
/// ends the command (status 143) and the lease is released. The values are read inside
/// the run and checked after it, so a failed check cannot leave the run waiting.
#[test]
fn the_lease_is_renewed_every_period_until_a_signal_ends_the_run() {
    let h = Harness::new("run_renews");
    let waiter = Manual::new();
    let (services, signals) = h.js.services_with_signals(waiter.clone());
    let words = run_words(&["--ttl", "60s"], &["sleep", "30"]);
    let (got, listed) = scope(|s| {
        let run = s.spawn(|| h.js.with(&words, services));
        let ends = EndsRun(signals.recv().unwrap(), ForwardedSignal::Terminate);
        waiter.running();
        h.advance(secs(50));
        waiter.step();
        let listed = h.js.go(&["--json", "lease", "list"]).json();
        drop(ends);
        (run.join().unwrap(), listed)
    });
    assert_eq!(listed["leases"][0]["expires_at_ms"], T0 + 50_000 + 60_000);
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
    let (services, signals) = h.js.services_with_signals(waiter.clone());
    let words = run_words(&["--ttl", "60s", "--renew-every", "7s"], &["sleep", "30"]);
    let got = scope(|s| {
        let run = s.spawn(|| h.js.with(&words, services));
        let ends = EndsRun(signals.recv().unwrap(), ForwardedSignal::Interrupt);
        waiter.step();
        drop(ends);
        run.join().unwrap()
    });
    assert_eq!(got.code, 130, "{}", got.err);
    assert_eq!(waiter.intervals(), vec![secs(7), secs(7)]);
}

/// Asserts that a renewal that finds the lease gone (the server expired it) sends the
/// command SIGTERM, waits for it to end (it ran its own handler first, and no process
/// of that id is left), exits 7 and does not try to release; that without `--ttl` the
/// period is 200s, a third of the server's default ttl.
#[test]
fn a_lost_lease_stops_the_command_and_exits_seven() {
    let h = Harness::new("run_lost");
    let waiter = Manual::new();
    let (services, signals) = h.js.services_with_signals(waiter.clone());
    let ready = fifo(&h.scratch, "ready");
    let stopped = h.scratch.path("stopped");
    let script = r#"trap 'echo stopped > "$1"; exit 0' TERM; echo $$ > "$0"; sleep 30 & wait"#;
    let words = run_words(
        &[],
        &["sh", "-c", script, path_str(&ready), path_str(&stopped)],
    );
    let (got, pid) = scope(|s| {
        let run = s.spawn(|| h.js.with(&words, services));
        let _ends = EndsRun(signals.recv().unwrap(), ForwardedSignal::Terminate);
        let pid = fs::read_to_string(&ready).unwrap();
        h.advance(secs(601));
        h.tick();
        waiter.release();
        (run.join().unwrap(), pid)
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
    assert_eq!(fs::read_to_string(stopped).unwrap(), "stopped\n");
    assert_reaped(&pid);
    assert!(h.listed().is_empty());
}

/// A waiter that returns at once for the grace period (an interval it was made for) and
/// leaves every other wait to the test.
struct GraceFree {
    grace: Duration,
    manual: Arc<Manual>,
}

impl Waiter for GraceFree {
    fn wait(&self, interval: Duration) {
        match interval == self.grace {
            true => {}
            false => self.manual.wait(interval),
        }
    }
}

/// Asserts that a command that ignores SIGTERM is killed once the grace period has
/// passed, together with what it started (a process of its own group holding the pipe
/// open is gone when `js` returns, so the pipe reaches its end without the output the
/// survivor would have written), and that `js` then exits 7. The grace period is 30s by
/// default and `--stop-grace` sets it: the waiter lets only that interval pass.
#[test]
fn a_command_that_ignores_sigterm_is_killed_after_the_grace_period() {
    let cases: [(&[&str], u64); 2] = [(&[], 30), (&["--stop-grace", "5s"], 5)];
    for (n, (flags, grace)) in cases.into_iter().enumerate() {
        let h = Harness::new(&format!("run_grace_{n}"));
        let manual = Manual::new();
        let waiter = Arc::new(GraceFree {
            grace: secs(grace),
            manual: manual.clone(),
        });
        let (services, _signals) = h.js.services_with_signals(waiter);
        let pipe = fifo(&h.scratch, "pipe");
        let script = r#"trap "" TERM; sh -c 'echo $PPID; sleep 30; echo survived' > "$0" & wait"#;
        let mut options = vec!["--ttl", "60s", "--renew-every", "7s"];
        options.extend(flags);
        let words = run_words(&options, &["sh", "-c", script, path_str(&pipe)]);
        let (got, first, rest) = scope(|s| {
            let run = s.spawn(|| h.js.with(&words, services));
            let mut reader = BufReader::new(File::open(&pipe).unwrap());
            let mut first = String::new();
            reader.read_line(&mut first).unwrap();
            h.advance(secs(61));
            h.tick();
            manual.release();
            let got = run.join().unwrap();
            let mut rest = String::new();
            reader.read_to_string(&mut rest).unwrap();
            (got, first, rest)
        });
        assert_eq!(got.code, 7, "{}", got.err);
        assert_eq!(rest, "");
        assert_reaped(&first);
        assert_eq!(manual.intervals(), vec![secs(7)]);
    }
}

/// Asserts that a stopped server's port stays taken for the rest of the test: no other
/// server can bind it, and a request to it fails like one to a server that is gone.
#[test]
fn a_stopped_server_keeps_its_port_and_refuses_every_request() {
    let mut h = Harness::new("stopped_keeps_port");
    let addr = h.js.server.strip_prefix("http://").unwrap().to_owned();
    h.stop();
    let err = std::net::TcpListener::bind(&addr).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse, "{err}");
    let got = h.js.go(&["lease", "list"]);
    assert_ne!(got.code, 0, "{}", got.err);
    assert!(got.err.contains("cannot talk to"), "{}", got.err);
}

/// Asserts that a renewal that fails for another reason (the server is gone) is
/// reported and tried again at the next period instead of stopping the command, and
/// that a release that fails every try is reported, after the pauses between the tries
/// went through the waiter, and makes the status 8 in place of the command's own
/// (here 143), which is said.
#[test]
fn a_renewal_that_cannot_reach_the_server_is_retried() {
    let mut h = Harness::new("run_unreachable");
    let waiter = Manual::new();
    let (services, signals) = h.js.services_with_signals(waiter.clone());
    let words = run_words(&["--ttl", "60s"], &["sleep", "30"]);
    let js = h.js.clone();
    let got = scope(|s| {
        let run = s.spawn(|| js.with(&words, services));
        let ends = EndsRun(signals.recv().unwrap(), ForwardedSignal::Terminate);
        waiter.running();
        h.stop();
        waiter.step();
        waiter.step();
        drop(ends);
        run.join().unwrap()
    });
    assert_eq!(got.code, 8, "{}", got.err);
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
    assert!(got.err.contains("; gave up after 3 tries\n"), "{}", got.err);
    let said =
        "js: the command ended with status 143; exiting 8 because the lease may still be held\n";
    assert!(got.err.ends_with(said), "{}", got.err);
    assert_eq!(waiter.instant(), vec![secs(3), secs(3)]);
}

/// A stream that fails (by panic) when `js` reports a failed renewal on it.
struct Tripwire;

impl Write for Tripwire {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        assert!(
            !buf.starts_with(b"js: could not renew"),
            "the stream is broken"
        );
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Asserts that a panic of `js` while it supervises the command (here a stream that
/// panics when a failed renewal is reported) still kills the command, reaped, and
/// releases the lease: the server saw the start, the renewal and the release.
#[test]
fn a_panic_kills_the_command_and_releases_the_lease() {
    let scratch = Scratch::new("run_panic");
    let (url, log) = sequence(vec![
        http(
            "201 Created",
            &format!(
                r#"{{"id":"1-1","host":"gpu-01","expires_at_ms":{}}}"#,
                T0 + 600_000
            ),
        ),
        http("500 Internal Server Error", r#"{"error":"boom"}"#),
        http("200 OK", "{}"),
    ]);
    let ready = fifo(&scratch, "ready");
    let script = r#"echo $$ > "$0"; exec sleep 30"#;
    let words = run_words(&[], &["sh", "-c", script, path_str(&ready)]);
    let args = against(&url, &scratch).args(&words);
    let waiter = Manual::new();
    let (caught, pid) = scope(|s| {
        let run = s.spawn(|| {
            let mut out = Vec::new();
            let services = services(waiter.clone());
            catch_unwind(AssertUnwindSafe(|| {
                js_cli::run_with(args, &mut out, &mut Tripwire, services)
            }))
        });
        let pid = fs::read_to_string(&ready).unwrap();
        waiter.release();
        (run.join().unwrap(), pid)
    });
    assert!(caught.is_err());
    assert_reaped(&pid);
    let seen = log.lock().unwrap().clone();
    let routes: Vec<&str> = seen
        .iter()
        .map(|line| line.trim_end_matches(" HTTP/1.1"))
        .collect();
    assert_eq!(
        routes,
        vec![
            "POST /v1/leases",
            "POST /v1/leases/1-1/renew",
            "DELETE /v1/leases/1-1"
        ]
    );
}

/// Asserts that the signal source is installed before the lease is asked for, so a
/// source that cannot be installed ends the run with no lease taken at all (the next
/// start gets the first lease id) and no command run.
#[test]
fn a_signal_source_that_fails_ends_the_run_before_any_lease() {
    let h = Harness::new("run_no_signals");
    let file = h.scratch.path("ran");
    let script = r#"echo ran > "$0""#;
    let words = run_words(&[], &["sh", "-c", script, path_str(&file)]);
    let failing = Services::new(
        Manual::new(),
        Arc::new(FixedClock(T0)),
        Box::new(|_| Err(io::Error::other("no handlers today"))),
    );
    let got = h.js.with(&words, failing);
    assert_eq!(got.code, 1, "{}", got.err);
    assert_eq!(got.err, "js: cannot watch for signals: no handlers today\n");
    assert!(!file.exists());
    let next = h.js.go(&acquire_words(&[]));
    assert!(next.out.starts_with("lease 1-1 "), "{}", next.out);
}

/// Services whose signal source delivers `signal` as soon as it is installed.
fn signalled_at_install(waiter: Arc<dyn Waiter>, signal: ForwardedSignal) -> Services {
    Services::new(
        waiter,
        Arc::new(FixedClock(T0)),
        Box::new(move |signaller| {
            signaller.deliver(signal);
            Ok(())
        }),
    )
}

/// Asserts that SIGHUP, SIGINT and SIGTERM that arrive before the poll of a pending
/// start release the pending lease, start no command and end `js` with 128 plus the
/// signal's number, naming it.
#[test]
fn a_signal_while_the_start_is_pending_releases_it() {
    let cases = [
        (ForwardedSignal::Hangup, 129, "SIGHUP"),
        (ForwardedSignal::Interrupt, 130, "SIGINT"),
        (ForwardedSignal::Terminate, 143, "SIGTERM"),
    ];
    for (n, (signal, code, name)) in cases.into_iter().enumerate() {
        let h = Harness::scripted(
            &format!("run_pending_signal_{n}"),
            vec![(Call::Start, true)],
        );
        let file = h.scratch.path("ran");
        let script = r#"echo ran > "$0""#;
        let words = run_words(&[], &["sh", "-c", script, path_str(&file)]);
        let got =
            h.js.with(&words, signalled_at_install(Arc::new(Never), signal));
        assert_eq!(got.code, code, "{}", got.err);
        let said = format!(
            "js: interrupted by {name} before the lease was held; asked the server to release it\n"
        );
        assert!(got.err.ends_with(&said), "{}", got.err);
        assert!(!file.exists());
        h.tick();
        assert!(h.listed().is_empty());
    }
}

/// A waiter that delivers a signal to the run instead of waiting.
struct Interrupting {
    slot: Arc<Mutex<Option<Signaller>>>,
    signal: ForwardedSignal,
}

impl Waiter for Interrupting {
    fn wait(&self, _interval: Duration) {
        self.slot
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .deliver(self.signal);
    }
}

/// Asserts that a signal that arrives during the pause between two polls of a pending
/// start is acted on at the next poll: the lease is released and `js` exits 143, after
/// one poll and one pause, with no command run.
#[test]
fn a_signal_during_the_pending_pause_releases_the_lease() {
    let h = Harness::scripted("run_pause_signal", vec![(Call::Start, true)]);
    let slot = Arc::new(Mutex::new(None));
    let waiter = Arc::new(Interrupting {
        slot: slot.clone(),
        signal: ForwardedSignal::Terminate,
    });
    let source = Box::new(move |signaller| {
        *slot.lock().unwrap() = Some(signaller);
        Ok(())
    });
    let file = h.scratch.path("ran");
    let script = r#"echo ran > "$0""#;
    let words = run_words(&[], &["sh", "-c", script, path_str(&file)]);
    let got = h.js.with(
        &words,
        Services::new(waiter, Arc::new(FixedClock(T0)), source),
    );
    assert_eq!(got.code, 143, "{}", got.err);
    assert!(
        got.err.contains("interrupted by SIGTERM before the lease"),
        "{}",
        got.err
    );
    assert!(!file.exists());
    h.tick();
    assert!(h.listed().is_empty());
}

/// Asserts that a signal that arrives before the lease is held (the start answers at
/// once, so there is no pending poll to stop) is not lost: it is handed to the command
/// the moment it runs, which ends with 128 plus the signal, and the lease is released.
#[test]
fn a_signal_before_the_command_starts_reaches_it() {
    let cases = [
        (ForwardedSignal::Hangup, 129),
        (ForwardedSignal::Interrupt, 130),
        (ForwardedSignal::Terminate, 143),
    ];
    for (n, (signal, code)) in cases.into_iter().enumerate() {
        let h = Harness::new(&format!("run_early_signal_{n}"));
        let words = run_words(&[], &["sleep", "30"]);
        let got =
            h.js.with(&words, signalled_at_install(Manual::new(), signal));
        assert_eq!(got.code, code, "{}", got.err);
        assert!(h.listed().is_empty());
    }
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

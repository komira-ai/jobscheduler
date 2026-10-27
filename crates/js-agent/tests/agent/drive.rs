//! The loop around the session: what it waits, prints and sends when it is stopped, with
//! a sleeper that counts instead of waiting.

use std::time::Duration;

use js_agent::{Sleeper, SystemSleeper, Wake};

use crate::support::{SECRET, beat_ok, code, linux_host, refused, run_scripted, welcome};

fn flags() -> [&'static str; 2] {
    ["--host-id", "gpu-1"]
}

/// Asserts that a server that never answers costs one hello and one counted wait per
/// try, the waits follow the capped backoff, each try is logged, and a stop request in the
/// middle ends the program with status 0 without a heartbeat (there is no session to
/// drain). Nothing here reads a clock: the sleeper counts.
#[test]
fn backoff_counts_sleeps_not_time() {
    let (ran, sent, waits) = run_scripted(&flags(), &linux_host(), vec![], 8);
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert_eq!(waits.len(), 8);
    let expected = [0.5, 1.0, 2.0, 4.0, 8.0, 16.0, 30.0, 30.0];
    let expected: Vec<Duration> = expected
        .iter()
        .map(|&s| Duration::from_secs_f64(s))
        .collect();
    assert_eq!(waits, expected);
    assert_eq!(sent.borrow().len(), 8);
    let failures = ran
        .err
        .lines()
        .filter(|l| l.contains("hello failed"))
        .count();
    assert_eq!(failures, 8, "{}", ran.err);
    assert!(!ran.err.contains("draining"), "{}", ran.err);
}

/// Asserts that a stop request after the session began sends one last heartbeat with the
/// phase `draining` and ends the program with status 0, saying so on stderr. The
/// heartbeat before it said `serving`.
#[test]
fn a_stop_request_announces_the_drain_and_exits_zero() {
    let replies = vec![welcome(1), beat_ok(), beat_ok()];
    let (ran, sent, waits) = run_scripted(&flags(), &linux_host(), replies, 2);
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert_eq!(waits, [Duration::from_secs(5); 2]);
    let phases: Vec<String> = sent
        .borrow()
        .iter()
        .skip(1)
        .map(|(_, body)| body["phase"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(phases, ["serving", "draining"]);
    assert!(
        ran.err
            .contains("registered, epoch 1, heartbeat every 5000ms"),
        "{}",
        ran.err
    );
    assert!(
        ran.err.contains("draining: the server was told"),
        "{}",
        ran.err
    );
    assert!(!ran.err.contains(SECRET) && !ran.out.contains(SECRET));
}

/// Asserts that a refusal for good ends the program with status 2, the status and code on
/// stderr, one request and no wait. It catches a program that retries a refusal.
#[test]
fn a_fatal_refusal_exits_two_with_the_code() {
    let replies = vec![refused(403, "host-bound"), welcome(1)];
    let (ran, sent, waits) = run_scripted(&flags(), &linux_host(), replies, 5);
    assert_eq!(ran.status(), code(2));
    assert_eq!(
        ran.err,
        "js-agent: the server refused the hello with status 403 (host-bound); not retrying\n"
    );
    assert_eq!(sent.borrow().len(), 1);
    assert!(waits.is_empty());
}

/// Asserts that a program that cannot install its signal handlers says why and exits with
/// status 1, before it reads a flag or probes the host.
#[test]
fn a_program_without_signal_handlers_exits_one() {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let install = || Err(std::io::Error::other("no runtime"));
    let args = vec![std::ffi::OsString::from("js-agent")];
    let status = js_agent::main_with(&install, args, &mut out, &mut err);
    assert_eq!(format!("{status:?}"), code(1));
    assert_eq!(String::from_utf8(err).unwrap(), "js-agent: no runtime\n");
    assert!(out.is_empty());
}

/// The sleeper of the program waits and reports that the time passed, when no stop request
/// came. (The stop request itself is the signal, which the binary test sends to a child
/// process.)
#[test]
fn the_system_sleeper_reports_elapsed_time() {
    let mut sleeper = SystemSleeper::install().unwrap();
    assert_eq!(sleeper.sleep(Duration::from_millis(1)), Wake::Elapsed);
}

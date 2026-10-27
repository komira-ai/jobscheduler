//! The release at the end of `lease run`: tried again through the waiter when the server
//! cannot be reached, and exit 8 when no try is answered.

use std::fs;
use std::thread::scope;

use crate::procs::{fifo, path_str, secs};
use crate::support::{Harness, Manual, Scratch, T0, against, http, run_words, sequence};

/// The answer of a server that grants a lease expiring ten minutes after [`T0`].
fn granted() -> String {
    http(
        "201 Created",
        &format!(
            r#"{{"id":"1-1","host":"gpu-01","expires_at_ms":{}}}"#,
            T0 + 600_000
        ),
    )
}

/// Asserts that a release the server fails once is tried again after a pause that went
/// through the waiter (3s) and then succeeds: the status is the command's own, nothing is
/// reported, and the server saw the start and two releases.
#[test]
fn a_release_that_fails_is_tried_again() {
    let scratch = Scratch::new("release_again");
    let (url, log) = sequence(vec![
        granted(),
        http("500 Internal Server Error", r#"{"error":"boom"}"#),
        http("200 OK", "{}"),
    ]);
    let waiter = Manual::new();
    let words = run_words(&[], &["sh", "-c", "exit 4"]);
    let got = against(&url, &scratch).waiting(&words, waiter.clone());
    assert_eq!(got.code, 4, "{}", got.err);
    assert!(!got.err.contains("could not release"), "{}", got.err);
    assert_eq!(waiter.instant(), vec![secs(3)]);
    let seen = log.lock().unwrap().clone();
    let routes: Vec<&str> = seen
        .iter()
        .map(|l| l.trim_end_matches(" HTTP/1.1"))
        .collect();
    assert_eq!(
        routes,
        vec![
            "POST /v1/leases",
            "DELETE /v1/leases/1-1",
            "DELETE /v1/leases/1-1"
        ]
    );
}

/// Asserts that a release that fails every try (three, with two pauses of 3s through the
/// waiter) is reported with the number of tries, and makes the status 8 in place of the
/// command's own (3), which is said.
#[test]
fn a_release_that_is_never_confirmed_exits_eight() {
    let scratch = Scratch::new("release_never");
    let (url, log) = sequence(vec![
        granted(),
        http("500 Internal Server Error", r#"{"error":"boom"}"#),
    ]);
    let waiter = Manual::new();
    let words = run_words(&[], &["sh", "-c", "exit 3"]);
    let got = against(&url, &scratch).waiting(&words, waiter.clone());
    assert_eq!(got.code, 8, "{}", got.err);
    let said = "js: could not release lease 1-1: unexpected answer 500 (boom); gave up after 3 tries\n\
                js: the command ended with status 3; exiting 8 because the lease may still be held\n";
    assert!(got.err.ends_with(said), "{}", got.err);
    assert_eq!(waiter.instant(), vec![secs(3), secs(3)]);
    assert_eq!(log.lock().unwrap().len(), 4);
}

/// Asserts that a release the server answers with a refusal (the lease expired while the
/// command ran, and the renewal clock never came) is reported once, not tried again, and
/// keeps the command's status: the server holds nothing for the host.
#[test]
fn a_release_the_server_refuses_is_not_tried_again() {
    let h = Harness::new("release_refused");
    let waiter = Manual::new();
    let (ready, go) = (fifo(&h.scratch, "ready"), fifo(&h.scratch, "go"));
    let script = r#"echo up > "$0"; read line < "$1""#;
    let words = run_words(&[], &["sh", "-c", script, path_str(&ready), path_str(&go)]);
    let got = scope(|s| {
        let run = s.spawn(|| h.js.waiting(&words, waiter.clone()));
        fs::read_to_string(&ready).unwrap();
        h.advance(secs(601));
        h.tick();
        fs::write(&go, "done\n").unwrap();
        run.join().unwrap()
    });
    assert_eq!(got.code, 0, "{}", got.err);
    assert!(
        got.err
            .contains("js: could not release lease 1-1: refused: "),
        "{}",
        got.err
    );
    assert!(!got.err.contains("gave up"), "{}", got.err);
    assert!(waiter.instant().is_empty());
}

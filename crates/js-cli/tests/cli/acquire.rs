//! `lease acquire`: the held lease, the refusals and the pending start.

use std::time::Duration;

use js_provider::Call;
use serde_json::json;

use crate::support::{Driven, Harness, Never, Scratch, T0, acquire_words, against, http, sequence};

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// When a lease asked for with `--max-wall 2h` (the words of [`acquire_words`]) ends on a
/// server whose clock reads [`T0`] when it is asked.
const WALL_END: u64 = T0 + 7_200_000;

/// What `js lease acquire --ttl` says.
const TTL_REMOVED: &str = concat!(
    "--ttl was removed: a lease has no time to live ",
    "(it ends when released or at its maximum wall time); remove the flag"
);

/// Asserts that a start of one host prints the lease as `lease <id> host <host> ends <ms>`
/// on stdout, the end being the maximum wall time (`--max-wall 2h`) from the server's
/// clock, and nothing on stderr. It catches a line that still says `expires` or prints an
/// end that is not the wall end.
#[test]
fn acquire_prints_the_lease() {
    let h = Harness::new("acquire_text");
    let got = h.js.go(&acquire_words(&[]));
    assert_eq!(got.code, 0, "{}", got.err);
    assert_eq!(got.out, format!("lease 1-1 host gpu-01 ends {WALL_END}\n"));
    assert_eq!(got.err, "");
}

/// Asserts that `--json` prints one JSON object with the id, the host and `ends_at_ms`,
/// and that the global flags may follow the subcommand.
#[test]
fn acquire_prints_json_and_takes_global_flags_last() {
    let h = Harness::new("acquire_json");
    let got = h.js.go_last(&acquire_words(&["--json"]));
    assert_eq!(got.code, 0, "{}", got.err);
    let expected = json!({ "id": "1-1", "host": "gpu-01", "ends_at_ms": WALL_END });
    assert_eq!(got.json(), expected);
}

/// Asserts that every flag of a start reaches the server: the labels are listed on the
/// lease and the lease ends at the wall time asked for.
#[test]
fn acquire_sends_its_flags() {
    let h = Harness::new("acquire_flags");
    let flags = [
        "--label",
        "team=infra",
        "--label",
        "k=vw",
        "--validation-run",
        "run-1",
        "--wait-pending",
        "5s",
    ];
    let got = h.js.go(&acquire_words(&flags));
    assert_eq!(got.code, 0, "{}", got.err);
    assert!(
        got.out.ends_with(&format!("ends {WALL_END}\n")),
        "{}",
        got.out
    );
    let listed = h.js.go(&["--json", "lease", "list"]).json();
    let labels = &listed["leases"][0]["labels"];
    assert_eq!(labels["team"], "infra");
    assert_eq!(labels["k"], "vw");
}

/// Asserts the round trip of `--request-id` through the client and a real server: a start
/// repeated with the same id and flags prints the lease the first one made (same id, same
/// host, same end) and takes no second host, while the same id with another label is
/// refused `request-id-conflict` (exit 3) and leaves that lease as it was; a bad id is a
/// 400 naming the field (exit 4). It catches a flag that never reaches the server (the
/// repeat would take the second host), a server that books again under a known id, and a
/// conflict answered with a lease.
#[test]
fn a_start_repeated_under_its_request_id_answers_the_same_lease() {
    let h = Harness::new("acquire_request_id");
    let first = h.js.go(&acquire_words(&["--request-id", "nightly-42"]));
    assert_eq!(first.code, 0, "{}", first.err);
    let expected = format!("lease 1-1 host gpu-01 ends {WALL_END}\n");
    assert_eq!(first.out, expected);
    let again = h.js.go(&acquire_words(&["--request-id", "nightly-42"]));
    assert_eq!(
        (again.code, again.out.as_str()),
        (0, expected.as_str()),
        "{}",
        again.err
    );
    let other = h.js.go(&acquire_words(&[
        "--request-id",
        "nightly-42",
        "--label",
        "k=v",
    ]));
    assert_eq!(other.code, 3, "{}", other.err);
    assert_eq!(other.err, "js: refused: request-id-conflict\n");
    let bad = h.js.go(&acquire_words(&["--request-id", "not an id"]));
    assert_eq!(bad.code, 4, "{}", bad.err);
    assert!(
        bad.err
            .starts_with("js: rejected: bad-request: request_id is not a valid request id"),
        "{}",
        bad.err
    );
    let listed = h.js.go(&["--json", "lease", "list"]).json();
    let leases = listed["leases"].as_array().unwrap();
    let ids: Vec<&str> = leases
        .iter()
        .map(|lease| lease["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["1-1"]);
}

/// Asserts that each answer of the server maps to its status and prints the code:
/// 409 refusals are exit 3 (no capacity once both hosts are taken, no capable host for a
/// request no host could ever hold), 422 and 400 refusals are exit 4 with the reason when there is
/// one.
#[test]
fn refusals_have_their_exit_status() {
    let h = Harness::new("acquire_refusals");
    assert_eq!(h.js.go(&acquire_words(&[])).code, 0);
    assert_eq!(h.js.go(&acquire_words(&[])).code, 0);
    let full = h.js.go(&acquire_words(&[]));
    assert_eq!((full.code, full.out.as_str()), (3, ""));
    assert_eq!(full.err, "js: refused: no-capacity\n");

    let cases: [(&[&str], u8, &str); 4] = [
        (&["--gpus", "99"], 3, "js: refused: no-capable-host\n"),
        (
            &["--class", "no-such-class"],
            4,
            "js: rejected: unknown-class\n",
        ),
        (
            &["--label", "jobscheduler.dev/x=y"],
            4,
            "js: rejected: reserved-label\n",
        ),
        (
            &["--class", "Not A Class"],
            4,
            "js: rejected: bad-request: ",
        ),
    ];
    for (extra, code, stderr) in cases {
        let got = h.js.go(&acquire_words(extra));
        assert_eq!(got.code, code, "{extra:?}: {}", got.err);
        assert!(got.err.starts_with(stderr), "{extra:?}: {}", got.err);
    }
}

/// Asserts that with `--json` a refusal is one JSON object on stderr naming its kind and
/// message, and stdout stays empty.
#[test]
fn refusals_are_json_with_json() {
    let h = Harness::new("acquire_refusal_json");
    let got = h.js.go(&acquire_words(&["--json", "--gpus", "99"]));
    assert_eq!((got.code, got.out.as_str()), (3, ""));
    let said: serde_json::Value = serde_json::from_str(&got.err).unwrap();
    assert_eq!(
        said,
        json!({ "error": "refused", "message": "refused: no-capable-host" })
    );
}

/// Asserts that a 202 is polled through the waiter until the lease is held: one wait of
/// two seconds, during which the server ticks, and the lease printed is the settled one
/// (with its host).
#[test]
fn a_pending_start_is_polled_until_held() {
    let h = Harness::scripted("pending_held", vec![(Call::Start, true)]);
    let waiter = h.ticking();
    let got =
        h.js.waiting(&acquire_words(&["--wait-pending", "10s"]), waiter.clone());
    assert_eq!(got.code, 0, "{}", got.err);
    assert_eq!(got.out, format!("lease 1-1 host h1 ends {WALL_END}\n"));
    assert_eq!(waiter.asked(), vec![secs(2)]);
}

/// Asserts that a pending start the provider never confirms and drops is a refusal
/// (exit 3, `pending-dropped`) as soon as the list no longer has it.
#[test]
fn a_pending_start_that_is_dropped_is_refused() {
    let h = Harness::scripted("pending_dropped", vec![(Call::Start, false)]);
    let got = h.js.waiting(&acquire_words(&[]), h.ticking());
    assert_eq!(got.code, 3, "{}", got.err);
    assert_eq!(got.err, "js: refused: pending-dropped\n");
}

/// Asserts that a pending start that nobody settles gives up after `--wait-pending`,
/// measured as the intervals asked of the waiter (two of two seconds for four), exits 5,
/// and does not leave the lease behind to be held with nobody to release it.
#[test]
fn a_pending_start_that_is_not_settled_exits_five() {
    let h = Harness::scripted("pending_unsettled", vec![(Call::Start, true)]);
    let waiter = Driven::new(|| {});
    let got =
        h.js.waiting(&acquire_words(&["--wait-pending", "4s"]), waiter.clone());
    assert_eq!((got.code, got.out.as_str()), (5, ""));
    assert_eq!(
        got.err,
        "js: lease 1-1 is still pending after 4s; released it\n"
    );
    assert_eq!(waiter.asked(), vec![secs(2), secs(2)]);
    h.tick();
    assert!(h.listed().is_empty());
}

/// Asserts that a pending lease someone else released meanwhile (the list shows it
/// releasing) counts as gone, not as pending forever.
#[test]
fn a_pending_start_released_meanwhile_is_gone() {
    let h = Harness::scripted(
        "pending_released",
        vec![(Call::Start, true), (Call::Stop, false)],
    );
    let js = h.js.clone();
    let waiter = Driven::new(move || {
        let got = js.go(&["lease", "release", "1-1"]);
        assert_eq!(got.code, 6, "{}", got.err);
    });
    let got = h.js.waiting(&acquire_words(&[]), waiter);
    assert_eq!(got.code, 3, "{}", got.err);
    assert_eq!(got.err, "js: refused: pending-dropped\n");
}

/// Asserts that a pending start whose poll fails (the list answers 500) is released
/// before the failure is reported: the server saw the start, the poll and then the
/// release of lease 1-1, and the exit is 1 with the poll's error.
#[test]
fn a_poll_that_fails_releases_the_pending_lease() {
    let scratch = Scratch::new("pending_poll_fails");
    let (url, log) = sequence(vec![
        http("202 Accepted", r#"{"id":"1-1"}"#),
        http(
            "500 Internal Server Error",
            r#"{"error":"state-write-failed"}"#,
        ),
        http("200 OK", "{}"),
    ]);
    let got = against(&url, &scratch).waiting(&acquire_words(&[]), std::sync::Arc::new(Never));
    assert_eq!((got.code, got.out.as_str()), (1, ""));
    assert_eq!(got.err, "js: unexpected answer 500 (state-write-failed)\n");
    let seen = log.lock().unwrap().clone();
    let routes: Vec<&str> = seen
        .iter()
        .map(|line| line.trim_end_matches(" HTTP/1.1"))
        .collect();
    assert_eq!(
        routes,
        vec!["POST /v1/leases", "GET /v1/leases", "DELETE /v1/leases/1-1"]
    );
}

/// The request lines a [`sequence`] server has read, without the protocol version.
fn routes(log: &std::sync::Mutex<Vec<String>>) -> Vec<String> {
    let seen = log.lock().unwrap().clone();
    seen.iter()
        .map(|line| line.trim_end_matches(" HTTP/1.1").to_owned())
        .collect()
}

/// Asserts that a start answered `201 {id, host, ends_at_ms}` prints `lease <id> host
/// <host> ends <ms>` with that number, and with `--json` an object whose `ends_at_ms` is
/// that number and which has no `expires_at_ms`; each start is one request. It catches a
/// client that reads the old `expires_at_ms` field (it would fail the answer as having no
/// usable field) and one that prints the end under another name.
#[test]
fn acquire_prints_the_wall_end() {
    let scratch = Scratch::new("acquire_wall_end");
    let granted = http(
        "201 Created",
        r#"{"id":"1-1","host":"gpu-01","ends_at_ms":1793617960284}"#,
    );
    let (url, log) = sequence(vec![granted]);
    let js = against(&url, &scratch);

    let text = js.go(&acquire_words(&[]));
    assert_eq!(text.code, 0, "{}", text.err);
    assert_eq!(text.out, "lease 1-1 host gpu-01 ends 1793617960284\n");
    assert_eq!(text.err, "");

    let json = js.go(&acquire_words(&["--json"]));
    assert_eq!(json.code, 0, "{}", json.err);
    assert_eq!(
        json.json(),
        json!({ "id": "1-1", "host": "gpu-01", "ends_at_ms": 1_793_617_960_284_u64 })
    );
    assert_eq!(routes(&log), vec!["POST /v1/leases"; 2]);
}

/// Asserts that `lease acquire --ttl <dur>` is exit 2 with the message that a lease has no
/// time to live, as text and as JSON, and that the server is sent nothing, whatever the
/// value (a duration, a number, something that is not one). It catches a flag that is
/// accepted and dropped (the user thinks the lease has a ttl), one sent on to the server,
/// and one clap refuses with its own words.
#[test]
fn acquire_with_ttl_is_refused_with_the_message() {
    let scratch = Scratch::new("acquire_ttl");
    let (url, log) = sequence(vec![http("500 Internal Server Error", "{}")]);
    let js = against(&url, &scratch);
    for value in ["60s", "90", "-5s", "soon"] {
        let got = js.go(&acquire_words(&["--ttl", value]));
        assert_eq!((got.code, got.out.as_str()), (2, ""), "{value}");
        assert_eq!(got.err, format!("js: {TTL_REMOVED}\n"), "{value}");
    }
    let json = js.go(&acquire_words(&["--json", "--ttl", "60s"]));
    assert_eq!(json.code, 2, "{}", json.err);
    let said: serde_json::Value = serde_json::from_str(&json.err).unwrap();
    assert_eq!(said, json!({ "error": "usage", "message": TTL_REMOVED }));
    assert!(routes(&log).is_empty(), "{:?}", routes(&log));
}

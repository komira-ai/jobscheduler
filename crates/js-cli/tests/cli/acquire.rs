//! `lease acquire`: the held lease, the refusals and the pending start.

use std::time::Duration;

use js_provider::Call;
use serde_json::json;

use crate::support::{Driven, Harness, T0, acquire_words};

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// Asserts that a start of one host prints the lease as `lease <id> host <host> expires
/// <ms>` on stdout, with the server's default ttl of 600s from the server's clock, and
/// nothing on stderr.
#[test]
fn acquire_prints_the_lease() {
    let h = Harness::new("acquire_text");
    let got = h.js.go(&acquire_words(&[]));
    assert_eq!(got.code, 0, "{}", got.err);
    assert_eq!(
        got.out,
        format!("lease 1-1 host gpu-01 expires {}\n", T0 + 600_000)
    );
    assert_eq!(got.err, "");
}

/// Asserts that `--json` prints one JSON object with the id, the host and the expiry,
/// and that the global flags may follow the subcommand.
#[test]
fn acquire_prints_json_and_takes_global_flags_last() {
    let h = Harness::new("acquire_json");
    let got = h.js.go_last(&acquire_words(&["--json"]));
    assert_eq!(got.code, 0, "{}", got.err);
    let expected = json!({ "id": "1-1", "host": "gpu-01", "expires_at_ms": T0 + 600_000 });
    assert_eq!(got.json(), expected);
}

/// Asserts that every flag of a start reaches the server: the ttl sets the expiry and
/// the labels are listed on the lease.
#[test]
fn acquire_sends_its_flags() {
    let h = Harness::new("acquire_flags");
    let flags = [
        "--ttl",
        "90s",
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
        got.out.ends_with(&format!("expires {}\n", T0 + 90_000)),
        "{}",
        got.out
    );
    let listed = h.js.go(&["--json", "lease", "list"]).json();
    let labels = &listed["leases"][0]["labels"];
    assert_eq!(labels["team"], "infra");
    assert_eq!(labels["k"], "vw");
}

/// Asserts that each answer of the server maps to its status and prints the code:
/// 409 refusals are exit 3 (no capacity once both hosts are taken, no fit for a request
/// no host satisfies), 422 and 400 refusals are exit 4 with the reason when there is
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
        (&["--gpus", "99"], 3, "js: refused: no-fit\n"),
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
        json!({ "error": "refused", "message": "refused: no-fit" })
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
    assert_eq!(
        got.out,
        format!("lease 1-1 host h1 expires {}\n", T0 + 600_000)
    );
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
/// and does not leave the lease behind to be held with nobody renewing it.
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

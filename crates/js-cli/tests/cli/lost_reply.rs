//! A start whose answer is lost: the request is sent once and the user is told the lease may
//! exist.
//!
//! A proxy between the CLI and a real server passes each request on, so the server acts,
//! and then loses or replaces the answer to the start.

use std::sync::Arc;

use crate::servers::{Fate, Proxy};
use crate::support::{Harness, Js, Never, acquire_words};

/// What the failure of a start whose answer is lost tells the user to do.
const HINT: &str =
    "the request may have created a lease: run \"js lease list\" before trying again";

/// The ways the answer to a start is lost, and what the failure says about each.
const FATES: [(Fate, &str); 2] = [
    (Fate::CloseAfter, "cannot talk to"),
    (Fate::Answer500, "unexpected answer 500 (boom)"),
];

/// A proxy to the server of `h` that gives the start the fate `start` and every other
/// request `others`, and the CLI against it.
fn lossy(
    h: &Harness,
    start: Fate,
    others: impl Fn(&str) -> Fate + Send + Sync + 'static,
) -> (Proxy, Js) {
    let rule = move |line: &str| match line {
        "POST /v1/leases" => start,
        other => others(other),
    };
    let proxy = Proxy::to(&h.js.server, rule);
    let via = Js {
        server: proxy.url.clone(),
        ..h.js.clone()
    };
    (proxy, via)
}

/// Asserts that a start of `lease acquire` whose answer is lost (the connection closed
/// before a byte of it, or a 500 in its place) is sent
/// once, fails with exit 1 and the advice to run `js lease list` before trying again, and
/// does nothing more: the server did create the lease, and `acquire` leaves it be.
#[test]
fn a_lost_answer_to_acquire_is_not_asked_again() {
    for (n, (fate, said)) in FATES.into_iter().enumerate() {
        let h = Harness::new(&format!("lost_acquire_{n}"));
        let (proxy, via) = lossy(&h, fate, |_| Fate::Deliver);
        let services = via.services(Arc::new(Never));
        let got = via.with(&acquire_words(&[]), services);
        assert_eq!(
            (got.code, got.out.as_str()),
            (1, ""),
            "{fate:?}: {}",
            got.err
        );
        assert!(got.err.starts_with("js: "), "{fate:?}: {}", got.err);
        assert!(got.err.contains(said), "{fate:?}: {}", got.err);
        assert!(
            got.err.ends_with(&format!("; {HINT}\n")),
            "{fate:?}: {}",
            got.err
        );
        assert_eq!(proxy.starts(), 1, "{fate:?}");
        assert_eq!(proxy.requests(), vec!["POST /v1/leases"], "{fate:?}");
        assert_eq!(h.listed(), vec!["1-1"], "{fate:?}");
    }
}

/// Asserts that with `--json` the failure of a start whose answer is lost has the kind
/// `acquire-unconfirmed` (and exit 1), so that a script can tell it from a server that
/// was merely down.
#[test]
fn an_unconfirmed_start_is_its_own_kind_in_json() {
    let h = Harness::new("lost_json");
    let (_proxy, via) = lossy(&h, Fate::CloseAfter, |_| Fate::Deliver);
    let got = via.go(&acquire_words(&["--json"]));
    assert_eq!(got.code, 1, "{}", got.err);
    let said: serde_json::Value = serde_json::from_str(&got.err).unwrap();
    assert_eq!(said["error"], "acquire-unconfirmed");
    assert!(said["message"].as_str().unwrap().ends_with(HINT), "{said}");
}

/// Asserts that a start whose connection is closed before the proxy passes it on is sent
/// once, fails with exit 1 and the same advice (the client cannot tell this from a lost
/// answer), and leaves the server with no lease. It catches a client that retries the
/// start on a closed connection, which a start whose answer was lost must never do.
#[test]
fn a_start_that_never_arrived_is_not_asked_again() {
    let h = Harness::new("lost_never");
    let (proxy, via) = lossy(&h, Fate::CloseBefore, |_| Fate::Deliver);
    let got = via.go(&acquire_words(&[]));
    assert_eq!((got.code, got.out.as_str()), (1, ""), "{}", got.err);
    assert!(got.err.contains("cannot talk to"), "{}", got.err);
    assert!(got.err.ends_with(&format!("; {HINT}\n")), "{}", got.err);
    assert_eq!(proxy.requests(), vec!["POST /v1/leases"]);
    assert!(h.listed().is_empty());
}

//! `lease renew`, `lease release` and `lease list`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_provider::Call;
use serde_json::json;

use crate::support::{Driven, Harness, T0, acquire_words, tick_on};

/// Asserts that a renewal prints the new expiry, as text and as JSON, that `--ttl` sets
/// how far it reaches from the server's clock, and that the lease stays listed.
#[test]
fn renew_prints_the_new_expiry() {
    let h = Harness::new("renew");
    assert_eq!(h.js.go(&acquire_words(&["--ttl", "60s"])).code, 0);
    h.advance(Duration::from_secs(30));

    let short = h.js.go(&["lease", "renew", "1-1", "--ttl", "90s"]);
    assert_eq!(short.code, 0, "{}", short.err);
    assert_eq!(
        short.out,
        format!("lease 1-1 expires {}\n", T0 + 30_000 + 90_000)
    );

    let plain = h.js.go(&["--json", "lease", "renew", "1-1"]);
    assert_eq!(
        plain.json(),
        json!({ "id": "1-1", "expires_at_ms": T0 + 30_000 + 600_000 })
    );
    assert_eq!(h.listed(), vec!["1-1"]);
}

/// Asserts that a renewal the server refuses is exit 3 with the code: an unknown lease
/// and one past its ttl (`expired`).
#[test]
fn renew_refusals_are_exit_three() {
    let h = Harness::new("renew_refused");
    let unknown = h.js.go(&["lease", "renew", "9-9"]);
    assert_eq!(
        (unknown.code, unknown.err.as_str()),
        (3, "js: refused: unknown-lease\n")
    );

    let held = h.js.go(&acquire_words(&["--ttl", "10s"]));
    assert_eq!(held.code, 0, "{}", held.err);
    h.advance(Duration::from_secs(11));
    let late = h.js.go(&["lease", "renew", "1-1"]);
    assert_eq!(
        (late.code, late.err.as_str()),
        (3, "js: refused: expired\n")
    );
}

/// Asserts that a lease still pending cannot be renewed (the server says `not-held`),
/// and that the list shows it pending, with no host: both asked from the waiter, while
/// the start is being polled.
#[test]
fn a_pending_lease_cannot_be_renewed() {
    let h = Harness::scripted("renew_pending", vec![(Call::Start, true)]);
    let (js, ticks, handle) = (h.js.clone(), h.ticks.clone(), h.rt.handle().clone());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    let waiter = Driven::new(move || {
        let renew = js.go(&["lease", "renew", "1-1"]);
        let listed = js.go(&["lease", "list"]);
        record
            .lock()
            .unwrap()
            .push((renew.code, renew.err, listed.out));
        tick_on(&handle, &ticks);
    });
    let started = h.js.waiting(&acquire_words(&[]), waiter);
    assert_eq!(started.code, 0, "{}", started.err);
    let expected = vec![(
        3,
        "js: refused: not-held\n".to_owned(),
        format!("1-1 pending host - expires {}\n", T0 + 600_000),
    )];
    assert_eq!(*seen.lock().unwrap(), expected);
}

/// Asserts that a release prints `freed`, as text and as JSON, that the host is free
/// for the next start, and that releasing it again is a refusal (exit 3, unknown lease).
#[test]
fn release_frees_the_lease() {
    let h = Harness::new("release");
    assert_eq!(h.js.go(&acquire_words(&[])).code, 0);
    let freed = h.js.go(&["lease", "release", "1-1"]);
    assert_eq!((freed.code, freed.out.as_str()), (0, "lease 1-1 freed\n"));
    assert!(h.listed().is_empty());
    let again = h.js.go(&["--json", "lease", "release", "1-1"]);
    assert_eq!((again.code, again.err.contains("unknown-lease")), (3, true));

    assert_eq!(h.js.go(&acquire_words(&[])).code, 0);
    let json = h.js.go(&["--json", "lease", "release", "1-2"]);
    assert_eq!(json.json(), json!({ "id": "1-2", "state": "freed" }));
}

/// Asserts that a release the provider has not confirmed is exit 6 and prints
/// `releasing`, and that a tick of the server then frees the lease.
#[test]
fn an_unconfirmed_release_is_exit_six() {
    let h = Harness::scripted(
        "release_unconfirmed",
        vec![(Call::Stop, false), (Call::Stop, false)],
    );
    assert_eq!(h.js.go(&acquire_words(&[])).code, 0);
    let got = h.js.go(&["lease", "release", "1-1"]);
    assert_eq!((got.code, got.out.as_str()), (6, "lease 1-1 releasing\n"));
    let json = h.js.go(&["--json", "lease", "release", "1-1"]);
    assert_eq!(
        (json.code, json.json()),
        (6, json!({ "id": "1-1", "state": "releasing" }))
    );
    h.tick();
    assert!(h.listed().is_empty());
}

/// Asserts that the list prints a line per lease (id, state, host or `-`, expiry), the
/// server's own body with `--json`, and nothing for an empty table.
#[test]
fn list_prints_the_leases() {
    let h = Harness::scripted("list", vec![(Call::Start, true)]);
    let empty = h.js.go(&["lease", "list"]);
    assert_eq!((empty.code, empty.out.as_str()), (0, ""));
    assert_eq!(
        h.js.go(&["--json", "lease", "list"]).json(),
        json!({ "leases": [] })
    );

    let (js, ticks, handle) = (h.js.clone(), h.ticks.clone(), h.rt.handle().clone());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    let waiter = Driven::new(move || {
        let text = js.go(&["lease", "list"]).out;
        let json = js.go(&["--json", "lease", "list"]).json();
        record
            .lock()
            .unwrap()
            .push((text, json["leases"][0]["state"].clone()));
        tick_on(&handle, &ticks);
    });
    assert_eq!(h.js.waiting(&acquire_words(&[]), waiter).code, 0);
    let pending = (
        format!("1-1 pending host - expires {}\n", T0 + 600_000),
        json!("pending"),
    );
    assert_eq!(*seen.lock().unwrap(), vec![pending]);
    let held = h.js.go(&["lease", "list"]);
    assert_eq!(
        held.out,
        format!("1-1 held host h1 expires {}\n", T0 + 600_000)
    );
}

/// Asserts that an id that is not two runs of digits around a dash is a usage error
/// before any request: it could name another route once it is part of a path.
#[test]
fn a_malformed_id_is_a_usage_error() {
    let h = Harness::new("ids");
    for id in ["x", "1-", "-1", "1-2/renew", "../x", "1-2-3", ""] {
        for verb in ["renew", "release"] {
            let got = h.js.go(&["lease", verb, "--", id]);
            assert_eq!(got.code, 2, "{verb} {id:?}");
            assert!(got.err.contains("lease id"), "{}", got.err);
        }
    }
}

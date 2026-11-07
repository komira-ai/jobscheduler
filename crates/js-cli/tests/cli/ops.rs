//! `lease release`, `lease list`, and the commands that were removed.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_provider::Call;
use serde_json::json;

use crate::support::{
    Driven, Harness, Scratch, T0, acquire_words, against, http, sequence, tick_on,
};

/// What `js lease run` says.
const RUN_REMOVED: &str = "js lease run was removed: leases are no longer renewed; submit a job";

/// When a lease asked for with `--max-wall 2h` ends on a server whose clock reads [`T0`].
const WALL_END: u64 = T0 + 7_200_000;

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

/// Asserts that the list prints a line per lease (id, state, host or `-`, end), the
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
        format!("1-1 pending host - ends {WALL_END}\n"),
        json!("pending"),
    );
    assert_eq!(*seen.lock().unwrap(), vec![pending]);
    let held = h.js.go(&["lease", "list"]);
    assert_eq!(held.out, format!("1-1 held host h1 ends {WALL_END}\n"));
}

/// Asserts that an id that is not two runs of digits around a dash is a usage error
/// before any request: it could name another route once it is part of a path.
#[test]
fn a_malformed_id_is_a_usage_error() {
    let h = Harness::new("ids");
    for id in ["x", "1-", "-1", "1-2/runs", "../x", "1-2-3", ""] {
        let got = h.js.go(&["lease", "release", "--", id]);
        assert_eq!(got.code, 2, "release {id:?}");
        assert!(got.err.contains("lease id"), "{}", got.err);
    }
}

/// The request lines a [`sequence`] server has read.
fn requests(log: &Mutex<Vec<String>>) -> Vec<String> {
    log.lock().unwrap().clone()
}

/// Asserts that `js lease renew <id>` is an unknown subcommand (clap's own error, exit 2,
/// naming `renew`), and that `js lease run --class c --max-wall 1h -- true` is exit 2 with
/// exactly the message that `lease run` was removed (as text and as JSON), whatever flags
/// and command follow; the server is sent nothing in either case, so no lease is taken
/// and no command is started. It catches a `renew` or `run` that is still wired to the
/// server, a `run` that takes a lease before it refuses, and a `run` whose arguments clap
/// rejects with its own message instead of the removal message.
#[test]
fn lease_renew_and_run_are_removed() {
    let scratch = Scratch::new("ops_removed");
    let (url, log) = sequence(vec![http("500 Internal Server Error", "{}")]);
    let js = against(&url, &scratch);

    let renew = js.go(&["lease", "renew", "1-1"]);
    assert_eq!((renew.code, renew.out.as_str()), (2, ""));
    assert!(
        renew.err.contains("unrecognized subcommand 'renew'"),
        "{}",
        renew.err
    );

    let words = [
        "lease",
        "run",
        "--class",
        "c",
        "--max-wall",
        "1h",
        "--",
        "true",
    ];
    let run = js.go(&words);
    assert_eq!((run.code, run.out.as_str()), (2, ""));
    assert_eq!(run.err, format!("js: {RUN_REMOVED}\n"));
    let bare = js.go(&["lease", "run"]);
    assert_eq!((bare.code, bare.err), (2, format!("js: {RUN_REMOVED}\n")));
    let flags = js.go(&["lease", "run", "--renew-every", "5s", "--", "x"]);
    assert_eq!((flags.code, flags.err), (2, format!("js: {RUN_REMOVED}\n")));

    let json = js.go(&["--json", "lease", "run", "--class", "c", "--", "true"]);
    assert_eq!(json.code, 2, "{}", json.err);
    let said: serde_json::Value = serde_json::from_str(&json.err).unwrap();
    assert_eq!(said, json!({ "error": "usage", "message": RUN_REMOVED }));

    assert!(requests(&log).is_empty(), "{:?}", requests(&log));
}

/// Asserts that a lease started with `--max-wall 2h` is still listed after an hour and
/// after a second short of two hours (well past the ten minutes that were once the time to
/// live, with nothing renewing it), and is gone the second the wall time is reached. It
/// catches a lease that still ends at a time to live no client extends, and one that
/// outlives its wall time.
#[test]
fn a_lease_ends_at_its_wall_time_and_not_before() {
    let h = Harness::new("ops_wall_end");
    assert_eq!(h.js.go(&acquire_words(&[])).code, 0);
    h.advance(Duration::from_secs(3600));
    h.tick();
    assert_eq!(h.listed(), vec!["1-1"]);
    h.advance(Duration::from_secs(3599));
    h.tick();
    assert_eq!(h.listed(), vec!["1-1"]);
    h.advance(Duration::from_secs(1));
    h.tick();
    assert!(h.listed().is_empty());
}

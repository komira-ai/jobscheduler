//! A start whose answer is lost: the request is sent once, the user is told the lease may
//! exist, and `lease run` finds it again by its label and releases it.
//!
//! A proxy between the CLI and a real server passes each request on, so the server acts,
//! and then loses, replaces or holds back the answer to the start.

use std::fs;
use std::sync::Arc;
use std::thread::scope;
use std::time::Duration;

use js_cli::ForwardedSignal;

use crate::procs::{fifo, path_str};
use crate::servers::{Fate, Proxy};
use crate::support::{EndsRun, Harness, Js, Manual, Never, acquire_words, run_words};

/// What the failure of a start whose answer is lost tells the user to do.
const HINT: &str =
    "the request may have created a lease: run \"js lease list\" before trying again";

/// The longest a request may take in the tests that hold an answer back for good.
const LIMIT: Duration = Duration::from_millis(500);

/// The ways the answer to a start is lost, and what the failure says about each.
const FATES: [(Fate, &str); 3] = [
    (Fate::CloseAfter, "cannot talk to"),
    (Fate::Answer500, "unexpected answer 500 (boom)"),
    (Fate::Stall, "cannot talk to"),
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
/// before a byte of it, a 500 in its place, no answer until the client's limit) is sent
/// once, fails with exit 1 and the advice to run `js lease list` before trying again, and
/// does nothing more: the server did create the lease, and `acquire` leaves it be.
#[test]
fn a_lost_answer_to_acquire_is_not_asked_again() {
    for (n, (fate, said)) in FATES.into_iter().enumerate() {
        let h = Harness::new(&format!("lost_acquire_{n}"));
        let (proxy, via) = lossy(&h, fate, |_| Fate::Deliver);
        let services = via.services(Arc::new(Never)).with_request_limit(LIMIT);
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

/// Asserts that a start of `lease run` whose answer is lost is sent once, fails with exit
/// 1 and the same advice, runs no command, and does not leak the lease: `run` lists the
/// leases, finds the one with its label, releases it and says so, and the server lists
/// nothing afterwards.
#[test]
fn a_lost_answer_to_run_is_not_asked_again_and_the_lease_is_released() {
    for (n, (fate, said)) in FATES.into_iter().enumerate() {
        let h = Harness::new(&format!("lost_run_{n}"));
        let (proxy, via) = lossy(&h, fate, |_| Fate::Deliver);
        let ran = h.scratch.path("ran");
        let script = r#"echo ran > "$0""#;
        let words = run_words(&[], &["sh", "-c", script, path_str(&ran)]);
        let services = via.services(Manual::new()).with_request_limit(LIMIT);
        let got = via.with(&words, services);
        assert_eq!(got.code, 1, "{fate:?}: {}", got.err);
        assert!(got.err.contains(said), "{fate:?}: {}", got.err);
        assert!(
            got.err.ends_with(&format!("; {HINT}\n")),
            "{fate:?}: {}",
            got.err
        );
        assert!(
            got.err
                .starts_with("js: released lease 1-1, which the lost answer left behind\n"),
            "{fate:?}: {}",
            got.err
        );
        assert!(!ran.exists());
        assert_eq!(proxy.starts(), 1, "{fate:?}");
        assert_eq!(
            proxy.requests(),
            vec!["POST /v1/leases", "GET /v1/leases", "DELETE /v1/leases/1-1"],
            "{fate:?}"
        );
        assert!(h.listed().is_empty(), "{fate:?}");
    }
}

/// Asserts that when the start never reached the server (the connection was closed before
/// the proxy passed it on) `run` looks, finds no lease with its label, says so, and
/// releases nothing.
#[test]
fn run_finds_no_lease_when_the_start_never_arrived() {
    let h = Harness::new("lost_never");
    let (proxy, via) = lossy(&h, Fate::CloseBefore, |_| Fate::Deliver);
    let got = via.waiting(&run_words(&[], &["true"]), Manual::new());
    assert_eq!(got.code, 1, "{}", got.err);
    assert!(
        got.err
            .starts_with("js: no lease carries this run's label; the start created none\n"),
        "{}",
        got.err
    );
    assert_eq!(proxy.requests(), vec!["POST /v1/leases", "GET /v1/leases"]);
    assert!(h.listed().is_empty());
}

/// Asserts that when `run` cannot look for the lease (the list fails, or the release of
/// what it finds does) it says so and still ends with the advice and exit 1; the lease is
/// left to its ttl.
#[test]
fn run_says_when_it_cannot_look_for_the_lost_lease() {
    let cases: [(&str, Fate, &str); 2] = [
        (
            "GET /v1/leases",
            Fate::Answer500,
            "unexpected answer 500 (boom)",
        ),
        ("DELETE /v1/leases/1-1", Fate::CloseBefore, "cannot talk to"),
    ];
    for (n, (route, fate, said)) in cases.into_iter().enumerate() {
        let h = Harness::new(&format!("lost_blind_{n}"));
        let (_proxy, via) = lossy(&h, Fate::CloseAfter, move |line| match line == route {
            true => fate,
            false => Fate::Deliver,
        });
        let got = via.waiting(&run_words(&[], &["true"]), Manual::new());
        assert_eq!(got.code, 1, "{route}: {}", got.err);
        let note =
            format!("js: could not look for the lease the lost answer may have left: {said}");
        assert!(got.err.contains(&note), "{route}: {}", got.err);
        assert!(
            got.err.ends_with(&format!("; {HINT}\n")),
            "{route}: {}",
            got.err
        );
        assert_eq!(h.listed(), vec!["1-1"], "{route}");
    }
}

/// Asserts that `run` marks its start with a label of its own, `js-run` and 32 hexadecimal
/// characters, next to the user's labels, that two runs do not share it, and that
/// `acquire` marks nothing.
#[test]
fn run_labels_its_start_and_acquire_does_not() {
    let h = Harness::new("lost_labels");
    let mut seen = Vec::new();
    for n in 0..2 {
        let waiter = Manual::new();
        let (services, signals) = h.js.services_with_signals(waiter);
        let (ready, go) = (
            fifo(&h.scratch, &format!("ready{n}")),
            fifo(&h.scratch, &format!("go{n}")),
        );
        let script = r#"echo up > "$0"; read line < "$1""#;
        let command = ["sh", "-c", script, path_str(&ready), path_str(&go)];
        let words = run_words(&["--label", "team=infra"], &command);
        let labels = scope(|s| {
            let run = s.spawn(|| h.js.with(&words, services));
            let _ends = EndsRun(signals.recv().unwrap(), ForwardedSignal::Terminate);
            fs::read_to_string(&ready).unwrap();
            let listed = h.js.go(&["--json", "lease", "list"]).json();
            fs::write(&go, "done\n").unwrap();
            assert_eq!(run.join().unwrap().code, 0);
            listed["leases"][0]["labels"].clone()
        });
        assert_eq!(labels["team"], "infra");
        let tag = labels["js-run"].as_str().unwrap().to_owned();
        assert!(
            tag.len() == 32 && tag.bytes().all(|b| b.is_ascii_hexdigit()),
            "{tag}"
        );
        seen.push(tag);
    }
    assert_ne!(seen[0], seen[1]);
    assert_eq!(h.js.go(&acquire_words(&[])).code, 0);
    let listed = h.js.go(&["--json", "lease", "list"]).json();
    assert!(
        listed["leases"][0]["labels"].get("js-run").is_none(),
        "{listed}"
    );
}

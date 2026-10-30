//! The lease calls time their wait for the table's lock and their saves of the lease
//! file on an injected clock, warn past one second and keep the record the hammer test's
//! guard prints.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use crate::support::{Harness, LogCapture, Scratch, Stepping, base_args, lease_body};

async fn served(name: &str, step_ms: u64) -> Harness {
    let scratch = Scratch::new(name);
    let config = crate::support::config(base_args(&scratch)).unwrap();
    let clock = Arc::new(Stepping::of(Duration::from_millis(step_ms)));
    Harness::over(scratch, config.with_monotonic(clock)).await
}

async fn one_lease_cycle(h: &Harness) {
    let made = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(made.status, 201, "{}", made.body);
    let id = made.json()["id"].as_str().unwrap().to_string();
    let renewed = h
        .call("POST", &format!("/v1/leases/{id}/renew"), Some(&json!({})))
        .await;
    assert_eq!(renewed.status, 200, "{}", renewed.body);
    let freed = h.call("DELETE", &format!("/v1/leases/{id}"), None).await;
    assert_eq!(freed.status, 200, "{}", freed.body);
}

/// Asserts that when every timed step is measured at 1.5 s, acquire, renew and release
/// each log one warning for the wait for the table's lock (`wait-acquire`, `wait-renew`,
/// `wait-release`, 1500 ms), and that the record holds those waits beside the saves
/// they were followed by: two for acquire and for release, one for renew. It catches a
/// handler that does not time its wait, one that times it under another handler's name,
/// and a record the server does not share with its table.
#[tokio::test]
async fn a_wait_for_the_table_over_a_second_warns_and_is_recorded() {
    let log = LogCapture::start();
    let h = served("timings_wait", 1500).await;
    one_lease_cycle(&h).await;
    let text = log.text();
    for name in ["wait-acquire", "wait-renew", "wait-release"] {
        let line = text
            .lines()
            .find(|l| l.contains(&format!("operation={name} ")));
        let line = line.unwrap_or_else(|| panic!("no warning for {name}: {text}"));
        assert!(
            line.contains("WARN") && line.contains("took_ms=1500"),
            "{line}"
        );
    }
    assert_eq!(
        h.running.timings().report(),
        "acquire: 1 timed, 1 over 1 s, longest 1500 ms; \
         acquire-settled: 1 timed, 1 over 1 s, longest 1500 ms; \
         release: 1 timed, 1 over 1 s, longest 1500 ms; \
         release-settled: 1 timed, 1 over 1 s, longest 1500 ms; \
         renew: 1 timed, 1 over 1 s, longest 1500 ms; \
         wait-acquire: 1 timed, 1 over 1 s, longest 1500 ms; \
         wait-release: 1 timed, 1 over 1 s, longest 1500 ms; \
         wait-renew: 1 timed, 1 over 1 s, longest 1500 ms"
    );
}

/// Asserts which side of the bound a wait of exactly 1 s is on: nothing is logged at
/// 1000 ms, the wait is still recorded, and at 1001 ms the wait is logged.
#[tokio::test]
async fn a_wait_of_exactly_a_second_is_not_slow_and_a_millisecond_more_is() {
    let log = LogCapture::start();
    let h = served("timings_edge_1000", 1000).await;
    one_lease_cycle(&h).await;
    assert!(!log.text().contains("slow"), "{}", log.text());
    let report = h.running.timings().report();
    assert!(
        report.contains("wait-acquire: 1 timed, 0 over 1 s, longest 1000 ms"),
        "{report}"
    );

    let h = served("timings_edge_1001", 1001).await;
    one_lease_cycle(&h).await;
    let text = log.text();
    assert!(
        text.contains("operation=wait-acquire took_ms=1001"),
        "{text}"
    );
}

/// Asserts that waits and saves under a second log nothing and are still recorded, so a
/// healthy run's report shows the short ones.
#[tokio::test]
async fn steps_under_a_second_log_nothing() {
    let log = LogCapture::start();
    let h = served("timings_quiet", 500).await;
    one_lease_cycle(&h).await;
    assert!(!log.text().contains("slow"), "{}", log.text());
    let report = h.running.timings().report();
    assert!(
        report.contains("wait-renew: 1 timed, 0 over 1 s, longest 500 ms"),
        "{report}"
    );
}

//! The failure-brake keys of an elastic pool in the configuration file: their defaults, the
//! plan they reach, and what is refused. Nothing here waits.

use std::time::Duration;

use crate::config_support::{PoolText, file, load, quote, refusal, server_with_agents};
use crate::support::Scratch;

fn mins(n: u64) -> Duration {
    Duration::from_secs(60 * n)
}

/// Asserts that a pool that says nothing about the brake gets a ceiling of 2 hours and a trip
/// after 3 failures, and that the keys, when given, reach the plan as written. It catches a
/// default that drifts and a key that is read and dropped.
#[test]
fn the_brake_keys_default_and_reach_the_plan() {
    let scratch = Scratch::new("brake_plan");
    let server = server_with_agents(&scratch);
    let plain = PoolText::elastic(&scratch);
    let config = load(&scratch, &file(&server, &[&plain])).unwrap();
    let plan = config.elastic_pool().unwrap();
    assert_eq!(plan.boot_fail_cooldown_max, mins(120));
    assert_eq!(plan.boot_fail_trip_after, 3);

    let given = plain
        .set("boot_fail_cooldown_max", &quote("30m"))
        .set("boot_fail_trip_after", "5");
    let config = load(&scratch, &file(&server, &[&given])).unwrap();
    let plan = config.elastic_pool().unwrap();
    assert_eq!(plan.boot_fail_cooldown_max, mins(30));
    assert_eq!(plan.boot_fail_trip_after, 5);
}

/// Asserts the refusals, each naming the pool and the key: a trip count of 0, above 100, not
/// an integer or negative; a ceiling below the first cooldown of 10 minutes, a zero and a
/// word. The edges (1, 100, exactly 10 minutes) load. It catches a brake that can be
/// configured away (a trip of 0) and a comparison off by one.
#[test]
fn a_bad_brake_value_is_refused_by_name() {
    let scratch = Scratch::new("brake_refused");
    let server = server_with_agents(&scratch);
    let cases = [
        ("boot_fail_trip_after", "0", "must be from 1 to 100"),
        ("boot_fail_trip_after", "101", "must be from 1 to 100"),
        ("boot_fail_trip_after", "-1", "must not be negative"),
        ("boot_fail_trip_after", "\"3\"", "must be an integer"),
        (
            "boot_fail_cooldown_max",
            "\"9m\"",
            "must be at least 10 minutes, the first cooldown",
        ),
        (
            "boot_fail_cooldown_max",
            "\"0s\"",
            "must be a positive duration",
        ),
        (
            "boot_fail_cooldown_max",
            "\"soon\"",
            "must be a positive duration",
        ),
    ];
    for (key, value, reason) in cases {
        let said = refusal(
            &scratch,
            &file(&server, &[&PoolText::elastic(&scratch).set(key, value)]),
        );
        assert!(
            said.starts_with(&format!("pool \"gpu\": {key}: {reason}")),
            "{key} = {value}: {said}"
        );
    }
    for (key, value) in [
        ("boot_fail_trip_after", "1"),
        ("boot_fail_trip_after", "100"),
        ("boot_fail_cooldown_max", "\"10m\""),
    ] {
        let pool = PoolText::elastic(&scratch).set(key, value);
        assert!(load(&scratch, &file(&server, &[&pool])).is_ok(), "{key}");
    }
}

/// Asserts that a static pool, which launches nothing, is refused each brake key by name.
/// It catches a key accepted by a pool it can never apply to.
#[test]
fn a_static_pool_is_refused_the_brake_keys() {
    let scratch = Scratch::new("brake_static");
    let server = server_with_agents(&scratch);
    for (key, value) in [
        ("boot_fail_trip_after", "3"),
        ("boot_fail_cooldown_max", "\"1h\""),
    ] {
        let pool = PoolText::static_pool(&scratch, "fixed").set(key, value);
        let said = refusal(&scratch, &file(&server, &[&pool]));
        assert!(
            said.ends_with(&format!("{key}: only for an elastic pool")),
            "{said}"
        );
    }
}

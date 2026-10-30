//! The host-lifetime keys of the configuration file: what a pool with accelerators must say
//! (`idle`, `max_age`, `min_host_lifetime`, and `server.max_lease_wall` beside it), the
//! rules between them, what a pool without accelerators may leave out, and the ceiling on a
//! lease's wall time on a running server. Nothing here waits.

use std::time::Duration;

use serde_json::json;

use crate::config_support::{PoolText, file, load, quote, refusal, server, server_with_agents};
use crate::support::{Harness, Scratch, ids, lease_body};

/// The reason a pool with accelerators is refused each of its required keys.
const NEED: &str = "required of a pool with accelerators: an idle host costs money by the hour, so the file names how long it may live";

/// The `[server]` table of [`server_with_agents`] without its lease ceiling.
fn without_ceiling(scratch: &Scratch) -> String {
    server_with_agents(scratch).replace("max_lease_wall = \"2h\"\n", "")
}

fn mins(n: u64) -> Duration {
    Duration::from_secs(60 * n)
}

/// Asserts that a pool whose provider has GPUs is refused for each of `idle`, `max_age`
/// and `min_host_lifetime` it leaves out, naming the pool and the key, and that a file
/// that sets all three loads. It catches a default creeping back into any of the three,
/// which is how an idle machine is left billing.
#[test]
fn an_accelerator_pool_must_say_idle_max_age_and_min_host_lifetime() {
    let scratch = Scratch::new("life_required");
    for key in ["idle", "max_age", "min_host_lifetime"] {
        let pool = PoolText::elastic(&scratch).without(key);
        let said = refusal(&scratch, &file(&server_with_agents(&scratch), &[&pool]));
        assert_eq!(said, format!("pool \"gpu\": {key}: {NEED}"));
    }
    let pool = PoolText::elastic(&scratch);
    assert!(load(&scratch, &file(&server_with_agents(&scratch), &[&pool])).is_ok());
}

/// Asserts that a pool with accelerators is refused when the `[server]` table has no
/// `max_lease_wall`: without it nothing bounds how long a draining host takes to empty or
/// what the cloud is told to enforce. It catches a pool that starts with no ceiling.
#[test]
fn an_accelerator_pool_needs_the_servers_lease_ceiling() {
    let scratch = Scratch::new("life_ceiling_needed");
    let pool = PoolText::elastic(&scratch);
    let said = refusal(&scratch, &file(&without_ceiling(&scratch), &[&pool]));
    assert_eq!(
        said,
        "server.max_lease_wall: required with a pool with accelerators: it bounds how long a lease, and so a draining host, can last"
    );
}

/// Asserts that a pool with no accelerators (the stand-in kind with a floor of none) loads
/// with none of the three keys and with no ceiling in the server table, and its plan says
/// so: no age, no minimum, no idle. With the ceiling set the plan carries it. It catches the
/// accelerator rule applied to every elastic pool, which would break the pools that exist.
#[test]
fn a_pool_without_accelerators_needs_none_of_them() {
    let scratch = Scratch::new("life_cpu_pool");
    let pool = PoolText::elastic(&scratch)
        .set("provider", "{ kind = \"any-mode\" }")
        .without("idle")
        .without("max_age")
        .without("min_host_lifetime");
    let config = load(&scratch, &file(&without_ceiling(&scratch), &[&pool])).unwrap();
    let plan = config.elastic_pool().unwrap();
    assert_eq!(plan.idle, None);
    assert_eq!(plan.max_age, None);
    assert_eq!(plan.min_host_lifetime, Duration::ZERO);
    assert_eq!(plan.max_lease_wall, None);

    let config = load(&scratch, &file(&server_with_agents(&scratch), &[&pool])).unwrap();
    assert_eq!(
        config.elastic_pool().unwrap().max_lease_wall,
        Some(mins(120))
    );
}

/// Asserts that the keys reach the plan the scaler is built from: `max_age`,
/// `min_host_lifetime` and the server's ceiling, as the file wrote them.
#[test]
fn the_lifetime_keys_reach_the_plan() {
    let scratch = Scratch::new("life_plan");
    let pool = PoolText::elastic(&scratch);
    let config = load(&scratch, &file(&server_with_agents(&scratch), &[&pool])).unwrap();
    let plan = config.elastic_pool().unwrap();
    assert_eq!(plan.max_age, Some(mins(120)));
    assert_eq!(plan.min_host_lifetime, mins(5));
    assert_eq!(plan.max_lease_wall, Some(mins(120)));
    let shown = format!("{plan:?}");
    assert!(shown.contains("max_age: Some(7200s)"), "{shown}");
}

/// Asserts the two rules between the keys, to the edge: `max_age` equal to `boot_timeout`
/// is refused (a machine would be retired before it could say hello) and one second more
/// is not; `min_host_lifetime` above `max_age` is refused (no host could be drained for
/// idleness) and equal to it is not. It catches a comparison off by one at either edge.
#[test]
fn max_age_must_outlast_boot_and_hold_the_minimum_lifetime() {
    let scratch = Scratch::new("life_rules");
    let server = server_with_agents(&scratch);
    let boot = PoolText::elastic(&scratch).set("boot_timeout", &quote("1h"));
    let equal = boot.set("max_age", &quote("1h"));
    assert_eq!(
        refusal(&scratch, &file(&server, &[&equal])),
        "pool \"gpu\": max_age: must be longer than boot_timeout"
    );
    let longer = equal
        .set("max_age", &quote("3601s"))
        .set("min_host_lifetime", &quote("3601s"));
    assert!(load(&scratch, &file(&server, &[&longer])).is_ok());
    let over = longer.set("min_host_lifetime", &quote("3602s"));
    assert_eq!(
        refusal(&scratch, &file(&server, &[&over])),
        "pool \"gpu\": min_host_lifetime: must not be longer than max_age"
    );
}

/// Asserts that a bad value of the three keys and of `server.max_lease_wall` is refused
/// naming the key (a zero and a word are not durations), without printing the value.
#[test]
fn a_bad_lifetime_value_is_refused_by_name() {
    let scratch = Scratch::new("life_values");
    let server = server_with_agents(&scratch);
    let duration = "must be a positive duration such as 600s, 10m or 1h";
    for key in ["max_age", "min_host_lifetime"] {
        let pool = PoolText::elastic(&scratch).set(key, &quote("0s"));
        assert_eq!(
            refusal(&scratch, &file(&server, &[&pool])),
            format!("pool \"gpu\": {key}: {duration}")
        );
    }
    let pool = PoolText::elastic(&scratch);
    let zero = server.replace("\"2h\"", "\"0s\"");
    assert_eq!(
        refusal(&scratch, &file(&zero, &[&pool])),
        format!("server.max_lease_wall: {duration}")
    );
}

/// Asserts, on a running server whose file sets `max_lease_wall` to two hours, that a
/// lease asking for a second more is refused 422 `wall-time-above-ceiling` and books
/// nothing, and that one asking for exactly the ceiling is granted. It catches a ceiling
/// that is read and not enforced, and an off-by-one at the edge.
#[tokio::test]
async fn a_lease_above_the_servers_ceiling_is_refused() {
    let scratch = Scratch::new("life_http");
    let pool = PoolText::static_pool(&scratch, "hosts");
    let head = server(&scratch, "listen = \"127.0.0.1:0\"");
    let config = load(&scratch, &file(&head, &[&pool])).unwrap();
    let h = Harness::over(scratch, config).await;

    let mut body = lease_body();
    body["max_wall_seconds"] = json!(7201);
    let refused = h.call("POST", "/v1/leases", Some(&body)).await;
    assert_eq!(refused.status, 422, "{}", refused.body);
    assert_eq!(
        refused.json(),
        json!({ "error": "wall-time-above-ceiling" })
    );
    assert!(ids(&h.call("GET", "/v1/leases", None).await).is_empty());

    body["max_wall_seconds"] = json!(7200);
    let granted = h.call("POST", "/v1/leases", Some(&body)).await;
    assert_eq!(granted.status, 201, "{}", granted.body);
}

/// Asserts the upper bound of 30 days on `max_age` and on `server.max_lease_wall`, to the
/// second: exactly 30 days loads and one second more is refused naming the key. It catches
/// a missing bound, which would let a typo make a host or a lease effectively unbounded.
#[test]
fn max_age_and_the_lease_ceiling_are_at_most_thirty_days() {
    let scratch = Scratch::new("life_bounds");
    let day30 = 30 * 24 * 3600;
    let server = server_with_agents(&scratch);
    for (secs, ok) in [(day30, true), (day30 + 1, false)] {
        let pool = PoolText::elastic(&scratch).set("max_age", &quote(&format!("{secs}s")));
        let loaded = load(&scratch, &file(&server, &[&pool]));
        assert_eq!(loaded.is_ok(), ok, "max_age {secs}");
        let head = server.replace("\"2h\"", &quote(&format!("{secs}s")));
        let pool = PoolText::elastic(&scratch);
        let loaded = load(&scratch, &file(&head, &[&pool]));
        assert_eq!(loaded.is_ok(), ok, "max_lease_wall {secs}");
    }
    let pool = PoolText::elastic(&scratch).set("max_age", &quote(&format!("{}s", day30 + 1)));
    assert_eq!(
        refusal(&scratch, &file(&server, &[&pool])),
        "pool \"gpu\": max_age: must not be longer than 30 days"
    );
    let head = server.replace("\"2h\"", &quote(&format!("{}s", day30 + 1)));
    let pool = PoolText::elastic(&scratch);
    assert_eq!(
        refusal(&scratch, &file(&head, &[&pool])),
        "server.max_lease_wall: must not be longer than 30 days"
    );
}

//! The load a registered host reports, as `GET /v1/hosts` lists it.

use serde_json::{Value, json};

use crate::agents_support::{JOIN, Session, beat, entry, hosts, join, post, start_agents};
use crate::support::{Harness, guard};

fn load(seq: u64) -> Value {
    json!({
        "seq": seq,
        "mono_usec": 61_000_000u64,
        "cpu_usage_usec": 125_000_000u64,
        "working_set_bytes": 7_000_000_000u64,
        "active_file_bytes": 250,
        "actions_cpu_limited": true,
        "covered": ["3-7"],
        "clocks": [{ "lease_id": "3-7", "started_mono_usec": 500_000 }],
        "minutes": [{
            "lease_id": "3-7",
            "minute": 0,
            "cpu_mean_millis": 1500,
            "cpu_peak_window_millis": 2000,
            "ws_max_bytes": 3_000_000_000u64,
        }],
    })
}

async fn beat_load(h: &Harness, session: &Session, load: Option<Value>) {
    let mut body = session.beat();
    if let Some(load) = load {
        body["load"] = load;
    }
    let reply = post(h, "/v1/agent/heartbeat", Some(JOIN), &body).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
}

/// Asserts the listing of a host's load in the units its field names state: nothing before
/// a load, then every figure of the latest reading (CPU time in microseconds, memory in
/// bytes, per-lease CPU in thousandths of a core, leases as `term-seq`); an older
/// reading leaves it unchanged; a heartbeat without a load removes it; and another host's
/// entry never carries it. It catches a unit changed between the wire and the listing, a
/// lease printed in the scheduler's `term.seq` form, a stale load left after the agent
/// stopped sending one, and a load that no client can poll.
#[tokio::test]
async fn the_hosts_listing_shows_the_latest_load_of_each_host() {
    let h = start_agents("agents_load", false).await;
    guard(async {
        let a = join(&h, "a").await;
        let b = join(&h, "b").await;
        assert!(entry(&hosts(&h).await, "a").get("load").is_none());
        beat_load(&h, &a, Some(load(5))).await;
        let listed = entry(&hosts(&h).await, "a");
        assert_eq!(
            listed["load"],
            json!({
                "seq": 5,
                "mono_usec": 61_000_000u64,
                "cpu_usage_usec": 125_000_000u64,
                "memory_working_set_bytes": 7_000_000_000u64,
                "memory_active_file_bytes": 250,
                "actions_cpu_limited": true,
                "covered": ["3-7"],
                "lease_clocks": [{ "lease": "3-7", "started_mono_usec": 500_000 }],
                "minutes": [{
                    "lease": "3-7",
                    "minute": 0,
                    "cpu_mean_millis": 1500,
                    "cpu_peak_window_millis": 2000,
                    "ws_max_bytes": 3_000_000_000u64,
                }],
            })
        );
        assert!(entry(&hosts(&h).await, "b").get("load").is_none());
        beat_load(&h, &a, Some(load(4))).await;
        assert_eq!(entry(&hosts(&h).await, "a")["load"]["seq"], 5);
        beat_load(&h, &b, Some(load(1))).await;
        assert_eq!(entry(&hosts(&h).await, "b")["load"]["seq"], 1);
        let plain = beat(&h, &a).await;
        assert_eq!(plain.status, 200);
        assert!(entry(&hosts(&h).await, "a").get("load").is_none());
        assert_eq!(entry(&hosts(&h).await, "b")["load"]["seq"], 1);
    })
    .await;
}

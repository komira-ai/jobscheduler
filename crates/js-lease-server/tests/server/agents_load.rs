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
    let h = start_agents("agents_load").await;
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

/// Asserts the listing of the per-second lists of a host's latest load: both lists in the
/// units their names state, oldest first; busy values alone (no pressure key) for a host
/// whose agent sent none of the other; a pressure list with a value above 1000 left out whole
/// while the busy list stays; and neither key once the host's latest beat has none, whether
/// that beat carries a load without them or no load. It catches a list printed in another
/// unit or order, a key printed empty, and seconds left on a host after its load changed.
#[tokio::test]
async fn the_hosts_listing_shows_the_seconds_of_the_latest_load() {
    let h = start_agents("agents_load_seconds").await;
    guard(async {
        let a = join(&h, "a").await;
        let mut with = load(1);
        with["second_cpu_millis"] = json!([1500, 0, 4000]);
        with["second_cpu_pressure_some_permille"] = json!([10, 0, 1000]);
        beat_load(&h, &a, Some(with)).await;
        let listed = entry(&hosts(&h).await, "a");
        assert_eq!(listed["load"]["second_cpu_millis"], json!([1500, 0, 4000]));
        assert_eq!(
            listed["load"]["second_cpu_pressure_some_permille"],
            json!([10, 0, 1000])
        );

        let mut busy = load(2);
        busy["second_cpu_millis"] = json!([250]);
        beat_load(&h, &a, Some(busy)).await;
        let listed = entry(&hosts(&h).await, "a");
        assert_eq!(listed["load"]["second_cpu_millis"], json!([250]));
        assert!(
            listed["load"]
                .get("second_cpu_pressure_some_permille")
                .is_none()
        );

        let mut broken = load(3);
        broken["second_cpu_millis"] = json!([7]);
        broken["second_cpu_pressure_some_permille"] = json!([5, 1001]);
        beat_load(&h, &a, Some(broken)).await;
        let listed = entry(&hosts(&h).await, "a");
        assert_eq!(listed["load"]["second_cpu_millis"], json!([7]));
        assert!(
            listed["load"]
                .get("second_cpu_pressure_some_permille")
                .is_none()
        );

        beat_load(&h, &a, Some(load(4))).await;
        let listed = entry(&hosts(&h).await, "a");
        assert_eq!(listed["load"]["seq"], 4);
        assert!(listed["load"].get("second_cpu_millis").is_none());
        let mut again = load(5);
        again["second_cpu_millis"] = json!([9]);
        beat_load(&h, &a, Some(again)).await;
        assert_eq!(beat(&h, &a).await.status, 200);
        assert!(entry(&hosts(&h).await, "a").get("load").is_none());
    })
    .await;
}

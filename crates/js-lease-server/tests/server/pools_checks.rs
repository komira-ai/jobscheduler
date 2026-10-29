//! The background checks of an elastic pool: when they run, what they find, how a failure
//! shows, and that a failure or a panic never stops the server. The cloud and the network
//! mode are fakes and time is a manual clock; nothing waits for time to pass.

use js_lease_server::network::{ApiFailure, NetworkError};
use serde_json::{Value, json};

use crate::pools_support::{Net, Options, Rig, a10, leaked, offer};
use crate::support::{T0, TOKEN, request};

/// A server over a cloud that offers one A10.
async fn rig(name: &str) -> Rig {
    Rig::start(name, vec![a10("gpu_1x_a10", 1, 129)], Options::default()).await
}

/// The reasons of the pool, as text.
fn reasons(pool: &Value) -> Vec<String> {
    let all = pool["reasons"].as_array().unwrap();
    all.iter().map(|r| r.as_str().unwrap().to_owned()).collect()
}

/// Asserts when the checks run and what an available pool shows. A pool begins `checking`
/// and asks nothing of the cloud or the network until a tick; its first pass makes it
/// `available` with both checks stamped by the injected clock; a tick at which no pass is
/// due asks nothing; the next pass is due 60 s after the last. Nothing is launched, minted
/// or listed. It catches a pool that is available before a pass, one that checks at every
/// tick, one that never checks again, and a check that launches.
#[tokio::test]
async fn a_pool_is_checking_until_a_pass_and_then_checked_every_minute() {
    let rig = rig("checks_cadence").await;
    let before = rig.pool().await;
    assert_eq!(before["state"], "checking");
    assert_eq!(before["credentials"], Value::Null);
    assert_eq!(before["network_check"], Value::Null);
    assert_eq!(before["blocked"], "unavailable");
    assert_eq!(before["next_launch"], Value::Null);
    assert_eq!(before["no_offer"], Value::Null);
    assert_eq!(before["reasons"], json!([]));
    assert_eq!((rig.cloud.catalogs(), rig.net.checks()), (0, 0));

    rig.step().await;
    let after = rig.pool().await;
    assert_eq!(after["state"], "available");
    let held = json!({ "ok": true, "reason": null, "checked_at_ms": T0 });
    assert_eq!(after["credentials"], held);
    assert_eq!(after["network_check"], held);
    assert_eq!(after["blocked"], Value::Null);
    assert_eq!(after["reasons"], json!([]));
    assert_eq!(
        (&after["name"], &after["kind"], &after["network"]),
        (&json!("gpu"), &json!("fake-cloud"), &json!("default"))
    );
    assert_eq!(
        (&after["exec"], &after["launch"]),
        (&json!("none"), &json!("plan"))
    );
    let instances = json!({
        "n": 0, "max": 4, "spend_cents_per_hour": 0, "max_spend_cents_per_hour": 1200
    });
    assert_eq!(after["instances"], instances);
    assert_eq!(
        (after["machines"].clone(), after["orphans"].clone()),
        (json!([]), json!([]))
    );
    assert_eq!(after["last_launch"], Value::Null);
    assert_eq!((rig.cloud.catalogs(), rig.net.checks()), (1, 1));

    rig.step().await;
    assert_eq!((rig.cloud.catalogs(), rig.net.checks()), (1, 1));
    rig.advance(59);
    rig.step().await;
    assert_eq!((rig.cloud.catalogs(), rig.net.checks()), (1, 1));
    rig.advance(1);
    rig.step().await;
    assert_eq!((rig.cloud.catalogs(), rig.net.checks()), (2, 2));
    let later = rig.pool().await;
    assert_eq!(later["credentials"]["checked_at_ms"], T0 + 60_000);
    rig.assert_clean();
}

/// Asserts that a failing check marks the pool unavailable with the design's message and
/// that the server goes on: a refused key shows as `credentials: failed (<code>)`, a
/// refused network credential as the network mode's own message, both are listed, and
/// the health route, the table's tick and a client route still answer. It catches a check
/// that exits, hangs or panics the server, and a message that names nothing.
#[tokio::test]
async fn a_failing_check_marks_the_pool_unavailable_and_the_server_goes_on() {
    let rig = rig("checks_failing").await;
    rig.cloud.fail("global/invalid-api-key");
    rig.net
        .fail(NetworkError::Credentials(ApiFailure::Rejected(401)));
    rig.step().await;
    let pool = rig.pool().await;
    let key = "credentials: failed (global/invalid-api-key)";
    let net = "network: credentials failed: refused with status 401";
    assert_eq!(pool["state"], "unavailable");
    assert_eq!(pool["blocked"], "unavailable");
    assert_eq!(pool["credentials"]["ok"], false);
    assert_eq!(pool["credentials"]["reason"], key);
    assert_eq!(pool["network_check"]["ok"], false);
    assert_eq!(pool["network_check"]["reason"], net);
    assert_eq!(reasons(&pool), [key, net]);

    let health = request(rig.running.addr(), "GET", "/healthz", None, None).await;
    assert_eq!((health.status, health.body.as_str()), (200, "ok"));
    rig.ticks.step().await;
    let leases = request(rig.running.addr(), "GET", "/v1/leases", Some(TOKEN), None).await;
    assert_eq!(leases.status, 200, "{}", leases.body);
    assert!(rig.logs.text().contains("the pool fails its checks"));
    rig.assert_clean();
}

/// Asserts the backoff and the recovery with no real time: a failed pool is checked again
/// 10 s later, then 20 s, then 40 s after the failure before; when the cloud answers again
/// the next pass makes it `available`, and it is checked every 60 s from then. It catches
/// a retry at every tick, a wait that does not double, and a pool that stays unavailable
/// after the cause is gone.
#[tokio::test]
async fn a_failed_pool_is_checked_after_10_s_then_20_s_and_recovers() {
    let rig = rig("checks_backoff").await;
    rig.cloud.fail("global/forbidden");
    rig.step().await;
    assert_eq!(rig.cloud.catalogs(), 1);
    let waits = [(9, 1, 2), (19, 1, 3), (39, 1, 4)];
    for (early, rest, calls) in waits {
        rig.advance(early);
        rig.step().await;
        assert_eq!(rig.cloud.catalogs(), calls - 1, "early by {early} s");
        rig.advance(rest);
        rig.step().await;
        assert_eq!(rig.cloud.catalogs(), calls, "after {early} s and {rest} s");
        if calls == 3 {
            rig.cloud.offers(vec![a10("gpu_1x_a10", 1, 129)]);
        }
    }
    let pool = rig.pool().await;
    assert_eq!(pool["state"], "available", "{pool}");
    assert_eq!(pool["reasons"], json!([]));
    assert!(rig.logs.text().contains("the pool passed its checks"));
    rig.advance(59);
    rig.step().await;
    assert_eq!(rig.cloud.catalogs(), 4);
    rig.advance(1);
    rig.step().await;
    assert_eq!(rig.cloud.catalogs(), 5);
    rig.assert_clean();
}

/// Asserts what is wrong with a catalog and that each kind is said once, for the first
/// offer that has it: no offer at all, an offer with no price, an offer with fewer GPUs
/// than the pool floor, an offer of another GPU class, and two of these at once; and that
/// a good catalog makes the pool available again. It catches a pool that launches from a
/// catalog it cannot price, and a message that does not name the offer.
#[tokio::test]
async fn a_bad_catalog_makes_the_pool_unavailable_and_names_the_offer() {
    let rig = rig("checks_catalog").await;
    let cases: Vec<(Vec<js_elastic::Offer>, Vec<&str>)> = vec![
        (vec![], vec!["catalog: no offers"]),
        (
            vec![offer("gpu_1x_a10", 1, Some("nvidia-a10"), None, true)],
            vec!["catalog: gpu_1x_a10 has no price"],
        ),
        (
            vec![offer("gpu_cpu", 0, None, Some(10), true)],
            vec!["catalog: gpu_cpu has 0 gpus, below the pool floor of 1"],
        ),
        (
            vec![offer("gpu_1x_h100", 1, Some("nvidia-h100"), Some(10), true)],
            vec!["catalog: gpu_1x_h100 has gpus of another class than the pool's"],
        ),
        (
            vec![
                offer("gpu_a", 0, None, None, true),
                offer("gpu_b", 0, None, None, true),
            ],
            vec![
                "catalog: gpu_a has no price",
                "catalog: gpu_a has 0 gpus, below the pool floor of 1",
            ],
        ),
        (vec![a10("gpu_1x_a10", 1, 129)], vec![]),
    ];
    for (offers, expected) in cases {
        rig.cloud.offers(offers);
        rig.advance(300);
        rig.step().await;
        let pool = rig.pool().await;
        assert_eq!(reasons(&pool), expected, "{pool}");
        let state = if expected.is_empty() {
            "available"
        } else {
            "unavailable"
        };
        assert_eq!(pool["state"], state);
        assert_eq!(pool["credentials"]["ok"], true);
    }
    rig.assert_clean();
}

/// Asserts that a check that panics is contained to its half of the pass: a panic in the
/// provider's catalog fails the credentials with a message, the network half still
/// answers, and the server serves on; then the same for a panic in the network mode. It
/// catches a panic that takes the check loop, the tick loop or the server down.
#[tokio::test]
async fn a_panicking_check_fails_its_half_and_nothing_else() {
    let rig = rig("checks_panic").await;
    rig.cloud.panic(true);
    rig.step().await;
    let pool = rig.pool().await;
    assert_eq!(pool["state"], "unavailable");
    assert_eq!(
        pool["credentials"]["reason"],
        "credentials: the check panicked"
    );
    assert_eq!(pool["network_check"]["ok"], true);
    assert_eq!(rig.net.checks(), 1);
    rig.ticks.step().await;
    let health = request(rig.running.addr(), "GET", "/healthz", None, None).await;
    assert_eq!(health.status, 200);

    rig.cloud.panic(false);
    rig.net.panic(true);
    rig.advance(300);
    rig.step().await;
    let pool = rig.pool().await;
    assert_eq!(pool["credentials"]["ok"], true);
    assert_eq!(
        pool["network_check"]["reason"],
        "network: the check panicked"
    );
    assert_eq!(reasons(&pool), ["network: the check panicked"]);

    rig.net.panic(false);
    rig.advance(300);
    rig.step().await;
    assert_eq!(rig.pool().await["state"], "available");
    rig.assert_clean();
}

/// Asserts that the mode a pool names is the mode its network check asks. The `default`
/// mode needs the address agents are told to dial: without `server.advertise_url` the
/// pool is unavailable and says so; with one it passes with no call to any service. A
/// `tailscale` pool is built (and so cannot reach Tailscale from a test) only when a
/// static build is pinned; without one it is unavailable and says so, and never calls.
/// It catches a mode that is not checked, and a pool that fetches nothing and says
/// nothing.
#[tokio::test]
async fn the_network_check_is_the_check_of_the_mode_the_pool_names() {
    let no_url = Options {
        advertise: None,
        net: Net::Default,
        ..Options::default()
    };
    let rig = Rig::start("checks_net_no_url", vec![a10("gpu_1x_a10", 1, 129)], no_url).await;
    rig.step().await;
    let pool = rig.pool().await;
    let said = "network: server.advertise_url is not set, so a launched host has nowhere to dial";
    assert_eq!(pool["state"], "unavailable");
    assert_eq!(pool["network_check"]["reason"], said);
    assert_eq!(pool["credentials"]["ok"], true);

    let plain = Options {
        net: Net::Default,
        ..Options::default()
    };
    let rig = Rig::start("checks_net_default", vec![a10("gpu_1x_a10", 1, 129)], plain).await;
    rig.step().await;
    assert_eq!(rig.pool().await["state"], "available");

    let unpinned = Options {
        net: Net::Tailscale { pinned: false },
        ..Options::default()
    };
    let rig = Rig::start(
        "checks_ts_unpinned",
        vec![a10("gpu_1x_a10", 1, 129)],
        unpinned,
    )
    .await;
    rig.step().await;
    let pool = rig.pool().await;
    assert_eq!(pool["network"], "tailscale");
    assert_eq!(pool["state"], "unavailable");
    assert_eq!(
        pool["network_check"]["reason"],
        "network: tailscale: no static build is pinned in this release"
    );
    rig.assert_clean();

    let pinned = Options {
        net: Net::Tailscale { pinned: true },
        ..Options::default()
    };
    let rig = Rig::start("checks_ts_pinned", vec![a10("gpu_1x_a10", 1, 129)], pinned).await;
    let pool = rig.pool().await;
    assert_eq!(
        (&pool["network"], &pool["state"]),
        (&json!("tailscale"), &json!("checking"))
    );
    rig.assert_clean();
}

/// The control of the grep for secrets: it finds a planted secret where one is, so a clean
/// result means something.
#[test]
fn the_grep_for_secrets_finds_a_planted_one() {
    let planted = format!("a {} b {}", crate::pools_support::NET_SECRET, TOKEN);
    assert_eq!(leaked(&planted).len(), 2);
    assert!(leaked("nothing here").is_empty());
}

/// Asserts the agent pins against the catalog. A pool that launches and has an agent
/// pinned for x86_64 only is unavailable while the catalog holds an offer of aarch64, and
/// says which offer, which architecture and which field; a pool in `plan` mode is not
/// held to it, and a live pool whose offers are all pinned is available. It catches a pool
/// that would launch a machine for which it has no agent, or fall back to another
/// architecture's.
#[tokio::test]
async fn a_live_pool_is_unavailable_for_an_offer_of_an_architecture_it_has_no_agent_for() {
    let mut arm = a10("gpu_1x_a10_arm", 1, 129);
    arm.arch = "aarch64".to_owned();
    let live = Options {
        live: true,
        exec: "container",
        ..Options::default()
    };
    let rig = Rig::start("checks_pins_live", vec![arm.clone()], live).await;
    rig.step().await;
    let pool = rig.pool().await;
    assert_eq!(pool["state"], "unavailable");
    assert_eq!(
        reasons(&pool),
        [
            "catalog: gpu_1x_a10_arm is for the architecture aarch64, which has no agent pin (agent.aarch64)"
        ]
    );

    let rig = Rig::start("checks_pins_plan", vec![arm], Options::default()).await;
    rig.step().await;
    assert_eq!(rig.pool().await["state"], "available");

    let pinned = Rig::start("checks_pins_ok", vec![a10("gpu_1x_a10", 1, 129)], live).await;
    pinned.step().await;
    assert_eq!(pinned.pool().await["state"], "available");
}

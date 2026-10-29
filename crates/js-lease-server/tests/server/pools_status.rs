//! The status routes: the plan of the next launch, masked; the offer chosen; why no offer
//! serves; the head of the wait list; and that no secret is in any reply or log.

use js_elastic::{AcquireId, Artifact, BootPlan, BootStep, HostName, SchedulerId};
use js_lease_server::bootstrap::{Fill, render};
use js_lease_server::network::{LaunchCtx, NetworkMode};
use js_registry::Launchable;
use serde_json::{Value, json};

use crate::pools_support::{
    ADVERTISE, FakeNet, NET_SECRET, Net, Options, Rig, a10, net_steps, offer,
};
use crate::support::{TOKEN, lease_body, request};

/// Asserts the next launch: the cheapest offer that has capacity and fits (an unavailable
/// cheaper one and a dearer one are passed over), and a plan that is exactly the masked
/// render of the plan built from the same pieces (agent address and digest, the provider's
/// host preparation, the network mode's steps), naming the secret slots, holding none of
/// the secrets, with nothing minted. It catches a plan the code would not follow and a
/// plan that carries a secret.
#[tokio::test]
async fn the_next_launch_is_the_cheapest_offer_with_a_masked_plan() {
    let mut cheap = a10("gpu_1x_cheap", 1, 50);
    cheap.available = false;
    let offers = vec![a10("gpu_2x_a10", 2, 258), cheap, a10("gpu_1x_a10", 1, 129)];
    let rig = Rig::start("status_plan", offers, Options::default()).await;
    rig.step().await;
    let pool = rig.pool().await;
    let next = &pool["next_launch"];
    assert_eq!(pool["state"], "available");
    assert_eq!(pool["no_offer"], Value::Null);
    assert_eq!(next["offer"], "gpu_1x_a10");
    assert_eq!(next["zone"], "us-west-1");
    assert_eq!(next["plan_error"], Value::Null);
    assert_eq!(
        next["secret_slots"],
        json!(["join_token", "network.fake.key"])
    );

    let scheduler = SchedulerId::new("preview").unwrap();
    let host = HostName::derive(&scheduler, &AcquireId::new("preview").unwrap());
    assert_eq!(next["name"], host.to_string());
    let net = FakeNet::new();
    let ctx = LaunchCtx {
        pool: "gpu",
        host: &host,
        valid_for: std::time::Duration::from_secs(600),
    };
    let plan = BootPlan {
        agent: Artifact {
            url: format!("{ADVERTISE}/v1/bootstrap/agent/x86_64"),
            sha256: [0xab; 32],
        },
        scheduler_url: ADVERTISE.to_owned(),
        pool: "gpu".to_owned(),
        host: host.clone(),
        host_prep: vec![BootStep::Run(vec![
            "apt-get".to_owned(),
            "install".to_owned(),
            "fake-driver".to_owned(),
        ])],
        network: net.describe(&ctx),
    };
    assert_eq!(plan.network, net_steps());
    let masked = render(&plan, Fill::Mask).unwrap();
    assert_eq!(next["plan"], masked);
    assert!(masked.contains("'***'"), "{masked}");
    assert!(!masked.contains(NET_SECRET));
    assert_eq!(rig.net.prepares(), 0);
    rig.assert_clean();
}

/// Asserts why no offer serves: the dimension named for a catalog with nothing that fits,
/// `available` when what fits has no capacity, `catalog` when there is no offer, and that
/// the request is the pool floor while none waits. It catches a status that shows a plan
/// for an offer that cannot serve.
#[tokio::test]
async fn no_offer_names_the_dimension_that_fails() {
    let mut full = a10("gpu_1x_a10", 1, 129);
    full.available = false;
    let cases = [
        (vec![full], "available"),
        (vec![offer("gpu_cpu", 0, None, Some(10), true)], "gpus"),
        (vec![], "catalog"),
    ];
    for (index, (offers, dimension)) in cases.into_iter().enumerate() {
        let name = format!("status_no_offer_{index}");
        let rig = Rig::start(&name, offers, Options::default()).await;
        rig.step().await;
        let pool = rig.pool().await;
        assert_eq!(
            pool["no_offer"],
            json!({ "request": "floor", "dimension": dimension })
        );
        assert_eq!(pool["next_launch"], Value::Null);
        rig.assert_clean();
    }
}

/// The wait list gets a whole-node request for one A10 and a few bytes of memory, with a
/// launchable that says yes, and returns its id.
async fn wait_for_small(rig: &Rig) -> String {
    let registry = rig.running.registry().unwrap();
    registry.set_launchable(Some(Launchable::new(|_| true)));
    let mut body = lease_body();
    body["class"] = json!("whole-node");
    body["capabilities"] = json!({});
    body["resources"]["gpu_class"] = json!("nvidia-a10");
    let bytes = body.to_string().into_bytes();
    let addr = rig.running.addr();
    let asked = request(addr, "POST", "/v1/leases", Some(TOKEN), Some(&bytes)).await;
    assert_eq!(asked.status, 202, "{}", asked.body);
    let waiting = registry.waiting();
    assert_eq!(waiting.len(), 1);
    waiting[0].id.to_string()
}

/// Asserts that the head of the wait list sets the next launch: with nothing waiting the
/// pool floor (1 GiB) takes the offer that has the memory; once a request for a few bytes
/// waits, the cheaper small offer serves it. It catches a plan made for the floor while a
/// request waits.
#[tokio::test]
async fn the_head_of_the_wait_list_sets_the_next_launch() {
    let mut tiny = a10("gpu_tiny", 1, 50);
    tiny.memory_bytes = 512 << 20;
    let offers = vec![a10("gpu_big", 1, 129), tiny];
    let rig = Rig::start("status_waiting", offers, Options::default()).await;
    rig.step().await;
    assert_eq!(rig.pool().await["next_launch"]["offer"], "gpu_big");
    wait_for_small(&rig).await;
    assert_eq!(rig.pool().await["next_launch"]["offer"], "gpu_tiny");
    rig.assert_clean();
}

/// Asserts that a plan that cannot be made says why, each in `plan_error` with no plan and
/// no slots: no `server.advertise_url`, no known digest for the offer's architecture, a
/// `tailscale` pool with no build pinned, and a plan over the step limit.
#[tokio::test]
async fn a_plan_that_cannot_be_made_says_why() {
    let offers = vec![a10("gpu_1x_a10", 1, 129)];
    let no_url = Options {
        advertise: None,
        ..Options::default()
    };
    let no_digest = Options {
        digest: false,
        ..Options::default()
    };
    let tailscale = Options {
        net: Net::Tailscale { pinned: false },
        ..Options::default()
    };
    let cases = [
        (no_url, "server.advertise_url is not set"),
        (no_digest, "the agent binary for x86_64 is not available"),
        (
            tailscale,
            "network: tailscale: no static build is pinned in this release",
        ),
    ];
    for (index, (options, said)) in cases.into_iter().enumerate() {
        let name = format!("status_plan_error_{index}");
        let rig = Rig::start(&name, offers.clone(), options).await;
        rig.step().await;
        let next = rig.pool().await["next_launch"].clone();
        assert_eq!(next["plan"], Value::Null);
        assert_eq!(next["plan_error"], said);
        assert_eq!(next["secret_slots"], json!([]));
        rig.assert_clean();
    }
    let rig = Rig::start("status_plan_steps", offers, Options::default()).await;
    rig.cloud
        .prep(vec![BootStep::Run(vec!["true".to_owned()]); 70]);
    rig.step().await;
    let next = rig.pool().await["next_launch"].clone();
    assert_eq!(next["plan_error"], "a plan holds at most 64 steps, not 72");
    rig.assert_clean();
}

/// Asserts that no secret is in any reply, `Debug` or log line through every state of a
/// pool: checking, available, and failing with a message that holds markup. The pages
/// are fetched too. It catches a status that prints a key, a token or a minted secret.
#[tokio::test]
async fn no_secret_is_in_any_reply_or_log() {
    let rig = Rig::start(
        "status_secrets",
        vec![a10("gpu_1x_a10", 1, 129)],
        Options::default(),
    )
    .await;
    rig.pools().await;
    assert_eq!(rig.get("/status").await.status, 200);
    rig.step().await;
    rig.pools().await;
    assert_eq!(rig.get("/status").await.status, 200);
    rig.cloud.fail("<b>bad</b> & \"worse\"");
    rig.advance(300);
    rig.step().await;
    let page = rig.get("/status").await;
    assert!(
        page.body
            .contains("&lt;b&gt;bad&lt;/b&gt; &amp; &quot;worse&quot;"),
        "{}",
        page.body
    );
    assert!(!page.body.contains("<b>"));
    rig.assert_clean();
}

/// Asserts the page: it is HTML with headers that forbid caching, sniffing and anything
/// it loads, and it shows the same state, the masked plan and the slots as the JSON.
#[tokio::test]
async fn the_page_shows_the_same_data() {
    let rig = Rig::start(
        "status_page",
        vec![a10("gpu_1x_a10", 1, 129)],
        Options::default(),
    )
    .await;
    let early = rig.get("/status").await;
    assert!(early.body.contains("not checked yet"), "{}", early.body);
    rig.step().await;
    let page = rig.get("/status").await;
    assert_eq!(page.status, 200);
    for header in [
        "content-type: text/html; charset=utf-8",
        "cache-control: no-store",
        "x-content-type-options: nosniff",
        "content-security-policy: default-src 'none'",
    ] {
        assert!(page.head.contains(header), "{header}: {}", page.head);
    }
    for text in [
        "<h2>gpu</h2>",
        "available",
        "gpu_1x_a10",
        "network.fake.key",
        "'***'",
    ] {
        assert!(
            page.body.contains(&text.replace('\'', "&#39;")),
            "{text}: {}",
            page.body
        );
    }
    rig.cloud.fail("global/forbidden");
    rig.advance(300);
    rig.step().await;
    let page = rig.get("/status").await;
    assert!(page.body.contains("failed at"), "{}", page.body);
    assert!(page.body.contains("credentials: failed (global/forbidden)"));
    rig.assert_clean();
}

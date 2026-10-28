//! The hello route with a one-time token, when two hellos of one host interleave.

use std::collections::BTreeMap;
use std::path::Path;

use js_agent_wire::InstanceIdentity;
use js_lease::{ManualClock, Timestamp};
use js_registry::{PoolConfig, Registry, SequentialSessions};
use js_types::{ProviderId, ResourceClass, Resources, Shape};
use serde_json::json;

use super::*;
use crate::adapt::SharedClock;

fn instance() -> InstanceIdentity {
    InstanceIdentity {
        provider: "lambdalabs".to_string(),
        instance_id: "i-1".to_string(),
    }
}

/// The body of a hello of `host` that names the instance `i-1`.
fn body(host: &str) -> Bytes {
    let hello = json!({
        "host_id": host,
        "pool": "burst",
        "agent_version": "0.1.0",
        "instance": { "provider": "lambdalabs", "instance_id": "i-1" },
        "shape": {
            "cpu_millis": 8000,
            "memory_bytes": 17179869184u64,
            "gpus": 1,
            "gpu_class": "nvidia-a100",
        },
        "phase": "serving",
    });
    Bytes::from(hello.to_string())
}

fn agents(name: &str) -> Arc<Agents> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/agent_api_unit")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let clock = ManualClock::new(Timestamp::from_millis(5_000_000));
    let shared = SharedClock::new(clock.clone());
    let resources = Resources::new(8000, 17179869184, 1, Some("nvidia-a100".to_string())).unwrap();
    let shape = Shape::new(resources, BTreeMap::new());
    let name = ProviderId::new("burst").unwrap();
    let pool = PoolConfig::new(name, ResourceClass::gpu_host(), shape, 4);
    let registry = Registry::with_sessions(pool, clock, SequentialSessions::new());
    Arc::new(Agents::over(registry, &shared, &dir))
}

/// Of two hellos of one host with one fresh token, the first is admitted fresh, then the
/// second (admitted again) is accepted by the registry before the first is asked, and the
/// registry then refuses the first (the host is live). The first hello's release must
/// leave the token used by that host: the host heartbeats with it and another host id is
/// refused as used. It catches a release that frees a token whose host the registry
/// accepted, which would let a second host register with the token. The second hello runs
/// inside the seam of the first, so the order is fixed and nothing waits on timing.
#[tokio::test]
async fn a_refused_fresh_hello_does_not_free_a_token_an_again_hello_registered() {
    let agents = agents("fresh_refused_again_accepted");
    let minted = agents
        .joins
        .mint(instance(), Duration::from_secs(600))
        .unwrap();
    let who = Credential::of_token(&minted.token);
    let second = || {
        let admitted = agents.joins.admit(&who, "host-a", Some(&instance()));
        assert_eq!(admitted, Ok(Admission::Again));
        let hello: Hello = serde_json::from_slice(&body("host-a")).unwrap();
        agents
            .registry
            .hello(&hello, &who)
            .expect("the second hello is accepted");
    };
    let first = serve_hello(&agents, who.clone(), true, Ok(body("host-a")), &second).await;
    let refused = first.expect_err("the registry refuses the first hello");
    assert_eq!((refused.status.as_u16(), refused.error), (409, "host-live"));
    assert_eq!(agents.joins.serves(&who, "host-a"), Ok(()));
    let other = agents.joins.admit(&who, "host-b", Some(&instance()));
    assert_eq!(other, Err(JoinRefusal::Used));
}

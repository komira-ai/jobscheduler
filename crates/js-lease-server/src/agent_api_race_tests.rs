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

/// The body of a hello of `host` that names the instance `i-1` and says it keeps a host
/// credential.
fn asking(host: &str) -> Bytes {
    let mut hello: serde_json::Value = serde_json::from_slice(&body(host)).unwrap();
    hello["accepts_host_credential"] = json!(true);
    Bytes::from(hello.to_string())
}

/// The rows of the join token file of the agents made by `agents(name)`.
fn rows(name: &str) -> usize {
    let file = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/agent_api_unit")
        .join(name)
        .join("joins");
    std::fs::read_to_string(file)
        .unwrap()
        .matches("[[join]]")
        .count()
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
    let first = serve_hello(
        &agents,
        who.clone(),
        Some(who.clone()),
        Ok(body("host-a")),
        &second,
    )
    .await;
    let refused = first.expect_err("the registry refuses the first hello");
    assert_eq!((refused.status.as_u16(), refused.error), (409, "host-live"));
    assert_eq!(agents.joins.serves(&who, "host-a"), Ok(()));
    let other = agents.joins.admit(&who, "host-b", Some(&instance()));
    assert_eq!(other, Err(JoinRefusal::Used));
}

/// A revoke that lands between the admit and the registry call: the registry still accepts
/// the hello (the host is registered), but the token is gone, so it is unknown afterwards
/// (the route answers its heartbeats 401) and serves no host. It catches a hello that
/// puts a revoked token back, for example by releasing the use as a rewrite of the entry.
#[tokio::test]
async fn a_revoke_between_admit_and_registry_leaves_the_token_gone() {
    let agents = agents("revoke_between");
    let minted = agents
        .joins
        .mint(instance(), Duration::from_secs(600))
        .unwrap();
    let who = Credential::of_token(&minted.token);
    let revoke = || {
        assert_eq!(agents.joins.revoke(&instance()), Ok(1));
    };
    let first = serve_hello(
        &agents,
        who.clone(),
        Some(who.clone()),
        Ok(body("host-a")),
        &revoke,
    )
    .await;
    assert!(first.is_ok());
    assert!(agents.joins.identify(&minted.token).is_none());
    assert_eq!(
        agents.joins.serves(&who, "host-a"),
        Err(JoinRefusal::NotAdmitted)
    );
}

/// Of two first hellos that both ask for a credential, the one admitted first is given the
/// credential and retires the token; the other, which reaches the token in the seam of the
/// first, finds it gone. One credential exists, it authenticates, the token does not, and
/// the file holds one row. The second hello runs inside the seam of the first, so the order
/// is fixed and nothing waits on timing. It catches two credentials for one token, and a
/// token that is left good beside its credential.
#[tokio::test]
async fn of_two_first_hellos_one_credential_is_made_and_the_token_retired() {
    let agents = agents("credential_race");
    let minted = agents
        .joins
        .mint(instance(), Duration::from_secs(600))
        .unwrap();
    let who = Credential::of_token(&minted.token);
    let raced = || {
        let other = agents
            .joins
            .admit_with(&who, "host-a", Some(&instance()), true);
        assert_eq!(other, Err(JoinRefusal::Expired));
        let another = agents
            .joins
            .admit_with(&who, "host-b", Some(&instance()), true);
        assert_eq!(another, Err(JoinRefusal::Expired));
    };
    let first = serve_hello(
        &agents,
        who.clone(),
        Some(who.clone()),
        Ok(asking("host-a")),
        &raced,
    )
    .await;
    let Json(welcome) = first.expect("the first hello is admitted");
    let credential = welcome.host_credential.expect("it is given the credential");
    assert!(agents.joins.identify(&minted.token).is_none());
    let digest = agents.joins.identify(credential.expose());
    assert!(digest.is_some());
    assert_eq!(agents.joins.registered_as(&digest.unwrap()), who);
    assert_eq!(rows("credential_race"), 1);
}

/// The registry refuses the hello that issued a credential (the host id is bound to another
/// credential, which a hello in the seam arranges): the answer is the refusal, the token is
/// unused again, no credential row remains, and a hello of another host with the token is
/// then given a credential. It catches a refused hello that leaves the token retired for a
/// credential nobody received, which would strand the instance until its boot time ran out.
#[tokio::test]
async fn a_refused_hello_gives_the_credential_back_with_the_token() {
    let agents = agents("credential_refused");
    let minted = agents
        .joins
        .mint(instance(), Duration::from_secs(600))
        .unwrap();
    let who = Credential::of_token(&minted.token);
    let rival = || {
        let hello: Hello = serde_json::from_slice(&body("host-a")).unwrap();
        let elsewhere = Credential::of_token("another-token");
        agents.registry.hello(&hello, &elsewhere).unwrap();
    };
    let first = serve_hello(
        &agents,
        who.clone(),
        Some(who.clone()),
        Ok(asking("host-a")),
        &rival,
    )
    .await;
    let refused = first.expect_err("the registry refuses the first hello");
    assert_eq!(
        (refused.status.as_u16(), refused.error),
        (403, "host-bound")
    );
    assert!(agents.joins.identify(&minted.token).is_some());
    assert_eq!(rows("credential_refused"), 1);
    let retry = serve_hello(
        &agents,
        who.clone(),
        Some(who.clone()),
        Ok(asking("host-b")),
        &|| (),
    )
    .await;
    let Json(welcome) = retry.expect("another host is admitted with the token");
    assert!(welcome.host_credential.is_some());
    assert!(agents.joins.identify(&minted.token).is_none());
}

/// The welcome that carries the credential is lost on the wire after the server admitted the
/// host. The host holds nothing that authenticates: the token is unknown, a hello that was
/// already past the check is refused, and no other host can use the token. The release of
/// the instance (the revoke the scaler's boot timeout makes) leaves nothing in the file. It
/// catches a token that stays good after its credential was issued, which would let the
/// host, or a copy of the token, register again.
#[tokio::test]
async fn a_lost_welcome_leaves_no_way_in_and_the_revoke_clears_the_file() {
    let agents = agents("credential_lost");
    let minted = agents
        .joins
        .mint(instance(), Duration::from_secs(600))
        .unwrap();
    let who = Credential::of_token(&minted.token);
    let sent = serve_hello(
        &agents,
        who.clone(),
        Some(who.clone()),
        Ok(asking("host-a")),
        &|| (),
    )
    .await;
    drop(sent.expect("the first hello is admitted"));
    assert!(agents.joins.identify(&minted.token).is_none());
    for host in ["host-a", "host-b"] {
        let retry = serve_hello(
            &agents,
            who.clone(),
            Some(who.clone()),
            Ok(asking(host)),
            &|| (),
        )
        .await;
        let refused = retry.expect_err("the retired token registers nothing");
        assert_eq!(
            (refused.status.as_u16(), refused.error),
            (403, "join-token-expired")
        );
    }
    assert_eq!(rows("credential_lost"), 1);
    assert_eq!(agents.joins.revoke(&instance()), Ok(1));
    assert_eq!(rows("credential_lost"), 0);
}

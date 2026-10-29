//! A pool of kind `joined` in the configuration file: machines that run `js-agent` and join
//! by hand, against a real server on loopback.

use std::net::SocketAddr;

use serde_json::{Value, json};

use crate::agents_support::{JOIN, Session, hello_body, session};
use crate::config_support::{PoolText, file, load, quote, quoted, refusal, server_with_agents};
use crate::support::{Harness, Reply, Scratch, TOKEN, request};

/// A joined pool `burst`, six nodes at most, that books 8 cores and 16 GiB and asks the hosts
/// for no contract. The text is the `[[pool]]` table.
fn joined() -> PoolText {
    PoolText::joined("burst")
}

/// A server over the file whose only pool is `pool`.
async fn serving(name: &str, pool: impl FnOnce(&Scratch) -> PoolText) -> Harness {
    let scratch = Scratch::new(name);
    let pool = pool(&scratch);
    let text = file(&server_with_agents(&scratch), &[&pool]);
    let config = load(&scratch, &text).unwrap();
    Harness::over(scratch, config).await
}

/// The hello of the build node `host`: no GPU, the shape of the pool, naming the instance
/// `<pool>:<instance>` and asking for a host credential.
fn hello(host: &str, instance: Option<&str>) -> Value {
    let mut body = hello_body(host);
    body["shape"] = json!({ "cpu_millis": 8000, "memory_bytes": 17179869184u64, "gpus": 0 });
    if let Some(instance) = instance {
        body["instance"] = json!({ "provider": "burst", "instance_id": instance });
        body["accepts_host_credential"] = json!(true);
    }
    body
}

async fn agent_call(h: &Harness, path: &str, token: &str, body: &Value) -> Reply {
    let addr: SocketAddr = h.running.agent_addr().unwrap();
    let bytes = body.to_string().into_bytes();
    request(addr, "POST", path, Some(token), Some(&bytes)).await
}

/// A one-time token for `<pool>:<instance>` from the client-token route.
async fn mint(h: &Harness, instance: &str) -> String {
    let body = json!({ "provider": "burst", "instance_id": instance });
    let reply = h.call("POST", "/v1/agent/join-tokens", Some(&body)).await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    reply.json()["token"].as_str().unwrap().to_owned()
}

/// Asserts the whole path of a hand-joined node against a real server: a one-time token is
/// minted for `burst:node-1`; the hello that names that instance registers the host and is
/// given a 64-digit host credential; the token then gets 401 on a heartbeat and on a second
/// hello; the credential carries the heartbeat; the host is listed. It catches a joined pool that does not register agents, one
/// that cannot admit one-time tokens, and a credential that is not issued.
#[tokio::test]
async fn a_node_joins_with_a_one_time_token_and_heartbeats_with_its_credential() {
    let h = serving("joined_e2e", |_| joined()).await;
    let token = mint(&h, "node-1").await;
    let welcome = agent_call(
        &h,
        "/v1/agent/hello",
        &token,
        &hello("node-1", Some("node-1")),
    )
    .await;
    let held: Session = session(&welcome, "node-1");
    let credential = welcome.json()["host_credential"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(credential.len(), 64, "{credential}");
    assert_ne!(credential, token);
    let again = agent_call(
        &h,
        "/v1/agent/hello",
        &token,
        &hello("node-1", Some("node-1")),
    )
    .await;
    assert_eq!(again.status, 401, "{}", again.body);
    let spent = agent_call(&h, "/v1/agent/heartbeat", &token, &held.beat()).await;
    assert_eq!(spent.status, 401, "{}", spent.body);
    let beat = agent_call(&h, "/v1/agent/heartbeat", &credential, &held.beat()).await;
    assert_eq!(beat.status, 200, "{}", beat.body);
    let hosts = h.call("GET", "/v1/hosts", None).await;
    assert_eq!(hosts.json()["hosts"][0]["id"], "node-1");
}

/// Asserts that a hello naming another host than the token's is refused 403
/// `join-token-instance`, and that the token still admits its own host afterwards. It
/// catches a token that admits any host of the pool.
#[tokio::test]
async fn a_hello_naming_another_host_is_refused() {
    let h = serving("joined_other", |_| joined()).await;
    let token = mint(&h, "node-1").await;
    let other = agent_call(
        &h,
        "/v1/agent/hello",
        &token,
        &hello("node-2", Some("node-2")),
    )
    .await;
    assert_eq!(other.status, 403, "{}", other.body);
    assert_eq!(other.json()["error"], "join-token-instance");
    let bare = agent_call(&h, "/v1/agent/hello", &token, &hello("node-1", None)).await;
    assert_eq!(bare.json()["error"], "join-token-instance");
    let own = agent_call(
        &h,
        "/v1/agent/hello",
        &token,
        &hello("node-1", Some("node-1")),
    )
    .await;
    session(&own, "node-1");
}

/// Asserts that the shared pool token of `join_token_file` admits hosts of a joined pool
/// beside one-time tokens, and that the client token is not a join token. It catches a
/// joined pool that drops the file, or one that admits the lease token.
#[tokio::test]
async fn the_shared_pool_token_still_admits_hosts() {
    let h = serving("joined_shared", |scratch| {
        let joins = scratch.write("pool-token", &format!("{JOIN}\n"), 0o600);
        joined().set("join_token_file", &quoted(&joins))
    })
    .await;
    let shared = agent_call(&h, "/v1/agent/hello", JOIN, &hello("node-3", None)).await;
    let held = session(&shared, "node-3");
    let beat = agent_call(&h, "/v1/agent/heartbeat", JOIN, &held.beat()).await;
    assert_eq!(beat.status, 200, "{}", beat.body);
    let token = mint(&h, "node-1").await;
    let one = agent_call(
        &h,
        "/v1/agent/hello",
        &token,
        &hello("node-1", Some("node-1")),
    )
    .await;
    session(&one, "node-1");
    let client = agent_call(&h, "/v1/agent/hello", TOKEN, &hello("node-4", None)).await;
    assert_eq!(client.status, 401, "{}", client.body);
}

/// Asserts that a joined pool counts as the one pool that registers agents: beside another
/// joined pool or an elastic pool, the later one is refused naming it; and a joined pool
/// beside a static pool is served. It catches two registries built from one file.
#[test]
fn two_registering_pools_are_refused() {
    let scratch = Scratch::new("joined_two");
    let server = server_with_agents(&scratch);
    let message = "pool \"second\": only one pool may register agents in this version";
    let second = PoolText::joined("second");
    assert_eq!(
        refusal(&scratch, &file(&server, &[&joined(), &second])),
        message
    );
    let elastic = PoolText::elastic(&scratch).set("name", &quote("second"));
    assert_eq!(
        refusal(&scratch, &file(&server, &[&joined(), &elastic])),
        message
    );
    let hosts = PoolText::static_pool(&scratch, "hosts");
    assert!(load(&scratch, &file(&server, &[&hosts, &joined()])).is_ok());
}

/// Asserts that each key of a pool that launches is refused on a joined pool, naming the
/// pool and the key, and that a bad `join_token_file` is refused naming it. It catches a
/// joined pool that takes a spend cap or a network it cannot use.
#[test]
fn a_joined_pool_refuses_the_keys_of_a_launching_pool() {
    let scratch = Scratch::new("joined_refusals");
    let server = server_with_agents(&scratch);
    let launching = [
        ("launch", "\"plan\""),
        ("provider", "{ kind = \"static\" }"),
        ("network", "{ kind = \"default\" }"),
        ("max_instances", "4"),
        ("max_spend_per_hour", "\"1.00\""),
        ("max_spend_per_day", "\"2.00\""),
        ("idle", "\"10m\""),
        ("boot_timeout", "\"5m\""),
    ];
    for (key, value) in launching {
        let said = refusal(&scratch, &file(&server, &[&joined().set(key, value)]));
        assert!(
            said.starts_with(&format!(
                "pool \"burst\": {key}: a joined pool launches nothing"
            )),
            "{said}"
        );
    }
    let missing = quoted(&scratch.path("absent-token"));
    let said = refusal(
        &scratch,
        &file(&server, &[&joined().set("join_token_file", &missing)]),
    );
    assert!(
        said.starts_with("pool \"burst\": join_token_file: "),
        "{said}"
    );
    let said = refusal(&scratch, &file(&server, &[&joined().without("shape")]));
    assert_eq!(said, "pool \"burst\": shape: required");
    let elastic = PoolText::elastic(&scratch).set("max_machines", "3");
    let said = refusal(&scratch, &file(&server, &[&elastic]));
    assert!(
        said.starts_with("pool \"gpu\": max_machines: unknown field"),
        "{said}"
    );
}

/// Asserts that a loaded joined pool launches nothing: the configuration carries no elastic
/// plan, and a server started on it lists no pool at `/v1/pools`. The launch pool, the check
/// loop and the status all come from the plan, so a joined pool that left a plan in the
/// registering side would show here. It catches a joined pool that starts to launch or to
/// check a provider it does not have.
#[tokio::test]
async fn a_joined_pool_launches_nothing() {
    let scratch = Scratch::new("joined_launches_nothing");
    let text = file(&server_with_agents(&scratch), &[&joined()]);
    let config = load(&scratch, &text).unwrap();
    assert!(config.elastic_pool().is_none());
    let h = Harness::over(scratch, config).await;
    let reply = request(h.addr(), "GET", "/v1/pools", None, None).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.json()["pools"], json!([]));
}

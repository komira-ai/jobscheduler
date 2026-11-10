//! A pool of kind `joined` in the configuration file: machines that run `js-agent` and join
//! by hand, against a real server on loopback.

use std::net::SocketAddr;

use serde_json::{Value, json};

use crate::agents_support::{JOIN, Session, hello_body, session};
use crate::config_support::{
    PoolText, file, load, quote, quoted, refusal, server, server_with_agents,
};
use crate::jobs_support::{JobBody, one_start, submit_body};
use crate::support::{Harness, Reply, Scratch, TOKEN, request};

/// A joined pool `burst`, six nodes at most, that books 8 cores and 16 GiB and asks the hosts
/// for no contract. The text is the `[[pool]]` table.
fn joined() -> PoolText {
    PoolText::joined("burst")
}

/// A server over the file whose only pool is `pool`, with the job service on a loopback port.
async fn serving(name: &str, pool: impl FnOnce(&Scratch) -> PoolText) -> Harness {
    serving_with(name, "", pool).await
}

/// [`serving`], with `extra` lines in the `[server]` table.
async fn serving_with(name: &str, extra: &str, pool: impl FnOnce(&Scratch) -> PoolText) -> Harness {
    let scratch = Scratch::new(name);
    let pool = pool(&scratch);
    let jobs = format!(
        "listen = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"\njob_listen = \"127.0.0.1:0\"\n{extra}"
    );
    let text = file(&server(&scratch, &jobs), &[&pool]);
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

/// Asserts that a joined pool counts as a pool that registers agents: beside another joined
/// pool or an elastic pool the file is valid, and the server refuses to start on it, naming
/// the later pool; and a joined pool beside a static pool is served. It catches two
/// registries built from one file.
#[test]
fn two_registering_pools_are_refused() {
    let scratch = Scratch::new("joined_two");
    let server = server_with_agents(&scratch);
    let message = "pool \"second\": several pools register agents; this version runs one, as the server keeps one registry of agents";
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
/// pool and the key, that a bad `join_token_file` is refused naming it, and that an elastic
/// pool giving the joined pool's `max_machines` beside `max_instances` is refused naming
/// both (on an elastic pool the two are one cap). It catches a joined pool that takes a
/// spend cap or a network it cannot use.
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
        ("max_age", "\"6h\""),
        ("min_host_lifetime", "\"15m\""),
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
    let said = refusal(
        &scratch,
        &file(&server, &[&joined().without("max_machines")]),
    );
    assert_eq!(said, "pool \"burst\": max_machines: required");
    let elastic = PoolText::elastic(&scratch).set("max_machines", "3");
    let said = refusal(&scratch, &file(&server, &[&elastic]));
    assert_eq!(
        said,
        "pool \"gpu\": max_machines: and max_instances are one cap by two names; give one"
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

/// The hello of a Linux x86-64 node `host`, which names its instance.
fn linux_hello(host: &str) -> Value {
    let mut body = hello(host, Some(host));
    body["capabilities"] = json!({ "os": "linux", "arch": "x86_64" });
    body
}

/// A joined pool that offers `os=linux` and `arch=x86_64` on every host.
fn linux() -> PoolText {
    joined().set("capabilities", "{ os = \"linux\", arch = \"x86_64\" }")
}

/// A job that requires the operating system `os`.
fn needing(id: &str, os: &str) -> Value {
    JobBody::new(id)
        .needs(1000, 1024)
        .constraints(json!({ "os": os }))
        .json()
}

/// Asserts that a joined pool with a `capabilities` table places a job that requires one:
/// the job for `os=linux` is accepted and queued, is handed to the node in the reply to
/// its heartbeat, and starts there; a job for another `os` fits nothing. The job is matched
/// against the pool's shape, so this catches a table that is read and dropped before the
/// registry's pool is built.
#[tokio::test]
async fn a_job_that_requires_a_capability_is_placed_on_a_joined_host() {
    let h = serving_with("joined_caps", "placement = \"js-sched\"", |_| linux()).await;
    let token = mint(&h, "node-1").await;
    let mut said = linux_hello("node-1");
    said["capabilities"]["per-job"] = json!("true");
    let welcome = agent_call(&h, "/v1/agent/hello", &token, &said).await;
    let held = session(&welcome, "node-1");
    let credential = welcome.json()["host_credential"]
        .as_str()
        .unwrap()
        .to_owned();
    let other = submit_body(&h, &needing("job-mac", "macos")).await;
    assert_eq!(other.status, 422, "{}", other.body);
    assert_eq!(other.json(), json!({ "error": "no-capable-host" }));
    let asked = submit_body(&h, &needing("job-linux", "linux")).await;
    assert_eq!(asked.status, 201, "{}", asked.body);
    h.ticks.step().await;
    let beat = agent_call(&h, "/v1/agent/heartbeat", &credential, &held.beat()).await;
    assert_eq!(beat.status, 200, "{}", beat.body);
    let (job, _allocation) = one_start(&beat.json());
    assert_eq!(job, "job-linux");
}

/// Asserts what the table is for: on a joined pool with no `capabilities`, the same job
/// for `os=linux` fits nothing (`no-capable-host`), because requests are matched against the pool's shape and
/// not against what a host reports; and with the table, a host that does not report the
/// pool's capabilities is refused 403 `shape-not-accepted`. It catches a joined pool whose
/// booked shape drifts from the capabilities it admits hosts by.
#[tokio::test]
async fn capabilities_decide_both_the_fit_and_the_admission() {
    let bare = serving("joined_bare", |_| joined()).await;
    let token = mint(&bare, "node-1").await;
    let welcome = agent_call(&bare, "/v1/agent/hello", &token, &linux_hello("node-1")).await;
    session(&welcome, "node-1");
    let asked = submit_body(&bare, &needing("job-linux", "linux")).await;
    assert_eq!(asked.status, 422, "{}", asked.body);
    assert_eq!(asked.json(), json!({ "error": "no-capable-host" }));

    let h = serving("joined_caps_admit", |_| linux()).await;
    let token = mint(&h, "node-1").await;
    let mut body = linux_hello("node-1");
    body["capabilities"] = json!({ "os": "linux" });
    let refused = agent_call(&h, "/v1/agent/hello", &token, &body).await;
    assert_eq!(refused.status, 403, "{}", refused.body);
    assert_eq!(
        refused.json(),
        json!({ "error": "shape-not-accepted", "reason": "below-floor" })
    );
}

/// Asserts that a bad `capabilities` table is refused naming it: a value that is not text,
/// and more entries than the wire carries.
#[test]
fn a_bad_capabilities_table_is_refused() {
    let scratch = Scratch::new("joined_caps_refused");
    let server = server_with_agents(&scratch);
    let number = joined().set("capabilities", "{ os = 1 }");
    let said = refusal(&scratch, &file(&server, &[&number]));
    assert_eq!(said, "pool \"burst\": capabilities.os: must be a string");
    let many: Vec<String> = (0..=js_agent_wire::limits::MAX_CAPABILITIES)
        .map(|n| format!("k{n} = \"v\""))
        .collect();
    let table = format!("{{ {} }}", many.join(", "));
    let said = refusal(
        &scratch,
        &file(&server, &[&joined().set("capabilities", &table)]),
    );
    assert!(said.starts_with("pool \"burst\": capabilities: "), "{said}");
}

/// Asserts that a joined pool mints only for its own name: a mint whose `provider` is
/// another name is a 400 naming `provider`, and writes no token, so the same instance can
/// still be minted for under the pool's name. It catches a token minted for a provider
/// that no hello of the pool can ever name.
#[tokio::test]
async fn a_mint_for_another_provider_is_refused() {
    let h = serving("joined_mint_provider", |_| joined()).await;
    let body = json!({ "provider": "elsewhere", "instance_id": "node-1" });
    let reply = h.call("POST", "/v1/agent/join-tokens", Some(&body)).await;
    assert_eq!(reply.status, 400, "{}", reply.body);
    assert!(
        reply.json()["reason"]
            .as_str()
            .unwrap()
            .contains("provider"),
        "{}",
        reply.body
    );
    mint(&h, "node-1").await;
}

/// A job for `memory_bytes` and one core, with no constraint.
fn wanting(id: &str, memory_bytes: u64) -> Value {
    JobBody::new(id).needs(1000, memory_bytes).json()
}

/// Asserts that a joined pool with `memory_schedulable_percent = 90` books
/// floor(16 GiB x 90 / 100) for a host: a job for one byte more fits nothing (422 `no-capable-host`) and exactly
/// that is accepted (201), and `GET /v1/hosts` shows the listed memory, the percent and the
/// bookable memory. A pool without the key books all 16 GiB: one byte more fits nothing and
/// 16 GiB is accepted. A host's hello still has to claim the whole listed memory. It catches
/// a joined pool that books the full memory whatever the key says.
#[tokio::test]
async fn a_joined_pool_books_the_memory_percent_it_is_given() {
    const LISTED: u64 = 17_179_869_184;
    const NINETY: u64 = LISTED * 90 / 100;
    let cut = serving("joined_mempct_90", |_| {
        joined().set("memory_schedulable_percent", "90")
    })
    .await;
    let token = mint(&cut, "node-1").await;
    let said = hello("node-1", Some("node-1"));
    let welcome = agent_call(&cut, "/v1/agent/hello", &token, &said).await;
    session(&welcome, "node-1");
    let listing = cut.call("GET", "/v1/hosts", None).await.json();
    let shown = &listing["hosts"][0];
    assert_eq!(shown["shape"]["memory_bytes"], NINETY, "{listing}");
    assert_eq!(
        shown["memory"],
        json!({
            "configured_bytes": LISTED,
            "schedulable_percent": 90,
            "bookable_bytes": NINETY,
        })
    );
    let ask = wanting("over", NINETY + 1);
    let over = submit_body(&cut, &ask).await;
    assert_eq!(over.status, 422, "{}", over.body);
    assert_eq!(over.json(), json!({ "error": "no-capable-host" }));
    let ask = wanting("exact", NINETY);
    let exact = submit_body(&cut, &ask).await;
    assert_eq!(exact.status, 201, "{}", exact.body);

    let full = serving("joined_mempct_default", |_| joined()).await;
    let token = mint(&full, "node-1").await;
    let said = hello("node-1", Some("node-1"));
    let welcome = agent_call(&full, "/v1/agent/hello", &token, &said).await;
    session(&welcome, "node-1");
    let ask = wanting("over", LISTED + 1);
    let over = submit_body(&full, &ask).await;
    assert_eq!(over.status, 422, "{}", over.body);
    let ask = wanting("all", LISTED);
    let all = submit_body(&full, &ask).await;
    assert_eq!(all.status, 201, "{}", all.body);
}

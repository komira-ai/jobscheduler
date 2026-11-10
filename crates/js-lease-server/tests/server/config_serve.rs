//! A server started from the configuration file: the agent routes on a port of their
//! own, the host contract a pool requires by default and its opt-out, the pool floor, and
//! the warning for a listener on every interface.

use std::net::SocketAddr;

use serde_json::{Value, json};

use crate::agents_support::{JOIN, Session, hello_body, session};
use crate::config_support::{PoolText, file, load, quote, quoted, server, server_with_agents};
use crate::support::{Harness, Reply, Scratch, TOKEN, request};

/// An elastic pool `burst` of the stand-in cloud kind (one GPU of class `nvidia-a10` at
/// least) that admits agents with [`JOIN`], with `exec` as given (the default when `None`).
fn pool(scratch: &Scratch, exec: Option<&str>) -> PoolText {
    let joins = scratch.write("joins", &format!("{JOIN}\n"), 0o600);
    let pool = PoolText::elastic(scratch)
        .set("name", &quote("burst"))
        .set("class", &quote("gpu-host"))
        .set("join_token_file", &quoted(&joins));
    match exec {
        Some(exec) => pool.set("exec", &quote(exec)),
        None => pool,
    }
}

/// A server on the file whose only pool is [`pool`].
async fn serving(name: &str, exec: Option<&str>) -> Harness {
    let scratch = Scratch::new(name);
    let text = file(&server_with_agents(&scratch), &[&pool(&scratch, exec)]);
    let config = load(&scratch, &text).unwrap();
    Harness::over(scratch, config).await
}

/// The hello of the pool `burst`: a host with one GPU of class `class`, reporting the
/// capabilities of `caps` besides its own.
fn hello(host: &str, class: &str, caps: &[(&str, &str)]) -> Value {
    let mut body = hello_body(host);
    body["shape"]["gpu_class"] = json!(class);
    for (key, value) in caps {
        body["capabilities"][key] = json!(value);
    }
    body
}

/// One request to `addr`.
async fn call(addr: SocketAddr, method: &str, path: &str, token: &str, body: &Value) -> Reply {
    let bytes = body.to_string().into_bytes();
    let sent = (method != "GET").then_some(bytes.as_slice());
    request(addr, method, path, Some(token), sent).await
}

/// Asserts the route split against a real server with both ports bound: `hello` and
/// heartbeat answer on `agent_listen` and are 404 on `listen`; the host and
/// operator routes and `/healthz` answer on `listen` and are 404 on `agent_listen`. With
/// one port the same checks cannot fail, so the server binds two. It catches a route left
/// on the wrong port, which would let a tag that may reach only the agent port reach
/// the operator routes.
#[tokio::test]
async fn agent_routes_answer_on_the_agent_port_only() {
    let h = serving("serve_split", Some("none")).await;
    let (listen, agent) = (h.addr(), h.running.agent_addr().unwrap());
    assert_ne!(listen, agent);
    let welcome = call(
        agent,
        "POST",
        "/v1/agent/hello",
        JOIN,
        &hello("h1", "nvidia-a10", &[]),
    )
    .await;
    let held: Session = session(&welcome, "h1");
    let beat = call(agent, "POST", "/v1/agent/heartbeat", JOIN, &held.beat()).await;
    assert_eq!(beat.status, 200, "{}", beat.body);
    h.ticks.step().await;

    let none = json!({});
    for path in ["/v1/agent/hello", "/v1/agent/heartbeat"] {
        let reply = call(listen, "POST", path, TOKEN, &none).await;
        assert_eq!(reply.status, 404, "{path}: {}", reply.body);
        let reply = call(listen, "POST", path, JOIN, &none).await;
        assert_eq!(reply.status, 401, "{path}: {}", reply.body);
    }
    let hosts = call(listen, "GET", "/v1/hosts", TOKEN, &none).await;
    assert_eq!(hosts.status, 200, "{}", hosts.body);
    assert_eq!(hosts.json()["hosts"][0]["id"], "h1");
    let health = request(listen, "GET", "/healthz", None, None).await;
    assert_eq!(health.status, 200);
    let minted = call(listen, "POST", "/v1/agent/join-tokens", TOKEN, &none).await;
    assert_ne!(minted.status, 404, "{}", minted.body);

    for (method, path) in [
        ("GET", "/v1/hosts"),
        ("GET", "/healthz"),
        ("POST", "/v1/agent/join-tokens"),
    ] {
        let reply = call(agent, method, path, JOIN, &none).await;
        assert_eq!(reply.status, 404, "{path}: {}", reply.body);
    }
}

/// The lease and run routes of earlier releases, as method and path.
const REMOVED_ROUTES: [(&str, &str); 8] = [
    ("POST", "/v1/leases"),
    ("GET", "/v1/leases"),
    ("DELETE", "/v1/leases/x"),
    ("POST", "/v1/leases/x/renew"),
    ("POST", "/v1/leases/x/runs"),
    ("GET", "/v1/leases/x/runs"),
    ("GET", "/v1/leases/x/runs/y"),
    ("DELETE", "/v1/leases/x/runs/y"),
];

/// Asserts that the lease and run routes of earlier releases are not routes: with the
/// client token on `listen` and with the join token on `agent_listen`, each answers 404
/// with exactly the status and body that an unknown path gets from the same listener and
/// token; with no token on `listen` each stays the 401 with an empty body that every path
/// gets, and on `agent_listen` each gets the 401 with an empty body that an unknown path
/// gets with no token there. Catches
/// a lease or run route that is served again, or one that answers differently from a path
/// the router does not have.
#[tokio::test]
async fn the_lease_routes_answer_404() {
    let h = serving("serve_no_lease_routes", Some("none")).await;
    let (listen, agent) = (h.addr(), h.running.agent_addr().unwrap());
    let none = json!({});
    let unknown = call(listen, "GET", "/v1/nothing", TOKEN, &none).await;
    assert_eq!(unknown.status, 404);
    assert_eq!(unknown.json(), json!({ "error": "not-found" }));
    let unknown_on_agent = call(agent, "GET", "/v1/nothing", JOIN, &none).await;
    assert_eq!(unknown_on_agent.status, 404);
    let unsigned = request(listen, "GET", "/v1/nothing", None, None).await;
    assert_eq!(
        (unsigned.status, unsigned.body.as_str()),
        (401, ""),
        "an unknown path with no token on the client listener"
    );
    let unsigned_on_agent = request(agent, "GET", "/v1/nothing", None, None).await;
    assert_eq!(
        (unsigned_on_agent.status, unsigned_on_agent.body.as_str()),
        (401, ""),
        "an unknown path with no token on the agent listener"
    );

    for (method, path) in REMOVED_ROUTES {
        let reply = call(listen, method, path, TOKEN, &none).await;
        assert_eq!(
            (reply.status, &reply.body),
            (404, &unknown.body),
            "{method} {path} on the client listener"
        );
        let reply = call(agent, method, path, JOIN, &none).await;
        assert_eq!(
            (reply.status, &reply.body),
            (404, &unknown_on_agent.body),
            "{method} {path} on the agent listener"
        );
        let reply = request(listen, method, path, None, None).await;
        assert_eq!(
            (reply.status, reply.body.as_str()),
            (401, ""),
            "{method} {path} with no token on the client listener"
        );
        let reply = request(agent, method, path, None, None).await;
        assert_eq!(
            (reply.status, reply.body.as_str()),
            (401, ""),
            "{method} {path} with no token on the agent listener"
        );
    }
}

/// Asserts what a pool requires of a host when its file says nothing about `exec`: the
/// container contract. A host from before the contract (it reports no runtime) is refused
/// `host-contract` naming `runtime`; one that reports the runtime, the mode and the
/// toolkit is admitted. It catches the design's default left off, and a refusal that does
/// not say what to fix.
#[tokio::test]
async fn a_pool_requires_the_container_contract_by_default() {
    let h = serving("serve_default_exec", None).await;
    let agent = h.running.agent_addr().unwrap();
    let old = hello("old", "nvidia-a10", &[]);
    let reply = call(agent, "POST", "/v1/agent/hello", JOIN, &old).await;
    assert_eq!(reply.status, 403, "{}", reply.body);
    assert_eq!(
        reply.json(),
        json!({ "error": "host-contract", "field": "runtime" })
    );
    let caps = [
        ("runtime", "container"),
        ("exec", "container"),
        ("gpu-toolkit", "true"),
    ];
    let new = hello("new", "nvidia-a10", &caps);
    let reply = call(agent, "POST", "/v1/agent/hello", JOIN, &new).await;
    session(&reply, "new");
}

/// Asserts the opt-out: a pool with `exec = "none"` admits a host that reports none of the
/// contract capabilities (an agent that predates them, or one run without a container
/// binary), and still keeps out a host below its floor: a GPU of another class is refused
/// `shape-not-accepted`. It catches an opt-out that still requires the contract, and a
/// pool with no floor.
#[tokio::test]
async fn exec_none_admits_hosts_without_the_contract_and_the_floor_still_holds() {
    let h = serving("serve_exec_none", Some("none")).await;
    let agent = h.running.agent_addr().unwrap();
    let old = hello("old", "nvidia-a10", &[]);
    session(
        &call(agent, "POST", "/v1/agent/hello", JOIN, &old).await,
        "old",
    );
    let other = hello("other", "nvidia-a100", &[]);
    let reply = call(agent, "POST", "/v1/agent/hello", JOIN, &other).await;
    assert_eq!(reply.status, 403, "{}", reply.body);
    assert_eq!(
        reply.json(),
        json!({ "error": "shape-not-accepted", "reason": "below-floor" })
    );
}

/// Asserts that a pool with no join token file admits no host with a token that every
/// host would share: the join token of the other tests is refused 401, as is the client
/// token, and a tick that has no token file to read again goes by. It catches a pool that
/// admits a shared token it was never given.
#[tokio::test]
async fn a_pool_without_a_join_token_file_admits_no_shared_token() {
    let scratch = Scratch::new("serve_no_join_file");
    let pool = PoolText::elastic(&scratch)
        .set("name", &quote("burst"))
        .set("class", &quote("gpu-host"))
        .set("exec", &quote("none"));
    let text = file(&server_with_agents(&scratch), &[&pool]);
    let config = load(&scratch, &text).unwrap();
    let h = Harness::over(scratch, config).await;
    h.ticks.step().await;
    let agent = h.running.agent_addr().unwrap();
    for token in [JOIN, TOKEN] {
        let hello = hello("h1", "nvidia-a10", &[]);
        let reply = call(agent, "POST", "/v1/agent/hello", token, &hello).await;
        assert_eq!(reply.status, 401, "{}", reply.body);
    }
}

/// A certificate for the wildcard test and its key, as PEM text.
fn identity() -> (String, String) {
    let names = vec!["127.0.0.1".to_string()];
    let made = rcgen::generate_simple_self_signed(names).unwrap();
    (made.cert.pem(), made.signing_key.serialize_pem())
}

/// Asserts that a listener on every interface is accepted with TLS and starts, with the
/// warning the design gives in the configuration's warnings (a wildcard is normal in a VPC
/// or a container, so it is not refused). It catches a wildcard that is refused, and a
/// server that starts without saying so.
#[tokio::test]
async fn a_wildcard_listener_starts_with_a_warning() {
    let scratch = Scratch::new("serve_wildcard");
    let (cert, key) = identity();
    let cert = scratch.write("cert.pem", &cert, 0o644);
    let key = scratch.write("key.pem", &key, 0o600);
    let extra = format!(
        "listen = \"0.0.0.0:0\"\ntls_cert = {}\ntls_key = {}",
        quoted(&cert),
        quoted(&key)
    );
    let hosts = PoolText::joined("hosts");
    let config = load(&scratch, &file(&server(&scratch, &extra), &[&hosts])).unwrap();
    assert_eq!(
        config.warnings(),
        [
            "server: warning: binds all interfaces (server.listen 0.0.0.0:0); /v1/pools and /status are served only to private peers"
        ]
    );
    let h = Harness::over(scratch, config).await;
    assert!(h.addr().ip().is_unspecified());
    h.running.shutdown().await.unwrap();
}

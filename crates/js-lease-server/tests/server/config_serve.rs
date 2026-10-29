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
/// heartbeat answer on `agent_listen` and are 404 on `listen`; the lease, host and
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
    assert_eq!(
        call(listen, "GET", "/v1/leases", TOKEN, &none).await.status,
        200
    );
    let health = request(listen, "GET", "/healthz", None, None).await;
    assert_eq!(health.status, 200);
    let minted = call(listen, "POST", "/v1/agent/join-tokens", TOKEN, &none).await;
    assert_ne!(minted.status, 404, "{}", minted.body);

    for (method, path) in [
        ("GET", "/v1/hosts"),
        ("GET", "/v1/leases"),
        ("GET", "/healthz"),
        ("POST", "/v1/agent/join-tokens"),
    ] {
        let reply = call(agent, method, path, JOIN, &none).await;
        assert_eq!(reply.status, 404, "{path}: {}", reply.body);
    }
}

/// Asserts that an agent port with no agent registration behind it serves nothing: every
/// path is 404 there, and the other port serves as ever. It catches a listener that is
/// bound and answers with a route of the other side.
#[tokio::test]
async fn an_agent_port_without_agents_serves_nothing() {
    let scratch = Scratch::new("serve_no_agents");
    let hosts = PoolText::static_pool(&scratch, "hosts");
    let extra = "listen = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"";
    let text = file(&server(&scratch, extra), &[&hosts]);
    let config = load(&scratch, &text).unwrap();
    let h = Harness::over(scratch, config).await;
    let agent = h.running.agent_addr().unwrap();
    for path in ["/healthz", "/v1/hosts", "/v1/agent/hello"] {
        let reply = call(agent, "GET", path, TOKEN, &json!({})).await;
        assert_eq!(reply.status, 404, "{path}");
    }
    let reply = h.call("GET", "/v1/hosts", None).await;
    assert_eq!(reply.status, 200);
    h.running.shutdown().await.unwrap();
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
    assert_eq!(reply.json(), json!({ "error": "shape-not-accepted" }));
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
    let hosts = PoolText::static_pool(&scratch, "hosts");
    let config = load(&scratch, &file(&server(&scratch, &extra), &[&hosts])).unwrap();
    assert_eq!(
        config.warnings(),
        ["server: warning: binds all interfaces (server.listen 0.0.0.0:0)"]
    );
    let h = Harness::over(scratch, config).await;
    assert!(h.addr().ip().is_unspecified());
    h.running.shutdown().await.unwrap();
}

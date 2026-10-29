//! The host contract on the wire, against a real server: a pool file with `exec` refuses
//! a host that lacks what the mode needs, naming the capability, and a pool file without
//! it admits every host as before.

use serde_json::{Value, json};

use crate::agents_support::{
    JOIN, POOL, agent_args, hello_body, hosts, listed, post, session, start_agents,
};
use crate::support::{Harness, Scratch};

/// A server whose pool file is [`POOL`] (one GPU per host) with `exec` added.
async fn start_with_exec(name: &str, exec: &str) -> Harness {
    let scratch = Scratch::new(name);
    let args = agent_args(&scratch, false);
    scratch.write(
        "agent-pool.toml",
        &format!("exec = \"{exec}\"\n{POOL}"),
        0o644,
    );
    Harness::with(scratch, args, None).await
}

/// The hello of [`hello_body`] reporting these capabilities besides its own.
fn reporting(host: &str, pairs: &[(&str, &str)]) -> Value {
    let mut body = hello_body(host);
    for (key, value) in pairs {
        body["capabilities"][key] = json!(value);
    }
    body
}

/// Asserts what a pool with `exec = "container"` answers on the wire, status, code and
/// field: a hello that reports nothing (an agent from before the contract) and one that
/// reports no toolkit are `host-contract` naming `runtime` and `gpu-toolkit`; one whose
/// `exec` is another mode is `execution-mismatch` naming `exec`; each is 403, so the
/// agent does not ask again, and none creates a host. A host that reports the runtime and
/// the toolkit is admitted and listed. It catches a server that admits a host with nothing
/// to run jobs with, and a refusal that does not say what to fix.
#[tokio::test]
async fn a_container_pool_names_what_the_host_lacks() {
    let h = start_with_exec("agents_contract", "container").await;
    let refused = |reply: crate::support::Reply, code: &str, field: &str| {
        assert_eq!(reply.status, 403, "{}", reply.body);
        assert_eq!(reply.json(), json!({ "error": code, "field": field }));
    };
    let old = hello_body("old");
    refused(
        post(&h, "/v1/agent/hello", Some(JOIN), &old).await,
        "host-contract",
        "runtime",
    );
    let runtime = [("runtime", "container"), ("exec", "container")];
    let bare = reporting("bare", &runtime);
    let reply = post(&h, "/v1/agent/hello", Some(JOIN), &bare).await;
    refused(reply, "host-contract", "gpu-toolkit");
    let other = [("runtime", "container"), ("exec", "sandbox-exec")];
    let reply = post(&h, "/v1/agent/hello", Some(JOIN), &reporting("mac", &other)).await;
    refused(reply, "execution-mismatch", "exec");
    assert!(hosts(&h).await["hosts"].as_array().unwrap().is_empty());

    let whole = [
        ("runtime", "container"),
        ("exec", "container"),
        ("gpu-toolkit", "true"),
    ];
    let reply = post(
        &h,
        "/v1/agent/hello",
        Some(JOIN),
        &reporting("good", &whole),
    )
    .await;
    session(&reply, "good");
    assert_eq!(listed(&hosts(&h).await), ["good"]);
}

/// Asserts that a pool file without `exec` admits a host that reports none of the contract
/// capabilities (an agent from before it) and a host that reports all of them, and sends no
/// `field` in any answer. It catches a server upgrade that refuses every deployed agent.
#[tokio::test]
async fn a_pool_without_exec_admits_old_and_new_agents() {
    let h = start_agents("agents_contract_none", false).await;
    let old = post(&h, "/v1/agent/hello", Some(JOIN), &hello_body("old")).await;
    assert!(old.json().get("field").is_none(), "{}", old.body);
    session(&old, "old");
    let whole = [
        ("runtime", "container"),
        ("exec", "container"),
        ("gpu-toolkit", "true"),
    ];
    let new = post(&h, "/v1/agent/hello", Some(JOIN), &reporting("new", &whole)).await;
    session(&new, "new");
    let mut small = hello_body("small");
    small["shape"]["cpu_millis"] = json!(1);
    let reply = post(&h, "/v1/agent/hello", Some(JOIN), &small).await;
    assert_eq!(reply.json(), json!({ "error": "shape-not-accepted" }));
}

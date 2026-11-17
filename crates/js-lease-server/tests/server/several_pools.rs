//! A server of several pools: a second `--agent-pool-config` names one more pool beside the
//! first, with its own shape, execution mode and shared join tokens (`join_token_file`), and
//! a hello joins the pool it names only with a token of that pool. Against a real server on
//! loopback.

use std::ffi::OsString;
use std::path::Path;

use serde_json::{Value, json};

use crate::agents_support::{JOIN, POOL, agent_args, hello_body, post, session};
use crate::support::{Harness, Scratch, base_args, config, replace};

/// The Macs' shared join token, in their pool's `join_token_file`.
const MAC: &str = "mac-join-token-0123456789-abcdefghijklm";

/// The Macs' pool: up to four Mac Studios of 24 cores and 64 GiB, macOS on arm64, run
/// natively under `sandbox-exec`, with `tokens` as its `join_token_file` when given.
fn mac_pool(tokens: Option<&Path>) -> String {
    let tokens = tokens.map_or(String::new(), |path| {
        format!("join_token_file = {path:?}\n")
    });
    format!(
        "name = \"mac-build\"\nclass = \"mac-host\"\nmax_machines = 4\nexec = \"sandbox-exec\"\n{tokens}\n\
         [shape]\ncpu_millis = 24000\nmemory_bytes = 68719476736\n\n\
         [capabilities]\nos = \"macos\"\narch = \"arm64\"\n"
    )
}

/// [`agent_args`] (the first pool `burst`, its tokens in `--agent-token-file`) and a second
/// `--agent-pool-config` whose file is `pool`.
fn two_pool_args(scratch: &Scratch, pool: &str) -> Vec<OsString> {
    let mut args = agent_args(scratch);
    let file = scratch.write("mac-pool.toml", pool, 0o644);
    args.extend(["--agent-pool-config".into(), file.into_os_string()]);
    args
}

/// A server of the pools `burst` and `mac-build`, the Macs' tokens holding [`MAC`].
async fn two_pools(name: &str) -> Harness {
    let scratch = Scratch::new(name);
    let tokens = scratch.write("mac-tokens", &format!("{MAC}\n"), 0o600);
    let args = two_pool_args(&scratch, &mac_pool(Some(&tokens)));
    Harness::with(scratch, args).await
}

/// The hello of the Mac `host`: more than its pool's shape, macOS on arm64 in the native
/// runtime, in the pool `pool`.
fn mac_hello(host: &str, pool: &str) -> Value {
    json!({
        "host_id": host,
        "pool": pool,
        "agent_version": "0.1.0",
        "shape": { "cpu_millis": 28000, "memory_bytes": 103079215104u64, "gpus": 0 },
        "capabilities": {
            "os": "macos",
            "arch": "arm64",
            "exec": "sandbox-exec",
            "runtime": "sandbox-exec",
        },
        "phase": "serving",
    })
}

/// The ids and booked cores of the hosts the server lists.
async fn listed(h: &Harness) -> Vec<(String, u64)> {
    let hosts = h.call("GET", "/v1/hosts", None).await.json();
    let hosts = hosts["hosts"].as_array().unwrap().iter();
    let each = hosts.map(|host| {
        let id = host["id"].as_str().unwrap().to_owned();
        (id, host["shape"]["cpu_millis"].as_u64().unwrap())
    });
    each.collect()
}

/// Asserts that each pool's hosts join it with its own token: a node of `burst` with the
/// first pool's token and a Mac of `mac-build` with the Macs', each welcomed into its own
/// pool, listed and booked at its own pool's shape, and each heartbeat carried by its own
/// token. It catches a second pool file that is read and never served, and every host booked
/// as the first pool.
#[tokio::test]
async fn each_host_joins_the_pool_its_token_belongs_to() {
    let h = two_pools("two_pools_join").await;
    let node = post(&h, "/v1/agent/hello", Some(JOIN), &hello_body("node-1")).await;
    let node = session(&node, "node-1");
    let mac = post(
        &h,
        "/v1/agent/hello",
        Some(MAC),
        &mac_hello("mac-1", "mac-build"),
    )
    .await;
    assert_eq!(mac.json()["pool"], "mac-build", "{}", mac.body);
    let mac = session(&mac, "mac-1");
    let beat = post(&h, "/v1/agent/heartbeat", Some(MAC), &mac.beat()).await;
    assert_eq!(beat.status, 200, "{}", beat.body);
    let beat = post(&h, "/v1/agent/heartbeat", Some(JOIN), &node.beat()).await;
    assert_eq!(beat.status, 200, "{}", beat.body);
    let hosts = listed(&h).await;
    assert_eq!(hosts, [("mac-1".into(), 24_000), ("node-1".into(), 8000)]);
}

/// Asserts that a shared token of one pool never registers a host into another: the first
/// pool's token with a hello that names `mac-build`, and the Macs' with one that names
/// `burst`, are each refused 403 `unknown-pool` and list nothing, although each hello meets
/// the pool it names. It catches a token of the Linux nodes that admits a host into the
/// Macs' pool (or the other way round), where it would be given the other pool's jobs.
#[tokio::test]
async fn a_token_of_one_pool_never_registers_a_host_into_another() {
    let h = two_pools("two_pools_cross").await;
    let crossed = [
        (JOIN, mac_hello("mac-1", "mac-build")),
        (MAC, hello_body("node-1")),
    ];
    for (token, hello) in crossed {
        let refused = post(&h, "/v1/agent/hello", Some(token), &hello).await;
        assert_eq!(refused.status, 403, "{}", refused.body);
        assert_eq!(refused.json(), json!({ "error": "unknown-pool" }));
    }
    assert!(listed(&h).await.is_empty());
}

/// Asserts that a one-time token on a server of several pools registers its host only into
/// the pool its instance's provider names: a token minted for `mac-build:mac-2` admits the
/// Mac into `mac-build`, and one minted for `burst:mac-3` is refused 403 `unknown-pool` on a
/// hello that names `mac-build`, and is still unused for its own pool. It catches a launched
/// host's token that admits it into any pool.
#[tokio::test]
async fn a_one_time_token_registers_its_host_only_into_its_instances_pool() {
    let h = two_pools("two_pools_one_time").await;
    let mint = |provider: &'static str, id: &'static str| {
        let h = &h;
        async move {
            let body = json!({ "provider": provider, "instance_id": id });
            let reply = h.call("POST", "/v1/agent/join-tokens", Some(&body)).await;
            assert_eq!(reply.status, 201, "{}", reply.body);
            reply.json()["token"].as_str().unwrap().to_owned()
        }
    };
    let own = mint("mac-build", "mac-2").await;
    let mut hello = mac_hello("mac-2", "mac-build");
    hello["instance"] = json!({ "provider": "mac-build", "instance_id": "mac-2" });
    session(
        &post(&h, "/v1/agent/hello", Some(&own), &hello).await,
        "mac-2",
    );
    let other = mint("burst", "mac-3").await;
    let mut hello = mac_hello("mac-3", "mac-build");
    hello["instance"] = json!({ "provider": "burst", "instance_id": "mac-3" });
    let refused = post(&h, "/v1/agent/hello", Some(&other), &hello).await;
    assert_eq!(refused.status, 403, "{}", refused.body);
    assert_eq!(refused.json(), json!({ "error": "unknown-pool" }));
    let mut node = hello_body("mac-3");
    node["instance"] = json!({ "provider": "burst", "instance_id": "mac-3" });
    session(
        &post(&h, "/v1/agent/hello", Some(&other), &node).await,
        "mac-3",
    );
}

/// Asserts what a restart does to a tainted Mac on a server of two pools: the server puts
/// it back awaiting, with its taint (the binding of the second pool is kept by the saves of
/// the first's), and its hello with the Macs' token admits it into `mac-build` again with
/// the taint and the Macs' shape, while an untainted node of `burst` simply says hello
/// again. It catches a restored host held to the first pool, which would refuse a tainted
/// Mac its own pool after every restart, and a taint of the second pool lost.
#[tokio::test]
async fn a_tainted_host_of_the_second_pool_joins_it_again_after_a_restart() {
    let first = two_pools("two_pools_restart").await;
    let node = post(&first, "/v1/agent/hello", Some(JOIN), &hello_body("node-1")).await;
    session(&node, "node-1");
    let mac = mac_hello("mac-1", "mac-build");
    let mac = session(
        &post(&first, "/v1/agent/hello", Some(MAC), &mac).await,
        "mac-1",
    );
    let mut beat = mac.beat();
    beat["taint"] = json!({ "reason": "residue", "check": "tmp" });
    let sent = post(&first, "/v1/agent/heartbeat", Some(MAC), &beat).await;
    assert_eq!(sent.json()["taint_ack"], true, "{}", sent.body);
    let Harness {
        running, scratch, ..
    } = first;
    running.shutdown().await.unwrap();
    let tokens = scratch.path("mac-tokens");
    let args = two_pool_args(&scratch, &mac_pool(Some(&tokens)));
    let h = Harness::with(scratch, replace(args, "--boot", "resume")).await;
    let hosts = h.call("GET", "/v1/hosts", None).await.json();
    let restored = &hosts["hosts"][0];
    assert_eq!(restored["id"], "mac-1", "{hosts}");
    assert_eq!(restored["taint"]["reason"], "residue", "{hosts}");
    let back = post(
        &h,
        "/v1/agent/hello",
        Some(MAC),
        &mac_hello("mac-1", "mac-build"),
    )
    .await;
    assert_eq!(back.json()["pool"], "mac-build", "{}", back.body);
    let node = post(&h, "/v1/agent/hello", Some(JOIN), &hello_body("node-1")).await;
    session(&node, "node-1");
    let hosts = h.call("GET", "/v1/hosts", None).await.json();
    assert_eq!(hosts["hosts"][0]["taint"]["reason"], "residue", "{hosts}");
    assert_eq!(
        listed(&h).await,
        [("mac-1".into(), 24_000), ("node-1".into(), 8000)]
    );
}

/// The message with which `args` are refused at start.
fn refused(args: Vec<OsString>) -> String {
    config(args)
        .err()
        .expect("the flags should be refused")
        .to_string()
}

/// Asserts each refusal of a second pool file, each naming the file: its name is the first
/// pool's; its `memory_schedulable_percent` differs from the first pool's; it has whole-node
/// settings (its class is `whole-node`); its shape names no `arch`, so the scheduler cannot
/// take its hosts as nodes; its `join_token_file` cannot be read. A third file whose name is the
/// second's is refused naming the third. It catches a
/// second pool that would be served with settings the server reads only from the first.
#[test]
fn a_second_pool_file_is_refused_naming_it() {
    let scratch = Scratch::new("two_pools_refusals");
    let file = scratch.path("mac-pool.toml");
    let at = format!("--agent-pool-config {}", file.display());
    let named_burst = mac_pool(None).replace("\"mac-build\"", "\"burst\"");
    let percent = mac_pool(None).replace("class", "memory_schedulable_percent = 80\nclass");
    let whole = mac_pool(None).replace("\"mac-host\"", "\"whole-node\"");
    let missing = scratch.path("no-such-tokens");
    let unread = mac_pool(Some(&missing));
    let cases = [
        (named_burst, "name: also the name of an earlier pool".to_owned()),
        (
            percent,
            "memory_schedulable_percent must be the first pool's (100): the placement engine books one share of every host's memory".to_owned(),
        ),
        (
            whole,
            "whole-node settings are the first pool's alone: give this pool a class that is not whole-node, and no whole-node keys".to_owned(),
        ),
    ];
    for (pool, reason) in cases {
        let said = refused(two_pool_args(&scratch, &pool));
        assert_eq!(said, format!("{at}: {reason}"));
    }
    let no_arch = mac_pool(None).replace("arch = \"arm64\"\n", "");
    let said = refused(two_pool_args(&scratch, &no_arch));
    assert!(
        said.starts_with(&format!(
            "{at}: the pool's shape cannot be taken as a node by the scheduler ("
        )),
        "{said}"
    );
    assert!(
        said.ends_with("); give the pool capabilities with an arch"),
        "{said}"
    );
    let said = refused(two_pool_args(&scratch, &unread));
    let head = format!("{at}: join_token_file: ");
    assert!(said.starts_with(&head), "{said}");
    assert!(said.contains("no-such-tokens"), "{said}");
    // A second file that cannot be read is refused naming it.
    let mut args = agent_args(&scratch);
    let gone = scratch.path("no-such-pool.toml");
    args.extend(["--agent-pool-config".into(), gone.clone().into_os_string()]);
    let said = refused(args);
    let head = format!("--agent-pool-config: {}: ", gone.display());
    assert!(said.starts_with(&head), "{said}");
    // A third file is held to the second's name too, not only to the first's.
    let mut args = two_pool_args(&scratch, &mac_pool(None));
    let third = scratch.write("third-pool.toml", &mac_pool(None), 0o644);
    args.extend(["--agent-pool-config".into(), third.clone().into_os_string()]);
    let said = refused(args);
    let at = format!("--agent-pool-config {}", third.display());
    assert_eq!(
        said,
        format!("{at}: name: also the name of an earlier pool")
    );
}

/// Asserts that the first pool's shared tokens may be its file's `join_token_file` in place of
/// `--agent-token-file`, that naming both is refused naming the file (two settings for one
/// set of tokens), and that a first pool's `join_token_file` that cannot be read is refused
/// naming the file and the key. It catches a first pool file whose `join_token_file` is ignored.
#[tokio::test]
async fn the_first_pools_tokens_are_its_join_token_file_or_the_flag_but_not_both() {
    let scratch = Scratch::new("first_pool_tokens");
    let tokens = scratch.write("burst-tokens", &format!("{JOIN}\n"), 0o600);
    let named = format!("join_token_file = {tokens:?}\n{POOL}");
    let path = scratch.write("first-pool.toml", &named, 0o644);
    let both = agent_args(&scratch);
    let both = replace(both, "--agent-pool-config", path.clone().into_os_string());
    let said = refused(both);
    let reason =
        "join_token_file and --agent-token-file both name the first pool's tokens: give one";
    assert_eq!(
        said,
        format!("--agent-pool-config {}: {reason}", path.display())
    );
    let missing = scratch.path("no-such-burst-tokens");
    let text = format!("join_token_file = {missing:?}\n{POOL}");
    let unread = scratch.write("unread-pool.toml", &text, 0o644);
    let args = base_args(&scratch);
    let said = refused(replace(
        args,
        "--agent-pool-config",
        unread.clone().into_os_string(),
    ));
    let head = format!(
        "--agent-pool-config {}: join_token_file: ",
        unread.display()
    );
    assert!(said.starts_with(&head), "{said}");
    let args = base_args(&scratch);
    let args = replace(args, "--agent-pool-config", path.into_os_string());
    let h = Harness::with(scratch, args).await;
    session(
        &post(&h, "/v1/agent/hello", Some(JOIN), &hello_body("node-1")).await,
        "node-1",
    );
}

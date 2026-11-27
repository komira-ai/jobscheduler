//! A server of several pools: a second `--agent-pool-config` names one more pool beside the
//! first, with its own shape and execution mode, and a hello joins the pool it names, by the
//! certificate of its host. Against a real server on loopback.

use std::ffi::OsString;

use serde_json::{Value, json};

use crate::agents_support::{agent_args, hello_body, post, session};
use crate::host_certs_support::{Options, Rig, Spec};
use crate::support::{Harness, Scratch, config, replace};

/// The Macs' pool: up to four Mac Studios of 24 cores and 64 GiB, macOS on arm64, run
/// natively under `sandbox-exec`.
pub fn mac_pool() -> String {
    String::from(
        "name = \"mac-build\"\nclass = \"mac-host\"\nmax_machines = 4\nexec = \"sandbox-exec\"\n\n\
         [shape]\ncpu_millis = 24000\nmemory_bytes = 68719476736\n\n\
         [capabilities]\nos = \"macos\"\narch = \"arm64\"\n",
    )
}

/// [`agent_args`] (the first pool `burst`) and a second
/// `--agent-pool-config` whose file is `pool`.
fn two_pool_args(scratch: &Scratch, pool: &str) -> Vec<OsString> {
    let mut args = agent_args(scratch);
    let file = scratch.write("mac-pool.toml", pool, 0o644);
    args.extend(["--agent-pool-config".into(), file.into_os_string()]);
    args
}

/// A server of the pools `burst` and `mac-build`.
async fn two_pools(name: &str) -> Harness {
    let scratch = Scratch::new(name);
    let args = two_pool_args(&scratch, &mac_pool());
    Harness::with(scratch, args).await
}

/// The hello of the Mac `host`: more than its pool's shape, macOS on arm64 in the native
/// runtime, in the pool `pool`.
pub fn mac_hello(host: &str, pool: &str) -> Value {
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

/// Asserts that each pool's hosts join the pool their hello names: a node of `burst` and a
/// Mac of `mac-build`, each presenting its own certificate, are welcomed into their own pool,
/// listed and booked at what their own hello reports (the Mac's 28 cores, more than its
/// pool's 24), and each heartbeat is carried by its own certificate. It catches a second pool
/// file that is read and never served, every host booked as the first pool, and a host
/// capped at its pool's shape.
#[tokio::test]
async fn each_host_joins_the_pool_its_hello_names_and_is_booked_at_its_hello() {
    let h = two_pools("two_pools_join").await;
    let node = post(&h, "/v1/agent/hello", None, &hello_body("node-1")).await;
    let node = session(&node, "node-1");
    let mac = post(
        &h,
        "/v1/agent/hello",
        None,
        &mac_hello("mac-1", "mac-build"),
    )
    .await;
    assert_eq!(mac.json()["pool"], "mac-build", "{}", mac.body);
    let mac = session(&mac, "mac-1");
    let beat = post(&h, "/v1/agent/heartbeat", None, &mac.beat()).await;
    assert_eq!(beat.status, 200, "{}", beat.body);
    let beat = post(&h, "/v1/agent/heartbeat", None, &node.beat()).await;
    assert_eq!(beat.status, 200, "{}", beat.body);
    let hosts = listed(&h).await;
    assert_eq!(hosts, [("mac-1".into(), 28_000), ("node-1".into(), 8000)]);
}

/// Asserts what a restart does to a tainted Mac on a server of two pools: the server puts
/// it back awaiting, with its taint (the binding of the second pool is kept by the saves of
/// the first's), and its hello admits it into `mac-build` again with
/// the taint and booked at the 28 cores its hello reports, while an untainted node of `burst` simply says hello
/// again. It catches a restored host held to the first pool, which would refuse a tainted
/// Mac its own pool after every restart, a taint of the second pool lost, and a restored Mac
/// booked at its pool's 24 cores instead of its hello's.
#[tokio::test]
async fn a_tainted_host_of_the_second_pool_joins_it_again_after_a_restart() {
    let first = two_pools("two_pools_restart").await;
    let node = post(&first, "/v1/agent/hello", None, &hello_body("node-1")).await;
    session(&node, "node-1");
    let mac = mac_hello("mac-1", "mac-build");
    let mac = session(&post(&first, "/v1/agent/hello", None, &mac).await, "mac-1");
    let mut beat = mac.beat();
    beat["taint"] = json!({ "reason": "residue", "check": "tmp" });
    let sent = post(&first, "/v1/agent/heartbeat", None, &beat).await;
    assert_eq!(sent.json()["taint_ack"], true, "{}", sent.body);
    let Harness {
        running, scratch, ..
    } = first;
    running.shutdown().await.unwrap();
    let args = two_pool_args(&scratch, &mac_pool());
    let h = Harness::with(scratch, replace(args, "--boot", "resume")).await;
    let hosts = h.call("GET", "/v1/hosts", None).await.json();
    let restored = &hosts["hosts"][0];
    assert_eq!(restored["id"], "mac-1", "{hosts}");
    assert_eq!(restored["taint"]["reason"], "residue", "{hosts}");
    let back = post(
        &h,
        "/v1/agent/hello",
        None,
        &mac_hello("mac-1", "mac-build"),
    )
    .await;
    assert_eq!(back.json()["pool"], "mac-build", "{}", back.body);
    let node = post(&h, "/v1/agent/hello", None, &hello_body("node-1")).await;
    session(&node, "node-1");
    let hosts = h.call("GET", "/v1/hosts", None).await.json();
    assert_eq!(hosts["hosts"][0]["taint"]["reason"], "residue", "{hosts}");
    assert_eq!(
        listed(&h).await,
        [("mac-1".into(), 28_000), ("node-1".into(), 8000)]
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
/// take its hosts as nodes. A third file whose name is the
/// second's is refused naming the third. It catches a
/// second pool that would be served with settings the server reads only from the first.
#[test]
fn a_second_pool_file_is_refused_naming_it() {
    let scratch = Scratch::new("two_pools_refusals");
    let file = scratch.path("mac-pool.toml");
    let at = format!("--agent-pool-config {}", file.display());
    let named_burst = mac_pool().replace("\"mac-build\"", "\"burst\"");
    let percent = mac_pool().replace("class", "memory_schedulable_percent = 80\nclass");
    let whole = mac_pool().replace("\"mac-host\"", "\"whole-node\"");
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
    let no_arch = mac_pool().replace("arch = \"arm64\"\n", "");
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
    // A second file that cannot be read is refused naming it.
    let mut args = agent_args(&scratch);
    let gone = scratch.path("no-such-pool.toml");
    args.extend(["--agent-pool-config".into(), gone.clone().into_os_string()]);
    let said = refused(args);
    let head = format!("--agent-pool-config: {}: ", gone.display());
    assert!(said.starts_with(&head), "{said}");
    // A third file is held to the second's name too, not only to the first's.
    let mut args = two_pool_args(&scratch, &mac_pool());
    let third = scratch.write("third-pool.toml", &mac_pool(), 0o644);
    args.extend(["--agent-pool-config".into(), third.clone().into_os_string()]);
    let said = refused(args);
    let at = format!("--agent-pool-config {}", third.display());
    assert_eq!(
        said,
        format!("{at}: name: also the name of an earlier pool")
    );
}

/// Asserts that a host that presents its certificate on a server of two pools is admitted into
/// the pool its hello names, whichever that is, with the certificate's credential:
/// the Mac `mac-1` is welcomed into `mac-build` and the node `node-1` into `burst`, each listed
/// at what its own hello reports (the Macs' 28 cores) A hello that carries an
/// instance of another provider is held to the pool it names: the Mac `mac-2`, whose instance
/// is the provider `burst`, is admitted into `mac-build`.
#[tokio::test]
async fn a_certificate_host_is_admitted_into_the_pool_its_hello_names() {
    let rig = Rig::start_with("host_cert_two_pools", Options::FILES, Some(&mac_pool())).await;
    for (host, hello) in [
        ("mac-1", mac_hello("mac-1", "mac-build")),
        ("node-1", hello_body("node-1")),
        ("mac-2", {
            let mut hello = mac_hello("mac-2", "mac-build");
            hello["instance"] = json!({ "provider": "burst", "instance_id": "mac-2" });
            hello
        }),
    ] {
        let identity = rig.ca.issue(&Spec::host(host));
        let welcome = rig
            .client(Some(&identity))
            .hello(None, &hello)
            .await
            .unwrap();
        assert_eq!(welcome.status, 200, "{host}: {}", welcome.body);
        let welcome = welcome.json();
        assert_eq!(welcome["pool"], hello["pool"], "{host}: {welcome}");
    }
    let hosts = rig.listing().await;
    let hosts: Vec<(&str, u64)> = hosts["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|host| {
            (
                host["id"].as_str().unwrap(),
                host["shape"]["cpu_millis"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        hosts,
        [("mac-1", 28_000), ("mac-2", 28_000), ("node-1", 8000)]
    );
}

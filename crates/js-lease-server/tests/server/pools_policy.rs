//! Who may read the status: the routes are on the listen port only, take no token, exist
//! only on a private address, and answer only a private peer.

use std::net::IpAddr;

use js_lease_server::is_private_address;
use serde_json::json;

use crate::agents_support::JOIN;
use crate::config_support::{PoolText, file, load, quoted, server};
use crate::pools_support::{Options, Rig, a10};
use crate::support::{Harness, Scratch, request};

/// Asserts that both routes answer on `server.listen` with no token and are 404 on the
/// agent port, which a launched host may reach. It catches a status route left on the
/// agent port.
#[tokio::test]
async fn the_status_routes_are_on_the_listen_port_only_and_need_no_token() {
    let rig = Rig::start(
        "policy_ports",
        vec![a10("gpu_1x_a10", 1, 129)],
        Options::default(),
    )
    .await;
    for path in ["/v1/pools", "/status"] {
        let reply = rig.get(path).await;
        assert_eq!(reply.status, 200, "{path}");
        let agent = rig.running.agent_addr().unwrap();
        let reply = request(agent, "GET", path, Some(JOIN), None).await;
        assert_eq!(reply.status, 404, "{path}");
    }
    rig.assert_clean();
}

/// Asserts that a server run on flags, which has no elastic pool, answers the route with
/// no pools.
#[tokio::test]
async fn a_server_without_an_elastic_pool_lists_none() {
    let h = Harness::new("policy_flags").await;
    let reply = request(h.addr(), "GET", "/v1/pools", None, None).await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.json(), json!({ "pools": [], "warnings": [] }));
}

/// Asserts which addresses are private: the loopback, the private ranges, link-local, the
/// tailnet's shared range and the wildcard are; anything else is public, including the
/// addresses next to each range's edge.
#[test]
fn private_addresses_are_the_loopback_private_ranges_and_the_tailnet() {
    let private = [
        "127.0.0.1",
        "10.1.2.3",
        "172.16.0.1",
        "172.31.255.255",
        "192.168.1.1",
        "169.254.1.1",
        "100.64.0.1",
        "100.127.255.255",
        "0.0.0.0",
        "::1",
        "::",
        "fd7a:115c:a1e0::1",
        "fe80::1",
        "::ffff:10.0.0.1",
        "::ffff:127.0.0.1",
    ];
    let public = [
        "8.8.8.8",
        "172.15.0.1",
        "172.32.0.1",
        "100.63.255.255",
        "100.128.0.1",
        "192.169.0.1",
        "203.0.113.7",
        "2001:4860:4860::8888",
        "fb00::1",
        "fec0::1",
        "::ffff:8.8.8.8",
    ];
    for text in private {
        assert!(
            is_private_address(text.parse::<IpAddr>().unwrap()),
            "{text}"
        );
    }
    for text in public {
        assert!(
            !is_private_address(text.parse::<IpAddr>().unwrap()),
            "{text}"
        );
    }
}

/// Loads a file with `listen` and TLS files and the shipped static pool.
fn listening(name: &str, listen: &str) -> js_lease_server::Config {
    let scratch = Scratch::new(name);
    let cert = scratch.write("cert.pem", "x", 0o644);
    let key = scratch.write("key.pem", "x", 0o600);
    let extra = format!(
        "listen = \"{listen}\"\ntls_cert = {}\ntls_key = {}",
        quoted(&cert),
        quoted(&key)
    );
    let hosts = PoolText::static_pool(&scratch, "hosts");
    load(&scratch, &file(&server(&scratch, &extra), &[&hosts])).unwrap()
}

/// Asserts the rule for a public bind: on a public address the status routes are not
/// served, and the configuration says so in its warnings; on a private address they are
/// served with no warning; on every interface they are served with the warning that
/// the bind is wide. It catches a status served without a token on a public address.
#[test]
fn a_public_bind_omits_the_status_and_says_so() {
    let public = listening("policy_public", "203.0.113.7:7300");
    assert!(!public.serves_status());
    assert_eq!(
        public.warnings(),
        [
            "server: warning: /v1/pools and /status are not served: server.listen 203.0.113.7:7300 is not a private address"
        ]
    );
    let private = listening("policy_private", "10.1.2.3:7300");
    assert!(private.serves_status());
    assert!(private.warnings().is_empty());
    let wide = listening("policy_wide", "0.0.0.0:7300");
    assert!(wide.serves_status());
    assert_eq!(
        wide.warnings(),
        [
            "server: warning: binds all interfaces (server.listen 0.0.0.0:7300); /v1/pools and /status are served only to private peers"
        ]
    );
}

/// Asserts that a listener on every interface serves the status over TLS and shows the
/// warning in both the JSON and the page.
#[tokio::test]
async fn a_wildcard_listener_shows_its_warning_in_the_status() {
    let options = Options {
        wildcard: true,
        ..Options::default()
    };
    let rig = Rig::start("policy_wildcard", vec![a10("gpu_1x_a10", 1, 129)], options).await;
    let line = "server: warning: binds all interfaces (server.listen 0.0.0.0:0); /v1/pools and /status are served only to private peers";
    assert_eq!(rig.pools().await["warnings"], json!([line]));
    let page = rig.get("/status").await;
    assert!(page.body.contains(line), "{}", page.body);
}

//! The bundle of a container the host does not wire: `create_loopback` against a backend
//! whose configuration names resolvers and a route wait.

use std::time::Duration;

use serde_json::json;

use super::tests::{config, scratch, start};
use super::*;

/// Reads the bundle's `resolv.conf` and the process arguments of `config.json` in `dir`.
fn bundle_of(dir: &Path) -> (String, serde_json::Value) {
    let resolv = std::fs::read_to_string(dir.join("etc/resolv.conf")).expect("resolv.conf");
    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("bundle/config.json")).expect("read"))
            .expect("json");
    (resolv, config["process"]["args"].clone())
}

/// Catches a container that asked for no network being held for a route nobody adds, or
/// resolving through resolvers it cannot reach: on a backend configured with resolvers and a
/// route wait, `create_loopback` writes the loopback `resolv.conf` and no `--wait-route`,
/// while `create` on the same backend writes both, so the difference is the call's.
#[tokio::test]
async fn a_loopback_bundle_has_neither_the_resolvers_nor_the_route_wait() {
    let dir = scratch("loopback-bundle");
    let mut configured = config(&dir, true);
    configured.resolv_conf = "nameserver 192.0.2.53\n".to_owned();
    configured.route_wait = Some(Duration::from_secs(7));
    let oci = start(&configured, &dir.join("scratch")).expect("started");

    let mut quiet = crate::container::tests::spec();
    quiet.dir = dir.join("quiet");
    oci.create_loopback(&quiet).await.expect("bundle");
    let (resolv, args) = bundle_of(&quiet.dir);
    assert_eq!(resolv, RESOLV_CONF);
    assert_eq!(args, json!(["/dev/js-init", "--", "tool", "--flag", "a b"]));

    let mut wired = crate::container::tests::spec();
    wired.dir = dir.join("wired");
    oci.create(&wired).await.expect("bundle");
    let (resolv, args) = bundle_of(&wired.dir);
    assert_eq!(resolv, "nameserver 192.0.2.53\n");
    assert_eq!(
        args,
        json!([
            "/dev/js-init",
            "--wait-route",
            "7",
            "--",
            "tool",
            "--flag",
            "a b"
        ])
    );
}

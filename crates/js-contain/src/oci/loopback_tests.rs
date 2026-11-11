//! The bundle of a container the host does not wire (`create_loopback` against a backend
//! whose configuration names resolvers and a route wait), and the listen ports a bundle's
//! init waits for, wired or not.

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

/// Proves a container with no network but listen ports still waits for them:
/// `create_loopback` writes `--wait-listen 30 <ports>` (and no `--wait-route`) before the
/// job's arguments. Catches a loopback container whose program starts before its own
/// ports listen.
#[tokio::test]
async fn a_loopback_bundle_waits_for_the_listen_ports() {
    let dir = scratch("loopback-listen");
    let mut configured = config(&dir, true);
    configured.route_wait = Some(Duration::from_secs(7));
    let oci = start(&configured, &dir.join("scratch")).expect("started");
    let mut spec = crate::container::tests::spec();
    spec.dir = dir.join("listening");
    spec.listen_ports = vec![5432, 8080];
    oci.create_loopback(&spec).await.expect("bundle");
    let (_, args) = bundle_of(&spec.dir);
    assert_eq!(
        args,
        json!([
            "/dev/js-init",
            "--wait-listen",
            "30",
            "5432,8080",
            "--",
            "tool",
            "--flag",
            "a b"
        ])
    );
}

/// Proves a spec's listen ports reach the bundle as `--wait-listen 30 <ports>` (comma
/// separated, in the spec's order), after the route wait and before the `--` of the job's
/// arguments, and that a spec without any writes no such option. Catches a container whose
/// init starts its program before the ports it was told to wait for listen, or one that
/// puts the options in an order the init refuses.
#[tokio::test]
async fn the_listen_ports_reach_the_bundle_after_the_route_wait() {
    let dir = scratch("listen-bundle");
    let mut configured = config(&dir, true);
    configured.route_wait = Some(Duration::from_secs(7));
    let oci = start(&configured, &dir.join("scratch")).expect("started");
    let args_of = |dir: &Path| bundle_of(dir).1;
    let mut spec = crate::container::tests::spec();
    spec.dir = dir.join("lease");
    spec.listen_ports = vec![5432, 8080];
    oci.create(&spec).await.expect("bundle");
    assert_eq!(
        args_of(&spec.dir),
        json!([
            "/dev/js-init",
            "--wait-route",
            "7",
            "--wait-listen",
            "30",
            "5432,8080",
            "--",
            "tool",
            "--flag",
            "a b"
        ])
    );
    spec.dir = dir.join("lease2");
    spec.listen_ports = Vec::new();
    oci.create(&spec).await.expect("bundle");
    assert_eq!(
        args_of(&spec.dir),
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

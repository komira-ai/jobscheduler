//! The cluster's own addresses a host's heartbeat teaches the server
//! (`docs/design/job-retry-and-completion-callback.md`, section 5.3): the address each beat
//! comes from is denied to every job's completion callback from then on.

use std::net::{IpAddr, SocketAddr};

use axum::body::Bytes;

use super::job_rig::{FakeAgent, rig};

/// Asserts that the address a beat that passed its checks comes from is denied to callbacks,
/// an IPv4 peer on a dual-stack listener as its IPv4 address too, while an address no beat
/// came from stays admitted; and that a beat refused (a body that is not a heartbeat, or one
/// of an epoch the registry does not hold), or one with no peer, teaches nothing. Catches a host's address left open
/// to a callback, and an unchecked request that grows the set.
#[tokio::test]
async fn a_heartbeats_peer_is_denied_to_callbacks() {
    let mut rig = rig("cluster_peers", true);
    rig.register("host-a", true);
    let agent = FakeAgent::new("host-a");
    let cluster = rig.agents.cluster.clone();
    let address = |text: &str| text.parse::<IpAddr>().expect(text);
    for text in ["10.1.2.3", "10.1.2.5", "10.1.2.6", "10.1.2.7"] {
        assert!(cluster.admits(address(text)), "{text}: before");
    }
    let peer = |text: &str| text.parse::<SocketAddr>().expect(text);
    rig.beat_from(&agent, peer("10.1.2.3:40000"))
        .await
        .expect("a beat");
    rig.beat_from(&agent, peer("[::ffff:10.1.2.5]:40001"))
        .await
        .expect("a beat");
    let refused = Some(axum::Extension(axum::extract::ConnectInfo(peer(
        "10.1.2.6:40002",
    ))));
    assert!(
        rig.beat_body(Bytes::from_static(b"{"), refused)
            .await
            .is_err()
    );
    let mut stale = rig.heartbeat(&agent, None);
    stale.epoch += 1000;
    let stale = Bytes::from(serde_json::to_vec(&stale).expect("json"));
    let from = Some(axum::Extension(axum::extract::ConnectInfo(peer(
        "10.1.2.8:40003",
    ))));
    assert!(rig.beat_body(stale, from).await.is_err(), "another epoch");
    rig.beat_body(
        Bytes::from(serde_json::to_vec(&rig.heartbeat(&agent, None)).expect("json")),
        None,
    )
    .await
    .expect("a beat");
    for (text, admitted) in [
        ("10.1.2.3", false),
        ("10.1.2.5", false),
        ("::ffff:10.1.2.5", false),
        ("10.1.2.6", true),
        ("10.1.2.8", true),
        ("10.1.2.7", true),
    ] {
        assert_eq!(cluster.admits(address(text)), admitted, "{text}");
    }
}

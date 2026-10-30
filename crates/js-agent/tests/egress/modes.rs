//! The two modes of the egress setting seen from a real container: the default, which gives
//! the job loopback and nothing else, and an allow-list, which holds the job until the
//! agent has wired its network.

use std::time::Duration;

use js_agent::egress::{Config, Egress};
use js_contain::container::Ended;

use crate::container::{ContainerHost, Network, PROBE, block_on, interfaces, section};
use crate::sys::{Far, Server, far, links, serial, table_loaded};

/// Asserts what the default mode gives, from inside a real container: the only interface is
/// `lo`, the routing table is empty, a connection to an address that exists on the far host
/// (and to the metadata address) fails at once as unreachable, the far host's listeners
/// never see a packet, the host got no veth and no firewall table, and the container's
/// `resolv.conf` is the backend's loopback one. The allow-list tests show the same probe
/// seeing an interface and a route and reaching the allowed endpoint, so this is the
/// default's doing and not the probe's.
#[test]
#[ignore = "needs root, crun, busybox, a job user: see the module docs"]
fn the_default_mode_gives_a_job_loopback_only_and_no_route() {
    let _serial = serial();
    let far_host = Far::new();
    let allowed = Server::tcp(&far_host.ns, far::ALLOWED);
    let metadata = Server::tcp(&far_host.ns, far::METADATA);
    let egress = Egress::start(Config::none()).expect("start");
    egress.check().expect("mode none has nothing to check");
    let script = format!(
        "{PROBE}echo '--dev'; cat /proc/net/dev; echo '--route'; cat /proc/net/route; \
         echo '--resolv'; cat /etc/resolv.conf; echo '--reach'; p 192.0.2.10 9300; \
         p 169.254.169.254 80; echo '--end'"
    );
    let outcome = block_on(async {
        let host = ContainerHost::new("none", Network::default()).await;
        let live = host.start(&script).await;
        let attachment = egress
            .attach("none", live.pid)
            .expect("attach does nothing");
        assert_eq!(attachment.interface(), None);
        live.finish().await
    });
    assert_eq!(outcome.ended, Ok(Ended::Exited(0)), "{}", outcome.said);
    let out = &outcome.stdout;
    assert_eq!(interfaces(&section(out, "dev")), ["lo"], "{}", outcome.said);
    let route = section(out, "route");
    assert_eq!(route.lines().count(), 1, "only the header: {route}");
    assert!(
        section(out, "resolv").contains("nameserver 127.0.0.1"),
        "{}",
        outcome.said
    );
    for target in ["192.0.2.10:9300", "169.254.169.254:80"] {
        let line = section(out, "reach")
            .lines()
            .find(|line| line.starts_with(&format!("reach {target}")))
            .unwrap_or_else(|| panic!("no probe of {target}: {}", outcome.said))
            .to_lowercase();
        assert!(!line.contains("pong"), "{line}");
        assert!(
            line.contains("unreachable"),
            "no route, so unreachable: {line}"
        );
    }
    assert_eq!(
        allowed.accepted() + metadata.accepted(),
        0,
        "a packet left the container"
    );
    assert!(links("jsv").is_empty(), "{:?}", links("jsv"));
    assert!(
        !table_loaded("inet", "js_egress"),
        "mode none loads no firewall"
    );
}

/// Asserts that an allow-list's container does not start its job until the agent has
/// wired it: with nobody to attach, the init gives up at the wait (here 3 s), the job's
/// first command never runs (its output is empty), the run ends as not run with the init's
/// words naming the missing route, and nothing of the job's was sent anywhere. The
/// allow-list's end-to-end test is the control: the same script, attached, prints.
#[test]
#[ignore = "needs root, crun, busybox, a job user: see the module docs"]
fn a_job_waits_for_its_network_and_never_starts_without_it() {
    let _serial = serial();
    let config = crate::sys::allow_list(&["192.0.2.10:9300"], &[]);
    let _egress = Egress::start(config.clone()).expect("start");
    let outcome = block_on(async {
        let network = Network {
            resolv_conf: config.resolv_conf(),
            route_wait: Some(Duration::from_secs(3)),
        };
        let host = ContainerHost::new("gate", network).await;
        let live = host.start("echo job-ran").await;
        live.finish().await
    });
    assert!(
        matches!(outcome.ended, Ok(Ended::NotRun(_))),
        "{}",
        outcome.said
    );
    assert!(outcome.stdout.is_empty(), "the job ran: {}", outcome.said);
    assert!(
        outcome.init_report.contains("no default route within 3 s"),
        "{}",
        outcome.said
    );
}

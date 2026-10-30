//! The job cannot alter the rules (control A), and the same attempts against a host-side
//! tamper show the checks can fail (control B).

use std::net::IpAddr;
use std::time::Duration;

use js_agent::egress::{Egress, EgressError};

use crate::container::{ContainerHost, Network, block_on};
use crate::sys::{
    Far, Holder, Reach, Server, allow_list, far, nft, run, serial, sysctl, tcp, text, tool, var,
};

/// `nsenter` into the user and network namespaces of `pid` as root: what a job holding every
/// capability of its own user namespace can do to its own network. Runs `program args`.
fn as_capable_job(pid: u32, program: &str, args: &[&str]) -> (bool, String) {
    let pid = pid.to_string();
    let mut all = vec!["--target", pid.as_str(), "--user", "--net", "--", program];
    all.extend_from_slice(args);
    let output = run(&tool("nsenter"), &all);
    (output.status.success(), text(&output))
}

/// Asserts control A. A job in a real container tries, one after the other, to take its
/// interfaces down, flush its routes, add an address, add a route to the metadata address and
/// switch IPv6 on: with no capability every attempt fails. Then the same attempts are made
/// with every capability of the job's own user namespace (entering it from the host), and
/// they succeed: the job's own namespace is its to reconfigure. What that buys it is
/// nothing: the rules live in the host's initial network namespace, where the job's
/// namespace has no table at all (a listing shows none; deleting one fails), and a connection
/// from the reconfigured namespace to another host, to the metadata address, or from an
/// address that is not its own, is still refused (the far host's listeners count nothing).
/// The firewall reads back unchanged afterwards. The spoofed-source attempt is also repeated
/// with the host's reverse-path filter off (control B for that defence): it then gets
/// through, so the filter is what stopped it.
#[test]
#[ignore = "needs root, crun, busybox, nsenter, a job user: see the module docs"]
fn a_job_cannot_alter_the_rules_even_with_every_capability_of_its_own_namespace() {
    let _serial = serial();
    let far_host = Far::new();
    let allowed = Server::tcp(&far_host.ns, far::ALLOWED);
    let other_host = Server::tcp(&far_host.ns, far::OTHER_HOST);
    let metadata = Server::tcp(&far_host.ns, far::METADATA);
    let config = allow_list(&["192.0.2.10:9300"], &[]);
    let egress = Egress::start(config.clone()).expect("start");
    let script = "echo started; \
        t() { o=$(\"$@\" 2>&1); echo \"tamper rc=$? [$*] $o\"; }; \
        t ip link set lo down; t ip link set eth0 down; t ip addr add 198.18.9.9/30 dev eth0; \
        t ip route flush all; t ip route add 169.254.169.254/32 dev eth0; \
        t sh -c 'echo 0 > /proc/sys/net/ipv6/conf/eth0/disable_ipv6'; echo done; sleep 40";
    let busybox = var("JS_B_BUSYBOX");
    let ip = tool("ip");
    let nft_bin = tool("nft");
    block_on(async {
        let network = Network {
            resolv_conf: config.resolv_conf(),
            route_wait: config.route_wait(),
        };
        let host = ContainerHost::new("tamper", network).await;
        let live = host.start(script).await;
        let attachment = egress.attach("tamper", live.pid).expect("attach");
        assert!(
            live.wait_for_line("done", Duration::from_secs(30)).await,
            "{}",
            live.stdout_so_far()
        );
        let first = live.stdout_so_far();
        let attempts: Vec<&str> = first
            .lines()
            .filter(|l| l.starts_with("tamper rc="))
            .collect();
        assert_eq!(attempts.len(), 6, "{first}");
        for line in &attempts {
            assert!(
                !line.starts_with("tamper rc=0 "),
                "the job changed its network: {line}"
            );
        }

        // The same attempts with every capability of the job's own user namespace.
        let capable = |program: &str, args: &[&str]| as_capable_job(live.pid, program, args);
        let (down, said) = capable(&ip, &["link", "set", "lo", "down"]);
        assert!(down, "the control cannot do what the job could not: {said}");
        assert!(capable(&ip, &["link", "set", "lo", "up"]).0);
        assert!(capable(&ip, &["route", "add", "169.254.169.254/32", "dev", "eth0"]).0);
        let (listing, listed) = capable(&nft_bin, &["list", "ruleset"]);
        assert!(listing, "{listed}");
        assert!(
            !listed.contains("js_egress"),
            "the rules are in the job's namespace: {listed}"
        );
        let (deleted, said) = capable(&nft_bin, &["delete", "table", "inet", "js_egress"]);
        assert!(
            !deleted,
            "there is no table of the host's in the job's namespace: {said}"
        );
        let before = (
            other_host.accepted(),
            metadata.accepted(),
            allowed.accepted(),
        );
        let (_, said) = capable(&busybox, &["nc", "-w", "2", "192.0.2.40", "9300"]);
        assert!(!said.contains("pong"), "{said}");
        let (_, said) = capable(&busybox, &["nc", "-w", "2", "169.254.169.254", "80"]);
        assert!(!said.contains("pong"), "{said}");
        let (_, said) = capable(&busybox, &["nc", "-w", "2", "192.0.2.10", "9300"]);
        assert!(
            said.contains("pong"),
            "the capable position works for what is allowed: {said}"
        );
        // A source address that is not the job's own.
        assert!(capable(&ip, &["addr", "add", "198.18.0.50/30", "dev", "eth0"]).0);
        let (_, said) = capable(
            &busybox,
            &["nc", "-s", "198.18.0.50", "-w", "2", "192.0.2.10", "9300"],
        );
        assert!(
            !said.contains("pong"),
            "a spoofed source got through: {said}"
        );
        assert_eq!(
            allowed.accepted(),
            before.2 + 1,
            "only the honest connection arrived"
        );
        // Control B of that defence: with the reverse-path filter off the spoof works.
        sysctl("net/ipv4/conf/jsv0/rp_filter", "0");
        let (_, said) = capable(
            &busybox,
            &["nc", "-s", "198.18.0.50", "-w", "2", "192.0.2.10", "9300"],
        );
        sysctl("net/ipv4/conf/jsv0/rp_filter", "1");
        assert!(
            said.contains("pong"),
            "the reverse-path filter was not what stopped it: {said}"
        );
        assert_eq!(other_host.accepted(), before.0);
        assert_eq!(metadata.accepted(), before.1);
        egress.check().expect("the firewall is unchanged");
        live.terminate().await;
        let outcome = live.finish().await;
        println!("{}", outcome.said);
        attachment.release().expect("release");
    });
}

/// Asserts control B: the rules are what keeps the other host out, and a change to them is
/// seen. From a holder in a network namespace the other host's endpoint is refused; after
/// root (the one party that can) empties the forward chain it is reached; and the agent's
/// read-back, which passed before, refuses with the rules error. A flush of the chain, a
/// removed forwarding switch and a rule added to the table are each caught.
#[test]
#[ignore = "needs root, ip, nft, unshare: see the module docs"]
fn the_rules_are_what_refuse_the_other_host_and_a_change_to_them_is_seen() {
    let _serial = serial();
    let far_host = Far::new();
    let other_host = Server::tcp(&far_host.ns, far::OTHER_HOST);
    let egress = Egress::start(allow_list(&["192.0.2.10:9300"], &[])).expect("start");
    let holder = Holder::new();
    let attachment = egress.attach("control", holder.pid()).expect("attach");
    egress.check().expect("loaded");
    assert!(matches!(tcp(&holder.ns, far::OTHER_HOST), Reach::Failed(_)));
    assert_eq!(other_host.accepted(), 0);

    nft(&[
        "add",
        "rule",
        "inet",
        "js_egress",
        "forward",
        "counter",
        "accept",
    ]);
    assert_eq!(
        egress.check(),
        Err(EgressError::Rules),
        "a rule added to the table"
    );
    nft(&["flush", "chain", "inet", "js_egress", "forward"]);
    assert_eq!(egress.check(), Err(EgressError::Rules), "a chain emptied");
    let reached = tcp(&holder.ns, far::OTHER_HOST);
    assert!(reached.connected(), "{reached:?}");
    assert_eq!(other_host.peers(), [IpAddr::V4(far::HOST)]);

    sysctl("net/ipv4/ip_forward", "0");
    assert_eq!(
        egress.check(),
        Err(EgressError::Forwarding),
        "forwarding off"
    );
    sysctl("net/ipv4/ip_forward", "1");
    drop(attachment);

    // A restart puts all of it back.
    let restarted = Egress::start(allow_list(&["192.0.2.10:9300"], &[])).expect("restart");
    restarted
        .check()
        .expect("a restart loads the firewall again");
}

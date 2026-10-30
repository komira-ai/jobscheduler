//! (e) A root helper, standing for a separate privileged process (a systemd unit), that
//! builds the container's network, and whether the rootless container can use it.
//!
//! - e1: the daemon creates the container; the helper then makes a veth pair whose peer is
//!   moved into the container's namespace, addresses and routes it, switches IPv6 off and
//!   loads the nft table there. The container never joins anything: it owns its namespace,
//!   and the helper reaches into it by pid.
//! - e2: the daemon makes a user and network namespace pair with `unshare` (no privilege);
//!   the helper plumbs the network namespace by pid; the container joins both by path.
//!   Joining the pair is the one way a rootless `crun` can use a namespace made before it.
//!
//! The helper runs through `sudo -n`; each call it makes is one a unit with
//! `CAP_NET_ADMIN`, `CAP_SYS_ADMIN` and `CAP_SYS_PTRACE` could make.

use std::time::Duration;

use crate::bundle::{Bundle, Join, Outcome, Spec};
use crate::host::{self, Sysctl, endpoints, serial};
use crate::report::row;
use crate::scenario::{
    ALLOW_LIST, Fixture, Holder, assert_allow_list_enforced, assert_ipv6_off,
    assert_job_cannot_tamper, counter, enter_as_root, listing, record_suite,
};

const TOOLS: [&str; 8] = [
    "crun",
    "newuidmap",
    "newgidmap",
    "sudo",
    "sysctl",
    "nsenter",
    "nft",
    "ip",
];
const RESTRICT: &str = "kernel.apparmor_restrict_unprivileged_userns";

/// Removes a veth pair (by its host end) when dropped.
struct Veth(String);

impl Drop for Veth {
    fn drop(&mut self) {
        let _ = host::sudo("ip", &["link", "del", &self.0]);
    }
}

fn suite(name: &str) -> Spec {
    let mut spec = Spec::new(name, "suite");
    spec.env
        .push(("JS_EGRESS_TARGETS".to_owned(), endpoints::ALL.to_owned()));
    spec
}

/// What the helper does for a namespace owned by someone else: IPv6 off, a veth pair with the
/// peer in the namespace of `pid`, 192.0.2.1/24 on the host end and 192.0.2.2/24 with a
/// default route in it, and the allow-list.
fn plumb(pid: u32, host_end: &str, peer: &str) -> Veth {
    let _ = host::sudo("ip", &["link", "del", host_end]);
    let off = enter_as_root(
        pid,
        "sysctl",
        &[
            "-w",
            "net.ipv6.conf.all.disable_ipv6=1",
            "net.ipv6.conf.default.disable_ipv6=1",
        ],
        "",
    );
    assert!(off.status.success(), "{}", host::text(&off));
    host::sudo_must(
        "ip",
        &[
            "link",
            "add",
            host_end,
            "type",
            "veth",
            "peer",
            "name",
            peer,
            "netns",
            &pid.to_string(),
        ],
    );
    let veth = Veth(host_end.to_owned());
    host::sudo_must("ip", &["addr", "add", "192.0.2.1/24", "dev", host_end]);
    host::sudo_must("ip", &["link", "set", host_end, "up"]);
    for args in [
        vec!["addr", "add", "192.0.2.2/24", "dev", peer],
        vec!["link", "set", peer, "up"],
        vec!["link", "set", "lo", "up"],
        vec!["route", "add", "default", "via", "192.0.2.1"],
    ] {
        let done = enter_as_root(pid, "ip", &args, "");
        assert!(done.status.success(), "ip {args:?}: {}", host::text(&done));
    }
    let loaded = enter_as_root(pid, "nft", &["-f", "-"], ALLOW_LIST);
    assert!(loaded.status.success(), "{}", host::text(&loaded));
    veth
}

fn assert_whole(outcome: &Outcome, table: &str) {
    assert_allow_list_enforced(outcome, "before");
    assert_allow_list_enforced(outcome, "after");
    assert_job_cannot_tamper(outcome);
    assert_ipv6_off(outcome);
    for comment in ["allow-a", "allow-b", "metadata", "default-reject"] {
        assert!(
            counter(table, comment).unwrap_or(0) >= 1,
            "{comment} never matched:\n{table}"
        );
    }
}

/// Asserts that a helper can give a rootless container a veth network with the allow-list,
/// by entering the container's own namespace by pid: the allowed endpoints answer, the rest
/// is refused by the rules, the job cannot edit them, IPv6 is off.
#[test]
#[ignore = "needs crun, nsenter, nft, passwordless sudo: see the egress-spike workflow"]
fn e1_helper_plumbs_a_veth_into_the_containers_namespace() {
    let _serial = serial();
    if !host::require(&TOOLS) {
        return;
    }
    let _restrict = Sysctl::set(RESTRICT, "0");
    let _fixture = Fixture::new();
    let mut spec = suite("e1");
    spec.gate = true;
    let created = Bundle::new(&spec)
        .create()
        .unwrap_or_else(|o| panic!("create: {o:?}"));
    let _veth = plumb(created.pid, "jsv1-h", "jsv1-c");
    created.start();
    assert!(
        created.wait_for("suite.done", Duration::from_secs(120)),
        "{:?}",
        created.so_far()
    );
    let table = listing(&enter_as_root(
        created.pid,
        "nft",
        &["list", "table", "inet", "js_egress"],
        "",
    ));
    let outcome = created.finish();
    record_suite("e1 root helper: veth into the container's netns", &outcome);
    assert_whole(&outcome, &table);
}

/// Asserts what joining a pre-made pair does: the daemon makes the user and network
/// namespaces, the helper plumbs the network one, the container joins both by path. Records
/// whether `crun` accepts it; when it does, the same checks as above hold.
#[test]
#[ignore = "needs crun, nsenter, nft, passwordless sudo: see the egress-spike workflow"]
fn e2_container_joins_a_user_and_network_namespace_pair_made_by_the_daemon() {
    let _serial = serial();
    if !host::require(&TOOLS) {
        return;
    }
    let _restrict = Sysctl::set(RESTRICT, "0");
    let _fixture = Fixture::new();
    let holder = Holder::start();
    let _veth = plumb(holder.pid(), "jsv2-h", "jsv2-c");
    let mut spec = suite("e2");
    spec.net = Join::Path(holder.ns("net"));
    spec.user = Join::Path(holder.ns("user"));
    let outcome = Bundle::new(&spec).run();
    let label = "e2 container joins daemon-made userns+netns pair";
    row(label, "container starts", outcome.get("uid") != "-");
    row(label, "error", outcome.first_error());
    record_suite(label, &outcome);
    let table = listing(&enter_as_root(
        holder.pid(),
        "nft",
        &["list", "table", "inet", "js_egress"],
        "",
    ));
    assert_eq!(outcome.exit, Some(0), "{outcome:?}");
    assert_whole(&outcome, &table);
}

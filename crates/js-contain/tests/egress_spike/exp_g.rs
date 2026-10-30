//! Privilege option B: the agent is root and runs each job's `crun` container, with a user
//! namespace that maps the container onto a non-root range of host ids, as a non-root job.
//! The network is made by the root agent itself: a named network namespace owned by the
//! initial user namespace, a veth pair, the nft allow-list inside, IPv6 off.
//!
//! - g1: root `crun` joins a root-made netns by path while making its own user namespace
//!   (the rootless case b1 refused).
//! - g2: the full path: allowed endpoints answer, the rest is refused by the rules, the job
//!   cannot edit them (also with CAP_NET_ADMIN inside its own user namespace), IPv6 off with a
//!   control that shows it on first; plus the control that a job sharing the host's user
//!   namespace with CAP_NET_ADMIN can edit them.
//! - g3: the composite, a kbf-task style inner `unshare` inside, under the real seccomp
//!   profile.

use crate::bundle::{Bundle, Join, Outcome, Spec};
use crate::host::{self, Sysctl, endpoints, serial};
use crate::report::row;
use crate::scenario::{
    ALLOW_LIST, Fixture, assert_allow_list_enforced, assert_ipv6_off, assert_job_cannot_tamper,
    counter, record_suite,
};

const TOOLS: [&str; 8] = [
    "crun",
    "newuidmap",
    "sudo",
    "sysctl",
    "nsenter",
    "nft",
    "ip",
    "setpriv",
];
const RESTRICT: &str = "kernel.apparmor_restrict_unprivileged_userns";

/// A named network namespace (and veth) made by root; removed when dropped.
struct RootNet {
    name: String,
    host_end: String,
}

impl RootNet {
    /// A bare namespace.
    fn new(name: &str, host_end: &str) -> Self {
        let _ = host::sudo("ip", &["netns", "del", name]);
        let _ = host::sudo("ip", &["link", "del", host_end]);
        host::sudo_must("ip", &["netns", "add", name]);
        Self {
            name: name.to_owned(),
            host_end: host_end.to_owned(),
        }
    }

    fn path(&self) -> String {
        format!("/run/netns/{}", self.name)
    }

    /// Runs `program args` inside the namespace as root.
    fn exec(&self, program: &str, args: &[&str], stdin: &str) -> std::process::Output {
        let ip = host::bin("ip");
        let program = host::bin(program);
        let mut all = vec![
            "-n",
            ip.as_str(),
            "netns",
            "exec",
            self.name.as_str(),
            program.as_str(),
        ];
        all.extend_from_slice(args);
        host::run_with_stdin("sudo", &all, stdin)
    }

    fn exec_must(&self, program: &str, args: &[&str]) {
        let output = self.exec(program, args, "");
        assert!(
            output.status.success(),
            "{program} {args:?}: {}",
            host::text(&output)
        );
    }

    fn lo_up(&self) {
        self.exec_must("ip", &["link", "set", "lo", "up"]);
    }

    fn ipv6_off(&self) {
        self.exec_must(
            "sysctl",
            &[
                "-w",
                "net.ipv6.conf.all.disable_ipv6=1",
                "net.ipv6.conf.default.disable_ipv6=1",
            ],
        );
    }

    /// The veth, its addresses and route, IPv6 off and the allow-list.
    fn plumb(&self) {
        let peer = "vgc";
        host::sudo_must(
            "ip",
            &[
                "link",
                "add",
                &self.host_end,
                "type",
                "veth",
                "peer",
                "name",
                peer,
                "netns",
                &self.name,
            ],
        );
        host::sudo_must(
            "ip",
            &["addr", "add", "192.0.2.1/24", "dev", &self.host_end],
        );
        host::sudo_must("ip", &["link", "set", &self.host_end, "up"]);
        self.lo_up();
        self.exec_must("ip", &["addr", "add", "192.0.2.2/24", "dev", peer]);
        self.exec_must("ip", &["link", "set", peer, "up"]);
        self.exec_must("ip", &["route", "add", "default", "via", "192.0.2.1"]);
        self.ipv6_off();
        let loaded = self.exec("nft", &["-f", "-"], ALLOW_LIST);
        assert!(loaded.status.success(), "{}", host::text(&loaded));
    }

    fn table(&self) -> String {
        host::text(&self.exec("nft", &["list", "table", "inet", "js_egress"], ""))
    }
}

impl Drop for RootNet {
    fn drop(&mut self) {
        let _ = host::sudo("ip", &["link", "del", &self.host_end]);
        let _ = host::sudo("ip", &["netns", "del", &self.name]);
    }
}

fn spec(name: &str, probe: &str, net: &RootNet) -> Spec {
    let mut spec = Spec::new(name, probe);
    spec.root = true;
    spec.net = Join::Path(net.path());
    spec.env
        .push(("JS_EGRESS_TARGETS".to_owned(), endpoints::ALL.to_owned()));
    spec
}

/// The job runs as a non-root id: id 1000 in a user namespace whose id 0 is a non-zero host id.
fn assert_non_root_job(outcome: &Outcome) {
    assert!(
        outcome.get("uid").starts_with("1000"),
        "{:?}",
        outcome.probe()
    );
    let map = outcome.get("uid_map");
    assert!(
        map.starts_with("0 ") && !map.starts_with("0 0 "),
        "uid_map {map}"
    );
}

/// Asserts that a root `crun` joins a network namespace made by root with `ip netns add`
/// while it makes the job's user namespace, and the job is a non-root id.
#[test]
#[ignore = "needs crun, nsenter, nft, passwordless sudo: see the egress-spike workflow"]
fn g1_root_crun_joins_a_root_made_netns_with_its_own_userns() {
    let _serial = serial();
    if !host::require(&TOOLS) {
        return;
    }
    let net = RootNet::new("js-spike-g1", "vg1h");
    let outcome = Bundle::new(&spec("g1", "id", &net)).run();
    let label = "g1 root crun joins root-made netns by path, own userns";
    row(label, "crun exit", format!("{:?}", outcome.exit));
    row(
        label,
        "job uid / uid_map",
        format!("{} / {}", outcome.get("uid"), outcome.get("uid_map")),
    );
    row(label, "error", outcome.first_error());
    assert_eq!(outcome.exit, Some(0), "{outcome:?}");
    assert_non_root_job(&outcome);
}

/// Asserts the root-made network end to end, and the controls beside it.
#[test]
#[ignore = "needs crun, nsenter, nft, passwordless sudo: see the egress-spike workflow"]
fn g2_root_made_netns_veth_and_table() {
    let _serial = serial();
    if !host::require(&TOOLS) {
        return;
    }
    let _restrict = Sysctl::set(RESTRICT, "1");
    let _fixture = Fixture::new();
    let net = RootNet::new("js-spike-g2", "vg2h");
    let label = "g2 root netns + veth + table, root crun, non-root job";
    // IPv6 control: with only `lo` and the default sysctls the job has a working IPv6 loopback.
    net.lo_up();
    let before = Bundle::new(&spec("g2v6a", "v6", &net)).run();
    net.ipv6_off();
    let after = Bundle::new(&spec("g2v6b", "v6", &net)).run();
    row(label, "ipv6 control before", before.get("v6.loopback"));
    row(label, "ipv6 control after", after.get("v6.loopback"));
    assert_eq!(before.get("v6.loopback"), "ok", "{before:?}");
    assert_ne!(after.get("v6.loopback"), "ok");
    assert_ne!(after.get("v6.loopback"), "-");

    net.plumb();
    let outcome = Bundle::new(&spec("g2", "suite", &net)).run();
    let table = net.table();
    record_suite(label, &outcome);
    assert_eq!(outcome.exit, Some(0), "{outcome:?}");
    assert_non_root_job(&outcome);
    assert_allow_list_enforced(&outcome, "before");
    assert_allow_list_enforced(&outcome, "after");
    assert_job_cannot_tamper(&outcome);
    assert_ipv6_off(&outcome);
    for comment in ["allow-a", "allow-b", "metadata", "default-reject"] {
        assert!(
            counter(&table, comment).unwrap_or(0) >= 1,
            "{comment} never matched:\n{table}"
        );
    }

    // Control 1: CAP_NET_ADMIN inside the job's own user namespace is not enough, because the
    // namespace is owned by the initial one.
    let mut own = spec("g2cap", "suite", &net);
    own.uid = 0;
    own.net_admin = true;
    let own = Bundle::new(&own).run();
    record_suite("g2 job root + CAP_NET_ADMIN in its own userns", &own);
    assert_eq!(own.get("tamper.succeeded"), "0", "{:?}", own.probe());
    assert!(counter(&net.table(), "default-reject").is_some());

    // Control 2: a job that shares the host's user namespace and holds CAP_NET_ADMIN can
    // flush the table (so the attempts above are real).
    let mut shared = spec("g2host", "suite", &net);
    shared.uid = 0;
    shared.net_admin = true;
    shared.user = Join::Host;
    let shared = Bundle::new(&shared).run();
    record_suite(
        "g2 control: job in the host userns with CAP_NET_ADMIN",
        &shared,
    );
    let succeeded: u32 = shared.get("tamper.succeeded").parse().unwrap_or(0);
    assert!(succeeded >= 2, "{:?}", shared.probe());
    assert!(
        !net.exec("nft", &["list", "table", "inet", "js_egress"], "")
            .status
            .success(),
        "the table survived"
    );
}

/// Asserts the composite under g2's network and the real seccomp profile: the outer job
/// reaches the allowed endpoints only, and its kbf-task style inner namespace has `lo` only.
#[test]
#[ignore = "needs crun, nsenter, nft, passwordless sudo: see the egress-spike workflow"]
fn g3_composite_under_the_root_made_network_and_real_seccomp() {
    let _serial = serial();
    if !host::require(&TOOLS) {
        return;
    }
    let _restrict = Sysctl::set(RESTRICT, "1");
    let _fixture = Fixture::new();
    let net = RootNet::new("js-spike-g3", "vg3h");
    net.plumb();
    let mut job = spec("g3", "composite", &net);
    job.seccomp = true;
    let outcome = Bundle::new(&job).run();
    let table = net.table();
    record_suite("g3 composite, root netns, real seccomp", &outcome);
    assert_eq!(outcome.exit, Some(0), "{outcome:?}");
    assert_non_root_job(&outcome);
    assert_eq!(outcome.get("seccomp"), "2");
    assert_allow_list_enforced(&outcome, "before");
    assert_allow_list_enforced(&outcome, "after");
    assert_job_cannot_tamper(&outcome);
    assert_ipv6_off(&outcome);
    assert!(
        counter(&table, "default-reject").unwrap_or(0) >= 1,
        "{table}"
    );
    assert_eq!(outcome.get("steps"), "all ok", "{:?}", outcome.probe());
    assert_eq!(outcome.get("inner.ifaces"), "lo");
    assert_eq!(outcome.get("inner.loopback"), "ok");
    for target in endpoints::ALL.split(',') {
        let value = outcome.get(&format!("inner.connect.{target}"));
        assert!(
            value.ends_with("errno 101"),
            "inner process: {target}: {value}"
        );
    }
}

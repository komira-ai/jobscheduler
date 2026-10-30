//! The production shape, and two controls that decide how far the table can be trusted.
//!
//! - f1/f2: the container joins the daemon-made pair with pasta and the table in it, and the
//!   job then does what `kbf-task` does, an inner `unshare(NEWUSER|NEWNET)`. Both levels are
//!   asserted: the outer job reaches the allowed endpoints only and cannot edit the table; the
//!   inner process has `lo` only and gets ENETUNREACH to every endpoint. Run with the
//!   restriction on and the distribution's crun profile; f2 adds the real seccomp profile.
//! - f3: the IPv6 control. In a namespace with `lo` up and the default sysctls, the job has a
//!   working IPv6 loopback; after the daemon sets `disable_ipv6`, it has none.

use crate::bundle::{Bundle, Outcome};
use crate::host::{self, Sysctl, endpoints, serial};
use crate::report::row;
use crate::scenario::{
    Fixture, Holder, assert_allow_list_enforced, assert_ipv6_off, assert_job_cannot_tamper,
    counter, enter_as_owner, pair_spec, record_suite, table_of,
};

const TOOLS: [&str; 9] = [
    "crun",
    "newuidmap",
    "newgidmap",
    "sudo",
    "sysctl",
    "nsenter",
    "nft",
    "ip",
    "pasta",
];
const RESTRICT: &str = "kernel.apparmor_restrict_unprivileged_userns";

fn composite(label: &str, seccomp: bool) -> (Outcome, String) {
    let _restrict = Sysctl::set(RESTRICT, "1");
    let _fixture = Fixture::new();
    let holder = Holder::start();
    let mut spec = pair_spec("f", "composite", &holder);
    spec.seccomp = seccomp;
    let bundle = Bundle::new(&spec);
    let log = bundle.dir.join("pasta.log");
    let _pasta = crate::exp_d::prepare_pair(&holder, &log, true);
    let outcome = bundle.run();
    let table = table_of(&holder);
    record_suite(label, &outcome);
    assert_eq!(outcome.exit, Some(0), "{outcome:?}");
    (outcome, table)
}

fn assert_composite(outcome: &Outcome, table: &str) {
    assert_allow_list_enforced(outcome, "before");
    assert_allow_list_enforced(outcome, "after");
    assert_job_cannot_tamper(outcome);
    assert_ipv6_off(outcome);
    assert!(
        counter(table, "default-reject").unwrap_or(0) >= 1,
        "{table}"
    );
    // The build command's own namespace.
    assert_eq!(outcome.get("steps"), "all ok", "{:?}", outcome.probe());
    assert_eq!(outcome.get("inner.ifaces"), "lo");
    assert_eq!(outcome.get("inner.loopback"), "ok");
    for target in endpoints::ALL.split(',') {
        let value = outcome.get(&format!("inner.connect.{target}"));
        assert!(
            value.ends_with("errno 101"),
            "inner process reached or was not told ENETUNREACH for {target}: {value}"
        );
    }
}

/// Asserts the production shape with the restriction on and the distribution's crun profile.
#[test]
#[ignore = "needs crun, pasta, nft, passwordless sudo: see the egress-spike workflow"]
fn f1_composite_pair_pasta_table_and_inner_unshare() {
    let _serial = serial();
    if !host::require(&TOOLS) {
        return;
    }
    let (outcome, table) = composite("f1 composite, restriction on, shipped crun profile", false);
    assert_composite(&outcome, &table);
}

/// Asserts the same with the real seccomp profile of the container runner in the bundle.
#[test]
#[ignore = "needs crun, pasta, nft, passwordless sudo: see the egress-spike workflow"]
fn f2_composite_under_the_real_seccomp_profile() {
    let _serial = serial();
    if !host::require(&TOOLS) {
        return;
    }
    row("f2", "seccomp entries applied", crate::seccomp::section().1);
    let (outcome, table) = composite(
        "f2 composite, restriction on, shipped crun profile, real seccomp",
        true,
    );
    assert_composite(&outcome, &table);
}

/// Asserts the IPv6 control: usable before the daemon turns it off, unusable after.
#[test]
#[ignore = "needs crun, nsenter, passwordless sudo: see the egress-spike workflow"]
fn f3_the_disable_ipv6_switch_takes_ipv6_away() {
    let _serial = serial();
    if !host::require(&TOOLS) {
        return;
    }
    let _restrict = Sysctl::set(RESTRICT, "0");
    let holder = Holder::start();
    let up = enter_as_owner(holder.pid(), "ip", &["link", "set", "lo", "up"], "");
    assert!(up.status.success(), "{}", host::text(&up));
    let before = Bundle::new(&pair_spec("f3a", "v6", &holder)).run();
    let off = enter_as_owner(
        holder.pid(),
        "sysctl",
        &[
            "-w",
            "net.ipv6.conf.all.disable_ipv6=1",
            "net.ipv6.conf.default.disable_ipv6=1",
        ],
        "",
    );
    assert!(off.status.success(), "{}", host::text(&off));
    let after = Bundle::new(&pair_spec("f3b", "v6", &holder)).run();
    let label = "f3 ipv6 control";
    row(
        label,
        "before: v6 loopback / disable_ipv6",
        format!(
            "{} / {}",
            before.get("v6.loopback"),
            before.get("v6.disable_all")
        ),
    );
    row(
        label,
        "after: v6 loopback / disable_ipv6",
        format!(
            "{} / {}",
            after.get("v6.loopback"),
            after.get("v6.disable_all")
        ),
    );
    assert_eq!(before.get("v6.loopback"), "ok", "{before:?}");
    assert_ne!(after.get("v6.loopback"), "ok");
    assert_ne!(after.get("v6.loopback"), "-");
    assert_eq!(after.get("v6.disable_all"), "1");
}

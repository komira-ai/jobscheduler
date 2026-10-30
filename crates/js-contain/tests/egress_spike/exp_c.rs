//! (c) An nft allow-list in the container's own network namespace that the job cannot edit.
//!
//! The namespace is owned by the container's user namespace, whose owner is the daemon user,
//! so the daemon enters it with every capability there (`nsenter --user --net`) and loads
//! the table before the job starts. The job runs with no capabilities, so it has no
//! `CAP_NET_ADMIN` over the namespace that its own user namespace owns.

use std::time::Duration;

use crate::bundle::{Bundle, Spec};
use crate::host::{self, Sysctl, endpoints, serial};
use crate::report::row;
use crate::scenario::{
    ALLOW_LIST, assert_job_cannot_tamper, counter, enter_as_owner, listing, record_suite,
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

fn suite(name: &str) -> Spec {
    let mut spec = Spec::new(name, "suite");
    spec.gate = true;
    spec.env
        .push(("JS_EGRESS_TARGETS".to_owned(), endpoints::ALL.to_owned()));
    spec
}

/// Asserts that the job's attempts to flush, delete from, add to the table and take `lo`
/// down all fail, and that the table the daemon loaded is whole afterwards (every rule is
/// still there, read back by the daemon).
#[test]
#[ignore = "needs crun, nsenter, nft, passwordless sudo: see the egress-spike workflow"]
fn c1_the_job_cannot_edit_the_allow_list_in_its_own_netns() {
    let _serial = serial();
    if !host::require(&TOOLS) {
        return;
    }
    let _restrict = Sysctl::set("kernel.apparmor_restrict_unprivileged_userns", "0");
    let created = Bundle::new(&suite("c1"))
        .create()
        .unwrap_or_else(|o| panic!("create: {o:?}"));
    let loaded = enter_as_owner(created.pid, "nft", &["-f", "-"], ALLOW_LIST);
    assert!(
        loaded.status.success(),
        "the daemon could not load the table: {}",
        host::text(&loaded)
    );
    created.start();
    assert!(
        created.wait_for("suite.done", Duration::from_secs(90)),
        "{:?}",
        created.so_far()
    );
    let after = listing(&enter_as_owner(
        created.pid,
        "nft",
        &["list", "table", "inet", "js_egress"],
        "",
    ));
    let outcome = created.finish();
    record_suite("c1 allow-list in own netns, job tampers", &outcome);
    assert_job_cannot_tamper(&outcome);
    for comment in ["ipv6", "metadata", "allow-a", "allow-b", "default-reject"] {
        assert!(
            counter(&after, comment).is_some(),
            "rule {comment} is gone after the job ran:\n{after}"
        );
    }
    row(
        "c1 allow-list in own netns, job tampers",
        "table intact after the job",
        true,
    );
}

/// Asserts the control of the test above: a job that does hold `CAP_NET_ADMIN` in the
/// namespace's user namespace can flush the table. Without this, "the job cannot edit" would
/// pass for a job whose `nft` never ran.
#[test]
#[ignore = "needs crun, nsenter, nft, passwordless sudo: see the egress-spike workflow"]
fn c2_a_job_with_net_admin_can_edit_it() {
    let _serial = serial();
    if !host::require(&TOOLS) {
        return;
    }
    let _restrict = Sysctl::set("kernel.apparmor_restrict_unprivileged_userns", "0");
    let mut spec = suite("c2");
    spec.uid = 0;
    spec.net_admin = true;
    let created = Bundle::new(&spec)
        .create()
        .unwrap_or_else(|o| panic!("create: {o:?}"));
    let loaded = enter_as_owner(created.pid, "nft", &["-f", "-"], ALLOW_LIST);
    assert!(loaded.status.success(), "{}", host::text(&loaded));
    created.start();
    assert!(
        created.wait_for("suite.done", Duration::from_secs(90)),
        "{:?}",
        created.so_far()
    );
    let after = enter_as_owner(
        created.pid,
        "nft",
        &["list", "table", "inet", "js_egress"],
        "",
    );
    let outcome = created.finish();
    record_suite("c2 control: job holds CAP_NET_ADMIN", &outcome);
    let succeeded: u32 = outcome.get("tamper.succeeded").parse().unwrap_or(0);
    assert!(succeeded >= 2, "{:?}", outcome.probe());
    assert!(
        !after.status.success(),
        "the table survived a job that holds CAP_NET_ADMIN:\n{}",
        listing(&after)
    );
}

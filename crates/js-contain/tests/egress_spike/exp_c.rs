//! (c) An nft allow-list in the job's network namespace that the job cannot edit.
//!
//! The daemon makes a user and network namespace pair (`Holder`), enters it with
//! `nsenter --user --net` (it owns the user namespace, so it has every capability there) and
//! loads the table. The container joins the pair by path and runs the job with no
//! capabilities, so the job has no `CAP_NET_ADMIN` over the namespace its own user
//! namespace owns.

use crate::bundle::Bundle;
use crate::host::{self, Sysctl, serial};
use crate::report::row;
use crate::scenario::{
    ALLOW_LIST, Holder, assert_job_cannot_tamper, counter, enter_as_owner, pair_spec, record_suite,
    table_of,
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

/// Asserts that the job's attempts to flush, delete from, add to the table and take `lo`
/// down all fail, and that the table the daemon loaded is whole afterwards (every rule is
/// still there, read back by the daemon).
#[test]
#[ignore = "needs crun, nsenter, nft, passwordless sudo: see the egress-spike workflow"]
fn c1_the_job_cannot_edit_the_allow_list_in_its_netns() {
    let _serial = serial();
    if !host::require(&TOOLS) {
        return;
    }
    let _restrict = Sysctl::set("kernel.apparmor_restrict_unprivileged_userns", "0");
    let holder = Holder::start();
    let loaded = enter_as_owner(holder.pid(), "nft", &["-f", "-"], ALLOW_LIST);
    assert!(
        loaded.status.success(),
        "the daemon could not load the table: {}",
        host::text(&loaded)
    );
    let up = enter_as_owner(holder.pid(), "ip", &["link", "set", "lo", "up"], "");
    assert!(up.status.success(), "{}", host::text(&up));
    // A route for `ip route flush all` to find: without one the flush fails with "FIB table
    // does not exist", which says nothing about permission.
    let route = enter_as_owner(
        holder.pid(),
        "ip",
        &["route", "add", "203.0.113.0/24", "dev", "lo"],
        "",
    );
    let outcome = Bundle::new(&pair_spec("c1", "suite", &holder)).run();
    assert!(route.status.success(), "{}", host::text(&route));
    let routes = host::text(&enter_as_owner(holder.pid(), "ip", &["route", "show"], ""));
    let after = table_of(&holder);
    record_suite("c1 allow-list in the pair's netns, job tampers", &outcome);
    assert_eq!(outcome.exit, Some(0), "{outcome:?}");
    assert_job_cannot_tamper(&outcome);
    for comment in ["ipv6", "metadata", "allow-a", "allow-b", "default-reject"] {
        assert!(
            counter(&after, comment).is_some(),
            "rule {comment} is gone after the job ran:\n{after}"
        );
    }
    assert!(
        routes.contains("203.0.113.0/24"),
        "the job removed the route: {routes}"
    );
    row(
        "c1 allow-list in the pair's netns, job tampers",
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
    let holder = Holder::start();
    let loaded = enter_as_owner(holder.pid(), "nft", &["-f", "-"], ALLOW_LIST);
    assert!(loaded.status.success(), "{}", host::text(&loaded));
    let mut spec = pair_spec("c2", "suite", &holder);
    spec.uid = 0;
    spec.net_admin = true;
    let outcome = Bundle::new(&spec).run();
    let after = enter_as_owner(
        holder.pid(),
        "nft",
        &["list", "table", "inet", "js_egress"],
        "",
    );
    record_suite("c2 control: job holds CAP_NET_ADMIN", &outcome);
    let succeeded: u32 = outcome.get("tamper.succeeded").parse().unwrap_or(0);
    assert!(succeeded >= 2, "{:?}", outcome.probe());
    assert!(
        !after.status.success(),
        "the table survived a job that holds CAP_NET_ADMIN:\n{}",
        host::text(&after)
    );
}

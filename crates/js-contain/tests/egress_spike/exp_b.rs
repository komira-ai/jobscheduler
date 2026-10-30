//! (b) The container joining a network namespace made beforehand, by path.

use crate::bundle::{Bundle, Join, Spec};
use crate::host::{self, Sysctl, serial};
use crate::report::row;
use crate::scenario::Holder;

const TOOLS: [&str; 6] = ["crun", "newuidmap", "newgidmap", "sudo", "sysctl", "ip"];

/// Removes a named network namespace when dropped.
struct NamedNetns(&'static str);

impl Drop for NamedNetns {
    fn drop(&mut self) {
        let _ = host::sudo("ip", &["netns", "del", self.0]);
    }
}

/// Asserts that a rootless `crun` cannot join a network namespace made by root with
/// `ip netns add` (owned by the initial user namespace): the container is never created.
/// The user namespace restriction is off, so the refusal is the kernel's ownership rule.
#[test]
#[ignore = "needs crun, newuidmap, passwordless sudo: see the egress-spike workflow"]
fn b1_a_namespace_owned_by_the_initial_userns_cannot_be_joined() {
    let _serial = serial();
    if !host::require(&TOOLS) {
        return;
    }
    let _restrict = Sysctl::set("kernel.apparmor_restrict_unprivileged_userns", "0");
    let _ = host::sudo("ip", &["netns", "del", "js-spike-b1"]);
    host::sudo_must("ip", &["netns", "add", "js-spike-b1"]);
    let _guard = NamedNetns("js-spike-b1");
    let mut spec = Spec::new("b1", "id");
    spec.net = Join::Path("/run/netns/js-spike-b1".to_owned());
    let outcome = Bundle::new(&spec).run();
    row(
        "b1 join root-made netns by path",
        "container starts",
        outcome.get("uid") != "-",
    );
    row(
        "b1 join root-made netns by path",
        "crun exit",
        format!("{:?}", outcome.exit),
    );
    row(
        "b1 join root-made netns by path",
        "error",
        outcome.first_error(),
    );
    assert_ne!(outcome.exit, Some(0), "{outcome:?}");
    assert_eq!(outcome.get("uid"), "-", "the job ran: {outcome:?}");
}

/// Asserts that a network namespace owned by a user namespace the daemon itself made cannot
/// be joined either while the container makes a user namespace of its own (the container's
/// namespace is a sibling, not a descendant, of the owner), and records the error.
#[test]
#[ignore = "needs crun, newuidmap, passwordless sudo: see the egress-spike workflow"]
fn b2_a_daemon_owned_netns_cannot_be_joined_from_a_new_userns() {
    let _serial = serial();
    if !host::require(&TOOLS) {
        return;
    }
    let _restrict = Sysctl::set("kernel.apparmor_restrict_unprivileged_userns", "0");
    let holder = Holder::start();
    let mut spec = Spec::new("b2", "id");
    spec.net = Join::Path(holder.ns("net"));
    let outcome = Bundle::new(&spec).run();
    row(
        "b2 join daemon-made netns, new userns",
        "container starts",
        outcome.get("uid") != "-",
    );
    row(
        "b2 join daemon-made netns, new userns",
        "error",
        outcome.first_error(),
    );
    assert_ne!(outcome.exit, Some(0), "{outcome:?}");
    assert_eq!(outcome.get("uid"), "-", "the job ran: {outcome:?}");
}

/// Asserts that the daemon, with no capability, cannot open the namespaces of a container
/// that `crun create` made (the container's init is not dumpable, and the daemon is not
/// `CAP_SYS_PTRACE` in the initial user namespace), so it can neither `nsenter` nor point
/// `pasta` at them. This is why the later tests use a namespace pair the daemon made itself.
#[test]
#[ignore = "needs crun, newuidmap, passwordless sudo: see the egress-spike workflow"]
fn b3_the_daemon_cannot_enter_the_namespaces_of_a_created_container() {
    let _serial = serial();
    if !host::require(&TOOLS) || !host::require(&["nsenter", "true"]) {
        return;
    }
    let _restrict = Sysctl::set("kernel.apparmor_restrict_unprivileged_userns", "0");
    let created = Bundle::new(&Spec::new("b3", "id"))
        .create()
        .unwrap_or_else(|o| panic!("create: {o:?}"));
    let label = "b3 daemon enters a crun-created container's namespaces";
    let user = crate::scenario::enter_as_owner(created.pid, "true", &[], "");
    row(
        label,
        "nsenter --user --net as the daemon",
        host::text(&user).trim(),
    );
    let pid = created.pid.to_string();
    let net_only = host::run_timeout(
        {
            let mut cmd = std::process::Command::new(host::bin("nsenter"));
            cmd.args(["--target", pid.as_str(), "--net", "--", &host::bin("true")]);
            cmd
        },
        std::time::Duration::from_secs(20),
    );
    row(
        label,
        "nsenter --net as the daemon",
        host::text(&net_only).trim(),
    );
    assert!(!user.status.success());
    assert!(!net_only.status.success());
}

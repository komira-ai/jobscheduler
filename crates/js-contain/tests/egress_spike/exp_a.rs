//! (a) The build command's own `unshare(CLONE_NEWUSER|CLONE_NEWNET)` inside a rootless crun
//! container, with `kernel.apparmor_restrict_unprivileged_userns` off and on, and with an
//! AppArmor profile that grants `userns` to the job.

use crate::bundle::{Outcome, Spec};
use crate::host::{self, JOB_PROFILE, JOB_PROFILE_NAME, Profile, Sysctl, endpoints, serial};
use crate::report::row;

const RESTRICT: &str = "kernel.apparmor_restrict_unprivileged_userns";
const TOOLS: [&str; 6] = [
    "crun",
    "newuidmap",
    "newgidmap",
    "sudo",
    "sysctl",
    "apparmor_parser",
];

/// The container's start and what the nested namespace gave the build command.
struct Case {
    started: Outcome,
    inner: Outcome,
}

impl Case {
    /// Whether the build command got a network namespace of its own with loopback.
    fn works(&self) -> bool {
        self.inner.get("steps") == "all ok"
            && self.inner.get("inner.loopback") == "ok"
            && self.inner.get("inner.ifaces") == "lo"
    }
}

fn run_case(label: &str, profile: Option<&str>) -> Case {
    let mut start = Spec::new("a-id", "id");
    start.apparmor = profile.map(str::to_owned);
    let started = crate::bundle::Bundle::new(&start).run();
    let mut inner = Spec::new("a-inner", "userns-netns");
    inner.apparmor = profile.map(str::to_owned);
    inner.env.push((
        "JS_EGRESS_TARGETS".to_owned(),
        endpoints::ALLOWED_A.to_owned(),
    ));
    let inner = crate::bundle::Bundle::new(&inner).run();
    row(label, "container starts", started.get("uid") != "-");
    row(label, "container error", started.first_error());
    row(label, "job caps (CapEff)", started.get("cap_eff"));
    row(label, "job no_new_privs", started.get("no_new_privs"));
    row(label, "inner unshare steps", inner.get("steps"));
    row(label, "inner interfaces", inner.get("inner.ifaces"));
    row(label, "inner loopback", inner.get("inner.loopback"));
    row(
        label,
        "inner connect outward",
        inner.get(&format!("inner.connect.{}", endpoints::ALLOWED_A)),
    );
    row(label, "inner error", inner.first_error());
    Case { started, inner }
}

fn record_host() {
    row(
        "a-host",
        "kernel",
        std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .unwrap_or_default()
            .trim(),
    );
    row("a-host", RESTRICT, host::sysctl_value(RESTRICT));
    row(
        "a-host",
        "user.max_user_namespaces",
        host::sysctl_value("user.max_user_namespaces"),
    );
    row(
        "a-host",
        "crun profile shipped (/etc/apparmor.d/crun)",
        std::path::Path::new("/etc/apparmor.d/crun").exists(),
    );
}

/// Asserts that with the restriction off the build command gets its namespace. This is the
/// reference the other two cases are read against: if it fails, nothing else here says
/// anything about AppArmor.
#[test]
#[ignore = "needs crun, newuidmap, passwordless sudo: see the egress-spike workflow"]
fn a1_inner_unshare_works_with_the_restriction_off() {
    let _serial = serial();
    if !host::require(&TOOLS) {
        return;
    }
    record_host();
    let _restrict = Sysctl::set(RESTRICT, "0");
    let case = run_case("a1 restriction off, no job profile", None);
    assert!(
        case.started.get("uid") != "-",
        "container did not start: {:?}",
        case.started
    );
    assert_eq!(case.started.get("cap_eff"), "0000000000000000");
    assert!(case.works(), "{:?}", case.inner);
}

/// Asserts that with the restriction on and no grant the build command's namespace is
/// refused, and records at which step.
#[test]
#[ignore = "needs crun, newuidmap, passwordless sudo: see the egress-spike workflow"]
fn a2_inner_unshare_is_blocked_by_the_restriction() {
    let _serial = serial();
    if !host::require(&TOOLS) {
        return;
    }
    let _restrict = Sysctl::set(RESTRICT, "1");
    let case = run_case("a2 restriction on, no job profile", None);
    assert!(
        !case.works(),
        "the restriction did not block the inner namespace: {:?}",
        case.inner
    );
}

/// Asserts that with the restriction on, a job profile that allows `userns` restores the
/// build command's namespace. Needs `crun` itself to start under the restriction.
#[test]
#[ignore = "needs crun, newuidmap, passwordless sudo: see the egress-spike workflow"]
fn a3_job_profile_with_userns_restores_the_inner_unshare() {
    let _serial = serial();
    if !host::require(&TOOLS) {
        return;
    }
    let _restrict = Sysctl::set(RESTRICT, "1");
    let profile = Profile::load(JOB_PROFILE_NAME, JOB_PROFILE);
    let case = run_case(
        "a3 restriction on, job profile with userns",
        Some(profile.name()),
    );
    assert!(
        case.started.get("uid") != "-",
        "container did not start: {:?}",
        case.started
    );
    assert!(case.works(), "{:?}", case.inner);
}

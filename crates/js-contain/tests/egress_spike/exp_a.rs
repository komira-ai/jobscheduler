//! (a) The build command's own `unshare(CLONE_NEWUSER|CLONE_NEWNET)` inside a rootless crun
//! container, with `kernel.apparmor_restrict_unprivileged_userns` off and on, with the
//! distribution's `crun` AppArmor profile and without it, and with a job profile.

use std::path::PathBuf;

use crate::bundle::{Bundle, Outcome, Spec, crun_copy};
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
    fn container_started(&self) -> bool {
        self.started.get("uid") != "-"
    }

    /// Whether the build command got a network namespace of its own with loopback.
    fn works(&self) -> bool {
        self.inner.get("steps") == "all ok"
            && self.inner.get("inner.loopback") == "ok"
            && self.inner.get("inner.ifaces") == "lo"
    }
}

fn run_case(label: &str, profile: Option<&str>, crun: Option<PathBuf>) -> Case {
    row(label, "sysctl in effect", host::sysctl_value(RESTRICT));
    let mut start = Spec::new("a-id", "id");
    start.apparmor = profile.map(str::to_owned);
    start.crun.clone_from(&crun);
    let started = Bundle::new(&start).run();
    let mut inner = Spec::new("a-inner", "userns-netns");
    inner.apparmor = profile.map(str::to_owned);
    inner.crun = crun;
    inner.env.push((
        "JS_EGRESS_TARGETS".to_owned(),
        endpoints::ALLOWED_A.to_owned(),
    ));
    let inner = Bundle::new(&inner).run();
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
    let profile =
        std::fs::read_to_string("/etc/apparmor.d/crun").unwrap_or_else(|_| "absent".to_owned());
    row(
        "a-host",
        "/etc/apparmor.d/crun",
        profile
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join(" "),
    );
}

/// The profile the test attaches to its copy of `crun`: what the distribution's profile
/// gives its own binary.
fn copy_profile(path: &std::path::Path) -> String {
    format!(
        "abi <abi/4.0>,\nprofile js_spike_crun {} flags=(unconfined) {{\n  userns,\n}}\n",
        path.display()
    )
}

/// Asserts that with the restriction off the build command gets its namespace. This is the
/// reference the other cases are read against.
#[test]
#[ignore = "needs crun, newuidmap, passwordless sudo: see the egress-spike workflow"]
fn a1_inner_unshare_works_with_the_restriction_off() {
    let _serial = serial();
    if !host::require(&TOOLS) {
        return;
    }
    record_host();
    let _restrict = Sysctl::set(RESTRICT, "0");
    let case = run_case("a1 restriction off", None, None);
    assert!(case.container_started(), "{:?}", case.started);
    assert_eq!(case.started.get("cap_eff"), "0000000000000000");
    assert!(case.works(), "{:?}", case.inner);
}

/// Asserts that with the restriction on and the distribution's `crun` (whose AppArmor
/// profile is attached to `/usr/bin/crun` and allows `userns`) the build command still
/// gets its namespace: the container and its children run under that profile.
#[test]
#[ignore = "needs crun, newuidmap, passwordless sudo: see the egress-spike workflow"]
fn a2_inner_unshare_works_with_the_restriction_on_under_the_shipped_crun_profile() {
    let _serial = serial();
    if !host::require(&TOOLS) {
        return;
    }
    let _restrict = Sysctl::set(RESTRICT, "1");
    let case = run_case("a2 restriction on, shipped crun profile", None, None);
    assert!(case.container_started(), "{:?}", case.started);
    assert!(case.works(), "{:?}", case.inner);
}

/// Asserts that a job profile that allows `userns`, set as the process's AppArmor profile
/// in `config.json`, gives the same result.
#[test]
#[ignore = "needs crun, newuidmap, passwordless sudo: see the egress-spike workflow"]
fn a3_job_profile_with_userns_works_with_the_restriction_on() {
    let _serial = serial();
    if !host::require(&TOOLS) {
        return;
    }
    let _restrict = Sysctl::set(RESTRICT, "1");
    let profile = Profile::load(JOB_PROFILE_NAME, JOB_PROFILE);
    let case = run_case(
        "a3 restriction on, job profile with userns",
        Some(profile.name()),
        None,
    );
    assert!(case.container_started(), "{:?}", case.started);
    assert!(case.works(), "{:?}", case.inner);
}

/// Asserts that a `crun` at a path no profile is attached to (a binary installed outside
/// `/usr/bin`) cannot make the container's user namespace when the restriction is on.
#[test]
#[ignore = "needs crun, newuidmap, passwordless sudo: see the egress-spike workflow"]
fn a4_an_unprofiled_crun_is_blocked_by_the_restriction() {
    let _serial = serial();
    if !host::require(&TOOLS) {
        return;
    }
    let _restrict = Sysctl::set(RESTRICT, "1");
    let case = run_case(
        "a4 restriction on, crun at an unprofiled path",
        None,
        Some(crun_copy()),
    );
    assert!(
        !case.works(),
        "the restriction did not block: {:?}",
        case.inner
    );
}

/// Asserts that attaching a `userns` profile to that path, as root once at install time,
/// restores both the container and the build command's namespace.
#[test]
#[ignore = "needs crun, newuidmap, passwordless sudo: see the egress-spike workflow"]
fn a5_a_userns_profile_on_the_crun_path_fixes_it() {
    let _serial = serial();
    if !host::require(&TOOLS) {
        return;
    }
    let _restrict = Sysctl::set(RESTRICT, "1");
    let copy = crun_copy();
    let profile = Profile::load("js_spike_crun", &copy_profile(&copy));
    let case = run_case(
        "a5 restriction on, crun path profile with userns",
        None,
        Some(copy),
    );
    drop(profile);
    assert!(case.container_started(), "{:?}", case.started);
    assert!(case.works(), "{:?}", case.inner);
}

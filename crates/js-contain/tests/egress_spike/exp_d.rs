//! (d) Rootless `pasta` (passt) as the container's network, and whether an allow-list can be
//! enforced on it.
//!
//! The daemon makes a user and network namespace pair (`Holder`), points `pasta` at its pid
//! (pasta enters both namespaces, makes a tap device there and relays its traffic with
//! ordinary sockets of its own), switches IPv6 off and loads the nft table inside the
//! namespace; the container then joins the pair by path. `pasta` itself filters nothing: the
//! control test shows it relays to any host address, so the table is the whole allow-list.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::bundle::{Bundle, Outcome};
use crate::host::{self, JOB_PROFILE, JOB_PROFILE_NAME, Profile, Sysctl, serial};
use crate::report::row;
use crate::scenario::{
    ALLOW_LIST, Fixture, Holder, assert_allow_list_enforced, assert_ipv6_off,
    assert_job_cannot_tamper, connected, counter, enter_as_owner, pair_spec, reach, record_suite,
    table_of,
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

/// A running `pasta`, killed when dropped.
pub struct Pasta(Child);

impl Drop for Pasta {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// What a run gave.
struct Run {
    outcome: Outcome,
    table: String,
}

/// Starts `pasta` against the namespaces of `pid` and waits until the namespace has a tap
/// device and a default route.
fn attach_pasta(pid: u32, log: &std::path::Path) -> Pasta {
    let stderr = std::fs::File::create(log).expect("pasta log");
    let child = Command::new(host::bin("pasta"))
        .args([
            "--foreground",
            "--config-net",
            "-4",
            "-t",
            "none",
            "-u",
            "none",
            "-T",
            "none",
            "-U",
            "none",
            "--no-map-gw",
        ])
        .arg(pid.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .unwrap_or_else(|error| panic!("cannot start pasta: {error}"));
    let pasta = Pasta(child);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let dev = std::fs::read_to_string(format!("/proc/{pid}/net/dev")).unwrap_or_default();
        let route = std::fs::read_to_string(format!("/proc/{pid}/net/route")).unwrap_or_default();
        let tap = dev
            .lines()
            .filter_map(|l| l.split_once(':'))
            .any(|(name, _)| name.trim() != "lo");
        if tap
            && route
                .lines()
                .skip(1)
                .any(|l| l.split_whitespace().nth(1) == Some("00000000"))
        {
            return pasta;
        }
        assert!(
            Instant::now() < deadline,
            "pasta did not set up the namespace; its output: {}",
            std::fs::read_to_string(log).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// What the daemon does to a pair before the job: IPv6 off, pasta attached, and (when
/// `rules`) the table loaded. The returned guard keeps pasta alive.
pub fn prepare_pair(holder: &Holder, log: &std::path::Path, rules: bool) -> Pasta {
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
    assert!(
        off.status.success(),
        "daemon could not switch IPv6 off in the namespace: {}",
        host::text(&off)
    );
    let pasta = attach_pasta(holder.pid(), log);
    if rules {
        let loaded = enter_as_owner(holder.pid(), "nft", &["-f", "-"], ALLOW_LIST);
        assert!(
            loaded.status.success(),
            "daemon could not load the table: {}",
            host::text(&loaded)
        );
    }
    pasta
}

fn run(label: &str, restricted: bool, job_profile: bool, rules: bool, seccomp: bool) -> Run {
    let _restrict = Sysctl::set(RESTRICT, if restricted { "1" } else { "0" });
    let profile = job_profile.then(|| Profile::load(JOB_PROFILE_NAME, JOB_PROFILE));
    let _fixture = Fixture::new();
    let holder = Holder::start();
    let mut spec = pair_spec("d", "suite", &holder);
    spec.seccomp = seccomp;
    if seccomp {
        row(
            label,
            "seccomp entries applied",
            crate::seccomp::section().1,
        );
    }
    spec.apparmor = profile.as_ref().map(|p| p.name().to_owned());
    let bundle = Bundle::new(&spec);
    let log = bundle.dir.join("pasta.log");
    let _pasta = prepare_pair(&holder, &log, rules);
    let outcome = bundle.run();
    let table = table_of(&holder);
    record_suite(label, &outcome);
    row(
        label,
        "pasta log",
        std::fs::read_to_string(&log).unwrap_or_default().trim(),
    );
    assert_eq!(outcome.exit, Some(0), "{outcome:?}");
    Run { outcome, table }
}

fn assert_whole(run: &Run) {
    assert_allow_list_enforced(&run.outcome, "before");
    assert_allow_list_enforced(&run.outcome, "after");
    assert_job_cannot_tamper(&run.outcome);
    assert_ipv6_off(&run.outcome);
    assert_eq!(run.outcome.get("net.default_route"), "yes");
    for comment in ["allow-a", "allow-b"] {
        assert!(
            counter(&run.table, comment).unwrap_or(0) >= 1,
            "{comment} never matched:\n{}",
            run.table
        );
    }
    for comment in ["metadata", "default-reject"] {
        assert!(
            counter(&run.table, comment).unwrap_or(0) >= 1,
            "{comment} never matched, so a refusal was not the rule's:\n{}",
            run.table
        );
    }
}

/// Asserts the allow-list on pasta, with no AppArmor restriction: the allowed endpoints
/// answer, the other port, the other host and the metadata address are refused by the
/// rules (their counters moved), the job's tamper attempts all fail, and IPv6 is off.
#[test]
#[ignore = "needs crun, pasta, nft, passwordless sudo: see the egress-spike workflow"]
fn d1_pasta_with_a_namespace_allow_list() {
    let _serial = serial();
    if !host::require(&TOOLS) {
        return;
    }
    assert_whole(&run(
        "d1 pasta + nft in the netns",
        false,
        false,
        true,
        false,
    ));
}

/// Asserts the control: with no table, pasta relays to every host address, the metadata
/// address included. A refusal in the test above is therefore the table's.
#[test]
#[ignore = "needs crun, pasta, nft, passwordless sudo: see the egress-spike workflow"]
fn d2_pasta_alone_filters_nothing() {
    let _serial = serial();
    if !host::require(&TOOLS) {
        return;
    }
    let run = run("d2 control: pasta, no table", false, false, false, false);
    let seen = reach(&run.outcome, "before");
    assert!(connected(&seen["allowed-a"]), "{seen:?}");
    assert!(
        connected(&seen["other-host"]),
        "pasta did not relay to another host: {seen:?}"
    );
    assert!(connected(&seen["other-port"]), "{seen:?}");
}

/// Asserts the same as the first test on a node that restricts unprivileged user namespaces,
/// with the job's AppArmor profile granting `userns` (the configuration the farm's nodes
/// would run).
#[test]
#[ignore = "needs crun, pasta, nft, passwordless sudo: see the egress-spike workflow"]
fn d3_pasta_with_a_namespace_allow_list_when_userns_is_restricted() {
    let _serial = serial();
    if !host::require(&TOOLS) {
        return;
    }
    assert_whole(&run(
        "d3 pasta + nft, userns restricted + job profile",
        true,
        true,
        true,
        false,
    ));
}

/// Asserts the first test with the container runner's real seccomp profile in the bundle:
/// crun's join of the pair, pasta's relay and the job's network calls all still work.
#[test]
#[ignore = "needs crun, pasta, nft, passwordless sudo: see the egress-spike workflow"]
fn d4_pasta_with_a_namespace_allow_list_under_the_real_seccomp_profile() {
    let _serial = serial();
    if !host::require(&TOOLS) {
        return;
    }
    assert_whole(&run(
        "d4 pasta + nft, real seccomp",
        false,
        false,
        true,
        true,
    ));
}

/// The same with the restriction on and the distribution's crun profile (no job profile).
#[test]
#[ignore = "needs crun, pasta, nft, passwordless sudo: see the egress-spike workflow"]
fn d5_pasta_with_a_namespace_allow_list_under_seccomp_and_the_restriction() {
    let _serial = serial();
    if !host::require(&TOOLS) {
        return;
    }
    assert_whole(&run(
        "d5 pasta + nft, real seccomp, restriction on, shipped crun profile",
        true,
        false,
        true,
        true,
    ));
}

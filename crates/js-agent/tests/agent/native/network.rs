//! The network of a native job against the agent's ceiling (`--native-network`): a job above
//! it is refused before anything is made, never run with less; the pf anchor is asked only
//! for a filtered (`on`, `allow-list`) job on an agent whose ceiling is `on`; declared ports
//! are refused. The runtime, the lifecycle and the pf check double are those of `pf_gate`,
//! over the real pool on a fake host and the fake launcher.

use std::fs;

use js_agent::lifecycle::Runtime;
use js_agent::native::{Ceiling, NativeRuntime};
use js_agent_wire::{HostPort, NetworkLevel, PortProtocol, PortRange, refusal};
use js_types::PortMap;

use super::lifecycle::Native;
use super::pf_gate::{Fake, Verdict, gated, job_at};
use super::{FakeAccounts, FakeLauncher, Rig, TERM, config, native, open_pool, pool_settings};
use crate::lifecycle::fake::{FakeRuntime, Log, ManualClock};
use crate::support::Scratch;

/// Asserts that a start was refused with `code` and made nothing: no account was lent and no
/// supervisor started, the run's scratch directory does not exist, the booking is intact and
/// the result is a start that failed with that code.
fn made_nothing(h: &Native, run_id: &str, code: &str) {
    h.made_no_run();
    let scratch = fs::canonicalize(h.dir.path("scratch")).unwrap();
    assert!(!scratch.join(run_id).exists(), "no scratch directory");
    assert_eq!(h.life.free().cpu_millis, 4000, "the booking is intact");
    let results = crate::lifecycle::results(&h.life);
    let result = results
        .iter()
        .find(|result| result.run_id == run_id)
        .expect("a result");
    assert_eq!(result.detail.as_deref(), Some(code));
}

/// One declared port.
fn one_port() -> Vec<PortMap> {
    vec![PortMap {
        host: 8080,
        container: 80,
    }]
}

/// Asserts that an `open` job on an `open` agent starts without the pf check being asked
/// once, with a check that would refuse if it were. Catches `check_native_egress` gating on
/// any network (`net != Off`) rather than on the filter the job needs.
#[test]
fn open_job_on_open_agent_never_asks_pf() {
    let check = Fake::new(Verdict::PfDisabled);
    let mut h = gated("net-open-open", Ceiling::Open, &check);
    assert_eq!(h.start(job_at("r1", NetworkLevel::Open)), ["started r1"]);
    assert_eq!(check.calls(), 0);
}

/// Asserts that on an `on` agent the anchor is checked before each `allow-list` (`on`) job and
/// never kept: two jobs, the anchor drops out between them; the first starts, the second is
/// refused `egress-unchecked`, and the check was asked twice. Catches a check cached from the
/// first job or skipped for the second.
#[test]
fn allow_list_job_on_on_agent_checks_pf_before_each_job() {
    let check = Fake::new(Verdict::Enforced);
    let mut h = gated("net-on-each", Ceiling::On, &check);
    assert_eq!(h.start(job_at("r1", NetworkLevel::On)), ["started r1"]);
    assert_eq!(check.calls(), 1);
    check.answer(Verdict::NotReferenced);
    assert_eq!(
        h.start(job_at("r2", NetworkLevel::On)),
        ["refused r2 egress-unchecked"]
    );
    assert_eq!(check.calls(), 2);
}

/// Asserts that an `allow-list` (`on`) job on an `open` agent is admitted, below the ceiling,
/// and starts without the anchor being asked: the filter is the operator's choice at the
/// ceiling `on` only, and an operator who chose `open` installed none, so there is nothing to
/// check. Catches an `open` agent that refuses every allow-listed job for want of an anchor
/// it was never told of.
#[test]
fn allow_list_job_on_open_agent_is_admitted_without_pf() {
    let check = Fake::new(Verdict::PfDisabled);
    let mut h = gated("net-open-on", Ceiling::Open, &check);
    assert_eq!(h.start(job_at("r1", NetworkLevel::On)), ["started r1"]);
    assert_eq!(check.calls(), 0);
}

/// Asserts that an `open` job on an `on` agent is refused `network-above-ceiling`, makes
/// nothing (no user, no scratch directory, the booking intact) and costs no pf check. Catches
/// the lowering this replaced (the job run behind the filter it did not ask for) and a
/// refusal that comes after the lease user was made.
#[test]
fn open_job_on_on_agent_is_refused_with_the_ceiling_code() {
    let check = Fake::new(Verdict::Enforced);
    let mut h = gated("net-on-open", Ceiling::On, &check);
    assert_eq!(
        h.start(job_at("r1", NetworkLevel::Open)),
        ["refused r1 network-above-ceiling"]
    );
    made_nothing(&h, "r1", refusal::NETWORK_ABOVE_CEILING);
    assert_eq!(check.calls(), 0);
}

/// Asserts, through the lifecycle, every pair of ceiling and asked level that is above the
/// ceiling: each is refused `network-above-ceiling` and makes nothing; and every pair at or
/// below it starts. Catches a level let through above a ceiling the pure table does not see
/// (the runtime reading another field than the body's `network`) and a ceiling read as
/// another.
#[test]
fn every_level_above_the_ceiling_is_refused_through_the_lifecycle() {
    let levels = [
        (NetworkLevel::Off, Ceiling::Off),
        (NetworkLevel::Loopback, Ceiling::Loopback),
        (NetworkLevel::On, Ceiling::On),
        (NetworkLevel::Open, Ceiling::Open),
    ];
    for ceiling in Ceiling::ALL {
        for (n, (level, least)) in levels.into_iter().enumerate() {
            let check = Fake::new(Verdict::Enforced);
            let mut h = gated(&format!("net-all-{ceiling}-{n}"), ceiling, &check);
            let said = h.start(job_at("r1", level));
            match least <= ceiling {
                true => assert_eq!(said, ["started r1"], "{level:?} under {ceiling}"),
                false => {
                    assert_eq!(
                        said,
                        ["refused r1 network-above-ceiling"],
                        "{level:?} above {ceiling}"
                    );
                    made_nothing(&h, "r1", refusal::NETWORK_ABOVE_CEILING);
                }
            }
        }
    }
}

/// Asserts that a start that declares one port is refused `ports-not-supported` with the text
/// that says why, by the runtime's own check and through the lifecycle, before anything is
/// made (no account is lent, no supervisor is started). Catches a refusal moved after the user is
/// made, and a port that is ignored and the job run without it.
#[test]
fn ports_are_refused_on_macos() {
    let rig = Rig::new("net-ports-rt");
    let runtime = &rig.rt;
    let mut body = native("/bin/echo", &[]);
    body.ports = one_port();
    let refused = runtime.check_native(&body).expect_err("a port");
    assert_eq!(
        (refused.code, refused.why.as_str()),
        (
            refusal::PORTS_NOT_SUPPORTED,
            "declared ports are not supported on macOS yet"
        )
    );
    assert!(rig.launcher.launched().is_empty());

    let check = Fake::new(Verdict::Enforced);
    let mut h = gated("net-ports", Ceiling::Open, &check);
    let mut spec = job_at("r1", NetworkLevel::Open);
    spec.native.as_mut().unwrap().ports = one_port();
    assert_eq!(h.start(spec), ["refused r1 ports-not-supported"]);
    made_nothing(&h, "r1", refusal::PORTS_NOT_SUPPORTED);
}

/// Asserts that the same start with an empty list of ports passes the runtime's check and
/// starts. Catches a check that refuses on the presence of the field rather than on a port.
#[test]
fn no_ports_is_not_refused() {
    let rig = Rig::new("net-no-ports-rt");
    let runtime = &rig.rt;
    let body = native("/bin/echo", &[]);
    assert!(body.ports.is_empty());
    assert_eq!(runtime.check_native(&body).map_err(|why| why.code), Ok(()));

    let check = Fake::new(Verdict::Enforced);
    let mut h = gated("net-no-ports", Ceiling::Open, &check);
    assert_eq!(h.start(job_at("r1", NetworkLevel::Open)), ["started r1"]);
}

/// Asserts that a level above the ceiling is refused before the declared ports are looked
/// at, as on Linux: an `open` job with a port on an `off` agent is `network-above-ceiling`.
/// Catches the two checks in the other order, which would tell a server the host could run
/// the job once it dropped its ports.
#[test]
fn the_ceiling_is_checked_before_the_ports() {
    let check = Fake::new(Verdict::Enforced);
    let mut h = gated("net-order", Ceiling::Off, &check);
    let mut spec = job_at("r1", NetworkLevel::Open);
    spec.native.as_mut().unwrap().ports = one_port();
    assert_eq!(h.start(spec), ["refused r1 network-above-ceiling"]);
}

/// Asserts that a runtime opens (over a pool it is given) with a ceiling of `open` and no pf
/// check, and still refuses to open with `on` and none, over a pool or as an agent's `open`
/// does (which refuses before it touches the real pool). Catches `open` demanding the
/// anchor's check at start, and an `on` that lost its gate.
#[test]
fn only_a_ceiling_of_on_needs_the_check_to_open() {
    let dir = Scratch::new("net-open-gate");
    let settings = pool_settings(&dir, 1);
    let accounts = FakeAccounts::new();
    let pool = open_pool(&dir, &settings, &accounts);
    let launcher = FakeLauncher::new();
    let mut cfg = config(&dir, &settings);
    cfg.network = Ceiling::Open;
    let over = |cfg| NativeRuntime::over(Some(pool.clone()), launcher.clone(), cfg, TERM);
    assert!(over(cfg.clone()).is_ok());
    cfg.network = Ceiling::On;
    assert!(over(cfg.clone()).is_err());
    assert!(NativeRuntime::open(launcher.clone(), cfg, TERM).is_err());
}

/// Asserts that a runtime that does not say how it limits a native job's network refuses an
/// `open` job as it refuses an `on` one, and passes `off` and `loopback`. Catches a default
/// that runs an `open` job unlimited because it predates the value.
#[test]
fn a_runtime_with_nothing_to_check_refuses_an_open_job() {
    let runtime = FakeRuntime::new(&Log::default(), &ManualClock::default());
    for (level, refused) in [
        (NetworkLevel::Open, true),
        (NetworkLevel::On, true),
        (NetworkLevel::Loopback, false),
        (NetworkLevel::Off, false),
    ] {
        let body = job_at("r1", level).native.unwrap();
        assert_eq!(
            runtime.check_native_egress(&body).is_err(),
            refused,
            "{level:?}"
        );
    }
}

/// The network lines of the profile the supervisor of `rig` was last asked to apply.
fn launched_network(rig: &Rig) -> Vec<String> {
    let profile = rig.launcher.last().profile;
    let lines = profile.lines().filter(|line| line.contains("network"));
    lines.map(str::to_owned).collect()
}

/// Asserts that the native runtime offers the host ports `--host-port` allows (none by
/// default, so the lifecycle refuses every start that names one `input-not-allowed`), and that
/// a start naming two of them under the ceiling `off` (a port at its own number and one with no
/// job port) has its supervisor apply a profile whose network is the `off` rules and a connect
/// to each of the two ports of the host's loopback, lowest first, and nothing else of
/// loopback; a start that names none under the same ceiling gets the `off` rules alone.
/// Catches a Mac agent that offers no host port, one that starts the job without the connect
/// it named, and one that opens all of loopback for it.
#[test]
fn a_start_naming_host_ports_reaches_those_ports_alone() {
    let mut bare = Rig::with("net-host-port-none", |config| config.network = Ceiling::Off);
    assert_eq!(bare.rt.host_ports(), []);
    let allowed = vec![
        PortRange::new(5432, 5432).unwrap(),
        PortRange::new(9000, 9010).unwrap(),
    ];
    let mut rig = Rig::with("net-host-port", |config| {
        config.network = Ceiling::Off;
        config.host_ports = allowed.clone();
    });
    assert_eq!(rig.rt.host_ports(), allowed);
    let port = |host_port, container_port| HostPort {
        host_port,
        container_port,
        protocol: PortProtocol::Tcp,
    };
    let mut spec = rig.spec("r1", native("/usr/bin/true", &[]));
    spec.host_ports = vec![port(9005, None), port(5432, Some(5432))];
    rig.rt.create_and_start(&spec).unwrap();
    assert_eq!(
        launched_network(&rig)[..4],
        [
            "(deny network*)",
            "(allow network-outbound (literal \"/private/var/run/syslog\"))",
            "(allow network* (remote ip \"localhost:5432\"))",
            "(allow network* (remote ip \"localhost:9005\"))",
        ]
    );
    let localhost = launched_network(&rig);
    let localhost = localhost.iter().filter(|line| line.contains("localhost"));
    assert_eq!(localhost.count(), 2);

    bare.start("r1", "/usr/bin/true", &[]);
    let network = launched_network(&bare);
    assert_eq!(
        network[..2],
        [
            "(deny network*)",
            "(allow network-outbound (literal \"/private/var/run/syslog\"))",
        ]
    );
    assert!(!network.iter().any(|line| line.contains("localhost")));
}

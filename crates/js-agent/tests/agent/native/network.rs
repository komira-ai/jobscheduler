//! The network of a native job against the agent's ceiling (`--native-network`): a job above
//! it is refused before anything is made, never run with less; the pf anchor is asked only
//! for a filtered (`on`, `allow-list`) job on an agent whose ceiling is `on`; declared ports
//! are refused. The runtime, the lifecycle and the pf check double are those of `pf_gate`,
//! over the fake helper.

use std::fs;

use js_agent::lifecycle::Runtime;
use js_agent::native::{Ceiling, NativeRuntime};
use js_agent_wire::{NetworkLevel, refusal};
use js_types::PortMap;

use super::fake_helper::FakeHelper;
use super::lifecycle::Native;
use super::pf_gate::{Fake, Verdict, gated, job_at};
use super::{TERM, config, native};
use crate::lifecycle::fake::{FakeRuntime, Log, ManualClock};
use crate::support::Scratch;

/// Asserts that a start was refused with `code` and made nothing: the helper heard of its
/// boot reset only, the run's scratch directory does not exist, the booking is intact and
/// the result is a start that failed with that code.
fn made_nothing(h: &Native, run_id: &str, code: &str) {
    assert_eq!(h.fake.verbs(), ["boot-reset"], "no user, no run");
    let scratch = fs::canonicalize(h.dir.path("scratch")).unwrap();
    assert!(!scratch.join(run_id).exists(), "no scratch directory");
    assert_eq!(h.life.free().cpu_millis, 4000, "the booking is intact");
    let results = h.life.results();
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
/// made (the helper hears of its boot reset only). Catches a refusal moved after the user is
/// made, and a port that is ignored and the job run without it.
#[test]
fn ports_are_refused_on_macos() {
    let dir = Scratch::new("net-ports-rt");
    let fake = FakeHelper::start(&dir);
    let runtime = NativeRuntime::open(fake.client(), config(&fake), TERM).unwrap();
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
    assert_eq!(fake.verbs(), ["boot-reset"]);

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
    let dir = Scratch::new("net-no-ports-rt");
    let fake = FakeHelper::start(&dir);
    let runtime = NativeRuntime::open(fake.client(), config(&fake), TERM).unwrap();
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

/// Asserts that a runtime opens with a ceiling of `open` and no pf check, and still refuses
/// to open with `on` and none. Catches `open` demanding the anchor's check at start, and an
/// `on` that lost its gate.
#[test]
fn only_a_ceiling_of_on_needs_the_check_to_open() {
    let dir = Scratch::new("net-open-gate");
    let fake = FakeHelper::start(&dir);
    let mut cfg = config(&fake);
    cfg.network = Ceiling::Open;
    assert!(NativeRuntime::open(fake.client(), cfg.clone(), TERM).is_ok());
    cfg.network = Ceiling::On;
    assert!(NativeRuntime::open(fake.client(), cfg, TERM).is_err());
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

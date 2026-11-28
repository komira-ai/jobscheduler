//! `Registry::judge_hello` and `Registry::take_hello`: the judgement of a hello, which records
//! nothing, and the taking of what it admitted, which cannot refuse. Each refusal the judgement
//! gives is the one `hello` gives for the same state.

mod support;

use js_agent_wire::{Hello, ProtocolRange};
use js_caps::{SCRATCH_DEFAULT_GIB, SCRATCH_QUOTA};
use std::time::Duration;

use js_agent_wire::Heartbeat;
use js_registry::{Credential, Refusal, Registry, SessionSource};
use js_types::ExecMode;
use js_types::ManualClock;
use js_types::cordon::Asker;
use support::*;

/// A fixture and the hello to put to it, and whether it comes certified.
struct Case {
    name: &'static str,
    fx: Fixture,
    hello: Hello,
    credential: Credential,
    certified: bool,
    code: &'static str,
}

fn case(name: &'static str, fx: Fixture, hello: Hello, code: &'static str) -> Case {
    Case {
        name,
        fx,
        hello,
        credential: cred("t"),
        certified: false,
        code,
    }
}

fn cases() -> Vec<Case> {
    let mut cases = Vec::new();
    let mut bad_id = hello("a");
    bad_id.host_id = "not a host".into();
    cases.push(case("bad host id", fixture(2), bad_id, "invalid-hello"));
    let mut protocol = hello("a");
    protocol.protocol = ProtocolRange { min: 90, max: 99 };
    cases.push(case(
        "protocol",
        fixture(2),
        protocol,
        "unsupported-protocol",
    ));
    let mut invalid = hello("a");
    invalid.agent_process = Some("not an id".into());
    cases.push(case("invalid", fixture(2), invalid, "invalid-hello"));
    let mut elsewhere = hello("a");
    elsewhere.pool = "elsewhere".into();
    cases.push(case("pool", fixture(2), elsewhere, "unknown-pool"));
    let listed = fixture(2);
    listed.registry.reserve([host("a")]);
    cases.push(case("listed", listed, hello("a"), "host-listed"));
    let bound = fixture(2);
    bound.join("a", "other");
    bound.pass_grace();
    cases.push(case("bound", bound, hello("a"), "host-bound"));
    let live = fixture(2);
    live.join("a", "t");
    cases.push(case("live", live, hello("a"), "host-live"));
    let contract = fixture_of(config(2).with_exec(ExecMode::Container));
    cases.push(case("contract", contract, hello("a"), "host-contract"));
    let mut linux = hello("a");
    linux.capabilities.insert("os".into(), "linux".into());
    cases.push(case("capability", fixture(2), linux, "shape-not-accepted"));
    let mut quota = hello("a");
    quota
        .capabilities
        .insert(SCRATCH_QUOTA.into(), "project".into());
    quota
        .capabilities
        .insert(SCRATCH_DEFAULT_GIB.into(), "1".into());
    cases.push(case(
        "quota without disk",
        fixture(2),
        quota,
        "shape-not-accepted",
    ));
    let full = fixture(1);
    full.join("b", "t");
    cases.push(case("full", full, hello("a"), "pool-full"));
    let mut certified = case("certified live", fixture(2), hello("a"), "host-live");
    certified.fx.join("a", "t");
    certified.certified = true;
    cases.push(certified);
    cases
}

fn put(case: &Case) -> Result<(), Refusal> {
    let registry = &case.fx.registry;
    match case.certified {
        true => registry.hello_as_certified_host(&case.hello, &case.credential),
        false => registry.hello(&case.hello, &case.credential),
    }
    .map(|_| ())
}

/// Catches a judgement that drifts from the hello it stands for: for every refusal a hello
/// can give before a secret is drawn, the judgement gives the same refusal, and the hello then
/// still gives it (the judgement changed nothing).
#[test]
fn judge_hello_refuses_exactly_what_hello_refuses() {
    for case in cases() {
        let judged = case
            .fx
            .registry
            .judge_hello(&case.hello, &case.credential, case.certified);
        let refusal = judged.map(drop).expect_err(case.name);
        assert_eq!(refusal.code(), case.code, "{}", case.name);
        assert_eq!(put(&case), Err(refusal), "{}", case.name);
    }
}

/// Catches a judgement that admits only by changing state, or that draws no secret: it lists
/// no host; the secret is drawn at the judgement (the first hello after it gets the second
/// secret, and taking the admission answers with the first); and a certified judgement sees the rebind a certified hello makes while an uncertified one
/// sees the bound host.
#[test]
fn judge_hello_draws_the_secret_and_records_nothing() {
    let fx = fixture(2);
    let admission = fx
        .registry
        .judge_hello(&hello("a"), &cred("t"), false)
        .unwrap();
    assert!(fx.registry.hosts().is_empty());
    let other = fx.hello(&hello("b"), &cred("t")).unwrap();
    assert_eq!(other.session, format!("{:064x}", 2));
    let welcome = fx.registry.take_hello(admission);
    assert_eq!(welcome.epoch, 1);
    assert_eq!(welcome.session, format!("{:064x}", 1));

    fx.pass_grace();
    let certificate = Credential::of_host_certificate(&host("a"));
    let refused = fx.registry.judge_hello(&hello("a"), &certificate, false);
    assert_eq!(refused.map(drop), Err(Refusal::HostBound));
    let rebinding = fx.registry.judge_hello(&hello("a"), &certificate, true);
    assert!(rebinding.is_ok());
    let bindings = fx.registry.bindings();
    assert_eq!(bindings[&host("a")].credential(), &cred("t"));
}

/// Catches a refused judgement that draws a secret anyway (a source that is not free to
/// draw, or one that counts): the first hello admitted after a refusal gets the first secret.
#[test]
fn a_refused_judgement_draws_no_secret() {
    let fx = fixture(2);
    let mut elsewhere = hello("a");
    elsewhere.pool = "elsewhere".into();
    let refused = fx.registry.judge_hello(&elsewhere, &cred("t"), false);
    assert_eq!(refused.map(drop), Err(Refusal::UnknownPool));
    let welcome = fx.hello(&hello("a"), &cred("t")).unwrap();
    assert_eq!(welcome.session, format!("{:064x}", 1));
}

/// Catches a take that judges again: a hello judged admissible is admitted although the pool
/// filled with another host in between, and the pool then holds both.
#[test]
fn a_judged_admission_is_taken_after_the_pool_filled() {
    let fx = fixture(1);
    let admission = fx
        .registry
        .judge_hello(&hello("a"), &cred("t"), false)
        .unwrap();
    fx.join("b", "t");
    assert_eq!(
        fx.registry
            .judge_hello(&hello("c"), &cred("t"), false)
            .map(drop),
        Err(Refusal::PoolFull)
    );
    let welcome = fx.registry.take_hello(admission);
    assert_eq!(welcome.epoch, 1);
    assert_eq!(welcome.pool, "builders");
    assert_eq!(fx.registry.hosts().len(), 2);
}

/// Catches a take that judges again or numbers the epoch from the judgement: a hello judged
/// admissible is admitted although the host said hello and went live in between, takes the
/// session over (the earlier one's secret no longer beats), and its epoch follows the record
/// as it stands.
#[test]
fn a_judged_admission_is_taken_after_the_host_went_live() {
    let fx = fixture(2);
    let admission = fx
        .registry
        .judge_hello(&hello("a"), &cred("t"), false)
        .unwrap();
    fx.join("a", "t");
    let welcome = fx.registry.take_hello(admission);
    assert_eq!(welcome.epoch, 2);
    let stale = fx.registry.heartbeat(&fx.heartbeat("a", 1), &cred("t"));
    assert!(stale.is_err(), "the replaced session no longer beats");
    let own = Heartbeat {
        session: welcome.session,
        ..heartbeat("a", 2)
    };
    assert!(fx.registry.heartbeat(&own, &cred("t")).is_ok());
}

/// Catches a take that records what the judgement saw of the host instead of what the registry
/// holds when it records: a cordon set between the judgement and the take is kept.
#[test]
fn a_judged_admission_keeps_what_was_set_on_the_host_in_between() {
    let fx = fixture(2);
    fx.join("a", "t");
    fx.pass_grace();
    let admission = fx
        .registry
        .judge_hello(&hello("a"), &cred("t"), false)
        .unwrap();
    let placed = fx
        .registry
        .cordon(&host("a"), Duration::from_secs(600), &Asker::Unnamed)
        .unwrap();
    assert!(placed.placed);
    let welcome = fx.registry.take_hello(admission);
    assert_eq!(welcome.epoch, 2);
    assert!(
        fx.registry
            .own_host(&host("a"))
            .unwrap()
            .cordon_until
            .is_some()
    );
}

/// A source that can make no bytes, as when the operating system's random source fails.
struct NoRandomness;

impl SessionSource for NoRandomness {
    fn fresh(&self) -> Option<[u8; 32]> {
        None
    }
}

/// Catches a session that is only found unavailable at the take: when no secret can be made
/// the judgement refuses `session-unavailable`, so nothing is left to refuse once a hello is
/// admitted, and a refusal that comes before it (a bad pool) is still the one given.
#[test]
fn session_unavailable_is_a_refusal_of_the_judgement() {
    let clock = ManualClock::default();
    let placer = sched_placer(100, &clock);
    let registry = Registry::with_sessions(config(4), clock, NoRandomness, placer);
    let refused = registry.judge_hello(&hello("a"), &cred("t"), false);
    assert_eq!(refused.map(drop), Err(Refusal::SessionUnavailable));
    let mut elsewhere = hello("a");
    elsewhere.pool = "elsewhere".into();
    let refused = registry.judge_hello(&elsewhere, &cred("t"), false);
    assert_eq!(refused.map(drop), Err(Refusal::UnknownPool));
    assert!(registry.hosts().is_empty());
}

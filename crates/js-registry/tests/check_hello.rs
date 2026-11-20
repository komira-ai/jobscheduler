//! `Registry::check_hello`: the checks of a hello without admitting it. Each refusal it gives
//! is the one `hello` gives for the same state, and it changes nothing.

mod support;

use js_agent_wire::{Hello, ProtocolRange};
use js_caps::{SCRATCH_DEFAULT_GIB, SCRATCH_QUOTA};
use js_registry::{Credential, Refusal};
use js_types::ExecMode;
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
    let mut small = hello("a");
    small.shape.cpu_millis = 1;
    cases.push(case("floor", fixture(2), small, "shape-not-accepted"));
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

/// Catches a check that drifts from the hello it stands for: for every refusal a hello can
/// give before a secret is drawn, the check gives the same refusal, and the hello then still
/// gives it (the check changed nothing).
#[test]
fn check_hello_refuses_exactly_what_hello_refuses() {
    for case in cases() {
        let checked = case
            .fx
            .registry
            .check_hello(&case.hello, &case.credential, case.certified);
        let refusal = checked.expect_err(case.name);
        assert_eq!(refusal.code(), case.code, "{}", case.name);
        assert_eq!(put(&case), Err(refusal), "{}", case.name);
    }
}

/// Catches a check that admits what hello admits only by changing state: it draws no secret
/// (the first hello after it gets the first secret), lists no host, and a certified check
/// sees the rebind a certified hello makes while an uncertified one sees the bound host.
#[test]
fn check_hello_admits_without_recording_anything() {
    let fx = fixture(2);
    assert_eq!(
        fx.registry.check_hello(&hello("a"), &cred("t"), false),
        Ok(())
    );
    assert!(fx.registry.hosts().is_empty());
    let welcome = fx.hello(&hello("a"), &cred("t")).unwrap();
    assert_eq!(welcome.epoch, 1);
    assert_eq!(welcome.session, format!("{:064x}", 1));
    fx.pass_grace();
    let certificate = Credential::of_host_certificate(&host("a"));
    let refused = fx.registry.check_hello(&hello("a"), &certificate, false);
    assert_eq!(refused, Err(Refusal::HostBound));
    assert_eq!(
        fx.registry.check_hello(&hello("a"), &certificate, true),
        Ok(())
    );
    let bindings = fx.registry.bindings();
    assert_eq!(bindings[&host("a")].credential(), &cred("t"));
}

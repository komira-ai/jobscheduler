//! Runs the conformance suite against the fake, the static pool, and each broken fake.

use std::collections::BTreeMap;

use js_conformance::{Check, Report, Subject, Verdict, run};
use js_pool_static::StaticPool;
use js_provider::{Call, Provider};
use js_provider_fake::{Defect, FakeProvider};
use js_types::{HostId, LeaseId, ProviderId};

/// One subject type for both providers, so the generic suite is exercised under one
/// instantiation that reaches every arm: the fake can be scripted, the pool cannot.
enum Under {
    Fake(FakeProvider),
    Pool(StaticPool),
}

impl Subject for Under {
    fn provider(&mut self) -> &mut dyn Provider {
        match self {
            Self::Fake(fake) => fake,
            Self::Pool(pool) => pool,
        }
    }

    fn booked(&self) -> BTreeMap<HostId, LeaseId> {
        match self {
            Self::Fake(fake) => fake.booked(),
            Self::Pool(pool) => pool.booked(),
        }
    }

    fn script_unknown(&mut self, call: Call, landed: bool) -> bool {
        match self {
            Self::Fake(fake) => {
                fake.script_unknown(call, landed);
                true
            }
            Self::Pool(_) => false,
        }
    }
}

const POOL_CONFIG: &str = "\
[[host]]
id = \"h1\"
gpu_class = \"conformance-gpu\"
[host.resources]
cpu_millis = 8000
memory_bytes = 1073741824
gpus = 1

[[host]]
id = \"h2\"
gpu_class = \"conformance-gpu\"
[host.resources]
cpu_millis = 8000
memory_bytes = 1073741824
gpus = 1
";

fn fake(defect: Option<Defect>) -> Under {
    let id = ProviderId::new("fake").unwrap();
    let provider = FakeProvider::new(id, js_conformance::standard_hosts());
    Under::Fake(match defect {
        Some(defect) => provider.with_defect(defect),
        None => provider,
    })
}

fn broken(defect: Defect) -> Report {
    run(|| fake(Some(defect)))
}

/// Asserts that the fake passes every check, with none skipped.
#[test]
fn the_fake_conforms() {
    let report = run(|| fake(None));
    assert!(
        report
            .results
            .iter()
            .all(|(_, verdict)| *verdict == Verdict::Passed),
        "{report:?}"
    );
    assert_eq!(report.results.len(), Check::ALL.len());
}

/// Asserts that the static pool passes every check except the one it cannot take
/// part in, which is reported as not applicable and not skipped silently.
#[test]
fn the_static_pool_conforms() {
    let make = || {
        let id = ProviderId::new("pool").unwrap();
        Under::Pool(StaticPool::from_toml(id, POOL_CONFIG).unwrap())
    };
    let report = run(make);
    assert!(report.failed().is_empty(), "{report:?}");
    assert_eq!(report.not_applicable(), vec![Check::UnknownDoesNotFree]);
}

/// Asserts that a fake that frees a booking on an Unknown observation fails exactly
/// the Unknown-does-not-free check.
#[test]
fn frees_on_unknown_is_caught() {
    let report = broken(Defect::FreesOnUnknownObserve);
    assert_eq!(report.failed(), vec![Check::UnknownDoesNotFree]);
    assert!(report.not_applicable().is_empty());
}

/// Asserts that a fake that books a host twice fails exactly the double-booking check.
#[test]
fn double_booking_is_caught() {
    assert_eq!(
        broken(Defect::DoubleBooks).failed(),
        vec![Check::NeverDoubleBooks]
    );
}

/// Asserts that a fake that books again on a repeated start fails exactly the
/// idempotence check.
#[test]
fn non_idempotent_start_is_caught() {
    assert_eq!(
        broken(Defect::RestartBooksAgain).failed(),
        vec![Check::StartIsIdempotent]
    );
}

/// Asserts that a fake that claims a stop it did not do fails exactly the
/// stop-then-absent check, which only the state hook can see.
#[test]
fn a_lying_stop_is_caught_by_the_state_hook() {
    let report = broken(Defect::StopKeepsBooking);
    assert_eq!(report.failed(), vec![Check::StopThenAbsent]);
    let (_, verdict) = &report.results[1];
    assert!(matches!(verdict, Verdict::Failed(text) if text.contains("expected")));
}

/// Asserts that each of the three start-contract rules has its own check, so a rule
/// without a check would be a missing entry in this list.
#[test]
fn every_start_contract_rule_has_a_check() {
    let rules = [
        Check::RefusesNonPositiveWallTime,
        Check::RefusesReservedLabel,
        Check::RefusesUnknownClass,
    ];
    for rule in rules {
        assert!(Check::ALL.contains(&rule));
    }
}

//! Runs the conformance suite against the fake, the static pool, and each broken fake.

use std::collections::BTreeMap;

use js_conformance::{Check, Report, Subject, Verdict, run};
use js_pool_static::StaticPool;
use js_provider::{
    Call, Flavour, HostCapacity, Observed, Provider, RefusalCode, StartAck, StartSpec, StopAck,
};
use js_provider_fake::{Defect, FakeProvider, Rank};
use js_types::{HostId, LeaseId, ProviderId, ResourceClass};

/// One subject type for both providers, so the generic suite is exercised under one
/// instantiation that reaches every arm: the fake can be scripted, the pool cannot.
enum Under {
    Fake(FakeProvider),
    Pool(StaticPool),
    /// A fake that cannot be made to list an unleasable class.
    Bare(FakeProvider),
    /// A fake whose capacity listing shows a second lease on every booked host.
    Phantom(Phantom),
}

/// A fake that lists a lease nobody started beside the lease of each booked host: the
/// listing and the booking table disagree, as a provider that books many leases per host
/// and keeps them out of one of the two would.
struct Phantom(FakeProvider);

impl Provider for Phantom {
    fn id(&self) -> ProviderId {
        self.0.id()
    }
    fn flavour(&self) -> Flavour {
        self.0.flavour()
    }
    fn classes(&self) -> Vec<ResourceClass> {
        self.0.classes()
    }
    fn start(&mut self, spec: StartSpec) -> StartAck {
        self.0.start(spec)
    }
    fn stop(&mut self, lease: &LeaseId) -> StopAck {
        self.0.stop(lease)
    }
    fn observe(&mut self, lease: &LeaseId) -> Observed {
        self.0.observe(lease)
    }
    fn capacity(&self) -> Vec<HostCapacity> {
        let mut listed = self.0.capacity();
        for host in &mut listed {
            if host.leased_to.is_some() {
                host.leases.push(LeaseId::new(9, 9));
            }
        }
        listed
    }
}

impl Subject for Under {
    fn provider(&mut self) -> &mut dyn Provider {
        match self {
            Self::Fake(fake) | Self::Bare(fake) | Self::Phantom(Phantom(fake)) => fake,
            Self::Pool(pool) => pool,
        }
    }

    fn booked(&self) -> BTreeMap<HostId, LeaseId> {
        match self {
            Self::Fake(fake) | Self::Bare(fake) | Self::Phantom(Phantom(fake)) => fake.booked(),
            Self::Pool(pool) => pool.booked(),
        }
    }

    fn script_unknown(&mut self, call: Call, landed: bool) -> bool {
        match self {
            Self::Fake(fake) | Self::Bare(fake) | Self::Phantom(Phantom(fake)) => {
                fake.script_unknown(call, landed);
                true
            }
            Self::Pool(_) => false,
        }
    }

    fn list_unleasable_class(&mut self, class: ResourceClass) -> bool {
        match self {
            Self::Fake(fake) => {
                let kept = std::mem::replace(fake, placeholder());
                *fake = kept.with_unleasable_class(class);
                true
            }
            Self::Pool(pool) => {
                let id = ProviderId::new("pool").unwrap();
                let kept = std::mem::replace(pool, StaticPool::from_toml(id, POOL_CONFIG).unwrap());
                *pool = kept.with_classes(vec![ResourceClass::gpu_host(), class]);
                true
            }
            Self::Bare(_) | Self::Phantom(_) => false,
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

fn placeholder() -> FakeProvider {
    let id = ProviderId::new("fake").unwrap();
    FakeProvider::new(id, js_conformance::standard_hosts())
}

fn fake(defect: Option<Defect>) -> Under {
    let provider = placeholder();
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

/// Asserts that the static pool passes every check except the two it cannot take
/// part in, which are reported as not applicable and not skipped silently.
#[test]
fn the_static_pool_conforms() {
    let make = || {
        let id = ProviderId::new("pool").unwrap();
        Under::Pool(StaticPool::from_toml(id, POOL_CONFIG).unwrap())
    };
    let report = run(make);
    assert!(report.failed().is_empty(), "{report:?}");
    assert_eq!(
        report.not_applicable(),
        vec![Check::UnknownDoesNotFree, Check::StartUnknownThenRetry]
    );
}

/// Asserts that a fake that frees a booking on an Unknown observation fails exactly
/// the Unknown-does-not-free check.
#[test]
fn frees_on_unknown_is_caught() {
    let report = broken(Defect::FreesOnUnknownObserve);
    assert_eq!(report.failed(), vec![Check::UnknownDoesNotFree]);
    assert!(report.not_applicable().is_empty());
}

/// Asserts that a fake that books a host twice fails the double-booking check, and the
/// start-Unknown check, whose second lease it also books onto a taken host.
#[test]
fn double_booking_is_caught() {
    assert_eq!(
        broken(Defect::DoubleBooks).failed(),
        vec![Check::NeverDoubleBooks, Check::StartUnknownThenRetry]
    );
}

/// Asserts that a listing which shows a lease the booking table does not hold fails the
/// double-booking check and nothing else: the check reads every lease of each host, not
/// only the first, so a second lease listed on a host is seen.
#[test]
fn a_second_lease_in_the_listing_alone_is_caught() {
    let report = run(|| Under::Phantom(Phantom(placeholder())));
    assert_eq!(report.failed(), vec![Check::NeverDoubleBooks]);
}

/// Asserts that a fake that books again on a repeated start fails the idempotence check,
/// and the start-Unknown check, whose retry it also books again.
#[test]
fn non_idempotent_start_is_caught() {
    assert_eq!(
        broken(Defect::RestartBooksAgain).failed(),
        vec![Check::StartIsIdempotent, Check::StartUnknownThenRetry]
    );
}

/// Asserts that a fake that claims a stop it did not do fails the stop-then-absent
/// check, which only the state hook can see, and the start-after-stop check, whose
/// table still holds the booking it was told was stopped.
#[test]
fn a_lying_stop_is_caught_by_the_state_hook() {
    let report = broken(Defect::StopKeepsBooking);
    assert_eq!(
        report.failed(),
        vec![Check::StopThenAbsent, Check::StartAfterStopIsRefused]
    );
    let (_, verdict) = &report.results[1];
    assert!(matches!(verdict, Verdict::Failed(text) if text.contains("expected")));
}

/// Asserts that a fake whose retry of a landed Unknown start books a second host fails
/// the start-Unknown check and nothing else, so that check reads the lookup by lease id
/// that design 5.6 requires before a retry.
#[test]
fn a_retry_after_an_unknown_start_that_landed_is_caught() {
    assert_eq!(
        broken(Defect::ForgetsUnknownStart).failed(),
        vec![Check::StartUnknownThenRetry]
    );
}

/// Asserts that a fake that observes Absent for a started lease fails the
/// start-then-present check, and the start-Unknown check that also reads the lookup.
#[test]
fn an_observation_that_misses_a_started_lease_is_caught() {
    assert_eq!(
        broken(Defect::ObservesAbsent).failed(),
        vec![Check::StartThenPresent, Check::StartUnknownThenRetry]
    );
}

/// Asserts that the refusal checks guard exactly the start-contract rules this slice
/// models: the codes the checks guard are the codes of `RefusalCode::START_CONTRACT`,
/// each once, so a rule without a check, or a check without a rule, fails here.
#[test]
fn the_refusal_checks_cover_the_start_contract() {
    let mut guarded: Vec<_> = Check::ALL.iter().filter_map(Check::guards).collect();
    let mut rules = RefusalCode::START_CONTRACT.to_vec();
    guarded.sort();
    rules.sort();
    assert_eq!(guarded, rules);
}

/// Asserts that every check is run once, in the order of `Check::ALL`.
#[test]
fn the_report_lists_each_check_once_in_order() {
    let report = run(|| fake(None));
    let ran: Vec<_> = report.results.iter().map(|(check, _)| *check).collect();
    assert_eq!(ran, Check::ALL.to_vec());
}

/// Asserts that a fake that books a host again for a start of a stopped lease fails
/// the start-after-stop check and nothing else: no other check starts a lease after
/// stopping it.
#[test]
fn a_resurrected_lease_is_caught() {
    assert_eq!(
        broken(Defect::ResurrectsStoppedLease).failed(),
        vec![Check::StartAfterStopIsRefused]
    );
}

/// Asserts that a conformant fake whose rank picks the highest host id first passes
/// every check, so no check names a host: where a lease lands is the provider's rank
/// (design 6.1), and the checks read it from the provider's booking table.
#[test]
fn a_provider_that_places_on_the_highest_host_first_conforms() {
    let make = || {
        let id = ProviderId::new("fake").unwrap();
        let hosts = js_conformance::standard_hosts();
        Under::Fake(FakeProvider::new(id, hosts).with_rank(Rank::HighestHostFirst))
    };
    let report = run(make);
    assert!(
        report
            .results
            .iter()
            .all(|(_, verdict)| *verdict == Verdict::Passed),
        "{report:?}"
    );
}

/// Asserts that the rank option is in force: the first lease lands on `h2`.
#[test]
fn the_highest_rank_option_places_on_h2_first() {
    let mut subject = {
        let id = ProviderId::new("fake").unwrap();
        let hosts = js_conformance::standard_hosts();
        FakeProvider::new(id, hosts).with_rank(Rank::HighestHostFirst)
    };
    subject.start(
        StartSpec::new(
            LeaseId::new(1, 1),
            js_conformance::standard_request(),
            &[ResourceClass::gpu_host()],
        )
        .unwrap(),
    );
    let placed: Vec<_> = subject
        .booked()
        .into_keys()
        .map(|h| h.as_str().to_string())
        .collect();
    assert_eq!(placed, vec!["h2".to_string()]);
}

/// A provider that refuses every start and books nothing.
struct Refuser;

impl Provider for Refuser {
    fn id(&self) -> ProviderId {
        ProviderId::new("refuser").unwrap()
    }
    fn flavour(&self) -> js_provider::Flavour {
        js_provider::Flavour::Delegated
    }
    fn classes(&self) -> Vec<ResourceClass> {
        vec![ResourceClass::gpu_host()]
    }
    fn start(&mut self, _: StartSpec) -> StartAck {
        StartAck::Refused(RefusalCode::NoCapacity)
    }
    fn stop(&mut self, _: &LeaseId) -> StopAck {
        StopAck::Stopped {
            read_back: Observed::Absent,
        }
    }
    fn observe(&mut self, _: &LeaseId) -> Observed {
        Observed::Absent
    }
    fn capacity(&self) -> Vec<js_provider::HostCapacity> {
        Vec::new()
    }
}

impl Subject for Refuser {
    fn provider(&mut self) -> &mut dyn Provider {
        self
    }
    fn booked(&self) -> BTreeMap<HostId, LeaseId> {
        BTreeMap::new()
    }
    fn script_unknown(&mut self, _: Call, _: bool) -> bool {
        true
    }
    fn list_unleasable_class(&mut self, _: ResourceClass) -> bool {
        true
    }
}

/// Asserts that a provider that refuses every start (and so never ends a lease with
/// `LeaseEnded`) fails each check that expects a
/// lease to start, so a provider booking nothing cannot pass by agreeing with an empty
/// table.
#[test]
fn a_provider_that_never_starts_anything_is_caught() {
    let report = run(|| Refuser);
    assert_eq!(
        report.failed(),
        vec![
            Check::StartThenPresent,
            Check::UnknownDoesNotFree,
            Check::StartIsIdempotent,
            Check::NeverDoubleBooks,
            Check::StartUnknownThenRetry,
            Check::StartAfterStopIsRefused,
            Check::NoFitIsRefused,
            Check::UnsupportedClassIsRefused,
        ]
    );
}

/// Asserts that a fake that books a host the request does not fit on fails the no-fit
/// check and nothing else: every other check asks for a request that fits.
#[test]
fn a_provider_that_ignores_the_shape_is_caught() {
    assert_eq!(
        broken(Defect::IgnoresShape).failed(),
        vec![Check::NoFitIsRefused]
    );
}

/// Asserts that a fake that leases a class it lists as unleasable fails the
/// unsupported-class check and nothing else: no other check lists such a class.
#[test]
fn leasing_an_unsupported_class_is_caught() {
    assert_eq!(
        broken(Defect::LeasesUnsupportedClass).failed(),
        vec![Check::UnsupportedClassIsRefused]
    );
}

/// Asserts that a subject that cannot list an unleasable class is reported as not
/// applicable for that check and passes every other.
#[test]
fn a_subject_without_an_unleasable_class_is_not_applicable() {
    let report = run(|| Under::Bare(placeholder()));
    assert!(report.failed().is_empty(), "{report:?}");
    assert_eq!(
        report.not_applicable(),
        vec![Check::UnsupportedClassIsRefused]
    );
}

//! The Compute Engine provider over the in-memory project against the provider contract's
//! conformance suite, and its own answers to the calls the suite leaves to each provider:
//! its name, what it offers, what it counts and that it sends no notices.

use std::sync::Arc;
use std::time::Duration;

use js_elastic::conformance::{FOREIGN_ACQUIRE, OTHER_SCHEDULER, Rig as Conformance, run_all};
use js_elastic::{
    AcquireId, AcquireRequest, Acquired, BootPayload, Capacity, CapacityProvider, HostName,
    HostRef, Offer, ProviderCaps, ProviderError, RefusalCode, SchedulerId,
};
use js_provider_gce::{
    ApiError, GceCatalogSettings, GceProvider, InstanceStatus, ManualClock, Method, PROVIDER_ID,
    parse_host_ref,
};
use js_types::{Model, ProviderId};

use crate::rig::{
    Account, HOLD, PROJECT, Rig, SCHEDULER, ZONE, ask, candidate, host, label, live, logged,
    machine, marks, name_of, rig, rig_over, row, set, stranger,
};

/// The provider under the suite: a fresh project and provider for each case, with room for
/// as many machines as the case asks for, whose catalog holds an x86_64 CPU type, an arm64
/// CPU type and an x86_64 GPU type.
struct GceRig(Rig);

impl GceRig {
    fn fresh(machines: usize) -> Rig {
        rig(live(machines))
    }
}

/// The error the cloud answers an insert with for `code`.
fn refusal(code: RefusalCode) -> ApiError {
    ApiError::answered(match code {
        RefusalCode::Capacity => "ZONE_RESOURCE_POOL_EXHAUSTED",
        RefusalCode::Quota => "QUOTA_EXCEEDED",
        RefusalCode::Shape => "INVALID_FIELD_VALUE",
        RefusalCode::Denied => "forbidden",
    })
}

impl Conformance for GceRig {
    fn provider(&self) -> &dyn CapacityProvider {
        &self.0.provider
    }

    fn scheduler(&self) -> SchedulerId {
        SchedulerId::new(SCHEDULER).unwrap()
    }

    fn request(&self, acquire: &AcquireId) -> AcquireRequest {
        AcquireRequest {
            name: HostName::derive(&self.scheduler(), acquire),
            acquire: acquire.clone(),
            pool: "pool-a".to_owned(),
            lifetime: None,
            offer: candidate(),
            labels: Vec::new(),
            payload: BootPayload::new("user-data".to_owned()),
        }
    }

    fn make_room(&mut self, machines: u32) {
        self.0 = Self::fresh(usize::try_from(machines).unwrap());
    }

    fn refuse_next(&mut self, code: RefusalCode) {
        self.0
            .account
            .fail_next(Method::InsertInstance, refusal(code));
    }

    fn lose_next_answer_that_landed(&mut self) {
        self.0
            .account
            .lose_after_next(Method::InsertInstance, ApiError::Timeout);
    }

    fn lose_next_answer_that_did_not_land(&mut self) {
        self.0.account.fail_next_call(ApiError::Timeout);
    }

    fn lose_next_launch_that_did_not_land(&mut self) {
        self.0
            .account
            .fail_next(Method::InsertInstance, ApiError::Timeout);
    }

    fn fail_read_after_next_terminate(&mut self) {
        self.0
            .account
            .fail_read_after_next_delete(ApiError::Timeout);
    }

    fn foreign_machine_exists(&mut self) -> Vec<HostRef> {
        let theirs = stranger(1);
        let unmarked = stranger(2);
        self.0.account.add_instance(row(
            &theirs,
            InstanceStatus::Running,
            &name_of(OTHER_SCHEDULER, FOREIGN_ACQUIRE),
            marks(OTHER_SCHEDULER, FOREIGN_ACQUIRE),
        ));
        self.0.account.add_instance(row(
            &unmarked,
            InstanceStatus::Running,
            "plain-machine",
            Vec::new(),
        ));
        vec![host(&theirs), host(&unmarked)]
    }

    fn fail_list(&mut self) {
        let reset = ApiError::Transport("the connection was reset".to_owned());
        self.0.account.fail_next(Method::ListInstances, reset);
    }

    fn forget(&mut self, gone: &HostRef) {
        let (_, instance) = parse_host_ref(gone).unwrap();
        self.0.account.remove_instance(&instance);
    }

    fn throttle_reads(&mut self) {
        let throttled = ApiError::answered("rateLimitExceeded");
        self.0.account.fail_next(Method::GetInstance, throttled);
    }

    fn outwait_unresolved(&mut self) {
        self.0.clock.advance(HOLD + Duration::from_secs(1));
    }

    fn launches(&self) -> usize {
        self.0.account.inserted()
    }

    fn terminates(&self) -> usize {
        self.0.account.deletes()
    }
}

/// The provider's catalog, awaited on a runtime of its own: the catalog runs its blocking
/// calls on tokio's blocking pool, which needs a runtime.
fn catalog(provider: &GceProvider) -> Result<Vec<Offer>, ProviderError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .build()
        .unwrap();
    runtime.block_on(provider.catalog())
}

/// Runs every case of the suite against the provider over the in-memory project. Each case
/// names the rule of the contract it pins (`js_elastic::conformance`).
#[test]
fn the_gce_provider_passes_the_provider_conformance_suite() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .build()
        .unwrap();
    let _entered = runtime.enter();
    run_all(&mut GceRig(GceRig::fresh(1)));
}

/// Asserts the provider's name, that it launches, that it sends no notices (a reclaimed
/// Spot instance is deleted and a list stops showing it), that it asks the boot payload for
/// no step beyond what the image carries, that it has no checks of its own, and that the
/// payload limit is the 256 KiB the cloud takes as one metadata value.
#[test]
fn the_provider_is_named_gce_launches_and_sends_no_notices() {
    let lab = GceRig::fresh(1);
    assert_eq!(lab.provider.id(), PROVIDER_ID);
    assert_eq!(lab.provider.id().as_str(), "gce");
    assert_eq!(ProviderId::new("gce"), Ok(PROVIDER_ID));
    assert!(lab.provider.can_launch());
    assert!(lab.provider.events().is_none());
    assert!(lab.provider.host_prep(&candidate()).is_empty());
    assert_eq!(lab.provider.payload_limit(), 262_144);
    assert_eq!(js_provider_gce::PAYLOAD_LIMIT, 262_144);
    assert!(lab.provider.checks().is_empty());
}

/// Asserts the capabilities before any catalog is read: Linux, both architectures an offer
/// states, the canonical models of the accelerators the translation table names (the A100,
/// the H100 and the B200), a whole machine and host ports. It catches capabilities kept as
/// a second list beside the table the catalog reads.
#[test]
fn the_capabilities_are_linux_both_architectures_and_the_tables_models() {
    let lab = GceRig::fresh(1);
    let caps = lab.provider.capabilities();
    let ProviderCaps {
        os,
        arch,
        accelerators,
        whole_host,
        host_ports,
    } = caps;
    assert_eq!(os, set(&["linux"]));
    assert_eq!(arch, set(&["arm64", "x86_64"]));
    for model in ["nvidia-a100", "nvidia-h100", "nvidia-b200"] {
        assert!(
            accelerators.contains(&Model::parse(model).unwrap()),
            "{model} in {accelerators:?}"
        );
    }
    assert!(whole_host);
    assert!(host_ports);
}

/// Asserts that every offer of the catalog is inside the capabilities, and that an
/// accelerator the translation table does not name, which reaches the catalog as its own
/// spelling, is inside them once a catalog has carried it and not before. It catches
/// capabilities that leave an offer out, which would refuse a job the provider could serve.
#[test]
fn capabilities_cover_an_accelerator_the_catalog_has_carried() {
    let account = Arc::new(Account::new());
    account.add_machine_type(machine("q9-l4-1", 8, &[("nvidia-l4", 1)], "X86_64"));
    let narrow = GceCatalogSettings::new(
        PROJECT,
        &[ZONE],
        &["on-demand"],
        Some(vec!["q9-l4-1".to_owned()]),
    )
    .unwrap();
    let provider = GceProvider::new(account, Arc::new(ManualClock::new()), narrow, live(1));
    let before = provider.capabilities();
    let offers = catalog(&provider).unwrap();
    let l4 = offers
        .iter()
        .find(|offer| offer.id == "q9-l4-1/on-demand")
        .unwrap();
    assert!(!before.covers(l4), "{l4:?} before the catalog");
    let after = provider.capabilities();
    assert!(offers.iter().all(|offer| after.covers(offer)));
    assert_eq!(after.accelerators.len(), before.accelerators.len() + 1);
    assert_eq!(after.arch, before.arch);

    let lab = GceRig::fresh(1);
    let all = catalog(&lab.provider).unwrap();
    let caps = lab.provider.capabilities();
    assert!(all.iter().all(|offer| caps.covers(offer)));
}

/// Asserts the catalog the provider returns: one offer for each purchase option of the
/// types its settings list that the project's zone has, the architecture of each, in the
/// order the catalog reads, and that a call that fails answers an error and the next call
/// answers again.
#[test]
fn the_catalog_is_the_settings_types_in_their_zones() {
    let lab = GceRig::fresh(1);
    let offers = catalog(&lab.provider).unwrap();
    let shape: Vec<(&str, &str)> = offers
        .iter()
        .map(|offer| (offer.id.as_str(), offer.arch.as_str()))
        .collect();
    assert_eq!(
        shape,
        [
            ("a2-highgpu-2g/on-demand", "x86_64"),
            ("a2-highgpu-2g/spot", "x86_64"),
            ("q9-standard-2/on-demand", "x86_64"),
            ("q9-standard-2/spot", "x86_64"),
            ("q9a-standard-2/on-demand", "arm64"),
            ("q9a-standard-2/spot", "arm64"),
        ]
    );
    assert!(
        offers
            .iter()
            .all(|offer| offer.zone.as_deref() == Some(ZONE))
    );
    lab.account.fail_next(
        Method::MachineTypes,
        ApiError::answered("rateLimitExceeded"),
    );
    assert_eq!(
        catalog(&lab.provider),
        Err(ProviderError::new(
            "machineTypes.aggregatedList failed: throttled (rateLimitExceeded)"
        ))
    );
    assert_eq!(catalog(&lab.provider).unwrap().len(), 6);
}

/// Asserts that a quota the catalog cannot read is logged and that the offers are still
/// returned: a note of the catalog is a warning of the provider, naming the region and the
/// error. It catches a note that is dropped in silence.
#[test]
fn a_quota_read_that_failed_is_logged_and_the_offers_stand() {
    let lab = GceRig::fresh(1);
    lab.account.fail_always(
        Method::RegionQuotas,
        ApiError::answered("rateLimitExceeded"),
    );
    let (offers, warnings) = logged(tracing::Level::WARN, || catalog(&lab.provider));
    assert_eq!(offers.unwrap().len(), 6);
    assert!(warnings.contains("a quota read failed"), "{warnings}");
    assert!(warnings.contains("region-one"), "{warnings}");
    assert!(
        warnings.contains("throttled (rateLimitExceeded)"),
        "{warnings}"
    );
}

/// Asserts that a catalog whose blocking call panics answers exactly
/// `ProviderError::new("the catalog call did not finish")`, and that the next catalog call
/// on the same provider returns the offers. It catches a panic that unwinds into the
/// caller, or one that leaves the provider unusable.
#[test]
fn a_catalog_call_that_panics_is_an_error_and_the_provider_goes_on() {
    let lab = GceRig::fresh(1);
    lab.account.panic_on_next_catalog();
    assert_eq!(
        catalog(&lab.provider),
        Err(ProviderError::new("the catalog call did not finish"))
    );
    assert_eq!(catalog(&lab.provider).unwrap().len(), 6);
}

/// Asserts what `capacity` counts against `max_machines`: a launched instance is supplied
/// and counted, from the launch's answer and with no call of its own; an instance with some
/// of our marks, not all, counts and is not supplied (a terminated one neither); an
/// instance a list shows terminated is neither supplied nor counted. It catches a bound
/// that leaves out what the launch guard counts, which would promise launches the guard
/// refuses, and a count read from the cloud on every call.
#[test]
fn capacity_counts_what_the_launch_guard_counts() {
    let lab = GceRig::fresh(4);
    assert!(matches!(
        lab.provider.acquire(&ask("a1")),
        Acquired::Accepted { .. }
    ));
    let calls = lab.account.methods().len();
    assert_eq!(
        lab.provider.capacity(),
        Capacity {
            supplied: 1,
            can_add: Some(3)
        }
    );
    assert_eq!(lab.account.methods().len(), calls, "capacity makes no call");
    let renamed = vec![label("js-scheduler", SCHEDULER)];
    for (text, status) in [
        (stranger(10), InstanceStatus::Running),
        (stranger(11), InstanceStatus::Terminated),
    ] {
        lab.account
            .add_instance(row(&text, status, "renamed", renamed.clone()));
    }
    lab.provider.list().unwrap();
    assert_eq!(
        lab.provider.capacity(),
        Capacity {
            supplied: 1,
            can_add: Some(2)
        }
    );
    let launched = lab.account.instances().remove(0);
    lab.account
        .set_status(&launched.id, InstanceStatus::Terminated);
    lab.provider.list().unwrap();
    assert_eq!(
        lab.provider.capacity(),
        Capacity {
            supplied: 0,
            can_add: Some(3)
        }
    );
}

/// Asserts that a provider over the same project that restarted, and so remembers nothing,
/// has the same capacity once a list has run. It catches a count that is only kept from the
/// launches of this run.
#[test]
fn a_restarted_provider_counts_what_a_list_shows() {
    let first = rig(live(3));
    for acquire in ["a1", "a2"] {
        assert!(matches!(
            first.provider.acquire(&ask(acquire)),
            Acquired::Accepted { .. }
        ));
    }
    let restarted = rig_over(Arc::clone(&first.account), live(3));
    assert_eq!(restarted.provider.capacity().supplied, 0);
    assert_eq!(restarted.provider.list().unwrap().len(), 2);
    assert_eq!(
        restarted.provider.capacity(),
        Capacity {
            supplied: 2,
            can_add: Some(1)
        }
    );
}

/// Asserts that the manual clock the rig reads is the one the provider reads: a hold set
/// short runs out when the clock moves past it and not before. It catches a provider that
/// reads another clock.
#[test]
fn the_provider_reads_the_clock_it_was_given() {
    let lab = rig(live(1).unknown_hold(Duration::from_secs(5)));
    lab.account
        .fail_next(Method::InsertInstance, ApiError::Timeout);
    assert_eq!(lab.provider.acquire(&ask("a1")), Acquired::Unknown);
    assert!(matches!(
        lab.provider.acquire(&ask("a1")),
        Acquired::Deferred { wait } if wait == Duration::from_secs(5)
    ));
    lab.clock.advance(Duration::from_secs(6));
    assert!(matches!(
        lab.provider.acquire(&ask("a1")),
        Acquired::Accepted { .. }
    ));
}

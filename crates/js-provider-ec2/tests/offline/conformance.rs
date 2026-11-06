//! The EC2 provider over the in-memory account against the provider contract's
//! conformance suite, and its own answers to the calls the suite leaves to each provider:
//! its name, what it offers, what it counts and that it sends no notices.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use js_elastic::conformance::{FOREIGN_ACQUIRE, OTHER_SCHEDULER, Rig as Conformance, run_all};
use js_elastic::{
    AcquireId, AcquireRequest, Acquired, BootPayload, Capacity, CapacityProvider, HostName,
    HostRef, ProviderCaps, ProviderError, RefusalCode, SchedulerId,
};
use js_provider_ec2::{
    Action, ApiError, Ec2Provider, GpuInfo, InstanceId, InstanceState, ManualClock, PROVIDER_ID,
};
use js_types::{Model, ProviderId};

use crate::rig::{
    Account, HOLD, Rig, SCHEDULER, ask, candidate, catalog, kind, live, logged, marks, rig,
    rig_over, row, set, stranger,
};

/// The provider under the suite: a fresh account and provider for each case, with room for
/// as many machines as the case asks for, whose catalog holds an x86_64 CPU type, an arm64
/// CPU type and an x86_64 GPU type.
struct Ec2Rig(Rig);

impl Ec2Rig {
    fn fresh(machines: usize) -> Rig {
        rig(live(machines))
    }
}

/// The error the cloud answers a launch with for `code`.
fn refusal(code: RefusalCode) -> ApiError {
    ApiError::answered(match code {
        RefusalCode::Capacity => "InsufficientInstanceCapacity",
        RefusalCode::Quota => "InstanceLimitExceeded",
        RefusalCode::Shape => "InvalidParameterValue",
        RefusalCode::Denied => "UnauthorizedOperation",
    })
}

impl Conformance for Ec2Rig {
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
            .fail_next(Action::RunInstances, refusal(code));
    }

    fn lose_next_answer_that_landed(&mut self) {
        self.0
            .account
            .lose_after_next(Action::RunInstances, ApiError::Timeout);
    }

    fn lose_next_answer_that_did_not_land(&mut self) {
        self.0.account.fail_next_call(ApiError::Timeout);
    }

    fn lose_next_launch_that_did_not_land(&mut self) {
        self.0
            .account
            .fail_next(Action::RunInstances, ApiError::Timeout);
    }

    fn fail_read_after_next_terminate(&mut self) {
        self.0
            .account
            .fail_read_after_next_terminate(ApiError::Timeout);
    }

    fn foreign_machine_exists(&mut self) -> Vec<HostRef> {
        let theirs = stranger(1);
        let unmarked = stranger(2);
        let marks = marks(OTHER_SCHEDULER, FOREIGN_ACQUIRE);
        self.0
            .account
            .add_instance(row(&theirs, InstanceState::Running, marks));
        self.0
            .account
            .add_instance(row(&unmarked, InstanceState::Running, Vec::new()));
        vec![HostRef(theirs), HostRef(unmarked)]
    }

    fn fail_list(&mut self) {
        let reset = ApiError::Transport("the connection was reset".to_owned());
        self.0.account.fail_next(Action::DescribeInstances, reset);
    }

    fn forget(&mut self, gone: &HostRef) {
        self.0
            .account
            .remove_instance(&InstanceId::new(gone.as_str()).unwrap());
    }

    fn throttle_reads(&mut self) {
        let throttled = ApiError::answered("RequestLimitExceeded");
        self.0
            .account
            .fail_next(Action::DescribeInstances, throttled);
    }

    fn outwait_unresolved(&mut self) {
        self.0.clock.advance(HOLD + Duration::from_secs(1));
    }

    fn launches(&self) -> usize {
        self.0.account.launched()
    }

    fn terminates(&self) -> usize {
        self.0.account.terminates()
    }
}

/// Runs every case of the suite against the provider over the in-memory account. Each
/// case names the rule of the contract it pins (`js_elastic::conformance`).
#[test]
fn the_ec2_provider_passes_the_provider_conformance_suite() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .build()
        .unwrap();
    let _entered = runtime.enter();
    run_all(&mut Ec2Rig(Ec2Rig::fresh(1)));
}

/// Asserts the provider's name, that it launches, that it sends no notices (a reclaimed
/// spot instance shows in a list), that it asks the boot payload for no step beyond what
/// the image carries, and that the payload limit is the 16 KB the cloud takes as user data.
#[test]
fn the_provider_is_named_ec2_launches_and_sends_no_notices() {
    let rig = Ec2Rig::fresh(1);
    assert_eq!(rig.provider.id(), PROVIDER_ID);
    assert_eq!(rig.provider.id().as_str(), "ec2");
    assert_eq!(ProviderId::new("ec2"), Ok(PROVIDER_ID));
    assert!(rig.provider.can_launch());
    assert!(rig.provider.events().is_none());
    assert!(rig.provider.host_prep(&candidate()).is_empty());
    assert_eq!(rig.provider.payload_limit(), 16_384);
    assert_eq!(js_provider_ec2::PAYLOAD_LIMIT, 16_384);
    assert!(rig.provider.checks().is_empty());
}

/// Asserts the capabilities before any catalog is read: Linux, both architectures an offer
/// states, the canonical models of the GPUs the translation table names (the A100 and the
/// H100, once each), a whole machine and host ports. It catches capabilities kept as a
/// second list beside the table the catalog reads.
#[test]
fn the_capabilities_are_linux_both_architectures_and_the_tables_gpu_models() {
    let rig = Ec2Rig::fresh(1);
    let model = |text: &str| Model::parse(text).unwrap();
    assert_eq!(
        rig.provider.capabilities(),
        ProviderCaps {
            os: set(&["linux"]),
            arch: set(&["arm64", "x86_64"]),
            accelerators: BTreeSet::from([model("nvidia-a100"), model("nvidia-h100")]),
            whole_host: true,
            host_ports: true,
        }
    );
}

/// Asserts that every offer of the catalog is inside the capabilities, and that a GPU the
/// translation table does not name, which reaches the catalog as its own spelling, is
/// inside them once a catalog has carried it. It catches capabilities that leave an offer
/// out, which would refuse a job the provider could serve.
#[test]
fn capabilities_cover_a_gpu_the_catalog_has_carried() {
    let rig = Ec2Rig::fresh(1);
    rig.account.add_instance_type(kind(
        "g9g.xlarge",
        4,
        "arm64",
        vec![GpuInfo {
            name: "Sample Grace".to_owned(),
            manufacturer: "NVIDIA".to_owned(),
            count: 1,
            memory_mib: 16 * 1024,
        }],
    ));
    rig.account
        .add_zone_offering("g9g.xlarge", crate::rig::ZONE)
        .add_on_demand_price("g9g.xlarge", "1.5");
    let before = rig.provider.capabilities();
    let offers = catalog(&rig.provider).unwrap();
    let grace = offers
        .iter()
        .find(|offer| offer.id == "g9g.xlarge/on-demand")
        .unwrap();
    assert!(!before.covers(grace), "{grace:?} before the catalog");
    let after = rig.provider.capabilities();
    assert!(offers.iter().all(|offer| after.covers(offer)));
    assert_eq!(after.accelerators.len(), before.accelerators.len() + 1);
    assert_eq!(after.arch, before.arch);
}

/// Asserts the catalog the provider returns: one offer for each zone and purchase option
/// of the types its settings list, the architecture of each, in the order the catalog
/// reads, and that a call that fails answers an error and the next call answers again.
#[test]
fn the_catalog_is_the_settings_types_in_their_zones() {
    let rig = Ec2Rig::fresh(1);
    let offers = catalog(&rig.provider).unwrap();
    let shape: Vec<(&str, &str)> = offers
        .iter()
        .map(|offer| (offer.id.as_str(), offer.arch.as_str()))
        .collect();
    assert_eq!(
        shape,
        [
            ("c9g.large/on-demand", "arm64"),
            ("c9g.large/spot", "arm64"),
            ("m9.large/on-demand", "x86_64"),
            ("m9.large/spot", "x86_64"),
            ("p9.24xlarge/on-demand", "x86_64"),
            ("p9.24xlarge/spot", "x86_64"),
        ]
    );
    assert!(
        offers
            .iter()
            .all(|offer| offer.zone.as_deref() == Some("zone-a1"))
    );
    rig.account.fail_next(
        Action::DescribeInstanceTypeOfferings,
        ApiError::answered("RequestLimitExceeded"),
    );
    assert_eq!(
        catalog(&rig.provider),
        Err(ProviderError::new(
            "DescribeInstanceTypeOfferings failed: throttled (RequestLimitExceeded)"
        ))
    );
    assert_eq!(catalog(&rig.provider).unwrap().len(), 6);
}

/// Asserts that a quota the catalog cannot read is logged and that the offers are still
/// returned: a note of the catalog is a warning of the provider, naming the quota family,
/// the purchase option and the error. It catches a note that is dropped in silence.
#[test]
fn a_quota_read_that_failed_is_logged_and_the_offers_stand() {
    let rig = Ec2Rig::fresh(1);
    rig.account.fail_always(
        Action::GetServiceQuota,
        ApiError::answered("RequestLimitExceeded"),
    );
    let (offers, warnings) = logged(tracing::Level::WARN, || catalog(&rig.provider));
    assert_eq!(offers.unwrap().len(), 6);
    assert!(warnings.contains("a quota read failed"), "{warnings}");
    assert!(warnings.contains("Standard"), "{warnings}");
    assert!(warnings.contains("OnDemand"), "{warnings}");
    assert!(
        warnings.contains("throttled (RequestLimitExceeded)"),
        "{warnings}"
    );
}

/// Asserts that a catalog whose blocking call panics answers exactly
/// `ProviderError::new("the catalog call did not finish")`, and that the next catalog call
/// on the same provider returns the offers. It catches a panic that unwinds into the
/// caller, or one that leaves the provider unusable.
#[test]
fn a_catalog_call_that_panics_is_an_error_and_the_provider_goes_on() {
    let rig = Ec2Rig::fresh(1);
    rig.account.panic_on_next_catalog();
    assert_eq!(
        catalog(&rig.provider),
        Err(ProviderError::new("the catalog call did not finish"))
    );
    assert_eq!(catalog(&rig.provider).unwrap().len(), 6);
}

/// Asserts what `capacity` counts against `max_machines`: a launched instance is supplied
/// and counted, from the launch's answer and with no call of its own; an instance with some
/// of our marks, not all, counts and is not supplied (a terminated one neither); an
/// instance a list shows terminated is neither supplied nor counted. It catches a bound
/// that leaves out what the launch guard counts, which would promise launches the guard
/// refuses, and a count read from the cloud on every call.
#[test]
fn capacity_counts_what_the_launch_guard_counts() {
    let rig = Ec2Rig::fresh(4);
    assert!(matches!(
        rig.provider.acquire(&ask("a1")),
        Acquired::Accepted { .. }
    ));
    let calls = rig.account.actions().len();
    assert_eq!(
        rig.provider.capacity(),
        Capacity {
            supplied: 1,
            can_add: Some(3)
        }
    );
    assert_eq!(rig.account.actions().len(), calls, "capacity makes no call");
    let renamed = vec![crate::rig::tag("js-scheduler", SCHEDULER)];
    for (text, state) in [
        (stranger(10), InstanceState::Running),
        (stranger(11), InstanceState::Terminated),
    ] {
        rig.account.add_instance(row(&text, state, renamed.clone()));
    }
    rig.provider.list().unwrap();
    assert_eq!(
        rig.provider.capacity(),
        Capacity {
            supplied: 1,
            can_add: Some(2)
        }
    );
    let launched = rig.account.instances().remove(0);
    rig.account
        .set_state(&launched.id, InstanceState::Terminated);
    rig.provider.list().unwrap();
    assert_eq!(
        rig.provider.capacity(),
        Capacity {
            supplied: 0,
            can_add: Some(3)
        }
    );
}

/// Asserts that a provider over the same account that restarted, and so remembers nothing,
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
    let account = Arc::new(Account::new());
    let clock = Arc::new(ManualClock::new());
    let config = live(1).unknown_hold(Duration::from_secs(5));
    let provider = Ec2Provider::new(
        account.clone(),
        clock.clone(),
        crate::rig::settings(),
        config,
    );
    account.fail_next(Action::RunInstances, ApiError::Timeout);
    assert_eq!(provider.acquire(&ask("a1")), Acquired::Unknown);
    assert!(matches!(
        provider.acquire(&ask("a1")),
        Acquired::Deferred { wait } if wait == Duration::from_secs(5)
    ));
    clock.advance(Duration::from_secs(6));
    assert!(matches!(
        provider.acquire(&ask("a1")),
        Acquired::Accepted { .. }
    ));
}

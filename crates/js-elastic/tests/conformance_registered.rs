//! The registered set of the conformance suite against a stand-in for a provider whose
//! machines register themselves: the stand-in keeping every rule passes the whole set, and
//! with one rule broken it fails exactly the case that pins that rule. The `expected` text
//! is the start of the case's assertion message, so each test also pins why the case
//! fails.

use std::collections::BTreeSet;
use std::sync::Mutex;

use js_elastic::conformance::{
    REGISTERED_CASES, RegisteredRig, registered_acquire_is_refused_with_capacity,
    registered_capacity_counts_machines_and_adds_none,
    registered_catalog_is_empty_and_capabilities_follow_the_machines,
    registered_list_holds_every_machine_and_a_failure_is_an_error,
    registered_observe_is_absent_only_after_removal,
    registered_release_refuses_and_changes_nothing,
    registered_verify_instance_matches_only_a_bound_instance, run_all_registered,
};
use js_elastic::{
    AcquireRequest, Acquired, BootStep, Capacity, CapacityProvider, CatalogFuture, Claim, Held,
    Hello, HostRef, HostStatus, Observed, Offer, ProviderCaps, ProviderError, Reason, Refusal,
    RefusalCode, Released,
};
use js_types::ProviderId;

/// Which rule the stand-in breaks, if any.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Flaw {
    /// Keeps every rule.
    None,
    /// Every acquire launches a machine.
    LaunchesOnAcquire,
    /// A release answers that the machine is gone.
    ReleasesAsGone,
    /// A list leaves out silent machines.
    ListsOnlyLive,
    /// A silent machine reads as absent.
    SilenceIsAbsent,
    /// A hello is matched by its name alone.
    MatchesByName,
    /// The capacity says it can add a machine.
    OffersALaunch,
    /// The catalog is an error.
    CatalogFails,
    /// The capabilities name an operating system no machine reported.
    WrongOs,
    /// The capabilities leave out the architecture the machines reported.
    NoArch,
}

/// One registered machine of the stand-in.
struct Machine {
    name: String,
    instance: Option<String>,
    silent: bool,
}

/// What the stand-in holds: its machines, and whether its next list fails.
#[derive(Default)]
struct Table {
    machines: Vec<Machine>,
    fail_next_list: bool,
}

/// A provider of registered machines, over a table the rig writes.
struct Registered {
    flaw: Flaw,
    table: Mutex<Table>,
}

impl Registered {
    fn has(&self, flaw: Flaw) -> bool {
        self.flaw == flaw
    }
}

fn held(machine: &Machine) -> Held {
    Held {
        id: HostRef(machine.name.clone()),
        acquire: String::new(),
        name: Some(machine.name.clone()),
        offer: String::new(),
        zone: None,
        status: HostStatus::Active,
    }
}

impl CapacityProvider for Registered {
    fn id(&self) -> ProviderId {
        ProviderId::known("registered")
    }

    fn can_launch(&self) -> bool {
        false
    }

    fn capabilities(&self) -> ProviderCaps {
        if self.table.lock().unwrap().machines.is_empty() {
            return ProviderCaps::default();
        }
        let os = match self.has(Flaw::WrongOs) {
            true => "macos",
            false => "linux",
        };
        let arch = match self.has(Flaw::NoArch) {
            true => BTreeSet::new(),
            false => BTreeSet::from(["x86_64".to_owned()]),
        };
        ProviderCaps {
            os: BTreeSet::from([os.to_owned()]),
            arch,
            ..ProviderCaps::default()
        }
    }

    fn catalog(&self) -> CatalogFuture<'_> {
        let answer = match self.has(Flaw::CatalogFails) {
            true => Err(ProviderError::new("no catalog")),
            false => Ok(Vec::new()),
        };
        Box::pin(std::future::ready(answer))
    }

    fn capacity(&self) -> Capacity {
        let supplied = self.table.lock().unwrap().machines.len();
        Capacity {
            supplied: u32::try_from(supplied).unwrap(),
            can_add: Some(u32::from(self.has(Flaw::OffersALaunch))),
        }
    }

    fn host_prep(&self, _offer: &Offer) -> Vec<BootStep> {
        Vec::new()
    }

    fn payload_limit(&self) -> usize {
        0
    }

    fn acquire(&self, request: &AcquireRequest) -> Acquired {
        match self.has(Flaw::LaunchesOnAcquire) {
            true => Acquired::Accepted {
                held: Held {
                    id: HostRef("launched".to_owned()),
                    acquire: request.acquire.to_string(),
                    name: Some(request.name.to_string()),
                    offer: request.offer.id.clone(),
                    zone: None,
                    status: HostStatus::Booting,
                },
                extras: Vec::new(),
            },
            false => Acquired::Refused(Refusal {
                code: RefusalCode::Capacity,
                reason: Reason::NotAllowed,
            }),
        }
    }

    fn list(&self) -> Result<Vec<Held>, ProviderError> {
        let mut table = self.table.lock().unwrap();
        if std::mem::take(&mut table.fail_next_list) {
            return Err(ProviderError::new("the list failed"));
        }
        let only_live = self.has(Flaw::ListsOnlyLive);
        let listed = table
            .machines
            .iter()
            .filter(|machine| !(only_live && machine.silent));
        Ok(listed.map(held).collect())
    }

    fn release(&self, _id: &HostRef) -> Released {
        match self.has(Flaw::ReleasesAsGone) {
            true => Released::Gone,
            false => Released::Refused(Claim::Foreign),
        }
    }

    fn observe(&self, id: &HostRef) -> Observed {
        let table = self.table.lock().unwrap();
        let machine = table.machines.iter().find(|m| m.name == id.as_str());
        match machine {
            None => Observed::Absent,
            Some(m) if !m.silent => Observed::Present(HostStatus::Active),
            Some(_) if self.has(Flaw::SilenceIsAbsent) => Observed::Absent,
            Some(_) => Observed::Unknown,
        }
    }

    fn verify_instance(&self, hello: &Hello) -> Option<HostRef> {
        let table = self.table.lock().unwrap();
        let machine = table.machines.iter().find(|m| m.name == hello.name)?;
        let by_name = self.has(Flaw::MatchesByName);
        let bound = machine.instance.is_some() && machine.instance == hello.instance_id;
        (by_name || bound).then(|| HostRef(machine.name.clone()))
    }
}

/// The rig of the stand-in: it writes the stand-in's table directly. With `lists_can_fail`
/// off it says its list cannot fail, as a provider that answers from memory.
struct StandInRig {
    provider: Registered,
    lists_can_fail: bool,
}

impl StandInRig {
    fn new(flaw: Flaw) -> Self {
        Self {
            provider: Registered {
                flaw,
                table: Mutex::new(Table::default()),
            },
            lists_can_fail: true,
        }
    }

    fn table(&self) -> std::sync::MutexGuard<'_, Table> {
        self.provider.table.lock().unwrap()
    }
}

impl RegisteredRig for StandInRig {
    fn provider(&self) -> &dyn CapacityProvider {
        &self.provider
    }

    fn start_over(&mut self) {
        *self.table() = Table::default();
    }

    fn register(&mut self, name: &str, instance: Option<&str>) -> HostRef {
        self.table().machines.push(Machine {
            name: name.to_owned(),
            instance: instance.map(str::to_owned),
            silent: false,
        });
        HostRef(name.to_owned())
    }

    fn platform(&self) -> (String, String) {
        ("linux".to_owned(), "x86_64".to_owned())
    }

    fn silence(&mut self, id: &HostRef) {
        let mut table = self.table();
        let silenced = table.machines.iter_mut().filter(|m| m.name == id.as_str());
        silenced.for_each(|machine| machine.silent = true);
    }

    fn remove(&mut self, id: &HostRef) {
        self.table().machines.retain(|m| m.name != id.as_str());
    }

    fn fail_list(&mut self) -> bool {
        self.table().fail_next_list = self.lists_can_fail;
        self.lists_can_fail
    }
}

/// Asserts that a provider of registered machines that keeps every rule passes every case
/// of the registered set, whether or not its list can fail, and that the run reports each
/// case as run. It catches a case that fails a provider keeping the rules, and an entry
/// point that skips a case.
#[test]
fn the_registered_set_passes_a_provider_that_keeps_its_rules() {
    for lists_can_fail in [true, false] {
        let mut rig = StandInRig::new(Flaw::None);
        rig.lists_can_fail = lists_can_fail;
        assert_eq!(run_all_registered(&mut rig), REGISTERED_CASES.len());
        assert!(!rig.provider().can_launch());
        assert_eq!(rig.provider().id().as_str(), "registered");
        assert!(rig.provider().host_prep(&offer()).is_empty());
        assert_eq!(rig.provider().payload_limit(), 0);
    }
}

fn offer() -> Offer {
    Offer {
        id: "any".to_owned(),
        zone: None,
        cpu_millis: 1,
        memory_bytes: 1,
        disk_bytes: 0,
        accelerator: None,
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        isa_level: None,
        exec: js_types::ExecMode::Container,
        price_cents_per_hour: None,
        price_unit: js_elastic::PriceUnit::UsCents,
        available: true,
        purchase: None,
        quota: None,
        block: None,
    }
}

/// Asserts that the acquire case fails, naming the answer, on a provider of registered
/// machines that launches on acquire.
#[test]
#[should_panic(
    expected = "registered_acquire_is_refused_with_capacity: an acquire of a provider that cannot launch answers Accepted"
)]
fn the_acquire_case_fails_on_a_provider_that_launches() {
    registered_acquire_is_refused_with_capacity(&mut StandInRig::new(Flaw::LaunchesOnAcquire));
}

/// Asserts that the release case fails on a provider that answers a release of a
/// registered machine as gone.
#[test]
#[should_panic(
    expected = "registered_release_refuses_and_changes_nothing: the release of reg-a answers Gone, not a refusal"
)]
fn the_release_case_fails_on_a_provider_that_releases_a_registered_machine() {
    registered_release_refuses_and_changes_nothing(&mut StandInRig::new(Flaw::ReleasesAsGone));
}

/// Asserts that the list case fails on a provider that leaves silent machines out of its
/// list.
#[test]
#[should_panic(
    expected = "registered_list_holds_every_machine_and_a_failure_is_an_error: the list holds every registered machine, silent ones included"
)]
fn the_list_case_fails_on_a_provider_that_lists_only_live_machines() {
    registered_list_holds_every_machine_and_a_failure_is_an_error(&mut StandInRig::new(
        Flaw::ListsOnlyLive,
    ));
}

/// Asserts that the observe case fails on a provider that reads a silent machine as
/// absent, which would free what is booked on it.
#[test]
#[should_panic(
    expected = "registered_observe_is_absent_only_after_removal: a silent machine is Unknown"
)]
fn the_observe_case_fails_on_a_provider_that_reads_silence_as_absence() {
    registered_observe_is_absent_only_after_removal(&mut StandInRig::new(Flaw::SilenceIsAbsent));
}

/// Asserts that the verify case fails on a provider that matches a hello by its name
/// alone.
#[test]
#[should_panic(
    expected = "registered_verify_instance_matches_only_a_bound_instance: a hello of reg-a with instance Some(\"i-2\") is not matched"
)]
fn the_verify_case_fails_on_a_provider_that_matches_by_name() {
    registered_verify_instance_matches_only_a_bound_instance(&mut StandInRig::new(
        Flaw::MatchesByName,
    ));
}

/// Asserts that the capacity case fails on a provider that says it could add a machine.
#[test]
#[should_panic(
    expected = "registered_capacity_counts_machines_and_adds_none: an empty provider supplies nothing and can add none"
)]
fn the_capacity_case_fails_on_a_provider_that_offers_a_launch() {
    registered_capacity_counts_machines_and_adds_none(&mut StandInRig::new(Flaw::OffersALaunch));
}

/// Asserts that the catalog case fails on a provider whose catalog is an error.
#[test]
#[should_panic(
    expected = "registered_catalog_is_empty_and_capabilities_follow_the_machines: the catalog is an empty answer, not an error"
)]
fn the_catalog_case_fails_on_a_provider_whose_catalog_errors() {
    registered_catalog_is_empty_and_capabilities_follow_the_machines(&mut StandInRig::new(
        Flaw::CatalogFails,
    ));
}

/// Asserts that the capability case fails on a provider whose capabilities name an
/// operating system the machine did not report.
#[test]
#[should_panic(
    expected = "registered_catalog_is_empty_and_capabilities_follow_the_machines: the operating systems are the one the machine reported"
)]
fn the_capability_case_fails_on_a_provider_that_reports_another_os() {
    registered_catalog_is_empty_and_capabilities_follow_the_machines(&mut StandInRig::new(
        Flaw::WrongOs,
    ));
}

/// Asserts that the capability case fails on a provider whose capabilities leave out the
/// architecture the machine reported.
#[test]
#[should_panic(
    expected = "registered_catalog_is_empty_and_capabilities_follow_the_machines: the architectures are the one the machine reported"
)]
fn the_capability_case_fails_on_a_provider_that_drops_the_arch() {
    registered_catalog_is_empty_and_capabilities_follow_the_machines(&mut StandInRig::new(
        Flaw::NoArch,
    ));
}

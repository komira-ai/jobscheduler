//! The cases a provider that cannot launch passes: one whose machines register
//! themselves (the `static` provider), so that [`CapacityProvider::can_launch`] is false
//! and [`CapacityProvider::acquire`] is refused. The cases of [`super::run_all`] do not
//! apply to such a provider: they assume a [`super::Rig`] that scripts a cloud which
//! launches, and a catalog with offers to launch. These pin what it answers instead, in
//! the order of the contract: acquire, release, list, observe, verify, then capacity and
//! capabilities.
//!
//! A provider crate runs them from one test file: it implements [`RegisteredRig`] over the
//! host table its provider reads and calls [`run_all_registered`], which returns how many
//! cases it ran, so the test can assert that none was skipped
//! ([`REGISTERED_CASES`]). Each case begins from [`RegisteredRig::start_over`], panics when
//! the provider breaks the rule, and starts every assertion message with its own name. No
//! case reads a clock or waits: the rig moves its own clock to silence a machine.

use js_types::ExecMode;

use super::{acquire_id, block_on, refusal_of};
use crate::answer::{Observed, RefusalCode, Released};
use crate::caps::{Capacity, ProviderCaps};
use crate::ids::{Hello, HostName, HostRef, SchedulerId};
use crate::offer::{Offer, PriceUnit};
use crate::provider::{AcquireRequest, CapacityProvider};
use crate::secret::BootPayload;

/// A provider that cannot launch, and the table of registered machines it reads, which a
/// case puts in a named state.
pub trait RegisteredRig {
    /// The provider under test, as the rig holds it now.
    fn provider(&self) -> &dyn CapacityProvider;

    /// Starts over: no machine is registered. Every case begins here.
    fn start_over(&mut self);

    /// A machine named `name` registers itself and is live, reporting the operating system
    /// and architecture of [`RegisteredRig::platform`]; when `instance` is set, its first
    /// hello names that instance id and the machine is bound to it. Returns the provider's
    /// reference for it.
    fn register(&mut self, name: &str, instance: Option<&str>) -> HostRef;

    /// The operating system and the architecture every machine of
    /// [`RegisteredRig::register`] reports, spelled as [`Offer::os`] and [`Offer::arch`]
    /// spell them.
    fn platform(&self) -> (String, String);

    /// The machine `id` goes silent: it has not been heard for longer than the provider's
    /// grace period, and not for so long that the provider forgets it. The other machines
    /// stay live.
    fn silence(&mut self, id: &HostRef);

    /// The operator removes the machine `id` from the table, by the provider's own way of
    /// giving a machine back.
    fn remove(&mut self, id: &HostRef);

    /// The next list fails, if the provider's list can fail at all. `false` for a
    /// provider whose list answers from memory and cannot.
    fn fail_list(&mut self) -> bool;
}

/// One case of the registered set.
pub type RegisteredCase = fn(&mut dyn RegisteredRig);

/// Every case of the registered set, in the order [`run_all_registered`] runs them.
pub const REGISTERED_CASES: [RegisteredCase; 7] = [
    registered_acquire_is_refused_with_capacity,
    registered_release_refuses_and_changes_nothing,
    registered_list_holds_every_machine_and_a_failure_is_an_error,
    registered_observe_is_absent_only_after_removal,
    registered_verify_instance_matches_only_a_bound_instance,
    registered_capacity_counts_machines_and_adds_none,
    registered_catalog_is_empty_and_capabilities_follow_the_machines,
];

/// Runs every case of [`REGISTERED_CASES`] against `rig`, in order, and returns how many
/// ran.
pub fn run_all_registered<R: RegisteredRig>(rig: &mut R) -> usize {
    let rig: &mut dyn RegisteredRig = rig;
    let mut ran = 0;
    for case in REGISTERED_CASES {
        case(&mut *rig);
        ran += 1;
    }
    ran
}

/// An acquire of `acquire` for a plain Linux offer, named as this suite's scheduler names
/// it.
fn request(acquire: &str) -> AcquireRequest {
    let scheduler = SchedulerId::new("registered-suite").expect("the suite's id is an id");
    let acquire = acquire_id(acquire);
    AcquireRequest {
        name: HostName::derive(&scheduler, &acquire),
        acquire,
        pool: "pool".to_owned(),
        offer: Offer {
            id: "any".to_owned(),
            zone: None,
            cpu_millis: 1000,
            memory_bytes: 1 << 30,
            disk_bytes: 0,
            accelerator: None,
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
            isa_level: None,
            exec: ExecMode::Container,
            price_cents_per_hour: None,
            price_unit: PriceUnit::UsCents,
            available: true,
            purchase: None,
        },
        lifetime: None,
        labels: Vec::new(),
        payload: BootPayload::new(String::new()),
    }
}

/// A hello by `name`, naming `instance` when set.
fn hello(name: &str, instance: Option<&str>) -> Hello {
    Hello {
        name: name.to_owned(),
        instance_id: instance.map(str::to_owned),
    }
}

/// The provider says it cannot launch; an acquire, and its repeat with the same acquire
/// id, answer `Refused(Capacity)`, and the list, the capacity and the unresolved calls are
/// the same after as before. Fails on a provider that launches, books a registered
/// machine for the acquire, or answers another code (the caller falls through to its next
/// provider on capacity only).
pub fn registered_acquire_is_refused_with_capacity(rig: &mut dyn RegisteredRig) {
    const CASE: &str = "registered_acquire_is_refused_with_capacity";
    rig.start_over();
    assert!(
        !rig.provider().can_launch(),
        "{CASE}: a provider of registered machines says it cannot launch"
    );
    rig.register("reg-a", None);
    let before = (rig.provider().list(), rig.provider().capacity());
    let request = request("registered");
    for _ in 0..2 {
        let answer = rig.provider().acquire(&request);
        assert_eq!(
            refusal_of(&answer),
            Some(RefusalCode::Capacity),
            "{CASE}: an acquire of a provider that cannot launch answers {answer:?}"
        );
    }
    let after = (rig.provider().list(), rig.provider().capacity());
    assert_eq!(after, before, "{CASE}: an acquire changes nothing");
    assert!(
        rig.provider().unresolved().is_empty(),
        "{CASE}: a refused acquire leaves no unresolved call"
    );
}

/// A release of a registered machine, and of text that is no reference of it, is refused,
/// and the list and the capacity are the same after; the machine is still present. Fails
/// on a provider that terminates or forgets a registered machine on release: a machine
/// that registers itself is given back by the operator, not by a release.
pub fn registered_release_refuses_and_changes_nothing(rig: &mut dyn RegisteredRig) {
    const CASE: &str = "registered_release_refuses_and_changes_nothing";
    rig.start_over();
    let id = rig.register("reg-a", None);
    let before = (rig.provider().list(), rig.provider().capacity());
    for released in [id.clone(), HostRef("not an id: of any machine".to_owned())] {
        let answer = rig.provider().release(&released);
        assert!(
            matches!(answer, Released::Refused(_)),
            "{CASE}: the release of {released} answers {answer:?}, not a refusal"
        );
    }
    let after = (rig.provider().list(), rig.provider().capacity());
    assert_eq!(after, before, "{CASE}: a release changes nothing");
    assert!(
        matches!(rig.provider().observe(&id), Observed::Present(_)),
        "{CASE}: the machine is still present"
    );
}

/// A list holds every registered machine, a silent one included, and a list that fails
/// (for a provider whose list can) is an error, never an empty list. Fails on a provider
/// that lists only live machines, which would read a silent one as gone.
pub fn registered_list_holds_every_machine_and_a_failure_is_an_error(rig: &mut dyn RegisteredRig) {
    const CASE: &str = "registered_list_holds_every_machine_and_a_failure_is_an_error";
    rig.start_over();
    let live = rig.register("reg-a", None);
    let silent = rig.register("reg-b", None);
    rig.silence(&silent);
    let listed = rig.provider().list();
    let mut ids: Vec<HostRef> = listed
        .iter()
        .flatten()
        .map(|held| held.id.clone())
        .collect();
    ids.sort();
    let mut registered = vec![live, silent];
    registered.sort();
    assert_eq!(
        ids, registered,
        "{CASE}: the list holds every registered machine, silent ones included"
    );
    if rig.fail_list() {
        let failed = rig.provider().list();
        assert!(failed.is_err(), "{CASE}: a failed list answers {failed:?}");
    }
}

/// A read by id answers `Present` for a live machine, `Unknown` (never `Absent`) once it
/// is silent, and `Absent` only once the operator has removed it; a machine never
/// registered is `Absent`. Fails on a provider that frees a machine for being silent:
/// silence is not proof that the work on it is gone.
pub fn registered_observe_is_absent_only_after_removal(rig: &mut dyn RegisteredRig) {
    const CASE: &str = "registered_observe_is_absent_only_after_removal";
    rig.start_over();
    let id = rig.register("reg-a", None);
    assert!(
        matches!(rig.provider().observe(&id), Observed::Present(_)),
        "{CASE}: a live machine is present"
    );
    rig.silence(&id);
    assert_eq!(
        rig.provider().observe(&id),
        Observed::Unknown,
        "{CASE}: a silent machine is Unknown"
    );
    assert!(
        !rig.provider().is_gone(&id),
        "{CASE}: a silent machine is not gone"
    );
    rig.remove(&id);
    assert_eq!(
        rig.provider().observe(&id),
        Observed::Absent,
        "{CASE}: a removed machine is absent"
    );
    let never = HostRef("reg-never".to_owned());
    assert_eq!(
        rig.provider().observe(&never),
        Observed::Absent,
        "{CASE}: a machine never registered is absent"
    );
}

/// A first hello is matched to a registered machine only by the instance id that machine
/// is bound to: another instance id, none, a machine bound to no instance and a name never
/// registered are all `None`. Fails on a provider that matches a hello by name alone.
pub fn registered_verify_instance_matches_only_a_bound_instance(rig: &mut dyn RegisteredRig) {
    const CASE: &str = "registered_verify_instance_matches_only_a_bound_instance";
    rig.start_over();
    let bound = rig.register("reg-a", Some("i-1"));
    rig.register("reg-b", None);
    assert_eq!(
        rig.provider().verify_instance(&hello("reg-a", Some("i-1"))),
        Some(bound),
        "{CASE}: the hello of the bound instance is matched"
    );
    let refused = [
        ("reg-a", Some("i-2")),
        ("reg-a", None),
        ("reg-b", Some("i-1")),
        ("reg-never", Some("i-1")),
    ];
    for (name, instance) in refused {
        assert_eq!(
            rig.provider().verify_instance(&hello(name, instance)),
            None,
            "{CASE}: a hello of {name} with instance {instance:?} is not matched"
        );
    }
}

/// With no machine the provider supplies none and can add none; it supplies each machine
/// that registers, a silent one included, until the operator removes it, and never says it
/// can add one. Fails on a provider that offers a launch, or stops counting a machine that
/// is only silent.
pub fn registered_capacity_counts_machines_and_adds_none(rig: &mut dyn RegisteredRig) {
    const CASE: &str = "registered_capacity_counts_machines_and_adds_none";
    rig.start_over();
    let supplying = |supplied| Capacity {
        supplied,
        can_add: Some(0),
    };
    assert_eq!(
        rig.provider().capacity(),
        supplying(0),
        "{CASE}: an empty provider supplies nothing and can add none"
    );
    let first = rig.register("reg-a", None);
    let second = rig.register("reg-b", None);
    assert_eq!(
        rig.provider().capacity(),
        supplying(2),
        "{CASE}: two registered machines are supplied"
    );
    rig.silence(&second);
    assert_eq!(
        rig.provider().capacity(),
        supplying(2),
        "{CASE}: a silent machine is still supplied"
    );
    rig.remove(&first);
    assert_eq!(
        rig.provider().capacity(),
        supplying(1),
        "{CASE}: a removed machine is no longer supplied"
    );
}

/// The catalog is an empty answer, not an error, with machines or without; the
/// capabilities are empty with no machine, and once one has registered they are exactly
/// the operating system and architecture it reported. Fails on a provider whose catalog
/// errors (an error means "not known", and a static provider knows it has nothing to
/// launch), or whose capabilities do not follow what the machines reported: another
/// operating system, or no architecture.
pub fn registered_catalog_is_empty_and_capabilities_follow_the_machines(
    rig: &mut dyn RegisteredRig,
) {
    const CASE: &str = "registered_catalog_is_empty_and_capabilities_follow_the_machines";
    rig.start_over();
    assert_eq!(
        rig.provider().capabilities(),
        ProviderCaps::default(),
        "{CASE}: with no machine there is no capability"
    );
    assert_eq!(
        block_on(rig.provider().catalog()),
        Ok(Vec::new()),
        "{CASE}: the catalog is an empty answer, not an error"
    );
    rig.register("reg-a", None);
    let (os, arch) = rig.platform();
    let caps = rig.provider().capabilities();
    assert_eq!(
        caps.os.into_iter().collect::<Vec<_>>(),
        [os],
        "{CASE}: the operating systems are the one the machine reported"
    );
    assert_eq!(
        caps.arch.into_iter().collect::<Vec<_>>(),
        [arch],
        "{CASE}: the architectures are the one the machine reported"
    );
    assert_eq!(
        block_on(rig.provider().catalog()),
        Ok(Vec::new()),
        "{CASE}: with a machine the catalog is still empty"
    );
}

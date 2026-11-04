//! The `static` machine provider over the registry's host table: what it says it supplies,
//! that it never launches, and what a read of one host by id finds as the host goes
//! silent, is booked, and is removed.

mod support;

use std::collections::BTreeSet;
use std::time::Duration;

use js_agent_wire::{DEFAULT_GRACE_MS, InstanceIdentity};
use js_elastic::conformance::{REGISTERED_CASES, RegisteredRig, run_all_registered};
use js_elastic::{
    AcquireId, AcquireRequest, Acquired, BootPayload, Capacity, CapacityProvider, Claim, GpuClass,
    Hello, HostName, HostRef, HostStatus, Observed, Offer, PriceUnit, ProviderCaps, Reason,
    Refusal, RefusalCode, Released, SchedulerId,
};
use js_provider::Pool;
use js_registry::{DEFAULT_LOST_RETENTION_MS, HostState, StaticCapacity};
use js_types::ExecMode;
use support::*;

/// One hour on the manual clock: well within the default lost-host retention.
const HOUR_MS: u64 = 3_600_000;

fn advance(fx: &Fixture, millis: u64) {
    fx.clock.advance(Duration::from_millis(millis));
}

fn provider(fx: &Fixture) -> StaticCapacity {
    StaticCapacity::new(fx.registry.clone())
}

fn reference(id: &str) -> HostRef {
    HostRef(id.to_owned())
}

/// A hello from `host` that also reports `arch`.
fn on_arch(host: &str, arch: &str) -> js_agent_wire::Hello {
    let mut hello = hello(host);
    hello.capabilities.insert("arch".into(), arch.into());
    hello
}

/// A hello from `host` that also reports one GPU of `class`.
fn with_gpu(host: &str, class: &str) -> js_agent_wire::Hello {
    let mut hello = on_arch(host, "x86_64");
    hello.shape.gpus = 1;
    hello.shape.gpu_class = Some(class.into());
    hello
}

/// A hello from `host` that names the instance `id` it runs on.
fn on_instance(host: &str, id: &str) -> js_agent_wire::Hello {
    let mut hello = hello(host);
    hello.instance = Some(InstanceIdentity {
        provider: "builders".into(),
        instance_id: id.into(),
    });
    hello
}

fn offer(os: &str, gpus: u32, class: Option<&str>) -> Offer {
    Offer {
        id: "any".to_owned(),
        zone: None,
        cpu_millis: 1000,
        memory_bytes: 1 << 30,
        disk_bytes: 0,
        gpus,
        gpu_class: class.map(|class| GpuClass::parse(class).unwrap()),
        os: os.to_owned(),
        arch: "x86_64".to_owned(),
        isa_level: None,
        exec: ExecMode::Container,
        price_cents_per_hour: None,
        price_unit: PriceUnit::UsCents,
        available: true,
    }
}

fn acquire(id: &str, offer: Offer) -> AcquireRequest {
    let scheduler = SchedulerId::new("sched").unwrap();
    let acquire = AcquireId::new(id).unwrap();
    AcquireRequest {
        name: HostName::derive(&scheduler, &acquire),
        acquire,
        pool: "builders".to_owned(),
        offer,
        lifetime: None,
        labels: Vec::new(),
        payload: BootPayload::new("payload".to_owned()),
    }
}

fn refused_for_capacity() -> Acquired {
    Acquired::Refused(Refusal {
        code: RefusalCode::Capacity,
        reason: Reason::NotAllowed,
    })
}

fn hello_of(name: &str, instance: Option<&str>) -> Hello {
    Hello {
        name: name.to_owned(),
        instance_id: instance.map(str::to_owned),
    }
}

/// Asserts that an acquire the hosts could serve (a macOS offer like the pool's) and one
/// they could not (a Linux GPU offer) both answer `Refused(Capacity)`, and so does a
/// repeat of the first with the same acquire id, while the list, the capacity, the host
/// table and the lease bookings are the same after as before. It catches an acquire that
/// books a host, adds one to the table, or answers anything but the capacity code the
/// caller falls through on.
#[test]
fn acquire_on_static_is_refused_with_capacity() {
    let fx = fixture(4);
    fx.join("a", "t");
    fx.join("b", "t");
    let static_hosts = provider(&fx);
    let listed = static_hosts.list();
    let supplied = static_hosts.capacity();
    let table = fx.registry.hosts();
    let booked = fx.provider.booked();

    let servable = acquire("servable", offer("macos", 0, None));
    let impossible = acquire("impossible", offer("linux", 8, Some("nvidia-h100")));
    for request in [&servable, &impossible, &servable] {
        assert_eq!(static_hosts.acquire(request), refused_for_capacity());
    }

    assert_eq!(static_hosts.list(), listed);
    assert_eq!(static_hosts.capacity(), supplied);
    assert_eq!(fx.registry.hosts(), table);
    assert_eq!(fx.provider.booked(), booked);
    assert!(booked.is_empty());
    assert!(static_hosts.unresolved().is_empty());
}

/// Asserts, on the registry's manual clock, that a live host observes `Present`, that it
/// observes `Unknown` once it is silent past the grace period and still an hour later
/// (within the lost-host retention), and `Absent` only after the operator retires,
/// releases and removes it. A silent host the operator leaves alone observes `Absent`
/// once the retention has forgotten it (it held nothing). A host id never registered is
/// `Absent`, and text that is no host id is `Unknown`. It catches a provider that frees a
/// host for being silent (design 5.6), and one whose removed host stays `Unknown`.
#[test]
fn a_silent_host_observes_unknown_and_a_removed_host_absent() {
    let fx = fixture(4);
    fx.join("a", "t");
    fx.join("c", "t");
    let static_hosts = provider(&fx);
    let a = reference("a");
    let c = reference("c");
    assert_eq!(
        static_hosts.observe(&a),
        Observed::Present(HostStatus::Active)
    );

    fx.pass_grace();
    assert_eq!(fx.registry.hosts()[0].state, HostState::Lost);
    assert_eq!(static_hosts.observe(&a), Observed::Unknown);
    assert!(!static_hosts.is_gone(&a));
    advance(&fx, HOUR_MS);
    assert_eq!(static_hosts.observe(&a), Observed::Unknown);
    assert_eq!(static_hosts.observe(&c), Observed::Unknown);

    let host_a = host("a");
    fx.registry.retire(&host_a).unwrap();
    assert_eq!(static_hosts.observe(&a), Observed::Unknown);
    fx.registry.release_host(&host_a).unwrap();
    assert_eq!(static_hosts.observe(&a), Observed::Unknown);
    fx.registry.remove(&host_a).unwrap();
    assert_eq!(static_hosts.observe(&a), Observed::Absent);
    assert!(static_hosts.is_gone(&a));

    assert_eq!(static_hosts.observe(&c), Observed::Unknown);
    advance(&fx, DEFAULT_LOST_RETENTION_MS);
    assert_eq!(static_hosts.observe(&c), Observed::Absent);

    assert_eq!(static_hosts.observe(&reference("zz")), Observed::Absent);
    let not_an_id = reference("not a host id: at all");
    assert_eq!(static_hosts.observe(&not_an_id), Observed::Unknown);
}

/// Asserts that a host that holds a lease, silent for longer than the grace period and
/// the lost-host retention together, still observes `Unknown` and still holds its lease;
/// and that a host put back after a restart that has not said hello since (awaiting) is
/// `Unknown` too, listed as awaiting, and keeps its lease past the retention. It catches
/// a retention path that forgets, and so frees, a booked host.
#[test]
fn a_booked_silent_host_never_observes_absent() {
    let mut fx = fixture(4);
    fx.join("a", "t");
    assert!(matches!(
        fx.provider.start(spec(1)),
        js_provider::StartAck::Started { .. }
    ));
    fx.provider.seat(&host("w"), lease(2), None).unwrap();
    let static_hosts = provider(&fx);
    assert_eq!(static_hosts.observe(&reference("w")), Observed::Unknown);

    fx.pass_grace();
    advance(&fx, DEFAULT_LOST_RETENTION_MS + 1);
    for id in ["a", "w"] {
        assert_eq!(
            static_hosts.observe(&reference(id)),
            Observed::Unknown,
            "{id}"
        );
    }
    assert_eq!(fx.provider.booked().get(&host("a")), Some(&lease(1)));
    assert_eq!(fx.provider.booked().get(&host("w")), Some(&lease(2)));
    let statuses: Vec<(String, HostStatus)> = static_hosts
        .list()
        .unwrap()
        .into_iter()
        .map(|held| (held.id.to_string(), held.status))
        .collect();
    assert_eq!(
        statuses,
        vec![
            ("a".to_owned(), HostStatus::Unknown("lost".to_owned())),
            ("w".to_owned(), HostStatus::Unknown("awaiting".to_owned())),
        ]
    );
    assert_eq!(static_hosts.capacity().supplied, 2);
}

/// Asserts that the provider cannot launch, that its catalog is an empty answer and not
/// an error, and that with no host it lists none, supplies none and reports no
/// capability. With hosts it reports the operating systems, architectures (in the one
/// spelling) and GPU classes they reported, leaving out a class not in the agent's
/// spelling and a host put back after a restart that has reported nothing; it supplies
/// every host that is live, lost or awaiting and offers no more; a host marked releasing
/// is listed as terminating and no longer supplied. A release is refused as foreign and
/// changes nothing. It catches a catalog that errors, a capacity that counts a host the
/// table does not hold or offers a launch, and capabilities that drift from the hosts.
#[test]
fn static_provider_reports_hosts_supplied_and_nothing_to_launch() {
    let fx = fixture(8);
    let static_hosts = provider(&fx);
    assert_eq!(static_hosts.id().as_str(), "builders");
    assert!(!static_hosts.can_launch());
    assert_eq!(static_hosts.catalog(), Ok(Vec::new()));
    assert_eq!(static_hosts.list(), Ok(Vec::new()));
    assert_eq!(static_hosts.capabilities(), ProviderCaps::default());
    let none = Capacity {
        supplied: 0,
        can_add: Some(0),
    };
    assert_eq!(static_hosts.capacity(), none);
    assert!(static_hosts.events().is_none());
    assert!(static_hosts.checks().is_empty());
    assert!(static_hosts.host_prep(&offer("macos", 0, None)).is_empty());
    assert_eq!(static_hosts.payload_limit(), 0);

    fx.hello(&on_arch("a", "aarch64"), &cred("t")).unwrap();
    fx.hello(&with_gpu("b", "nvidia-a10"), &cred("t")).unwrap();
    fx.hello(&with_gpu("odd", "Not_A_Class"), &cred("t"))
        .unwrap();
    fx.provider.seat(&host("w"), lease(1), None).unwrap();
    let caps = static_hosts.capabilities();
    assert_eq!(caps.os, BTreeSet::from(["macos".to_owned()]));
    assert_eq!(
        caps.arch,
        BTreeSet::from(["arm64".to_owned(), "x86_64".to_owned()])
    );
    assert_eq!(
        caps.accelerators,
        BTreeSet::from([GpuClass::parse("nvidia-a10").unwrap()])
    );
    assert!(caps.whole_host && caps.host_ports);
    let four = Capacity {
        supplied: 4,
        can_add: Some(0),
    };
    assert_eq!(static_hosts.capacity(), four);

    fx.join("d", "t");
    let host_d = host("d");
    fx.registry.drain(&host_d).unwrap();
    assert_eq!(static_hosts.capacity().supplied, 5);
    fx.registry.release_host(&host_d).unwrap();
    assert_eq!(static_hosts.capacity(), four);
    assert_eq!(
        static_hosts.observe(&reference("d")),
        Observed::Present(HostStatus::Terminating)
    );

    let table = fx.registry.hosts();
    for id in ["a", "d", "zz"] {
        assert_eq!(
            static_hosts.release(&reference(id)),
            Released::Refused(Claim::Foreign),
            "{id}"
        );
    }
    assert_eq!(fx.registry.hosts(), table);

    let listed = static_hosts.list().unwrap();
    let ids: Vec<String> = listed.iter().map(|held| held.id.to_string()).collect();
    assert_eq!(ids, ["a", "b", "d", "odd", "w"]);
    let d = &listed[2];
    assert_eq!(d.name.as_deref(), Some("d"));
    assert_eq!(d.status, HostStatus::Terminating);
    assert_eq!((d.acquire.as_str(), d.offer.as_str()), ("", ""));
    assert_eq!(d.zone, None);
    assert_eq!(listed[0].status, HostStatus::Active);

    fx.pass_grace();
    assert_eq!(static_hosts.capacity(), four);
}

/// Asserts that a first hello carrying the instance id a host is bound to is matched to
/// that host, and that one with another instance id, with none, for a host bound to no
/// instance, for a host the table does not hold, or with a name that is no host id is
/// not; and that verifying admits nothing and changes no host or binding. It catches a
/// verification that matches by name alone, or that admits or creates a host.
#[test]
fn verify_instance_matches_only_a_bound_instance() {
    let fx = fixture(4);
    fx.hello(&on_instance("a", "i-1"), &cred("t")).unwrap();
    fx.join("b", "t");
    let static_hosts = provider(&fx);
    let table = fx.registry.hosts();
    let bindings = fx.registry.bindings();

    assert_eq!(
        static_hosts.verify_instance(&hello_of("a", Some("i-1"))),
        Some(reference("a"))
    );
    for refused in [
        hello_of("a", Some("i-2")),
        hello_of("a", None),
        hello_of("b", Some("i-1")),
        hello_of("b", None),
        hello_of("zz", Some("i-1")),
        hello_of("not a host id: at all", Some("i-1")),
    ] {
        assert_eq!(static_hosts.verify_instance(&refused), None, "{refused:?}");
    }

    assert_eq!(fx.registry.hosts(), table);
    assert_eq!(fx.registry.bindings(), bindings);
}

/// The registry's host table as the conformance suite's rig: a pool of up to eight hosts on
/// the manual clock, with the static provider over it.
struct StaticRig {
    fx: Fixture,
    provider: StaticCapacity,
    /// Each registered host that is not silenced or removed, with its epoch.
    live: Vec<(String, u64)>,
}

impl StaticRig {
    fn new() -> Self {
        let fx = fixture(8);
        let provider = provider(&fx);
        Self {
            fx,
            provider,
            live: Vec::new(),
        }
    }
}

impl RegisteredRig for StaticRig {
    fn provider(&self) -> &dyn CapacityProvider {
        &self.provider
    }

    fn start_over(&mut self) {
        *self = Self::new();
    }

    fn register(&mut self, name: &str, instance: Option<&str>) -> HostRef {
        let hello = match instance {
            Some(id) => on_instance(name, id),
            None => hello(name),
        };
        let welcome = self.fx.hello(&hello, &cred("t")).unwrap();
        self.live.push((name.to_owned(), welcome.epoch));
        reference(name)
    }

    /// Moves the clock past the grace period and has every other live host beat, so only
    /// `id` (and hosts silenced before) is lost.
    fn silence(&mut self, id: &HostRef) {
        self.live.retain(|(name, _)| name != id.as_str());
        advance(&self.fx, DEFAULT_GRACE_MS + 1);
        for (name, epoch) in &self.live {
            self.fx.beat(name, *epoch);
        }
    }

    /// The operator's path: retire, release, remove.
    fn remove(&mut self, id: &HostRef) {
        self.live.retain(|(name, _)| name != id.as_str());
        let id = host(id.as_str());
        self.fx.registry.retire(&id).unwrap();
        self.fx.registry.release_host(&id).unwrap();
        self.fx.registry.remove(&id).unwrap();
    }

    /// The registry answers from memory: its list cannot fail.
    fn fail_list(&mut self) -> bool {
        false
    }
}

/// Asserts that the static provider passes every case of the machine-provider conformance
/// suite's registered set (the cases for a provider that cannot launch), that the run
/// reports every case of the set as run, and that the provider says it cannot launch. It
/// proves the static provider meets the contract the launching providers meet, and fails
/// if any case is wrong: an acquire that launches or changes the table, a release that
/// gives a host back, a list that leaves out a silent host, a silent host read as absent,
/// a hello matched without its bound instance, a capacity that offers a launch, or a
/// catalog that errors.
#[test]
fn the_static_provider_passes_the_provider_conformance_suite() {
    let mut rig = StaticRig::new();
    assert!(!rig.provider().can_launch());
    assert_eq!(run_all_registered(&mut rig), REGISTERED_CASES.len());
}

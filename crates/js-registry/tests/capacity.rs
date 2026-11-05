//! The `static` machine provider over the registry's host table: what it says it supplies,
//! that it never launches, and what a read of one host by id finds as the host goes
//! silent, is booked, and is removed.

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js_agent_wire::{DEFAULT_GRACE_MS, Heartbeat, InstanceIdentity, Taint};
use js_elastic::conformance::{REGISTERED_CASES, RegisteredRig, block_on, run_all_registered};
use js_elastic::{
    AcquireId, AcquireRequest, Acquired, BootPayload, Capacity, CapacityProvider, Claim, Hello,
    HostName, HostRef, HostStatus, Observed, Offer, PriceUnit, ProviderCaps, Reason, Refusal,
    RefusalCode, Released, SchedulerId,
};
use js_provider::Pool;
use js_registry::{
    DEFAULT_LOST_RETENTION_MS, HostState, MIN_LOST_RETENTION_MS, PoolConfig, StaticCapacity,
};
use js_types::{
    ExecMode, Model, ProviderId, Request, ResourceClass, Resources, Shape, accelerator_of,
};
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
        accelerator: class.map(|class| accelerator_of(&Model::parse(class).unwrap(), gpus, None)),
        os: os.to_owned(),
        arch: "x86_64".to_owned(),
        isa_level: None,
        exec: ExecMode::Container,
        price_cents_per_hour: None,
        price_unit: PriceUnit::UsCents,
        available: true,
        purchase: None,
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

/// Asserts that an acquire the hosts could serve (a macOS x86_64 offer, which their
/// reported capabilities cover) and one they could not (a Linux GPU offer, which they do
/// not) both answer `Refused(Capacity)`, and so does a repeat of the first with the same
/// acquire id; and that a release of each host, booked live, booked lost, awaiting with a
/// lease and unbooked live, is refused as foreign. The bookings, the host table, the
/// bindings, the list and the capacity are the same after as before. It catches an
/// acquire that books a host or answers anything but the capacity code the caller falls
/// through on, and an acquire or a release that clears or changes a booking or a binding.
#[test]
fn acquire_on_static_is_refused_with_capacity() {
    let mut fx = fixture(8);
    let a = fx.hello(&on_arch("a", "x86_64"), &cred("t")).unwrap().epoch;
    assert_eq!(started_on(&fx.provider.start(spec(1))), Some("a"));
    fx.hello(&on_arch("l", "x86_64"), &cred("t")).unwrap();
    assert_eq!(started_on(&fx.provider.start(spec(2))), Some("l"));
    fx.pass_grace();
    fx.beat("a", a);
    fx.join("b", "t");
    fx.provider.seat(&host("w"), lease(3), None).unwrap();
    let static_hosts = provider(&fx);
    let states: Vec<(String, HostState)> = fx
        .registry
        .hosts()
        .into_iter()
        .map(|view| (view.id.as_str().to_owned(), view.state))
        .collect();
    assert_eq!(
        states,
        [
            ("a".to_owned(), HostState::Live),
            ("b".to_owned(), HostState::Live),
            ("l".to_owned(), HostState::Lost),
            ("w".to_owned(), HostState::Awaiting),
        ]
    );

    let servable = acquire("servable", offer("macos", 0, None));
    let impossible = acquire("impossible", offer("linux", 8, Some("nvidia-h100")));
    let caps = static_hosts.capabilities();
    assert!(caps.covers(&servable.offer), "{caps:?}");
    assert!(!caps.covers(&impossible.offer), "{caps:?}");

    let snapshot = |fx: &Fixture, provider: &StaticCapacity| {
        (
            fx.provider.booked(),
            fx.registry.hosts(),
            fx.registry.bindings(),
            provider.list(),
            provider.capacity(),
        )
    };
    let before = snapshot(&fx, &static_hosts);
    assert_eq!(before.0.len(), 3);
    for request in [&servable, &impossible, &servable] {
        assert_eq!(static_hosts.acquire(request), refused_for_capacity());
    }
    for id in ["a", "l", "w", "b"] {
        assert_eq!(
            static_hosts.release(&reference(id)),
            Released::Refused(Claim::Foreign),
            "{id}"
        );
    }
    assert_eq!(snapshot(&fx, &static_hosts), before);
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
    assert_eq!(block_on(static_hosts.catalog()), Ok(Vec::new()));
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
        BTreeSet::from([Model::parse("nvidia-a10").unwrap()])
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
        let mut hello = match instance {
            Some(id) => on_instance(name, id),
            None => hello(name),
        };
        hello.capabilities.insert("arch".into(), "x86_64".into());
        let welcome = self.fx.hello(&hello, &cred("t")).unwrap();
        self.live.push((name.to_owned(), welcome.epoch));
        reference(name)
    }

    fn platform(&self) -> (String, String) {
        ("macos".to_owned(), "x86_64".to_owned())
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

/// The one-minute lost-host retention of the boundary tests.
const RETENTION_MS: u64 = MIN_LOST_RETENTION_MS;

/// A pool of up to `max_machines` hosts that requires no operating system, with a
/// one-minute lost-host retention.
fn open_pool(max_machines: usize) -> Fixture {
    let shape = Shape::new(
        Resources::new(4000, 8 << 30, 0, None).unwrap(),
        BTreeMap::new(),
    );
    let name = ProviderId::new("builders").unwrap();
    let config = PoolConfig::new(name, class(), shape, max_machines);
    fixture_of(config.with_lost_retention(RETENTION_MS).unwrap())
}

/// A hello from `host` reporting `os`, `arch` and one GPU of `gpu`, each only when set.
fn reporting(
    host: &str,
    os: Option<&str>,
    arch: Option<&str>,
    gpu: Option<&str>,
) -> js_agent_wire::Hello {
    let mut hello = hello(host);
    hello.capabilities = BTreeMap::new();
    let reported = [("os", os), ("arch", arch)];
    for (key, value) in reported {
        value.into_iter().for_each(|value| {
            hello.capabilities.insert(key.into(), value.into());
        });
    }
    hello.shape.gpus = u64::from(gpu.is_some());
    hello.shape.gpu_class = gpu.map(Into::into);
    hello
}

/// The operator's way to give a host back: retire, release, remove.
fn operator_removes(fx: &Fixture, name: &str) {
    let id = host(name);
    fx.registry.retire(&id).unwrap();
    fx.registry.release_host(&id).unwrap();
    fx.registry.remove(&id).unwrap();
}

/// The capabilities as three plain lists: operating systems, architectures, GPU classes.
fn caps_of(provider: &StaticCapacity) -> (Vec<String>, Vec<String>, Vec<String>) {
    let caps = provider.capabilities();
    let classes = caps
        .accelerators
        .iter()
        .map(|class| class.as_str().to_owned());
    (
        caps.os.into_iter().collect(),
        caps.arch.into_iter().collect(),
        classes.collect(),
    )
}

fn texts(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

/// Asserts the exact capabilities through the life of a table: none while only an
/// awaiting host is in it; the union of several operating systems, architectures and GPU
/// classes once hosts report them (a host that leaves out its `os` or `arch` adds none);
/// a value drops out when its sole host is removed and stays while another host still
/// reports it; a replacement hello replaces what the host reported; and a host the
/// retention forgets takes its values with it. It catches capabilities kept stale after
/// a host changed or left, or a union that loses a value another host still reports.
#[test]
fn capabilities_follow_replacement_removal_and_forgetting() {
    let fx = open_pool(8);
    let static_hosts = provider(&fx);
    fx.provider.seat(&host("w"), lease(1), None).unwrap();
    assert_eq!(static_hosts.capabilities(), ProviderCaps::default());
    assert_eq!(static_hosts.capacity().supplied, 1);

    let hosts = [
        reporting("l1", Some("linux"), Some("x86_64"), Some("nvidia-a10")),
        reporting("l2", Some("linux"), Some("aarch64"), Some("nvidia-h100")),
        reporting("m", Some("macos"), None, None),
        reporting("n", None, Some("x86_64"), None),
    ];
    let epochs: Vec<u64> = hosts
        .iter()
        .map(|hello| fx.hello(hello, &cred("t")).unwrap().epoch)
        .collect();
    assert_eq!(
        caps_of(&static_hosts),
        (
            texts(&["linux", "macos"]),
            texts(&["arm64", "x86_64"]),
            texts(&["nvidia-a10", "nvidia-h100"]),
        )
    );

    operator_removes(&fx, "l2");
    assert_eq!(
        caps_of(&static_hosts),
        (
            texts(&["linux", "macos"]),
            texts(&["x86_64"]),
            texts(&["nvidia-a10"])
        )
    );
    operator_removes(&fx, "l1");
    assert_eq!(
        caps_of(&static_hosts),
        (texts(&["macos"]), texts(&["x86_64"]), texts(&[]))
    );

    fx.pass_grace();
    let replaced = reporting("m", Some("linux"), Some("aarch64"), None);
    let m = fx.hello(&replaced, &cred("t")).unwrap().epoch;
    assert!(m > epochs[2]);
    assert_eq!(
        caps_of(&static_hosts),
        (texts(&["linux"]), texts(&["arm64", "x86_64"]), texts(&[]))
    );

    advance(&fx, DEFAULT_GRACE_MS);
    fx.beat("m", m);
    advance(&fx, RETENTION_MS);
    assert_eq!(static_hosts.observe(&reference("n")), Observed::Absent);
    assert_eq!(
        caps_of(&static_hosts),
        (texts(&["linux"]), texts(&["arm64"]), texts(&[]))
    );
}

/// Asserts that a lost host that beats again, and an awaiting host whose agent says hello,
/// observe `Present` again with their leases still booked on them; and that a host whose
/// whole-node lease is committed but not yet reported ready observes `Present` as a
/// machine while that lease reads `Unknown`: the machine is there, the allocation is not
/// ready. It catches a provider that keeps a recovered host `Unknown`, drops a booking on
/// recovery, or reads a lease's readiness as the machine's presence.
#[test]
fn a_host_recovers_from_silence_and_a_preparing_host_is_present() {
    let mut fx = fixture(4);
    let a = fx.join("a", "t");
    assert_eq!(started_on(&fx.provider.start(spec(1))), Some("a"));
    fx.provider.seat(&host("w"), lease(2), None).unwrap();
    let static_hosts = provider(&fx);
    fx.pass_grace();
    for id in ["a", "w"] {
        assert_eq!(
            static_hosts.observe(&reference(id)),
            Observed::Unknown,
            "{id}"
        );
    }
    fx.beat("a", a);
    fx.join("w", "t");
    for id in ["a", "w"] {
        assert_eq!(
            static_hosts.observe(&reference(id)),
            Observed::Present(HostStatus::Active),
            "{id}"
        );
    }
    assert_eq!(fx.provider.booked().get(&host("a")), Some(&lease(1)));
    assert_eq!(fx.provider.booked().get(&host("w")), Some(&lease(2)));

    let whole = ResourceClass::whole_node();
    let name = ProviderId::new("builders").unwrap();
    let mut fx = fixture_of(PoolConfig::new(name, whole.clone(), pool_shape(), 4));
    fx.join("h", "t");
    let request = Request {
        class: whole.id().clone(),
        ..request()
    };
    let spec = js_provider::StartSpec::new(lease(1), request, &[whole]).unwrap();
    assert_eq!(fx.provider.start(spec), js_provider::StartAck::Unknown);
    assert_eq!(fx.provider.booked().get(&host("h")), Some(&lease(1)));
    assert_eq!(
        fx.provider.observe(&lease(1)),
        js_provider::Observed::Unknown
    );
    assert_eq!(
        provider(&fx).observe(&reference("h")),
        Observed::Present(HostStatus::Active)
    );
}

/// Asserts the boundaries on the manual clock with a one-minute retention: a host is
/// `Present` exactly at the grace period and `Unknown` one millisecond later, still
/// `Unknown` exactly at grace plus retention and `Absent` one millisecond later. An
/// awaiting host whose lease has ended is kept until grace plus retention after it was
/// seated and forgotten one millisecond later. A tainted host, and a cordoned one while
/// its cordon holds, stay `Unknown` long past the retention; the cordoned one is
/// forgotten once its cordon has lapsed. It catches an off-by-one at either deadline, a
/// retention that ignores its setting, and one that forgets a host kept for its taint or
/// cordon.
#[test]
fn grace_and_retention_boundaries_decide_unknown_and_absent() {
    let fx = open_pool(4);
    let static_hosts = provider(&fx);
    let a = reference("a");
    fx.hello(&reporting("a", None, None, None), &cred("t"))
        .unwrap();
    advance(&fx, DEFAULT_GRACE_MS);
    assert_eq!(
        static_hosts.observe(&a),
        Observed::Present(HostStatus::Active)
    );
    advance(&fx, 1);
    assert_eq!(static_hosts.observe(&a), Observed::Unknown);
    advance(&fx, RETENTION_MS - 1);
    assert_eq!(static_hosts.observe(&a), Observed::Unknown);
    advance(&fx, 1);
    assert_eq!(static_hosts.observe(&a), Observed::Absent);

    let mut fx = open_pool(4);
    let static_hosts = provider(&fx);
    let w = reference("w");
    fx.provider.seat(&host("w"), lease(1), None).unwrap();
    assert!(matches!(
        fx.provider.stop(&lease(1)),
        js_provider::StopAck::Stopped { .. }
    ));
    advance(&fx, DEFAULT_GRACE_MS + RETENTION_MS);
    assert_eq!(static_hosts.observe(&w), Observed::Unknown);
    advance(&fx, 1);
    assert_eq!(static_hosts.observe(&w), Observed::Absent);

    let fx = open_pool(4);
    let static_hosts = provider(&fx);
    let tainted = fx
        .hello(&reporting("t", None, None, None), &cred("t"))
        .unwrap()
        .epoch;
    fx.hello(&reporting("c", None, None, None), &cred("t"))
        .unwrap();
    let report = Heartbeat {
        taint: Some(Taint {
            reason: "residue".to_owned(),
            run_id: None,
            check: None,
        }),
        ..fx.heartbeat("t", tainted)
    };
    fx.registry.heartbeat(&report, &cred("t")).unwrap();
    let kept_for = DEFAULT_GRACE_MS + RETENTION_MS;
    let cordon = Duration::from_millis(kept_for + 2 * HOUR_MS);
    fx.registry.cordon(&host("c"), cordon).unwrap();
    advance(&fx, kept_for + HOUR_MS);
    for id in ["t", "c"] {
        assert_eq!(
            static_hosts.observe(&reference(id)),
            Observed::Unknown,
            "{id}"
        );
    }
    advance(&fx, 2 * HOUR_MS);
    assert_eq!(static_hosts.observe(&reference("t")), Observed::Unknown);
    assert_eq!(static_hosts.observe(&reference("c")), Observed::Absent);
}

/// Asserts that a host's bound instance still matches while the host is silent and,
/// after a restart, while it awaits its first hello with the binding put back; that it no
/// longer matches once the operator removes the host or the retention forgets it; and
/// that the same host id registered again on another instance matches that instance and
/// not the old one. It catches a verification that keeps a stale identity after the host
/// left or changed machines, and one that loses a binding put back after a restart.
#[test]
fn verify_instance_follows_the_binding_through_silence_restart_and_reuse() {
    let fx = open_pool(4);
    let static_hosts = provider(&fx);
    fx.hello(&on_instance("a", "i-1"), &cred("t")).unwrap();
    fx.hello(&on_instance("f", "i-9"), &cred("t")).unwrap();
    fx.pass_grace();
    assert_eq!(
        static_hosts.verify_instance(&hello_of("a", Some("i-1"))),
        Some(reference("a"))
    );

    let binding = fx.registry.bindings()[&host("a")].clone();
    let restarted = open_pool(4);
    restarted
        .provider
        .seat(&host("a"), lease(1), Some(binding))
        .unwrap();
    let after_restart = provider(&restarted);
    assert_eq!(restarted.registry.hosts()[0].state, HostState::Awaiting);
    assert_eq!(
        after_restart.verify_instance(&hello_of("a", Some("i-1"))),
        Some(reference("a"))
    );
    assert_eq!(
        after_restart.verify_instance(&hello_of("a", Some("i-2"))),
        None
    );

    operator_removes(&fx, "a");
    assert_eq!(
        static_hosts.verify_instance(&hello_of("a", Some("i-1"))),
        None
    );
    advance(&fx, RETENTION_MS);
    assert_eq!(static_hosts.observe(&reference("f")), Observed::Absent);
    assert_eq!(
        static_hosts.verify_instance(&hello_of("f", Some("i-9"))),
        None
    );

    fx.hello(&on_instance("a", "i-2"), &cred("t")).unwrap();
    assert_eq!(
        static_hosts.verify_instance(&hello_of("a", Some("i-2"))),
        Some(reference("a"))
    );
    assert_eq!(
        static_hosts.verify_instance(&hello_of("a", Some("i-1"))),
        None
    );
}

/// Asserts that a lost host holding a lease and an awaiting host holding one, both marked
/// as ending, list as terminating, observe `Unknown` (they are silent), are no longer
/// supplied, and keep their leases. It catches a status that lets the host's silence hide
/// its releasing mark, a capacity that still counts an ending host, and a mark that frees
/// a booking.
#[test]
fn releasing_lost_and_awaiting_hosts_list_as_terminating() {
    let mut fx = fixture(4);
    fx.join("l", "t");
    assert_eq!(started_on(&fx.provider.start(spec(1))), Some("l"));
    fx.pass_grace();
    fx.provider.seat(&host("w"), lease(2), None).unwrap();
    let static_hosts = provider(&fx);
    assert_eq!(static_hosts.capacity().supplied, 2);
    for id in ["l", "w"] {
        fx.registry.mark_ending(&host(id));
    }
    let statuses: Vec<(String, HostStatus)> = static_hosts
        .list()
        .unwrap()
        .into_iter()
        .map(|held| (held.id.to_string(), held.status))
        .collect();
    assert_eq!(
        statuses,
        [
            ("l".to_owned(), HostStatus::Terminating),
            ("w".to_owned(), HostStatus::Terminating),
        ]
    );
    for id in ["l", "w"] {
        assert_eq!(
            static_hosts.observe(&reference(id)),
            Observed::Unknown,
            "{id}"
        );
    }
    assert_eq!(static_hosts.capacity().supplied, 0);
    assert_eq!(fx.provider.booked().get(&host("l")), Some(&lease(1)));
    assert_eq!(fx.provider.booked().get(&host("w")), Some(&lease(2)));
}

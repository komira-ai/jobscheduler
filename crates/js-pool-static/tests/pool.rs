//! Behaviour of the static pool through the public provider interface.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use js_pool_static::{ConfigError, EXAMPLE_CONFIG, SeatError, StaticPool};
use js_provider::{Flavour, Observed, Provider, RefusalCode, StartAck, StartSpec, StopAck};
use js_types::{
    Admission, ClassId, HostId, LeaseId, ProviderId, Request, ResourceClass, Resources,
};

fn pool(config: &str) -> StaticPool {
    StaticPool::from_toml(ProviderId::new("pool").unwrap(), config).unwrap()
}

fn example() -> StaticPool {
    pool(EXAMPLE_CONFIG)
}

fn request(gpu_class: &str, cpu_millis: u64) -> Request {
    Request {
        class: ClassId::new("gpu-host").unwrap(),
        resources: Resources::new(cpu_millis, 1, 1, Some(gpu_class.into())).unwrap(),
        capabilities: BTreeMap::new(),
        max_wall: Duration::from_secs(600),
        labels: BTreeMap::new(),
        validation_run: None,
    }
}

fn spec(seq: u64, request: Request) -> StartSpec {
    StartSpec::new(LeaseId::new(1, seq), request, &[ResourceClass::gpu_host()]).unwrap()
}

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

/// Asserts that the pool reports its id, the delegated flavour and the one shipped
/// class.
#[test]
fn identity_flavour_and_classes() {
    let p = example();
    assert_eq!(p.id().as_str(), "pool");
    assert_eq!(p.flavour(), Flavour::Delegated);
    assert_eq!(p.classes(), vec![ResourceClass::gpu_host()]);
}

/// Asserts that leases take the first free fitting host by id, that a booked host is
/// skipped, and that a third lease on a two-host pool is refused for capacity.
#[test]
fn placement_is_first_free_host_by_id() {
    let mut p = example();
    let first = p.start(spec(1, request("nvidia-a100", 1000)));
    assert_eq!(
        first,
        StartAck::Started {
            host: host("gpu-01")
        }
    );
    let second = p.start(spec(2, request("nvidia-a100", 1000)));
    assert_eq!(
        second,
        StartAck::Started {
            host: host("gpu-02")
        }
    );
    let third = p.start(spec(3, request("nvidia-a100", 1000)));
    assert_eq!(third, StartAck::Refused(RefusalCode::NoCapacity));
    assert_eq!(p.booked().len(), 2);
}

/// Asserts that a host that does not fit is skipped even when it sorts first.
#[test]
fn a_host_that_does_not_fit_is_skipped() {
    let config = "[[host]]\nid = \"a\"\ngpu_class = \"g\"\n[host.resources]\ncpu_millis = 1000\nmemory_bytes = 8\ngpus = 1\n\n\
                  [[host]]\nid = \"b\"\ngpu_class = \"g\"\n[host.resources]\ncpu_millis = 9000\nmemory_bytes = 8\ngpus = 1\n";
    let mut p = pool(config);
    let ack = p.start(spec(1, request("g", 5000)));
    assert_eq!(ack, StartAck::Started { host: host("b") });
}

/// Asserts that a request no host could satisfy is refused as a misfit, not as a
/// wait: wrong GPU class, too much CPU, or a missing capability.
#[test]
fn a_request_no_host_satisfies_is_no_fit() {
    let mut p = example();
    let wrong_class = p.start(spec(1, request("nvidia-h100", 1000)));
    assert_eq!(wrong_class, StartAck::Refused(RefusalCode::NoFit));
    let too_big = p.start(spec(2, request("nvidia-a100", 64001)));
    assert_eq!(too_big, StartAck::Refused(RefusalCode::NoFit));
    let mut needs_rdma = request("nvidia-a100", 1000);
    needs_rdma.capabilities.insert("rdma".into(), "yes".into());
    assert_eq!(
        p.start(spec(3, needs_rdma)),
        StartAck::Refused(RefusalCode::NoFit)
    );
    assert!(p.booked().is_empty());
}

/// Asserts that a required capability the host reports is honoured.
#[test]
fn required_capabilities_are_matched() {
    let mut p = example();
    let mut wants = request("nvidia-a100", 1000);
    wants.capabilities.insert("driver".into(), "550".into());
    assert_eq!(
        p.start(spec(1, wants)),
        StartAck::Started {
            host: host("gpu-01")
        }
    );
}

/// Asserts that a second start of the same lease answers the same host and books
/// nothing more.
#[test]
fn start_is_idempotent_on_the_lease() {
    let mut p = example();
    let a = p.start(spec(1, request("nvidia-a100", 1000)));
    let b = p.start(spec(1, request("nvidia-a100", 1000)));
    assert_eq!(a, b);
    assert_eq!(p.booked().len(), 1);
}

/// Asserts that a class the pool does not serve is refused, even when the request was
/// checked against the caller's own class list.
#[test]
fn an_unserved_class_is_refused() {
    let mut p = example();
    let id = ClassId::new("packed").unwrap();
    let packed = ResourceClass::new(id.clone(), Admission::Book(Default::default()), 0);
    let mut r = request("nvidia-a100", 1000);
    r.class = id;
    let s = StartSpec::new(LeaseId::new(1, 1), r, &[packed]).unwrap();
    assert_eq!(p.start(s), StartAck::Refused(RefusalCode::UnsupportedClass));
}

/// Asserts that stop frees the host, reads back Absent, and that the host is leased
/// again by the next start.
#[test]
fn stop_frees_the_host_and_reads_back_absent() {
    let mut p = example();
    p.start(spec(1, request("nvidia-a100", 1000)));
    let lease = LeaseId::new(1, 1);
    assert_eq!(
        p.stop(&lease),
        StopAck::Stopped {
            read_back: Observed::Absent
        }
    );
    assert!(p.booked().is_empty());
    let again = p.start(spec(2, request("nvidia-a100", 1000)));
    assert_eq!(
        again,
        StartAck::Started {
            host: host("gpu-01")
        }
    );
}

/// Asserts that stopping a lease the pool never held is a stop that reads back Absent.
#[test]
fn stopping_an_unknown_lease_reads_back_absent() {
    let mut p = example();
    let ack = p.stop(&LeaseId::new(9, 9));
    assert_eq!(
        ack,
        StopAck::Stopped {
            read_back: Observed::Absent
        }
    );
}

/// Asserts that observe answers from the pool's own table, Present while held and
/// Absent otherwise, and never Unknown.
#[test]
fn observe_answers_present_or_absent() {
    let mut p = example();
    let lease = LeaseId::new(1, 1);
    assert_eq!(p.observe(&lease), Observed::Absent);
    p.start(spec(1, request("nvidia-a100", 1000)));
    assert_eq!(
        p.observe(&lease),
        Observed::Present {
            host: host("gpu-01")
        }
    );
}

/// Asserts that capacity lists every host with its shape and the lease holding it.
#[test]
fn capacity_lists_hosts_and_holders() {
    let mut p = example();
    p.start(spec(1, request("nvidia-a100", 1000)));
    let listed = p.capacity();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].host, host("gpu-01"));
    assert_eq!(listed[0].leased_to, Some(LeaseId::new(1, 1)));
    assert_eq!(listed[1].leased_to, None);
    assert_eq!(listed[1].shape.resources().gpus(), 8);
}

/// Asserts that the pool loads from a file path and that an unreadable path is
/// refused with the path named.
#[test]
fn config_file_loads_and_missing_file_is_refused() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("example.toml");
    let id = ProviderId::new("pool").unwrap();
    let loaded = StaticPool::from_config_file(id.clone(), &path).unwrap();
    assert_eq!(loaded.capacity().len(), 2);
    let missing = Path::new(env!("CARGO_MANIFEST_DIR")).join("absent.toml");
    let err = StaticPool::from_config_file(id, &missing).unwrap_err();
    assert!(matches!(err, ConfigError::Read { .. }));
    assert!(err.to_string().contains("absent.toml"));
}

/// A class admitted by `Book` (it gates only on GPUs), which a static pool does not lease.
fn shared_class() -> ResourceClass {
    let gate = std::collections::BTreeSet::from([js_types::Dim::Gpus]);
    let id = ClassId::new("gpu-shared").unwrap();
    ResourceClass::new(id, Admission::Book(gate), 1)
}

/// Asserts that a class admitted by `Book` is refused as unsupported and books
/// nothing, while a `BookAll` class of the same pool still gets a whole host, so a
/// class added as data never silently receives whole-host semantics.
#[test]
fn a_class_that_is_not_book_all_is_refused() {
    let mut pool = example().with_classes(vec![ResourceClass::gpu_host(), shared_class()]);
    let known = [ResourceClass::gpu_host(), shared_class()];
    let mut shared = request("nvidia-a100", 1000);
    shared.class = shared_class().id().clone();
    let shared = StartSpec::new(LeaseId::new(1, 1), shared, &known).unwrap();
    assert_eq!(
        pool.start(shared),
        StartAck::Refused(RefusalCode::UnsupportedClass)
    );
    assert!(pool.booked().is_empty());
    let whole = StartSpec::new(LeaseId::new(1, 2), request("nvidia-a100", 1000), &known).unwrap();
    assert!(matches!(pool.start(whole), StartAck::Started { .. }));
    assert_eq!(pool.booked().len(), 1);
}

/// Asserts that a start of a stopped lease id is refused with `LeaseEnded`, books
/// nothing and leaves the lease Absent, that a stop that arrives before the lease was
/// ever started ends it the same way, and that a different lease still starts.
#[test]
fn a_stopped_lease_never_starts_again() {
    let mut p = example();
    let ended = LeaseId::new(1, 1);
    p.start(spec(1, request("nvidia-a100", 1000)));
    p.stop(&ended);
    let late = p.start(spec(1, request("nvidia-a100", 1000)));
    assert_eq!(late, StartAck::Refused(RefusalCode::LeaseEnded));
    assert!(p.booked().is_empty());
    assert_eq!(p.observe(&ended), Observed::Absent);

    p.stop(&LeaseId::new(1, 2));
    let early_stop = p.start(spec(2, request("nvidia-a100", 1000)));
    assert_eq!(early_stop, StartAck::Refused(RefusalCode::LeaseEnded));
    assert!(p.booked().is_empty());

    let fresh = p.start(spec(3, request("nvidia-a100", 1000)));
    assert_eq!(
        fresh,
        StartAck::Started {
            host: host("gpu-01")
        }
    );
}

/// Asserts that a seated lease is booked as a start would have booked it: it observes
/// `Present` on its host, `booked` lists it, a start places the next lease around it, a
/// second seat of the same pair changes nothing, and a stop frees the host.
#[test]
fn a_seated_lease_is_booked_like_a_started_one() {
    let mut p = example();
    let lease = LeaseId::new(3, 7);
    p.seat(&host("gpu-01"), lease).unwrap();
    p.seat(&host("gpu-01"), lease).unwrap();
    assert_eq!(
        p.observe(&lease),
        Observed::Present {
            host: host("gpu-01")
        }
    );
    assert_eq!(p.booked(), BTreeMap::from([(host("gpu-01"), lease)]));

    let next = p.start(spec(1, request("nvidia-a100", 1000)));
    assert_eq!(
        next,
        StartAck::Started {
            host: host("gpu-02")
        }
    );
    assert!(matches!(p.stop(&lease), StopAck::Stopped { .. }));
    assert_eq!(p.observe(&lease), Observed::Absent);
    assert_eq!(p.booked().len(), 1);
}

/// Asserts that a seat the pool cannot honour books nothing and says why: a host it
/// does not list, a host another lease holds, a lease that holds another host, and a
/// lease that was stopped.
#[test]
fn a_seat_that_cannot_be_honoured_is_refused() {
    let mut p = example();
    let (a, b) = (LeaseId::new(1, 1), LeaseId::new(1, 2));
    p.seat(&host("gpu-01"), a).unwrap();

    let unlisted = p.seat(&host("gpu-99"), b).unwrap_err();
    assert_eq!(unlisted, SeatError::Unlisted(host("gpu-99")));
    let taken = p.seat(&host("gpu-01"), b).unwrap_err();
    let holder = SeatError::Taken {
        host: host("gpu-01"),
        holder: a,
    };
    assert_eq!(taken, holder);
    let elsewhere = p.seat(&host("gpu-02"), a).unwrap_err();
    let held = SeatError::Elsewhere {
        lease: a,
        host: host("gpu-01"),
    };
    assert_eq!(elsewhere, held);
    p.stop(&b);
    assert_eq!(p.seat(&host("gpu-02"), b).unwrap_err(), SeatError::Ended(b));

    assert_eq!(p.booked(), BTreeMap::from([(host("gpu-01"), a)]));
    for err in [unlisted, taken, elsewhere] {
        assert!(!err.to_string().is_empty());
    }
    assert_eq!(SeatError::Ended(b).to_string(), "lease 1-2 was stopped");
}

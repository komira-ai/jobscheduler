//! Behaviour of the static pool through the public provider interface.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use js_pool_static::{ConfigError, EXAMPLE_CONFIG, StaticPool};
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

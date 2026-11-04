//! A lease that shares a host books its GPUs under the host's own class: a request for the
//! family `nvidia-a100` is admitted on a host of `nvidia-a100-sxm4-80gb`, and books room there.

use std::collections::BTreeMap;
use std::time::Duration;

use js_pool_static::HostBook;
use js_provider::{RefusalCode, StartAck, StartSpec};
use js_types::{Admission, ClassId, HostId, LeaseId, Request, ResourceClass, Resources, Shape};

const HOST_CLASS: &str = "nvidia-a100-sxm4-80gb";
const ANY: &dyn Fn(&HostId, &Shape) -> bool = &|_, _| true;

fn host() -> HostId {
    HostId::new("a").unwrap()
}

fn sharing() -> ResourceClass {
    ResourceClass::new(
        ClassId::new("share").unwrap(),
        Admission::Book(Default::default()),
        0,
    )
}

fn gpus(count: u64, class: &str) -> Resources {
    Resources::new(1000, 1, count, Some(class.to_string())).unwrap()
}

fn eight_gpu_book() -> HostBook {
    let offered = Resources::new(100_000, 100, 8, Some(HOST_CLASS.to_string()))
        .unwrap()
        .with_disk_bytes(100);
    let mut book = HostBook::new();
    book.insert_host(host(), Shape::new(offered, BTreeMap::new()));
    book
}

fn spec(seq: u64, resources: Resources) -> StartSpec {
    let class = sharing();
    let request = Request {
        class: class.id().clone(),
        resources,
        capabilities: BTreeMap::new(),
        max_wall: Duration::from_secs(60),
        labels: BTreeMap::new(),
        validation_run: None,
        probe_host: None,
    };
    StartSpec::new(LeaseId::new(1, seq), request, std::slice::from_ref(&class)).unwrap()
}

/// Asserts that single-GPU requests for the family `nvidia-a100` start on an empty host of
/// `nvidia-a100-sxm4-80gb` until its eight GPUs are used, that the ninth is refused for
/// capacity, and that each booking is held under the host's class. Catches a booking that
/// compared class strings and refused the first request of a family on a variant host.
#[test]
fn a_family_request_shares_a_variant_host_until_its_gpus_are_used() {
    let mut book = eight_gpu_book();
    for seq in 1..=8 {
        let ack = book.start_where(&spec(seq, gpus(1, "nvidia-a100")), None, ANY);
        assert_eq!(ack, StartAck::Started { host: host() });
    }
    let held = book.held(&LeaseId::new(1, 1)).unwrap();
    assert_eq!(held.resources.gpu_class(), Some(HOST_CLASS));
    assert_eq!(held.resources.gpus(), 1);
    let ninth = book.start_where(&spec(9, gpus(1, "nvidia-a100")), None, ANY);
    assert_eq!(ninth, StartAck::Refused(RefusalCode::NoCapacity));
}

/// Asserts that a stored share of class `nvidia-a100` is seated back on the variant host,
/// is held under the host's class and counts against its room: seven more GPUs fit and an
/// eighth does not. Catches a recovery path that refused the share for its class spelling.
#[test]
fn seating_a_family_share_on_a_variant_host_counts_against_room() {
    let mut book = eight_gpu_book();
    let seated = book.seat_as(&host(), LeaseId::new(1, 1), Some(gpus(1, "nvidia-a100")));
    assert_eq!(seated, Ok(()));
    let held = book.held(&LeaseId::new(1, 1)).unwrap();
    assert_eq!(held.resources.gpu_class(), Some(HOST_CLASS));
    let seven = Resources::new(1000, 1, 7, Some("nvidia-a100".to_string())).unwrap();
    let all = book.start_where(&spec(2, seven), None, ANY);
    assert_eq!(all, StartAck::Started { host: host() });
    let more = book.start_where(&spec(3, gpus(1, "nvidia-a100")), None, ANY);
    assert_eq!(more, StartAck::Refused(RefusalCode::NoCapacity));
}

/// Asserts that a request for another model is not started on the host and that a lease
/// asking no GPU books its resources unchanged, with the disk it asked for, beside a GPU
/// lease. Catches a booking that rewrote a class the host does not serve, or dropped disk.
#[test]
fn another_model_is_not_started_and_a_cpu_lease_books_as_asked() {
    let mut book = eight_gpu_book();
    let other = book.start_where(&spec(1, gpus(1, "nvidia-h100")), None, ANY);
    assert_eq!(other, StartAck::Refused(RefusalCode::NoFit));
    let plain = Resources::new(1000, 1, 0, None).unwrap().with_disk_bytes(7);
    let cpu = book.start_where(&spec(2, plain.clone()), None, ANY);
    assert_eq!(cpu, StartAck::Started { host: host() });
    assert_eq!(book.held(&LeaseId::new(1, 2)).unwrap().resources, plain);
    let with_disk = gpus(2, "nvidia-a100").with_disk_bytes(5);
    let gpu = book.start_where(&spec(3, with_disk), None, ANY);
    assert_eq!(gpu, StartAck::Started { host: host() });
    let held = book.held(&LeaseId::new(1, 3)).unwrap();
    assert_eq!(held.resources.disk_bytes(), 5);
    assert_eq!(held.resources.gpu_class(), Some(HOST_CLASS));
}

/// Asserts that a GPU share seated on a host with no GPU class is booked as given, and is
/// then refused for room because the host has no GPU. Catches a rewrite that invented a class.
#[test]
fn a_gpu_share_on_a_host_without_a_class_is_booked_as_given() {
    let mut book = HostBook::new();
    let cpu_only = Resources::new(100_000, 100, 0, None).unwrap();
    book.insert_host(host(), Shape::new(cpu_only, BTreeMap::new()));
    let seated = book.seat_as(&host(), LeaseId::new(1, 1), Some(gpus(1, "nvidia-a100")));
    assert_eq!(seated, Err(js_pool_static::SeatError::NoRoom(host())));
}

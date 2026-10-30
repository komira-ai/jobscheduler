//! The share of a static host's memory the pool books: `memory_schedulable_percent`.

use std::collections::BTreeMap;
use std::time::Duration;

use js_pool_static::{ConfigError, StaticPool};
use js_provider::{Provider, RefusalCode, StartAck, StartSpec};
use js_types::{ClassId, LeaseId, MemoryPercent, ProviderId, Request, ResourceClass, Resources};

const GIB: u64 = 1 << 30;

/// The memory the hosts below list: 251 GiB.
const LISTED: u64 = 251 * GIB;

/// What 90 percent of it books, rounded down.
const NINETY: u64 = LISTED * 90 / 100;

fn hosts(percent: &str) -> String {
    format!(
        "{percent}[[host]]\nid = \"big\"\ngpu_class = \"nvidia-a100\"\n[host.resources]\ncpu_millis = 64000\nmemory_bytes = {LISTED}\ngpus = 1\n"
    )
}

fn pool(text: &str) -> StaticPool {
    StaticPool::from_toml(ProviderId::new("pool").unwrap(), text).unwrap()
}

fn asking(seq: u64, memory_bytes: u64) -> StartSpec {
    let request = Request {
        class: ClassId::new("gpu-host").unwrap(),
        resources: Resources::new(1000, memory_bytes, 1, Some("nvidia-a100".into())).unwrap(),
        capabilities: BTreeMap::new(),
        max_wall: Duration::from_secs(600),
        labels: BTreeMap::new(),
        validation_run: None,
    };
    StartSpec::new(LeaseId::new(1, seq), request, &[ResourceClass::gpu_host()]).unwrap()
}

fn refused(text: &str) -> String {
    let id = ProviderId::new("pool").unwrap();
    StaticPool::from_toml(id, text).unwrap_err().to_string()
}

/// Asserts that at 90 a host listing 251 GiB books exactly floor(251 GiB x 90 / 100)
/// bytes: a lease of that many bytes is admitted and a lease of one byte more finds no
/// host that fits. It catches a percent that is not applied, one that rounds up, and
/// one applied to the request instead of the host.
#[test]
fn ninety_percent_books_exactly_the_cut_memory() {
    let mut exact = pool(&hosts("memory_schedulable_percent = 90\n"));
    let ack = exact.start(asking(1, NINETY));
    assert!(matches!(ack, StartAck::Started { .. }), "{ack:?}");
    let mut over = pool(&hosts("memory_schedulable_percent = 90\n"));
    let ack = over.start(asking(1, NINETY + 1));
    assert_eq!(ack, StartAck::Refused(RefusalCode::NoFit));
}

/// Asserts that a file without the key books all of the memory it lists, and that 100
/// written out is the same: the default changes nothing. One byte more than the listed
/// memory is refused, the listed memory is admitted.
#[test]
fn the_default_of_a_hundred_books_all_of_it() {
    for text in [hosts(""), hosts("memory_schedulable_percent = 100\n")] {
        let mut whole = pool(&text);
        let ack = whole.start(asking(1, LISTED));
        assert!(matches!(ack, StartAck::Started { .. }), "{ack:?}");
        let mut over = pool(&text);
        let ack = over.start(asking(1, LISTED + 1));
        assert_eq!(ack, StartAck::Refused(RefusalCode::NoFit));
    }
}

/// Asserts that 0, 101, a negative number, a word and a fraction are refused when the
/// file is read, each message naming the key (the last three also the line). It catches a
/// percent of 0 that books nothing without saying so, and one above 100 that books more than the host has.
#[test]
fn zero_and_above_a_hundred_are_refused() {
    for bad in ["0", "101", "1000", "256"] {
        let said = refused(&hosts(&format!("memory_schedulable_percent = {bad}\n")));
        assert_eq!(said, "memory_schedulable_percent must be from 1 to 100");
    }
    for bad in ["-1", "-5", "\"ninety\"", "90.5"] {
        let said = refused(&hosts(&format!("memory_schedulable_percent = {bad}\n")));
        assert!(said.contains("line 1"), "{bad}: {said}");
        assert!(said.contains("memory_schedulable_percent"), "{bad}: {said}");
    }
    let err = StaticPool::from_toml(
        ProviderId::new("pool").unwrap(),
        &hosts("memory_schedulable_percent = 0\n"),
    );
    assert!(matches!(err, Err(ConfigError::Parse(_))));
}

/// Asserts that the percent a caller passes replaces the one in the file, that it is
/// applied once (not on top of the file's), and that no percent passed leaves the file's.
/// It catches the two being multiplied.
#[test]
fn a_percent_passed_by_the_caller_replaces_the_files() {
    let text = hosts("memory_schedulable_percent = 50\n");
    let id = || ProviderId::new("pool").unwrap();
    let eighty = Some(MemoryPercent::new(80).unwrap());
    let mut replaced = StaticPool::from_toml_with_percent(id(), &text, eighty).unwrap();
    let ack = replaced.start(asking(1, LISTED * 80 / 100));
    assert!(matches!(ack, StartAck::Started { .. }), "{ack:?}");
    let mut kept = StaticPool::from_toml_with_percent(id(), &text, None).unwrap();
    assert_eq!(
        kept.start(asking(1, LISTED / 2 + 1)),
        StartAck::Refused(RefusalCode::NoFit)
    );
}

/// Asserts that the capacity listing shows the booked shape, the memory the file lists,
/// the percent and the bookable memory, which is the memory of the booked shape. It
/// catches a listing that shows the unreduced figure as bookable.
#[test]
fn capacity_shows_what_is_listed_and_what_is_bookable() {
    let listed = pool(&hosts("memory_schedulable_percent = 90\n")).capacity();
    let host = &listed[0];
    assert_eq!(host.memory.configured_bytes(), LISTED);
    assert_eq!(host.memory.percent().get(), 90);
    assert_eq!(host.memory.bookable_bytes(), NINETY);
    assert_eq!(host.shape.resources().memory_bytes(), NINETY);
    let plain = pool(&hosts("")).capacity();
    assert_eq!(plain[0].memory.percent(), MemoryPercent::FULL);
    assert_eq!(plain[0].shape.resources().memory_bytes(), LISTED);
}

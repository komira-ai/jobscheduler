//! The lease file across releases, in both directions: the previous release reads what
//! this one writes, and this one reads what the previous one wrote, with `lease.shares`
//! present and absent.
//!
//! The previous release is v0.1.120 (commit c9931d91), the last release before the file
//! gained `lease.shares`. Its code is in this test in two forms:
//!
//! - Files it wrote, kept beside this module in `cross_release/`. They were made by running
//!   that release's own `js-lease` (its `LeaseTable`, `read_bookings` and `HostBook::seat`)
//!   from a checkout of the tag, and are checked in as they came out:
//!   `written_by_previous_release.toml` is `scenario.rs` run there, and
//!   `rewritten_by_previous_release.toml` is `written_by_this_release.toml` restarted
//!   there as its server restarts: `read_bookings`, each booking seated with
//!   `HostBook::seat`, then `LeaseTable::open` over that book.
//! - Its reader: `cross_release/previous_store.rs` is lines 24 to 126 of that release's
//!   `crates/js-lease/src/store.rs`, unchanged (`git show
//!   v0.1.120:crates/js-lease/src/store.rs | sed -n 24,126p` prints it), and
//!   [`previous`] parses and writes with it exactly as that release's `store::read` and
//!   `store::write` do (`toml::from_str`, `toml::to_string`). Every file this test makes
//!   is read with it, so a field this release adds that the previous one would refuse
//!   fails here.
//!
//! Running the previous release's crate itself, as a dependency, is not done: it would bring
//! a second `js-types`, which `layers.toml` forbids (`single_source`).

use std::collections::BTreeMap;
use std::fs;

use js_lease::{Booking, Boot, LeaseState, LeaseTable, read_bookings};
use js_pool_static::{HostBook, SeatError};
use js_provider::{Flavour, HostCapacity, Observed, Provider, StartAck, StartSpec, StopAck};
use js_types::{Admission, ClassId, LeaseId, ProviderId, Request, ResourceClass, Resources, Shape};

use crate::support::{Scratch, clock, fake, gpu_host_request, host, open, request, secs, settings};

include!("cross_release/scenario.rs");

/// The previous release's reader and writer of the file.
mod previous {
    use std::collections::BTreeMap;

    use js_lease::LeaseState;
    use serde::{Deserialize, Serialize};

    include!("cross_release/previous_store.rs");

    /// Reads `text` as the previous release's `store::read` does.
    pub fn read(text: &str) -> Document {
        toml::from_str(text).unwrap()
    }

    /// Writes `document` as the previous release's `store::write` does.
    pub fn write(document: &Document) -> String {
        toml::to_string(document).unwrap()
    }
}

const WRITTEN_BY_PREVIOUS: &str = include_str!("cross_release/written_by_previous_release.toml");
const WRITTEN_BY_THIS: &str = include_str!("cross_release/written_by_this_release.toml");
const REWRITTEN_BY_PREVIOUS: &str =
    include_str!("cross_release/rewritten_by_previous_release.toml");

/// A provider whose hosts take several leases each, as a registry with a placer books
/// them: [`HostBook`] with the gate that admits a class that shares a host.
struct Sharing {
    book: HostBook,
}

impl Sharing {
    fn new() -> Self {
        let resources = Resources::new(8000, 8, 1, Some("g".into())).unwrap();
        let shape = Shape::new(resources, BTreeMap::new());
        let mut book = HostBook::new();
        book.insert_host(host("h1"), shape.clone());
        book.insert_host(host("h2"), shape);
        Self { book }
    }

    /// Seats `bookings` as the server does at a restart: each with what it books, a lease
    /// that shares its host with its own resources. The refusals are returned.
    fn seated(bookings: &[Booking]) -> (Self, Vec<SeatError>) {
        let mut provider = Self::new();
        let refused = bookings.iter().filter_map(|booking| {
            let shares = booking.shares.clone();
            let seated = provider.book.seat_as(&booking.host, booking.lease, shares);
            seated.err()
        });
        let refused = refused.collect();
        (provider, refused)
    }
}

impl Provider for Sharing {
    fn id(&self) -> ProviderId {
        ProviderId::new("sharing").unwrap()
    }
    fn flavour(&self) -> Flavour {
        Flavour::Delegated
    }
    fn classes(&self) -> Vec<ResourceClass> {
        vec![ResourceClass::whole_node(), sharing()]
    }
    fn start(&mut self, spec: StartSpec) -> StartAck {
        let gate = HostBook::listed_gate(&self.classes(), &spec);
        self.book.start_where(&spec, gate, &|_, _| true)
    }
    fn stop(&mut self, lease: &LeaseId) -> StopAck {
        StopAck::Stopped {
            read_back: self.book.stop(lease),
        }
    }
    fn observe(&mut self, lease: &LeaseId) -> Observed {
        self.book.read(lease)
    }
    fn capacity(&self) -> Vec<HostCapacity> {
        self.book.hosts()
    }
}

fn sharing() -> ResourceClass {
    let id = ClassId::new("share").unwrap();
    ResourceClass::new(id, Admission::Book(Default::default()), 0)
}

fn sharing_request(resources: Resources) -> Request {
    Request {
        class: sharing().id().clone(),
        resources,
        ..request(3600)
    }
}

/// Every axis of a share: cores, memory, and a GPU with its class.
fn with_gpu() -> Resources {
    Resources::new(4000, 4, 1, Some("g".into())).unwrap()
}

/// A share with no GPU, so its row leaves `gpus` and `gpu_class` out.
fn without_gpu() -> Resources {
    Resources::new(2000, 2, 0, None).unwrap()
}

/// The file this release writes for two leases that share `h1` and a whole-node lease on
/// `h2`: `lease.shares` present on the first two and absent on the third.
fn shared_host_written(scratch: &Scratch) -> String {
    let boot = Boot::Initialise;
    let path = scratch.file();
    let mut table = LeaseTable::open(Sharing::new(), clock(), settings(), path, boot).unwrap();
    let first = table.acquire(sharing_request(with_gpu()), None).unwrap();
    let second = table.acquire(sharing_request(without_gpu()), None).unwrap();
    let whole = table.acquire(request(3600), None).unwrap();
    let hosts = [first.host, second.host, whole.host];
    assert_eq!(hosts, [host("h1"), host("h1"), host("h2")]);
    drop(table);
    fs::read_to_string(scratch.file()).unwrap()
}

/// The bookings of the three leases of [`shared_host_written`], as this release reads them
/// from a file that kept every row.
fn shared_host_bookings() -> Vec<Booking> {
    let held = |sequence: u64, on: &str, shares: Option<Resources>| Booking {
        lease: LeaseId::new(1, sequence),
        host: host(on),
        state: LeaseState::Held,
        shares,
    };
    vec![
        held(1, "h1", Some(with_gpu())),
        held(2, "h1", Some(without_gpu())),
        held(3, "h2", None),
    ]
}

/// Restarts a table over the file of `scratch` as the server does: the bookings read,
/// seated with what they book, then the table opened over them. The leases it lists and
/// the seats refused.
fn restarted(scratch: &Scratch) -> (Vec<LeaseId>, Vec<SeatError>) {
    let bookings = read_bookings(scratch.file()).unwrap();
    let (provider, refused) = Sharing::seated(&bookings);
    let path = scratch.file();
    let table = LeaseTable::open(provider, clock(), settings(), path, Boot::Resume);
    let listed = table
        .unwrap()
        .list()
        .into_iter()
        .map(|lease| lease.id)
        .collect();
    (listed, refused)
}

/// Asserts that this release, over leases that book whole hosts (the only leases the
/// previous release could make), writes the file the previous release wrote for the same
/// steps byte for byte: `lease.shares` is absent and nothing else moved, so a file of a
/// deployment that places as before is the file it always was.
#[test]
fn whole_host_leases_are_written_as_the_previous_release_wrote_them() {
    let scratch = Scratch::new("cross_release_whole");
    assert_eq!(whole_hosts_written(&scratch), WRITTEN_BY_PREVIOUS);
}

/// Asserts that this release reads the file the previous release wrote: every lease is a
/// booking of the whole host (`shares` absent reads as `None`), a restart seats both and
/// lists both, and the file it writes back is the file it read but for the term, which
/// a restart moves on.
#[test]
fn this_release_reads_the_previous_release_file() {
    let scratch = Scratch::new("cross_release_from_previous");
    fs::write(scratch.file(), WRITTEN_BY_PREVIOUS).unwrap();
    let bookings = read_bookings(scratch.file()).unwrap();
    let shares: Vec<_> = bookings
        .iter()
        .map(|booking| booking.shares.clone())
        .collect();
    assert_eq!(shares, vec![None, None]);
    let (listed, refused) = restarted(&scratch);
    assert_eq!(listed, vec![LeaseId::new(1, 1), LeaseId::new(1, 2)]);
    assert!(refused.is_empty(), "{refused:?}");
    let next_term = WRITTEN_BY_PREVIOUS.replacen("term = 1\n", "term = 2\n", 1);
    assert_eq!(fs::read_to_string(scratch.file()).unwrap(), next_term);
}

/// Asserts that the previous release reads the file this release writes with
/// `lease.shares` present on two leases and absent on one: its reader takes every lease,
/// keeps each row it does not know in the lease's `extra`, and writes the file back so that
/// this release reads the same bookings, shares included, as it wrote. The file itself is
/// pinned, so `written_by_this_release.toml` is what this release writes.
#[test]
fn the_previous_release_reads_this_release_file() {
    let scratch = Scratch::new("cross_release_to_previous");
    let written = shared_host_written(&scratch);
    assert_eq!(written, WRITTEN_BY_THIS);
    assert_eq!(
        read_bookings(scratch.file()).unwrap(),
        shared_host_bookings()
    );

    let document = previous::read(&written);
    assert_eq!(document.lease_count, 3);
    let kept: Vec<_> = document
        .lease
        .iter()
        .map(|lease| (lease.sequence, lease.host.clone(), lease.state))
        .collect();
    let named = |on: &str| Some(on.to_owned());
    let expected = [
        (1, named("h1"), LeaseState::Held),
        (2, named("h1"), LeaseState::Held),
    ];
    assert_eq!(kept[..2], expected);
    assert_eq!(kept[2], (3, named("h2"), LeaseState::Held));
    let unknown: Vec<Vec<&String>> = document
        .lease
        .iter()
        .map(|l| l.extra.keys().collect())
        .collect();
    let shares = "shares".to_owned();
    assert_eq!(unknown, vec![vec![&shares], vec![&shares], vec![]]);
    assert!(document.extra.is_empty(), "{:?}", document.extra);

    fs::write(scratch.file(), previous::write(&document)).unwrap();
    assert_eq!(
        read_bookings(scratch.file()).unwrap(),
        shared_host_bookings()
    );
}

/// Asserts what a restart of the previous release does to this release's file, from the
/// file that release wrote when it was restarted over `written_by_this_release.toml`:
/// it seats every lease as booking the whole host, so the second lease of `h1` is refused
/// as taken and dropped (that release cannot serve a class that shares a host at all), and
/// the rows it keeps, `lease.shares` included, come back to this release unchanged: the
/// lease that kept `h1` is seated here again with its own share, and this release's
/// rewrite still names it. (That release writes the row's keys sorted, as a table it does
/// not know; this one reads them in any order.) It also checks the reader
/// of [`previous`] against that release's own output: reading this release's file with it
/// and dropping the refused lease writes the same bytes.
#[test]
fn a_restart_of_the_previous_release_keeps_the_rows_of_the_leases_it_keeps() {
    let mut document = previous::read(WRITTEN_BY_THIS);
    document.lease.retain(|lease| lease.sequence != 2);
    document.lease_count = 2;
    document.term = 2;
    assert_eq!(previous::write(&document), REWRITTEN_BY_PREVIOUS);

    let scratch = Scratch::new("cross_release_round_trip");
    fs::write(scratch.file(), REWRITTEN_BY_PREVIOUS).unwrap();
    let mut expected = shared_host_bookings();
    expected.remove(1);
    assert_eq!(read_bookings(scratch.file()).unwrap(), expected);
    let (listed, refused) = restarted(&scratch);
    assert_eq!(listed, vec![LeaseId::new(1, 1), LeaseId::new(1, 3)]);
    assert!(refused.is_empty(), "{refused:?}");
    assert_eq!(read_bookings(scratch.file()).unwrap(), expected);
}

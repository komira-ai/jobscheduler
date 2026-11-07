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
//!   `written_by_previous_release.toml` is the steps of `scenario.rs` run there, where the
//!   second acquire also asked for a 30 s time to live and was renewed ten seconds in (a
//!   release that has neither is not run that way; see the whole-host test below), and
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

use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use js_lease::{
    AcquireError, Booking, Boot, LeaseState, LeaseTable, ManualClock, Timestamp, read_bookings,
};
use js_provider::{HostBook, SeatError};
use js_provider::{HostCapacity, Observed, Pool, RefusalCode, StartAck, StartSpec, StopAck};
use js_types::{Admission, ClassId, LeaseId, ProviderId, Request, ResourceClass, Resources, Shape};

use crate::support::{Scratch, clock, fake, gpu_host_request, host, open, request, settings};

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
/// them: [`HostBook`] with the gate that admits a class that shares a host. A stop of a
/// lease in `unanswered` gets no answer and stops nothing, as a provider that cannot be
/// reached.
struct Sharing {
    book: HostBook,
    unanswered: BTreeSet<LeaseId>,
}

impl Sharing {
    fn new() -> Self {
        let resources = Resources::new(8000, 8, 1, Some("g".into()))
            .unwrap()
            .with_disk_bytes(DISK);
        let shape = Shape::new(resources, BTreeMap::new());
        let mut book = HostBook::new();
        book.insert_host(host("h1"), shape.clone());
        book.insert_host(host("h2"), shape);
        Self {
            book,
            unanswered: BTreeSet::new(),
        }
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

impl Pool for Sharing {
    fn id(&self) -> ProviderId {
        ProviderId::new("sharing").unwrap()
    }
    fn classes(&self) -> Vec<ResourceClass> {
        vec![ResourceClass::whole_node(), sharing()]
    }
    fn start(&mut self, spec: StartSpec) -> StartAck {
        let gate = HostBook::listed_gate(&self.classes(), &spec);
        self.book.start_where(&spec, gate, &|_, _| true)
    }
    fn stop(&mut self, lease: &LeaseId) -> StopAck {
        if self.unanswered.contains(lease) {
            return StopAck::Unknown;
        }
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

/// The scratch disk of a host: 4 GiB.
const DISK: u64 = 4 << 30;

/// Every axis of a share: cores, memory, a GPU with its class, and 3 GiB of disk.
fn with_gpu() -> Resources {
    Resources::new(4000, 4, 1, Some("g".into()))
        .unwrap()
        .with_disk_bytes(3 << 30)
}

/// A share with no GPU, so its row leaves `gpus` and `gpu_class` out, and 1 GiB of disk.
fn without_gpu() -> Resources {
    Resources::new(2000, 2, 0, None)
        .unwrap()
        .with_disk_bytes(1 << 30)
}

/// The share of the lease that is releasing: one core, no disk.
fn releasing_share() -> Resources {
    Resources::new(1000, 1, 0, None).unwrap()
}

/// A share of `cpu_millis` cores and nothing else.
fn cores(cpu_millis: u64) -> Request {
    sharing_request(Resources::new(cpu_millis, 1, 0, None).unwrap())
}

/// Asserts that `h1` of `table` has exactly `left` bytes of disk unbooked: a share asking
/// one byte more is refused for capacity (`h2` is held whole), and one asking `left` starts
/// on `h1`. It catches a restored share whose disk is not booked again.
fn assert_disk_left(table: &mut LeaseTable<Sharing, ManualClock>, left: u64) {
    let asking = |disk: u64| {
        let resources = Resources::new(500, 1, 0, None).unwrap();
        sharing_request(resources.with_disk_bytes(disk))
    };
    let over = table.acquire(asking(left + 1)).unwrap_err();
    let refused = AcquireError::Refused(RefusalCode::NoCapacity);
    assert_eq!(over.to_string(), refused.to_string());
    let fits = table.acquire(asking(left)).unwrap();
    assert_eq!(fits.host, host("h1"));
}

/// The file this release writes for two leases that share `h1` (between them every axis,
/// disk included), a whole-node lease on `h2`, and a fourth lease sharing `h1` that is
/// released while the provider does not answer its stop, so it is left releasing:
/// `lease.shares` present on the first, second and fourth and absent on the third.
fn shared_host_written(scratch: &Scratch) -> String {
    let boot = Boot::Initialise;
    let path = scratch.file();
    let mut table = LeaseTable::open(Sharing::new(), clock(), settings(), path, boot).unwrap();
    let first = table.acquire(sharing_request(with_gpu())).unwrap();
    let second = table.acquire(sharing_request(without_gpu())).unwrap();
    let whole = table.acquire(request(3600)).unwrap();
    let fourth = table.acquire(sharing_request(releasing_share())).unwrap();
    let hosts = [first.host, second.host, whole.host, fourth.host];
    assert_eq!(hosts, [host("h1"), host("h1"), host("h2"), host("h1")]);
    table.provider_mut().unanswered.insert(fourth.id);
    let outcome = table.release(&fourth.id).unwrap();
    assert_eq!(outcome, js_lease::ReleaseOutcome::Releasing);
    drop(table);
    fs::read_to_string(scratch.file()).unwrap()
}

/// The bookings of the four leases of [`shared_host_written`], as this release reads them
/// from a file that kept every row.
fn shared_host_bookings() -> Vec<Booking> {
    let held = |sequence: u64, on: &str, shares: Option<Resources>| Booking {
        lease: LeaseId::new(1, sequence),
        host: host(on),
        state: LeaseState::Held,
        shares,
        // Every lease of the scenario is acquired at the clock's start and runs an hour.
        started_at: Timestamp::from_millis(1_000_000),
        ends_at: Timestamp::from_millis(4_600_000),
    };
    let releasing = Booking {
        state: LeaseState::Releasing,
        ..held(4, "h1", Some(releasing_share()))
    };
    vec![
        held(1, "h1", Some(with_gpu())),
        held(2, "h1", Some(without_gpu())),
        held(3, "h2", None),
        releasing,
    ]
}

/// Restarts a table over the file of `scratch` as the server does: the bookings read,
/// seated with what they book, then the table opened over them, with no answer to a stop
/// of a lease in `unanswered`. The table and the seats refused.
fn restarted(
    scratch: &Scratch,
    unanswered: &[LeaseId],
) -> (LeaseTable<Sharing, ManualClock>, Vec<SeatError>) {
    let bookings = read_bookings(scratch.file()).unwrap();
    let (mut provider, refused) = Sharing::seated(&bookings);
    provider.unanswered.extend(unanswered.iter().copied());
    let path = scratch.file();
    let table = LeaseTable::open(provider, clock(), settings(), path, Boot::Resume);
    (table.unwrap(), refused)
}

/// The leases `table` lists, in id order.
fn listed(table: &LeaseTable<Sharing, ManualClock>) -> Vec<LeaseId> {
    table.list().into_iter().map(|lease| lease.id).collect()
}

/// `text`, a lease file, with each lease's `expires_at_ms` set to that lease's `ends_at_ms`:
/// the one difference between what the previous release wrote and what this release writes
/// for the same steps. That release set a lease's expiry to the end of its time to live
/// (here 60 s after the acquire, and 70 s for the lease it renewed ten seconds in); this
/// one has no time to live and writes the wall end there, which the previous release reads
/// as the lease's expiry.
fn with_the_wall_end_as_the_expiry(text: &str) -> String {
    let mut out = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        if line.starts_with("expires_at_ms = ") {
            let end = lines.next().unwrap_or_default();
            out.push(end.replace("ends_at_ms", "expires_at_ms"));
            out.push(end.to_owned());
        } else {
            out.push(line.to_owned());
        }
    }
    out.join("\n") + "\n"
}

/// Asserts that this release, over leases that book whole hosts (the only leases the
/// previous release could make), writes the file the previous release wrote for the same
/// steps but for each lease's `expires_at_ms`, which is its `ends_at_ms` here (the
/// previous file has the end of the time to live, 1 060 000 and, for the lease it renewed,
/// 1 070 000): `lease.shares` is absent and nothing else moved, so a file of a deployment
/// that places as before is the file it always was, and the previous release reads each
/// lease as expiring when its wall time ends. It fails if any other row, or a `ttl_ms`
/// row, differs, or if a held lease is written with an expiry before its wall end.
#[test]
fn whole_host_leases_are_written_as_the_previous_release_wrote_them_but_for_the_expiry() {
    let scratch = Scratch::new("cross_release_whole");
    let written = whole_hosts_written(&scratch);
    assert!(WRITTEN_BY_PREVIOUS.contains("expires_at_ms = 1060000\n"));
    assert!(WRITTEN_BY_PREVIOUS.contains("expires_at_ms = 1070000\n"));
    assert_eq!(
        written,
        with_the_wall_end_as_the_expiry(WRITTEN_BY_PREVIOUS)
    );
    assert!(!written.contains("ttl_ms"), "{written}");
}

/// Asserts that this release reads the file the previous release wrote: every lease is a
/// booking of the whole host (`shares` absent reads as `None`), a restart seats both and
/// lists both, and the file it writes back is the file it read but for the term, which
/// a restart moves on, and each lease's `expires_at_ms`, which becomes its `ends_at_ms`.
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
    let (table, refused) = restarted(&scratch, &[]);
    assert_eq!(listed(&table), vec![LeaseId::new(1, 1), LeaseId::new(1, 2)]);
    assert!(refused.is_empty(), "{refused:?}");
    let next_term = with_the_wall_end_as_the_expiry(WRITTEN_BY_PREVIOUS).replacen(
        "term = 1\n",
        "term = 2\n",
        1,
    );
    assert_eq!(fs::read_to_string(scratch.file()).unwrap(), next_term);
}

/// Asserts that the previous release reads the file this release writes with
/// `lease.shares` present on three leases (one of them releasing) and absent on one: its
/// reader takes every lease, the releasing one as releasing,
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
    assert_eq!(document.lease_count, 4);
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
    assert_eq!(kept[3], (4, named("h1"), LeaseState::Releasing));
    let unknown: Vec<Vec<&String>> = document
        .lease
        .iter()
        .map(|l| l.extra.keys().collect())
        .collect();
    let shares = "shares".to_owned();
    let rows = vec![vec![&shares], vec![&shares], vec![], vec![&shares]];
    assert_eq!(unknown, rows);
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
/// as taken and dropped (that release cannot serve a class that shares a host at all). A
/// releasing lease it keeps as releasing, its row included, though its seat is refused too:
/// that release keeps a releasing lease until a stop of it is answered. The rows it keeps,
/// `lease.shares` included, come back to this release unchanged: the lease that kept `h1`
/// is seated here again with its own share, the releasing one is seated again with its
/// share and stays releasing while its stop is not answered, and this release's rewrite
/// still names both. (That release writes the row's keys sorted, as a table it does
/// not know; this one reads them in any order.) It also checks the reader
/// of [`previous`] against that release's own output: reading this release's file with it
/// and dropping the refused lease writes the same bytes.
#[test]
fn a_restart_of_the_previous_release_keeps_the_rows_of_the_leases_it_keeps() {
    let mut document = previous::read(WRITTEN_BY_THIS);
    document.lease.retain(|lease| lease.sequence != 2);
    document.lease_count = 3;
    document.term = 2;
    assert_eq!(previous::write(&document), REWRITTEN_BY_PREVIOUS);

    let scratch = Scratch::new("cross_release_round_trip");
    fs::write(scratch.file(), REWRITTEN_BY_PREVIOUS).unwrap();
    let mut expected = shared_host_bookings();
    expected.remove(1);
    assert_eq!(read_bookings(scratch.file()).unwrap(), expected);
    let releasing = LeaseId::new(1, 4);
    let (mut table, refused) = restarted(&scratch, &[releasing]);
    let kept = vec![LeaseId::new(1, 1), LeaseId::new(1, 3), releasing];
    assert_eq!(listed(&table), kept);
    assert!(refused.is_empty(), "{refused:?}");
    assert_eq!(read_bookings(scratch.file()).unwrap(), expected);
    assert_disk_left(&mut table, DISK - (3 << 30));
}

/// Asserts that this release, restarted over the file it wrote, seats every lease with
/// what it booked, disk included: the shares of `h1` are seated beside each other, the
/// releasing one stays releasing and keeps its core booked until a stop is answered, and
/// the 4 GiB of disk the first two book stay booked, so `h1` has none left. It catches a
/// share put back without its disk or a releasing share put back without its booking,
/// either of which would let a restart overbook the host.
#[test]
fn this_release_restarts_over_its_own_file_with_every_share_booked() {
    let scratch = Scratch::new("cross_release_own_restart");
    fs::write(scratch.file(), WRITTEN_BY_THIS).unwrap();
    let releasing = LeaseId::new(1, 4);
    let (mut table, refused) = restarted(&scratch, &[releasing]);
    let all: Vec<LeaseId> = (1..=4).map(|n| LeaseId::new(1, n)).collect();
    assert_eq!(listed(&table), all);
    assert!(refused.is_empty(), "{refused:?}");
    assert_eq!(
        read_bookings(scratch.file()).unwrap(),
        shared_host_bookings()
    );

    // The releasing lease still books its core: with 7 of `h1`'s 8 cores booked, a share
    // of one core and a millicore is refused, and stays refused while its stop is not
    // answered; once a stop is, the core is free.
    let state = |table: &LeaseTable<Sharing, ManualClock>| {
        let leases = table.list();
        leases
            .iter()
            .find(|lease| lease.id == releasing)
            .map(|lease| lease.state)
    };
    assert_eq!(state(&table), Some(LeaseState::Releasing));
    let refused = AcquireError::Refused(RefusalCode::NoCapacity).to_string();
    let over = table.acquire(cores(1001)).unwrap_err();
    assert_eq!(over.to_string(), refused);
    table.tick();
    assert_eq!(state(&table), Some(LeaseState::Releasing));
    let over = table.acquire(cores(1001)).unwrap_err();
    assert_eq!(over.to_string(), refused);
    table.provider_mut().unanswered.clear();
    let report = table.tick();
    assert_eq!(report.freed, vec![releasing]);
    assert_eq!(state(&table), None);
    assert_eq!(table.acquire(cores(1001)).unwrap().host, host("h1"));
    assert_disk_left(&mut table, 0);
}

/// Asserts that the previous release keeps the start of a whole-node lease that was ready
/// after its acquire: its reader takes `started_at_ms` into the lease's `extra`, its writer
/// writes it back, and this release reads the same booking from that rewrite, start and end
/// included. It catches a field the previous release would refuse or drop.
#[test]
fn the_previous_release_keeps_the_start_of_a_lease_ready_after_its_acquire() {
    let scratch = Scratch::new("cross_release_started");
    let clock = clock();
    drop(crate::started::ready_later(&scratch, &clock));
    let written = fs::read_to_string(scratch.file()).unwrap();
    let bookings = read_bookings(scratch.file()).unwrap();
    let ready = crate::support::T0 + crate::started::READY_AFTER_MS;
    assert_eq!(bookings[0].started_at, Timestamp::from_millis(ready));

    let document = previous::read(&written);
    let kept: Vec<&String> = document.lease[0].extra.keys().collect();
    assert_eq!(kept, [&"started_at_ms".to_owned()]);
    fs::write(scratch.file(), previous::write(&document)).unwrap();
    assert_eq!(read_bookings(scratch.file()).unwrap(), bookings);
}

/// Asserts the `request` row across releases, both ways. This release writes the row for a
/// lease acquired under a request id; the previous release's reader takes it into the
/// lease's `extra` (it would refuse nothing) and its writer writes it back; this release
/// opens that rewrite and still knows the id: a repeat of the acquire answers the same held
/// lease. A file the previous release wrote has no such row and is read as before
/// (`this_release_reads_the_previous_release_file`). It catches a row the previous release
/// would refuse or drop, which would turn a rollback and roll forward into a second lease
/// on the next repeat.
#[test]
fn the_previous_release_keeps_the_request_row_of_a_lease() {
    let scratch = Scratch::new("cross_release_request");
    let clock = clock();
    let mut table = open(fake(), &clock, &scratch);
    let named = js_types::RequestId::new("r1").unwrap();
    let lease = table
        .acquire_with_id(named.clone(), gpu_host_request(3600))
        .unwrap();
    let written = fs::read_to_string(scratch.file()).unwrap();
    let provider = table.into_provider();

    let document = previous::read(&written);
    let kept: Vec<&String> = document.lease[0].extra.keys().collect();
    assert_eq!(kept, [&"request".to_owned()]);
    fs::write(scratch.file(), previous::write(&document)).unwrap();
    let mut table = open(provider, &clock, &scratch);
    let again = table.acquire_with_id(named, gpu_host_request(3600));
    assert_eq!(again.unwrap(), lease);
}

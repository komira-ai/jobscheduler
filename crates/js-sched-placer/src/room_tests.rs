//! What a pass holds each host to: the bookings its agent sums ([`crate::room`]), the disk a
//! job that names none books, and the host the walk chooses when the store would find one full.

use std::collections::BTreeMap;
use std::time::Duration;

use js_jobs::{AllocationRecord, Booked, Capacity, EndReason, JobId, JobState, Needs, NewJob};
use js_registry::Placer;
use js_types::sched::usage::NodeLoad;
use js_types::{HostId, ManualClock, Resources, Shape, Timestamp};

use super::{OpenHost, Room, booked_on, on_host};
use crate::engine::Settings;
use crate::engine::jobs::{JobStarts, NoStarts, PassMetrics};
use crate::engine::rig::{Tables, at, host, spec_of};
use crate::place::SchedPlacer;

const GIB: u64 = 1 << 30;

/// A booking of `cpu` millicores and `memory` bytes with the disk `disk`.
fn shape(cpu: u64, memory: u64, disk: u64) -> Booked {
    Booked::Shape {
        cpu_millis: cpu,
        memory_bytes: memory,
        disk_bytes: disk,
        accelerators: 0,
    }
}

/// A capacity of `cpu` millicores, `memory` bytes and `disk` bytes.
fn capacity(cpu: u64, memory: u64, disk: u64) -> Capacity {
    Capacity {
        cpu_millis: cpu,
        memory_bytes: memory,
        disk_bytes: disk,
        accelerators: 0,
    }
}

/// The offer of a host that books up to `capacity`, reporting load admission or not.
fn offer(capacity: Capacity, load_admission: bool) -> OpenHost {
    OpenHost {
        load_admission,
        capacity: Some(capacity),
        ..OpenHost::default()
    }
}

/// Asserts that a share that names no disk books the host's quota default, and nothing else
/// changes: a share that names its disk, a host with no quota default and a whole host book
/// as they are. Catches a booking that leaves the agent's default disk out.
#[test]
fn a_share_that_names_no_disk_books_the_hosts_quota_default() {
    assert_eq!(on_host(shape(1, 2, 0), Some(3 * GIB)), shape(1, 2, 3 * GIB));
    assert_eq!(on_host(shape(1, 2, 5), Some(3 * GIB)), shape(1, 2, 5));
    assert_eq!(on_host(shape(1, 2, 0), None), shape(1, 2, 0));
    assert_eq!(on_host(Booked::WholeHost, Some(3 * GIB)), Booked::WholeHost);
    let quota = OpenHost {
        quota_disk_bytes: Some(GIB),
        ..OpenHost::default()
    };
    let open = BTreeMap::from([(host("a"), quota)]);
    assert_eq!(
        booked_on(&open, &host("a"), shape(1, 2, 0)),
        shape(1, 2, GIB)
    );
    assert_eq!(booked_on(&open, &host("b"), shape(1, 2, 0)), shape(1, 2, 0));
}

/// A placer and job tables over the hosts `a` and `b`, each of 8 cores and 16 GiB, on a clock
/// the test sets.
struct Rig {
    placer: SchedPlacer,
    clock: ManualClock,
    tables: Tables,
}

/// A pass's metrics, told nothing of.
struct Quiet;

impl PassMetrics for Quiet {
    fn job_placed(&self, _: Duration) {}

    fn store_write_failed(&self) {}
}

impl Rig {
    /// A rig whose engine knows both hosts, with both clocks at 5 s.
    fn new() -> Self {
        let clock = ManualClock::new(Timestamp::from_millis(0));
        let placer = SchedPlacer::new(Settings::default(), clock.clone());
        let caps = [("arch".to_owned(), "x86_64".to_owned())];
        let resources = Resources::new(8000, 16 * GIB, 0, None).unwrap();
        for name in ["a", "b"] {
            let shape = Shape::new(resources.clone(), caps.clone().into_iter().collect());
            placer.engine().host_up(&host(name), &shape, at(0));
        }
        let rig = Self {
            placer,
            clock,
            tables: Tables::new(),
        };
        rig.set(5_000);
        rig
    }

    /// Both clocks at `ms`.
    fn set(&self, ms: u64) {
        self.clock.set(Timestamp::from_millis(ms));
        self.tables.set_ms(ms);
    }

    /// Records the job `id` as placed on `on`, booking `booked`.
    fn hold(&self, id: &str, on: &str, booked: Booked) -> AllocationRecord {
        let id = JobId::parse(id).unwrap();
        let resources = Resources::new(1000, GIB, 0, None).unwrap();
        let new = NewJob {
            id: Some(id.clone()),
            spec: spec_of(Needs::Shape(resources), 600),
            priority: 0,
            client: None,
        };
        self.tables.jobs.insert_queued(new).unwrap();
        self.tables.jobs.place(&id, host(on), booked).unwrap().1
    }

    /// Two load readings of `name` that show `busy` millicores in use, as its agent sends
    /// them, after the engine holds the live allocations of the tables, leaving both clocks at
    /// 10 s.
    fn measure(&self, name: &str, busy: u64) {
        let reading = |seq: u64, used: u64| NodeLoad {
            seq,
            mono_usec: seq * 5_000_000,
            cpu_usage_usec: used * 5_000,
            ..NodeLoad::default()
        };
        let live = self.tables.jobs.live_allocations().unwrap();
        self.placer.engine().sync_jobs(&live);
        self.set(5_000);
        self.placer.heard(&host(name), Some(&reading(1, 0)));
        self.set(10_000);
        self.placer.heard(&host(name), Some(&reading(2, busy)));
    }

    /// A pass over `open`: the jobs it placed.
    fn pass(&self, open: &BTreeMap<HostId, OpenHost>) -> Vec<JobId> {
        let starts: (&dyn JobStarts, &dyn PassMetrics) = (&NoStarts, &Quiet);
        let placed = self.placer.place_jobs(&self.tables.jobs, open, starts);
        placed.unwrap()
    }

    /// The host of the job `id`, if it is placed.
    fn host_of(&self, id: &str) -> Option<HostId> {
        let job = self.tables.jobs.get_job(&JobId::parse(id).unwrap());
        job.unwrap().and_then(|job| job.host)
    }

    /// Whether the job `id` is still queued.
    fn queued(&self, id: &str) -> bool {
        let job = self.tables.jobs.get_job(&JobId::parse(id).unwrap());
        job.unwrap()
            .is_some_and(|job| job.state == JobState::Queued)
    }
}

/// Asserts that a host whose agent sums bookings, with live allocations booking all its 8000
/// millicores although its measured load is low, takes no job while it is the only host: the
/// job stays queued and the walk runs no round for it. Catches a walk that chooses the host and
/// leaves the store to answer that it is full, on every pass.
#[test]
fn a_host_whose_bookings_sum_to_its_cores_takes_no_job_by_its_low_load() {
    let rig = Rig::new();
    for n in 0..8 {
        rig.hold(&format!("held-{n}"), "a", shape(1000, GIB, 0));
    }
    rig.measure("a", 500);
    rig.tables.share("j");
    let open = BTreeMap::from([(host("a"), offer(capacity(8000, 16 * GIB, 0), false))]);
    assert!(rig.pass(&open).is_empty());
    assert!(rig.queued("j"));
    assert_eq!(rig.placer.job_rounds(), 0, "no host was tried");
}

/// Asserts that, of two hosts, the one that reports load admission takes the job (packed by
/// its load, within the slack) when the other, the one the scheduler packs first, offers only 6
/// cores and its live allocations book all six: that host has no room by its agent's sum.
/// Catches a job sent to the fuller host, refused there by the store, and left queued while
/// the other host has room.
#[test]
fn a_job_goes_to_the_host_that_reports_load_admission_when_the_other_is_full_by_bookings() {
    let rig = Rig::new();
    for n in 0..6 {
        rig.hold(&format!("a-{n}"), "a", shape(1000, GIB, 0));
    }
    for n in 0..2 {
        rig.hold(&format!("b-{n}"), "b", shape(1000, GIB, 0));
    }
    rig.measure("a", 500);
    rig.measure("b", 500);
    rig.tables.share("j");
    let open = BTreeMap::from([
        (host("a"), offer(capacity(6000, 16 * GIB, 0), false)),
        (host("b"), offer(capacity(8800, 16 * GIB, 0), true)),
    ]);
    assert_eq!(rig.pass(&open), vec![JobId::parse("j").unwrap()]);
    assert_eq!(rig.host_of("j"), Some(host("b")));
}

/// Asserts that a host whose live allocations book all the memory it offers gets no job that
/// books memory, though its CPU is free and its pool shape is larger: the job goes to the other
/// host. Catches a walk that sizes the host by its shape and not by what its agent offers.
#[test]
fn a_host_whose_bookings_fill_its_memory_gets_no_job_that_books_memory() {
    let rig = Rig::new();
    rig.hold("big", "a", shape(1000, 4 * GIB, 0));
    rig.measure("a", 100);
    rig.measure("b", 3000);
    rig.tables.share("j");
    let open = BTreeMap::from([
        (host("a"), offer(capacity(8000, 4 * GIB, 0), false)),
        (host("b"), offer(capacity(8000, 16 * GIB, 0), false)),
    ]);
    assert_eq!(rig.pass(&open).len(), 1);
    assert_eq!(rig.host_of("j"), Some(host("b")));
}

/// Asserts that jobs that name no disk book the host's quota default: on a host with 10 GiB of
/// disk and a default of 3 GiB the pass places three and not a fourth, and the allocation the
/// store records for each books 3 GiB. Catches a booking of no disk, which lets every job on.
#[test]
fn jobs_that_name_no_disk_book_the_quota_default_and_stop_at_the_disk() {
    let rig = Rig::new();
    rig.measure("a", 0);
    for id in ["j1", "j2", "j3", "j4"] {
        rig.tables.share(id);
    }
    let offered = OpenHost {
        quota_disk_bytes: Some(3 * GIB),
        ..offer(capacity(8000, 16 * GIB, 10 * GIB), false)
    };
    let open = BTreeMap::from([(host("a"), offered)]);
    assert_eq!(rig.pass(&open).len(), 3);
    assert!(rig.queued("j4"));
    let live = rig.tables.jobs.live_allocations().unwrap();
    assert_eq!(live.len(), 3);
    for allocation in live {
        assert_eq!(allocation.booked, shape(1000, GIB, 3 * GIB));
    }
}

/// Asserts the ledger: a host has room for a placement until the live allocations and the
/// placements added fill it, on CPU and on disk (a placement that names none books the quota
/// default); a freed allocation does not count, and one on a host the pass does not offer
/// changes nothing.
#[test]
fn the_ledger_sums_live_allocations_and_each_placement_added() {
    let rig = Rig::new();
    let one = rig.hold("one", "a", shape(2000, 2 * GIB, 0));
    let two = rig.hold("two", "a", shape(5000, 2 * GIB, 0));
    rig.hold("gone", "a", shape(4000, 2 * GIB, 0));
    let gone = JobId::parse("gone").unwrap();
    let freed = rig.tables.jobs.free(&gone, EndReason::Exited).unwrap();
    let elsewhere = rig.hold("elsewhere", "b", shape(8000, 16 * GIB, 0));
    let live = [one, two, freed, elsewhere];
    let offered = OpenHost {
        quota_disk_bytes: Some(GIB),
        ..offer(capacity(8000, 16 * GIB, 3 * GIB), false)
    };
    let open = BTreeMap::from([(host("a"), offered)]);
    let mut room = Room::of(&open, &live);
    assert!(room.fits(&host("a"), shape(1000, GIB, 0)));
    assert!(!room.fits(&host("a"), shape(1001, GIB, 0)), "7000 booked");
    room.add(&host("a"), shape(1000, GIB, 0));
    assert!(!room.fits(&host("a"), shape(1, 0, 0)), "8000 booked");

    let mut by_disk = Room::of(&open, &[]);
    for _ in 0..3 {
        assert!(by_disk.fits(&host("a"), shape(1, 1, 0)));
        by_disk.add(&host("a"), shape(1, 1, 0));
    }
    assert!(!by_disk.fits(&host("a"), shape(1, 1, 0)), "3 GiB booked");
}

/// Asserts that a host with no capacity always has room, one unknown to the ledger too, and
/// that a whole-host booking fits only a host with nothing live on it and shuts it to shares.
#[test]
fn a_host_with_no_capacity_always_fits_and_a_whole_host_fits_an_empty_host_only() {
    let rig = Rig::new();
    let held = rig.hold("held", "a", Booked::WholeHost);
    let open = BTreeMap::from([
        (host("a"), offer(capacity(8000, 16 * GIB, 0), false)),
        (host("b"), OpenHost::default()),
    ]);
    let room = Room::of(&open, std::slice::from_ref(&held));
    assert!(
        !room.fits(&host("a"), shape(1, 1, 0)),
        "a whole host is booked"
    );
    assert!(!room.fits(&host("a"), Booked::WholeHost));
    assert!(room.fits(&host("b"), shape(u64::MAX, u64::MAX, 0)));
    assert!(room.fits(&host("c"), shape(u64::MAX, u64::MAX, 0)));
    let mut room = Room::of(&open, &[]);
    assert!(room.fits(&host("a"), Booked::WholeHost));
    room.add(&host("a"), Booked::WholeHost);
    assert!(!room.fits(&host("a"), shape(1, 1, 0)));
    room.add(&host("c"), shape(1, 1, 0));
}

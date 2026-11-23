//! The hosts' measured memory in a walk over the queued jobs. Every host is of 8 cores and
//! 16 GiB and holds four jobs that book one core and 5 GiB each: 20 GiB of bookings, more than
//! the host has. A host's memory is counted as what its fresh reading measured in use on the
//! whole host plus the bookings of the jobs placed since the reading, against the share of the
//! host's memory the engine may book; with no fresh reading that carries the host's memory it
//! is counted by its bookings. Readings are fed with the farm time they arrive at, and the walk
//! runs at a farm time the test names.

use std::collections::BTreeMap;

use js_jobs::Needs;
use js_sched::{MemoryPercent, RefusalKind};
use js_types::sched::usage::NodeLoad;
use js_types::{Candidate, Held, LeaseId, Resources};

use super::jobs::OpenHost;
use super::rig::{Tables, at, candidate, engine_with, host};
use super::{Engine, Settings};

const GIB: u64 = 1 << 30;

/// A reading numbered `seq`, taken at `seq * 5` seconds on the agent's clock, of a host busy
/// one core and with `host_used` bytes in use on the whole host (`None`: an agent that does not
/// send it).
fn reading(seq: u64, host_used: Option<u64>) -> NodeLoad {
    NodeLoad {
        seq,
        mono_usec: seq * 5_000_000,
        cpu_usage_usec: seq * 5_000_000,
        memory_working_set_bytes: GIB,
        host_memory_used_bytes: host_used,
        ..NodeLoad::default()
    }
}

/// The engine, sharing `percent` of each host's memory, over the host `a` holding four jobs
/// that book one core and 5 GiB each.
fn twenty_booked(percent: u32) -> Engine {
    let mut a: Candidate = candidate("a", &[]);
    a.leases = (1..=4)
        .map(|seq| Held {
            lease: LeaseId::new(1, seq),
            resources: Resources::new(1_000, 5 * GIB, 0, None).unwrap(),
            whole: false,
        })
        .collect();
    let settings = Settings {
        memory: MemoryPercent::new(percent).unwrap(),
        ..Settings::default()
    };
    engine_with(settings, &[a])
}

/// Tells the engine two readings of `a` at 5 s and 10 s farm time, each with `host_used` in
/// use: fresh at 10 s, and the second covers the jobs held at the first.
fn measures(e: &mut Engine, host_used: Option<u64>) {
    e.heard(&host("a"), Some(&reading(1, host_used)), at(5_000));
    e.heard(&host("a"), Some(&reading(2, host_used)), at(10_000));
}

/// Walks the new jobs `ids`, each of 200 m and `memory` bytes, over `a` at `now_ms`: the jobs
/// placed, in order.
fn walk(e: &mut Engine, ids: &[&str], memory: u64, now_ms: u64) -> Vec<String> {
    let tables = Tables::new();
    let needs = || Needs::Shape(Resources::new(200, memory, 0, None).unwrap());
    let jobs: Vec<_> = ids
        .iter()
        .map(|id| tables.submit_with(id, needs(), 600))
        .collect();
    let open = BTreeMap::from([(host("a"), OpenHost::default())]);
    let (decided, _) = e.walk_jobs(&jobs, &open, at(now_ms));
    decided
        .into_iter()
        .map(|(job, _, _, _)| job.as_str().to_owned())
        .collect()
}

/// The figures of `a`'s latest memory refusal of `kind`: (counted, new, limit, measured).
fn refused(e: &mut Engine, kind: RefusalKind) -> (u64, u64, u64, Option<u64>) {
    let refused = e.take_refused();
    let (_, r) = refused.latest(&host("a"), kind).expect("a memory refusal");
    (r.counted, r.new, r.limit, r.measured_memory_bytes)
}

/// Behaviour: a host whose jobs book 20 GiB, more than its 16 GiB, but whose fresh reading
/// measured 8 GiB in use on the whole host (half its memory) takes a job of 4 GiB. Catches
/// placement that adds up every running job's booked peak, which refuses a host booked full
/// while half its memory is free.
#[test]
fn a_host_booked_past_its_memory_but_using_half_takes_the_next_job() {
    let mut e = twenty_booked(100);
    measures(&mut e, Some(8 * GIB));
    assert_eq!(walk(&mut e, &["j1"], 4 * GIB, 10_000), ["j1"]);
}

/// Behaviour: a host that measured 13 GiB in use of its 16 GiB refuses a job of 4 GiB for
/// memory, the refusal counting the measurement and carrying it (13 + 4 > 16 GiB), and takes a
/// job of 3 GiB, which fills it exactly. Where the engine may book half of each host's memory
/// (the limit is 8 GiB), a host that measured 6 GiB takes a job of 2 GiB and refuses one of
/// 2 GiB and a byte. Catches a measured host filled past the one percent limit, a limit
/// compared with `>=`, and a refusal that does not say what was measured.
#[test]
fn a_host_measured_near_its_limit_refuses_with_the_measurement() {
    let mut e = twenty_booked(100);
    measures(&mut e, Some(13 * GIB));
    assert!(walk(&mut e, &["j1"], 4 * GIB, 10_000).is_empty());
    assert_eq!(
        refused(&mut e, RefusalKind::PredictedMemory),
        (13 * GIB, 4 * GIB, 16 * GIB, Some(13 * GIB))
    );
    assert_eq!(walk(&mut e, &["j2"], 3 * GIB, 10_000), ["j2"]);

    let mut e = twenty_booked(50);
    measures(&mut e, Some(6 * GIB));
    assert!(walk(&mut e, &["j1"], 2 * GIB + 1, 10_000).is_empty());
    assert_eq!(
        refused(&mut e, RefusalKind::PredictedMemory),
        (6 * GIB, 2 * GIB + 1, 8 * GIB, Some(6 * GIB))
    );
    assert_eq!(walk(&mut e, &["j2"], 2 * GIB, 10_000), ["j2"]);
}

/// Behaviour: on a host that measured 4 GiB in use, one walk over five jobs of 4 GiB places
/// three (4 + 3 * 4 = 16 GiB) and refuses the fourth, counted 16 GiB: the jobs placed since the
/// reading are counted by their bookings, so one pass cannot overfill the host. Once two more
/// readings have covered them and measured 7 GiB in use, the next job is placed by the new
/// measurement. Catches jobs placed between two readings left out of the count until a reading
/// sees them, which lets a single pass place far past the host's memory.
#[test]
fn a_burst_placed_between_two_readings_is_counted_by_its_bookings() {
    let mut e = twenty_booked(100);
    measures(&mut e, Some(4 * GIB));
    let ids = ["j1", "j2", "j3", "j4", "j5"];
    assert_eq!(walk(&mut e, &ids, 4 * GIB, 10_000), ["j1", "j2", "j3"]);
    assert_eq!(
        refused(&mut e, RefusalKind::PredictedMemory),
        (16 * GIB, 4 * GIB, 16 * GIB, Some(4 * GIB))
    );

    e.heard(&host("a"), Some(&reading(3, Some(7 * GIB))), at(15_000));
    e.heard(&host("a"), Some(&reading(4, Some(7 * GIB))), at(20_000));
    assert_eq!(walk(&mut e, &["j6"], 4 * GIB, 20_000), ["j6"]);
}

/// Behaviour: with no fresh reading of the host's memory the host is counted by its bookings
/// (20 GiB, over its 16 GiB) and refuses a job of 1 GiB, the refusal carrying no measurement:
/// a host never measured; a host whose reading of 8 GiB went stale (at 25.001 s, more than
/// three windows after the reading of 10 s; at 25 s it still takes the job); and a host whose
/// fresh reading does not carry the host's memory (an agent of an earlier release). Catches a
/// missing or stale reading taken as no memory in use, and a stale measurement still counted.
#[test]
fn with_no_fresh_reading_of_memory_the_bookings_count() {
    let mut e = twenty_booked(100);
    assert!(walk(&mut e, &["j1"], GIB, 0).is_empty());
    assert_eq!(
        refused(&mut e, RefusalKind::PredictedMemory),
        (20 * GIB, GIB, 16 * GIB, None)
    );

    let mut e = twenty_booked(100);
    measures(&mut e, Some(8 * GIB));
    assert!(walk(&mut e, &["j1"], GIB, 25_001).is_empty());
    assert_eq!(
        refused(&mut e, RefusalKind::PredictedMemory),
        (20 * GIB, GIB, 16 * GIB, None)
    );
    let mut e = twenty_booked(100);
    measures(&mut e, Some(8 * GIB));
    assert_eq!(walk(&mut e, &["j1"], GIB, 25_000), ["j1"]);

    let mut e = twenty_booked(100);
    measures(&mut e, None);
    assert!(walk(&mut e, &["j1"], GIB, 10_000).is_empty());
    assert_eq!(
        refused(&mut e, RefusalKind::PredictedMemory),
        (20 * GIB, GIB, 16 * GIB, None)
    );
}

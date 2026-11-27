//! The CPU time a freed job's result reports, handed by the pass ([`SchedPlacer::learn_freed`])
//! to the scheduler's plausibility check of its host's load readings.

use std::collections::BTreeMap;
use std::time::Duration;

use js_jobs::{EndReason, JobId, JobState, NewJob};
use js_registry::Placer as _;
use js_types::sched::usage::NodeLoad;
use js_types::{ManualClock, Resources, Shape, Timestamp};

use super::{GIB, MODULE_COLD, Tables, exited, host, needs, of_class, pass};
use crate::engine::Settings;
use crate::place::SchedPlacer;

/// Reading `seq` of a host whose CPU counter is stuck at 0.
fn stuck(seq: u64) -> NodeLoad {
    NodeLoad {
        seq,
        mono_usec: seq * 5_000_000,
        ..NodeLoad::default()
    }
}

/// Asserts that a pass hands the CPU time of a job freed since the last pass to the
/// scheduler: a job placed after the host's first reading ends with 60 core-seconds, the
/// next pass frees it, and a reading whose counter did not move then marks the host's signal
/// implausible, so the host is charged by its jobs' bookings. Catches the freed job's CPU time
/// not handed over, which leaves a stuck counter believed.
#[test]
fn a_pass_hands_a_freed_jobs_cpu_time_to_the_plausibility_check() {
    let tables = Tables::new();
    let clock = ManualClock::new(Timestamp::from_millis(0));
    let settings = Settings {
        cold: MODULE_COLD,
        ..Settings::default()
    };
    let placer = SchedPlacer::new(settings, clock.clone());
    let caps = BTreeMap::from([("arch".to_owned(), "x86_64".to_owned())]);
    let shape = Shape::new(Resources::new(32_000, 16 * GIB, 0, None).unwrap(), caps);
    placer.engine().host_up(&host("a"), &shape, super::at(0));
    clock.set(Timestamp::from_millis(1_000));
    placer.heard(&host("a"), Some(&stuck(1)));

    clock.set(Timestamp::from_millis(2_000));
    let id = JobId::parse("busy").unwrap();
    let new = NewJob {
        id: Some(id.clone()),
        spec: of_class("busy", needs(4_000, GIB)),
        priority: 0,
        client: None,
    };
    tables.jobs.insert_queued(new).unwrap();
    assert_eq!(pass(&placer, &tables), vec![id.clone()]);
    tables
        .jobs
        .transition(&id, JobState::Placed, JobState::Starting)
        .unwrap();
    tables
        .jobs
        .transition(&id, JobState::Starting, JobState::Running)
        .unwrap();
    let end = exited(0, GIB, Duration::from_secs(60));
    tables
        .jobs
        .end_and_free(&id, end, EndReason::Exited)
        .unwrap();

    clock.set(Timestamp::from_millis(3_000));
    assert!(pass(&placer, &tables).is_empty());
    assert!(!placer.engine().load_of("a").unwrap().implausible_lease_cpu);
    clock.set(Timestamp::from_millis(6_000));
    placer.heard(&host("a"), Some(&stuck(2)));
    let view = placer.engine().load_of("a").unwrap();
    assert!(view.implausible_lease_cpu);
    assert!(view.fallback);
}

//! The history the server teaches at its start ([`super::learn`]): the server's own job
//! tables over a store that can fail a read.

use std::sync::Arc;
use std::time::Duration;

use js_jobs::{
    Booked, EndReason, JobEnd, JobId, JobState, NewJob, Output, ResourceUsage, Retention,
};
use js_sched_placer::place::SchedPlacer;
use js_types::HostId;

use js_state::ManualClock;

use crate::agent_api::job_rig::{spec, tables_on};
use crate::job_store::ServerJobs;

/// Runs the job `name` to its exit with `usage` as what its host reported.
fn ran(clock: &ManualClock, jobs: &ServerJobs, name: &str, usage: Option<ResourceUsage>) {
    let id = JobId::parse(name).unwrap();
    let new = NewJob {
        id: Some(id.clone()),
        spec: spec(),
        priority: 0,
        client: None,
    };
    jobs.insert_queued(new).unwrap();
    let booked = Booked::Shape {
        cpu_millis: 1500,
        memory_bytes: 1 << 30,
        disk_bytes: 0,
        accelerators: 0,
    };
    jobs.place(&id, HostId::new("a").unwrap(), booked).unwrap();
    jobs.transition(&id, JobState::Placed, JobState::Starting)
        .unwrap();
    jobs.transition(&id, JobState::Starting, JobState::Running)
        .unwrap();
    clock.advance(Duration::from_secs(60));
    let end = JobEnd::Exited {
        exit_code: 0,
        output: Output {
            usage,
            ..Output::default()
        },
    };
    jobs.end_and_free(&id, end, EndReason::Exited).unwrap();
}

/// Asserts that a start counts the ended jobs the store holds that taught a class a memory
/// sample, and no others, and that a store whose read fails is logged and teaches none,
/// without refusing the start. Catches a start that forgets the history, one that counts a
/// job that ended with no usage as taught, and a failed read that stops the server.
#[test]
fn a_start_teaches_the_ended_jobs_and_a_failed_read_teaches_none() {
    let clock = Arc::<ManualClock>::default();
    let (store, jobs) = tables_on(clock.clone(), Retention::DEFAULT);
    ran(&clock, &jobs, "unmeasured", None);
    assert_eq!(super::learn(&SchedPlacer::default(), &jobs), 0);
    ran(
        &clock,
        &jobs,
        "measured",
        Some(ResourceUsage {
            cpu_time: Duration::from_secs(40),
            peak_memory_bytes: 1 << 29,
            cpu_throttle: None,
        }),
    );
    assert_eq!(super::learn(&SchedPlacer::default(), &jobs), 1);
    store.fail_read_after(0);
    assert_eq!(super::learn(&SchedPlacer::default(), &jobs), 0);
}

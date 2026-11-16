//! The history the server teaches at its start ([`super::learn`]): the server's own job
//! tables over a store that can fail a read.

use js_jobs::{Booked, EndReason, JobEnd, JobId, JobState, NewJob, Output};
use js_sched_placer::place::SchedPlacer;
use js_types::HostId;

use crate::agent_api::job_rig::{spec, tables};

/// Asserts that a start teaches the runs of the ended jobs the store holds, and that a store
/// whose read fails is logged and teaches none, without refusing the start. Catches a start
/// that forgets the history, and a failed read that stops the server.
#[test]
fn a_start_teaches_the_ended_jobs_and_a_failed_read_teaches_none() {
    let (store, jobs) = tables();
    let id = JobId::parse("ran").unwrap();
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
    let end = JobEnd::Exited {
        exit_code: 0,
        output: Output::default(),
    };
    jobs.end_and_free(&id, end, EndReason::Exited).unwrap();
    assert_eq!(super::learn(&SchedPlacer::default(), &jobs), 1);
    store.fail_read_after(0);
    assert_eq!(super::learn(&SchedPlacer::default(), &jobs), 0);
}

//! Two servers' placers over one real Postgres store, placing at once: a job is placed once and
//! a host is never booked past its capacity, whichever server's pass gets there first. Every
//! clock is manual; the servers run on threads, and the test waits for them, not for time.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::thread;

use js_agent_wire::NetworkCeiling;
use js_jobs::{Capacity, JobId, JobState};
use js_sched_placer::engine::jobs::{NoStarts, OpenHost};
use js_sched_placer::place::SchedPlacer;
use js_state::Store;
use js_state::postgres::{PostgresConfig, PostgresStore, test_database};
use js_types::HostId;

use super::tests::{clock, host, placer_on, queue};
use crate::job_store::{ServerIds, ServerJobs};
use crate::metrics::NoMetrics;
use crate::sched::engine_rig::GIB;

/// One server's job tables: a store handle of its own on the database `url`, as a second
/// process has.
fn tables_on(url: &str) -> ServerJobs {
    let config = PostgresConfig::parse(url).expect("the test URL parses");
    let store: Arc<dyn Store> = Arc::new(PostgresStore::open(&config).expect("opens"));
    js_jobs::JobStore::new(Arc::new(store), ServerIds::random()).expect("the job store opens")
}

/// The hosts `names`, each offering its 8 cores and 16 GiB as the capacity a placement is held to.
fn open_with_capacity(names: &[&str]) -> BTreeMap<HostId, OpenHost> {
    let offer = OpenHost {
        ceiling: NetworkCeiling::default(),
        host_jobs: false,
        rank: None,
        capacity: Some(Capacity {
            cpu_millis: 8000,
            memory_bytes: 16 * GIB,
            disk_bytes: 0,
            accelerators: 0,
        }),
    };
    names
        .iter()
        .map(|name| (host(name), offer.clone()))
        .collect()
}

/// The passes one server makes: `rounds` passes of its placer over its tables.
fn serve(placer: &SchedPlacer, jobs: &ServerJobs, rounds: usize) {
    let open = open_with_capacity(&["host-a", "host-b"]);
    for _ in 0..rounds {
        // A failed read or write of a pass is the next pass's to mend, as in the server.
        let _ = placer.place_jobs(jobs, &open, (&NoStarts, &NoMetrics));
    }
}

/// Two servers whose registries both hold the hosts `host-a` and `host-b` (8 cores each) place
/// 24 one-core jobs queued in the one store, from two threads, at once. Every placed job is on
/// one host and has one allocation, no host books more than its 8 cores, the 16 cores are all
/// booked once each server has read what the other wrote, and what is left queued is queued. It
/// catches two servers that place one job twice (each from its own picture of the queue), two
/// servers that each give a host its last cores, and a server that never sees what the other
/// wrote (its cached tables), which leaves cores unbooked while jobs wait.
#[test]
fn two_servers_place_each_job_once_and_never_book_a_host_past_its_cores() {
    let database = test_database();
    let url = database.url();
    let (first, second) = (tables_on(&url), tables_on(&url));
    for n in 0..24 {
        queue(&first, &format!("job-{n:02}"), 1, 0);
    }
    let time = clock();
    let (placer_a, placer_b) = (
        placer_on(&time, &["host-a", "host-b"]),
        placer_on(&time, &["host-a", "host-b"]),
    );
    thread::scope(|scope| {
        scope.spawn(|| serve(&placer_a, &first, 6));
        scope.spawn(|| serve(&placer_b, &second, 6));
    });
    // Each server reads again what the other wrote, so the passes that follow fill what is
    // left: nothing waits for a write of the server's own.
    serve(&placer_a, &first, 2);
    serve(&placer_b, &second, 2);

    let mut per_host: BTreeMap<String, u64> = BTreeMap::new();
    let mut placed = 0;
    for n in 0..24 {
        let id = JobId::parse(&format!("job-{n:02}")).unwrap();
        let job = first.get_job(&id).unwrap().unwrap();
        let allocation = first.get_allocation(&id).unwrap();
        match job.state {
            JobState::Queued => assert!(allocation.is_none(), "{id} is queued with an allocation"),
            JobState::Placed => {
                let allocation = allocation.expect("a placed job has its allocation");
                assert_eq!(job.host.as_ref(), Some(&allocation.host), "{id}");
                *per_host.entry(allocation.host.to_string()).or_default() += 1;
                placed += 1;
            }
            other => panic!("{id} is {other:?}"),
        }
    }
    assert_eq!(
        placed, 16,
        "every core of the two hosts is booked, and no more"
    );
    for name in ["host-a", "host-b"] {
        let booking = first.booked_on(&host(name)).unwrap();
        assert!(booking.cpu_millis <= 8000, "{name} books {booking:?}");
        assert_eq!(
            booking.allocations,
            per_host.get(name).copied().unwrap_or(0),
            "{name}: a placed job's allocation is not on its host, or a host books a job twice"
        );
    }
}

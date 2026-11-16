//! Two servers over one real Postgres store, serving at once: both answer, a job submitted to
//! one is read from the other, a job is placed once and a host is booked only to its cores
//! whichever server's tick gets there first, and a host's heartbeat is served only by the
//! server that heard its newest hello.
//!
//! The database is one `js_state::postgres::test_database`; the servers run in this process on
//! manual clocks and ticks, so nothing waits on time. The database calls block, so the database
//! is made outside the async runtime the servers run on.

use std::collections::BTreeSet;

use js_state::Store;
use js_state::postgres::test_database;
use tokio::runtime::Runtime;

use crate::agents_support::{beat, job_host_args};
use crate::jobs_support::{job_agent, job_of, job_state, per_job_hello, submit};
use crate::store_support::store_file;
use crate::support::{Harness, Scratch, replace, request};

/// The flags of a server over the store `url`, with the job service on.
fn flags(scratch: &Scratch, url: &str) -> Vec<std::ffi::OsString> {
    let mut args = job_host_args(scratch);
    args.extend(["--store".into(), store_file(scratch, url).into_os_string()]);
    args
}

/// Twelve jobs of 1.5 cores and 1 GiB are submitted to the first server of two that serve one
/// store, and the host `h1` (8 cores, which the engine's CPU slack lets book 8.8) says hello to
/// both, so both of them place on it. Both servers' ticks run, at once, for several rounds.
/// Exactly five jobs are placed (the sixth would book 9 cores), each with one allocation on `h1`
/// and no job twice; both servers answer for every job; and of the host's two connections only
/// the newest, the second server's, is served a heartbeat: the first server's answers 403
/// `session-mismatch`. It catches two servers that place one job twice or book the host past
/// its cores, a job submitted to one that the other does not see, and a server that serves a
/// host whose newer hello another server heard.
#[test]
fn two_servers_serve_one_store_place_each_job_once_and_keep_to_the_hosts_cores() {
    let database = test_database();
    let url = database.url();
    let runtime = Runtime::new().unwrap();
    let (first, second) = runtime.block_on(async {
        let scratch = Scratch::new("two_servers_a");
        let args = flags(&scratch, &url);
        let first = Harness::with(scratch, args).await;
        let scratch = Scratch::new("two_servers_b");
        let args = replace(flags(&scratch, &url), "--boot", "resume");
        let second = Harness::with(scratch, args).await;
        (first, second)
    });
    runtime.block_on(async {
        for server in [&first, &second] {
            let health = request(server.addr(), "GET", "/healthz", None, None).await;
            assert_eq!(health.status, 200, "both servers serve");
        }
        let hello = per_job_hello("h1");
        let on_first = job_agent(&first, &hello).await;
        let on_second = job_agent(&second, &hello).await;
        let ids: Vec<String> = (0..12).map(|n| format!("two-{n:02}")).collect();
        for id in &ids {
            let made = submit(&first, id).await;
            assert_eq!(made.status, 201, "{}", made.body);
        }
        for id in &ids {
            let read = job_of(&second, id).await;
            assert_eq!(
                read.status, 200,
                "the other server reads {id}: {}",
                read.body
            );
        }

        for _ in 0..4 {
            tokio::join!(first.ticks.step(), second.ticks.step());
        }

        let mut placed = BTreeSet::new();
        for id in &ids {
            let (one, other) = (job_state(&first, id).await, job_state(&second, id).await);
            assert_eq!(one, other, "{id}: the servers disagree about the job");
            if one != "JOB_STATE_QUEUED" {
                assert_eq!(one, "JOB_STATE_PLACED", "{id}");
                placed.insert(id.clone());
            }
        }
        assert_eq!(
            placed.len(),
            5,
            "the cores of h1 hold five jobs: {placed:?}"
        );
        let store = first.running.store().expect("the server has a store");
        let rows = store.scan_prefix("alloc/").expect("scans");
        let booked: BTreeSet<String> = rows
            .iter()
            .map(|(key, _)| key.trim_start_matches("alloc/").to_owned())
            .collect();
        assert_eq!(
            booked, placed,
            "each placed job has one allocation, and no other does"
        );
        let on_host = store.scan_prefix("idx/alloc/host/h1/").expect("scans");
        assert_eq!(on_host.len(), 5, "every allocation is on h1");

        // The host said hello to the second server last, so its connection is the newest.
        assert_eq!(beat(&first, &on_first).await.status, 403);
        assert_eq!(beat(&second, &on_second).await.status, 200);
    });
    runtime.block_on(async {
        first.running.shutdown().await.unwrap();
        second.running.shutdown().await.unwrap();
    });
}

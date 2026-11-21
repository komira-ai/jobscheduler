//! What a server reads of the `node/` rows: lost past the longest grace, the pools it serves,
//! the marks and taints of the hosts, and a store that cannot be read. Every clock is manual.

use std::time::Duration;

use js_agent_wire::Phase;
use js_jobs::{Isolation, ScaleMark};
use js_types::HostId;

use super::*;
use crate::agent_api::job_rig::{BOOKED, Rig, clocked_rig, placed, spec};
use crate::agent_api::peer_rig::NEVER;
use crate::agent_api::peer_rig::{Row, write_row};

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

/// The rig of a server whose pool is `burst`, with `host-a` joined through the route.
async fn joined(name: &str) -> Rig {
    let mut rig = clocked_rig(name, false, NEVER);
    rig.join("host-a", false).await;
    rig
}

/// Asserts that a host is lost only once it was last heard more than the registry's longest
/// grace ago: heard just now it is not, at exactly the grace it is not, one millisecond past
/// it is. Catches a row judged lost at the grace itself, and one never judged lost.
#[tokio::test]
async fn a_row_is_lost_only_past_the_longest_grace() {
    let rig = joined("rows_grace").await;
    let registry = &rig.agents.registry;
    let seen = || read(registry, &rig.jobs).unwrap().remove(0);
    let first = seen();
    assert!(!first.lost);
    let silent_until = first
        .row
        .record
        .heard_at
        .saturating_add(registry.longest_grace());
    rig.advance(silent_until.since(js_state::Clock::now(&*rig.clock)));
    assert!(!seen().lost);
    rig.advance(Duration::from_millis(1));
    assert!(seen().lost);
}

/// Asserts that a row is left out once its host was last heard more than the registry's
/// forgetting time ago (the longest grace plus the retention), and is read at exactly that
/// time. Catches a host the registry forgot still listed and counted from the row that
/// outlives it.
#[tokio::test]
async fn a_row_silent_past_the_forgetting_time_is_left_out() {
    let rig = joined("rows_forgotten").await;
    let registry = &rig.agents.registry;
    let heard = read(registry, &rig.jobs)
        .unwrap()
        .remove(0)
        .row
        .record
        .heard_at;
    let until = heard.saturating_add(registry.forget_after());
    rig.advance(until.since(js_state::Clock::now(&*rig.clock)));
    assert_eq!(read(registry, &rig.jobs).unwrap().len(), 1);
    rig.advance(Duration::from_millis(1));
    assert!(read(registry, &rig.jobs).unwrap().is_empty());
}

/// Asserts that a row of a pool the server does not serve, and a row with no booked shape, are
/// left out, while a row of the pool is read with the shape the registry places it on and the
/// phase it announced. Catches a host of another pool offered to this server's jobs, and a host
/// with nothing booked judged on a shape it does not have.
#[tokio::test]
async fn rows_of_unserved_pools_and_without_a_booked_shape_are_left_out() {
    let rig = clocked_rig("rows_pools", false, NEVER);
    let row = |host, pool, booked| Row {
        host,
        pool,
        host_jobs: None,
        booked,
    };
    write_row(&rig.jobs, &row("host-x", "elsewhere", true), Phase::Serving);
    write_row(&rig.jobs, &row("host-y", "burst", false), Phase::Serving);
    write_row(&rig.jobs, &row("host-z", "burst", true), Phase::Draining);
    let seen = read(&rig.agents.registry, &rig.jobs).unwrap();
    let hosts: Vec<&HostId> = seen.iter().map(Seen::host).collect();
    assert_eq!(hosts, [&host("host-z")]);
    assert_eq!(seen[0].row.record.state.phase, Phase::Draining);
    assert_eq!(seen[0].placed.resources().cpu_millis(), 8000);
    assert!(!seen[0].reports_host_jobs());
}

/// Asserts that a row carries the scale-down mark of its host and the taint row the store
/// holds for it, and a host with neither carries none. Catches a mark or taint attached to
/// another host's row, and one dropped.
#[tokio::test]
async fn a_row_carries_the_mark_and_the_taint_of_its_host() {
    let mut rig = joined("rows_marks").await;
    rig.join("host-b", false).await;
    rig.join("host-c", false).await;
    assert!(
        rig.jobs
            .mark_host(&host("host-a"), ScaleMark::Releasing, None)
            .unwrap()
    );
    let mut host_job = spec();
    host_job.constraints.isolation = Isolation::Host;
    placed(&rig.jobs, "job-t", "host-b", host_job, BOOKED);
    let seen = read(&rig.agents.registry, &rig.jobs).unwrap();
    let by_host = |name: &str| seen.iter().find(|one| one.host() == &host(name)).unwrap();
    assert_eq!(by_host("host-a").mark, Some(ScaleMark::Releasing));
    assert!(by_host("host-a").taint.is_none());
    assert_eq!(by_host("host-b").mark, None);
    let taint = by_host("host-b").taint.as_ref().unwrap();
    assert_eq!(taint.job.to_string(), "job-t");
    assert_eq!(
        (by_host("host-c").mark, by_host("host-c").taint.is_none()),
        (None, true)
    );
}

/// Asserts that a read fails, and says so rather than answering with fewer rows, when the
/// store's clock cannot be read, and when the scan of the rows, of the marks or of the taints
/// fails. Catches a read that treats a failed scan as an empty table.
#[tokio::test]
async fn a_failed_read_of_the_store_is_an_error_at_each_step() {
    let rig = joined("rows_errors").await;
    let registry = &rig.agents.registry;
    assert_eq!(read(registry, &rig.jobs).unwrap().len(), 1);
    rig.store.fail_clock(true);
    assert!(read(registry, &rig.jobs).is_err());
    rig.store.fail_clock(false);
    for reads_let_through in 0..3 {
        rig.store.fail_read_after(reads_let_through);
        assert!(read(registry, &rig.jobs).is_err(), "{reads_let_through}");
    }
}

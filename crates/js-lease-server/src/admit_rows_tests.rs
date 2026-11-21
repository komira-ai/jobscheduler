//! Admission over the `node/` rows of the job store: a host only a row knows is capable, as a
//! host the registry holds is, and a refusal is never taken on less than the rows.

use std::time::Duration;

use js_agent_wire::Phase;
use js_jobs::{Isolation, ScaleMark};
use js_types::JobNetwork as Reach;
use js_types::Resources;

use super::*;
use crate::agent_api::job_rig::{Rig, clocked_rig};
use crate::agent_api::peer_rig::NEVER;
use crate::agent_api::peer_rig::{Row, peer_of, write_row};
use crate::api::rig::{Lines, process_sink};
use crate::job_api::submit::Admission;

/// What a job of a core and a half and a GiB, on a container, needs.
fn wanted() -> Requirements {
    Requirements {
        resources: Resources::new(1500, 1 << 30, 0, None).unwrap(),
        disk_bytes: None,
        os: None,
        arch: None,
        isa_level: None,
        os_version: None,
        exec: None,
        accelerator: None,
        network: Reach::None,
        host_ports: Vec::new(),
        layered_rootfs: false,
        mount_providers: Vec::new(),
    }
}

/// Admission over `rig`'s registry and job tables, and no launching provider.
fn admission(rig: &Rig) -> JobAdmission {
    JobAdmission::new(
        Admit::default(),
        rig.agents.registry.clone(),
        Arc::clone(&rig.jobs),
    )
}

/// A row of `host` in the pool `pool`, with a booked shape, whose claim says `host_jobs`.
fn row<'a>(host: &'a str, pool: &'a str, host_jobs: Option<bool>) -> Row<'a> {
    Row {
        host,
        pool,
        host_jobs,
        booked: true,
    }
}

/// Asserts how a host known by its row is viewed: serving when heard within the longest
/// grace, lost past it; leaving the pool only when its row carries a release mark, not a drain
/// mark or none; on the shape the registry places it on. Catches a row that is never lost, a
/// drain taken for a release (a drained host comes back), and a release that does not show.
#[tokio::test]
async fn a_host_known_by_its_row_is_viewed_by_its_hearing_and_its_mark() {
    let mut rig = clocked_rig("admit_view", false, NEVER);
    for name in ["host-a", "host-b", "host-c"] {
        rig.join(name, false).await;
    }
    let jobs = &rig.jobs;
    let (a, b) = (
        HostId::new("host-a").unwrap(),
        HostId::new("host-b").unwrap(),
    );
    assert!(jobs.mark_host(&a, ScaleMark::Draining, None).unwrap());
    assert!(jobs.mark_host(&b, ScaleMark::Releasing, None).unwrap());
    let views = |rig: &Rig| -> Vec<HostView> {
        let seen = node_rows::read(&rig.agents.registry, &rig.jobs).unwrap();
        seen.iter().map(row_host).collect()
    };
    let before = views(&rig);
    let shown: Vec<_> = before
        .iter()
        .map(|view| (view.id.as_str(), view.state, view.draining))
        .collect();
    assert_eq!(
        shown,
        [
            ("host-a", HostState::Serving, false),
            ("host-b", HostState::Serving, true),
            ("host-c", HostState::Serving, false),
        ]
    );
    assert_eq!(before[0].total.cpu_millis(), 8000);
    assert_eq!(
        before[0].facts.get("arch").map(String::as_str),
        Some("x86_64")
    );
    rig.advance(rig.agents.registry.longest_grace() + Duration::from_millis(1));
    assert!(views(&rig).iter().all(|view| view.state == HostState::Lost));
}

/// Asserts the regression: a server whose registry holds no host admits a job over the hosts
/// whose rows the store holds (a host that said hello to another server), refuses it with no
/// row, and counts a lost host, a host with a drain mark and a host in both the registry and
/// the rows as capable, but not a host with a release mark, a host of a pool it does not serve,
/// or, for a job on the host itself, a host whose claim does not report `host-jobs`. Catches a
/// submit refused `no-capable-host` before the hosts said hello there, and a row counted
/// that the registry would not count.
#[tokio::test]
async fn a_job_is_admitted_over_the_hosts_the_rows_hold() {
    let mut first = clocked_rig("admit_rows_a", false, NEVER);
    let second = peer_of("admit_rows_b", &first);
    let jobs = &second.jobs;
    let at = admission(&second);
    let want = wanted();
    let ever = |isolation| at.can_ever_serve(&want, isolation);
    assert!(!ever(Isolation::Contained).await);

    write_row(jobs, &row("host-u", "elsewhere", None), Phase::Serving);
    assert!(!ever(Isolation::Contained).await);

    write_row(jobs, &row("host-r", "burst", Some(true)), Phase::Serving);
    let released = HostId::new("host-r").unwrap();
    assert!(
        jobs.mark_host(&released, ScaleMark::Releasing, None)
            .unwrap()
    );
    assert!(!ever(Isolation::Contained).await);

    write_row(jobs, &row("host-d", "burst", Some(false)), Phase::Serving);
    let drained = HostId::new("host-d").unwrap();
    assert!(jobs.mark_host(&drained, ScaleMark::Draining, None).unwrap());
    assert!(ever(Isolation::Contained).await);
    assert!(!ever(Isolation::Host).await);

    write_row(jobs, &row("host-h", "burst", Some(true)), Phase::Serving);
    assert!(ever(Isolation::Host).await);

    second.advance(second.agents.registry.longest_grace() + Duration::from_millis(1));
    assert!(ever(Isolation::Contained).await);
    assert!(ever(Isolation::Host).await);

    first.join("host-a", false).await;
    assert!(
        admission(&first)
            .can_ever_serve(&wanted(), Isolation::Contained)
            .await
    );
}

/// Asserts that a job is admitted, with the failure logged, when the rows cannot be read, and
/// is refused again once they can: a refusal is never taken on less than the rows. Catches a
/// store failure that refuses jobs `no-capable-host`, and one that is not said.
#[tokio::test]
async fn a_job_is_admitted_unjudged_when_the_rows_cannot_be_read() {
    process_sink();
    let lines = Lines::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(lines.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let rig = clocked_rig("admit_unread", false, NEVER);
    let at = admission(&rig);
    rig.store.fail_clock(true);
    assert!(at.can_ever_serve(&wanted(), Isolation::Contained).await);
    let text = lines.take();
    let said = "the hosts' rows could not be read: the job is admitted unjudged";
    assert!(text.contains(said), "{text}");
    rig.store.fail_clock(false);
    assert!(!at.can_ever_serve(&wanted(), Isolation::Contained).await);
}

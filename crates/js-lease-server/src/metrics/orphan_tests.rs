//! The gauges of the hosts that hold orphans: what a scrape reads of the hosts' rows, and the
//! lines it writes for them.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use js_jobs::{NodeHello, NodeState, Orphan, Retention};
use js_state::{ManualClock as StoreClock, StoreTime};
use js_types::HostId;

use super::Recorder;
use super::render::{JobsSeen, Snapshot, render};
use super::scrape::orphans_seen;
use crate::agent_api::job_rig::tables_on;

fn hello() -> NodeHello {
    NodeHello {
        session: None,
        pool: "burst".to_owned(),
        claim: None,
        shape: None,
        labels: BTreeMap::new(),
        phase: js_agent_wire::Phase::Serving,
        agent_process: None,
        agent_version: None,
    }
}

fn at(secs: u64) -> StoreTime {
    StoreTime::from_duration(Duration::from_secs(secs))
}

/// Asserts what a scrape reads of the hosts' rows: a host whose row holds orphans is read with
/// how many and how long the oldest has been listed by the store's clock (the earliest
/// first-seen time, whatever the ids' order), and a host that holds none is not read at all.
/// It catches an age read from the newest orphan, and a gauge written for every host.
#[test]
fn a_scrape_reads_each_host_that_holds_orphans_with_its_oldest() {
    let clock = Arc::new(StoreClock::default());
    clock.advance(Duration::from_secs(1000));
    let (_, jobs) = tables_on(clock, Retention::DEFAULT);
    for host in ["host-a", "host-b"] {
        jobs.hello_node(&HostId::new(host).unwrap(), &hello())
            .unwrap();
    }
    let row = jobs
        .read_node(&HostId::new("host-b").unwrap())
        .unwrap()
        .unwrap();
    let orphans = BTreeMap::from([
        (
            "a-newer".to_owned(),
            Orphan {
                first_seen: at(900),
                end_asked: false,
            },
        ),
        (
            "z-older".to_owned(),
            Orphan {
                first_seen: at(400),
                end_asked: true,
            },
        ),
    ]);
    let state = NodeState {
        orphans,
        ..row.record.state.clone()
    };
    jobs.settle_node(&row, state).unwrap().unwrap();
    let (now, rows) = jobs.nodes().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        orphans_seen(&rows, now),
        [("host-b".to_owned(), 2, now.since(at(400)))]
    );
}

/// Asserts the lines of the two gauges: one of each for each host that holds orphans, by
/// host, the age in seconds; and the families alone, with no line, when no host holds one.
/// It catches a host closed by orphans that a dashboard cannot see, and a gauge of 0 for
/// every other host.
#[test]
fn each_host_that_holds_orphans_has_its_count_and_oldest_age() {
    let snapshot = |jobs: JobsSeen| Snapshot {
        jobs: Some(jobs),
        hosts: Vec::new(),
        curves: None,
        telemetry: None,
        memory: None,
    };
    let mut held = JobsSeen::default();
    held.orphans("host-b", 2, Duration::from_millis(1500));
    let text = render(&snapshot(held), &Recorder::default());
    assert!(
        text.contains("\njs_host_orphans{host=\"host-b\"} 2\n"),
        "{text}"
    );
    assert!(
        text.contains("\njs_host_oldest_orphan_age_seconds{host=\"host-b\"} 1.5\n"),
        "{text}"
    );
    let none = render(&snapshot(JobsSeen::default()), &Recorder::default());
    assert!(none.contains("# TYPE js_host_orphans gauge\n"), "{none}");
    assert!(!none.contains("js_host_orphans{"), "{none}");
    assert!(
        !none.contains("js_host_oldest_orphan_age_seconds{"),
        "{none}"
    );
}

/// Asserts a scrape of a server end to end: after a host's heartbeat lists a job the store
/// holds no record of, the scrape's text has that host's orphan count and the age of its
/// orphan, read from its row at the store's clock, and nothing for the host that lists none.
/// It catches a scrape that reads the hosts' rows and drops what it found, so a host closed by
/// an orphan would show no gauge.
#[tokio::test]
async fn a_scrape_of_a_server_shows_the_host_its_orphan_closes() {
    let mut rig = crate::agent_api::job_rig::clocked_rig(
        "metrics_orphans",
        true,
        crate::job_expiry::JobLimits::default(),
    );
    rig.register("host-a", true);
    rig.register("host-b", true);
    let mut agent = crate::agent_api::job_rig::FakeAgent::new("host-a");
    agent.hold("ghost", "00", js_agent_wire::HeldState::Running);
    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(reply.unknown_jobs, ["ghost"]);
    rig.beat(&crate::agent_api::job_rig::FakeAgent::new("host-b"))
        .await
        .unwrap();
    rig.advance(Duration::from_secs(90));
    let shared = super::tests::shared_over(&rig);
    let page = std::num::NonZeroUsize::MIN;
    let seen = super::scrape::snapshot(&shared, page).await.unwrap();
    let text = render(&seen, &Recorder::default());
    assert!(
        text.contains("\njs_host_orphans{host=\"host-a\"} 1\n"),
        "{text}"
    );
    assert!(!text.contains("js_host_orphans{host=\"host-b\"}"), "{text}");
    let age = "js_host_oldest_orphan_age_seconds{host=\"host-a\"} ";
    let line = text.lines().find(|line| line.starts_with(age)).unwrap();
    let secs: f64 = line[age.len()..].parse().unwrap();
    assert!((90.0..91.0).contains(&secs), "{line}");
}

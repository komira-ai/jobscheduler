//! A start's network against the runtime's ceiling: a job above it is refused with a code
//! that names it, before anything is made, and never run with less; a job at or under it runs
//! at the level it asked for.

use js_agent::lifecycle::Event;
use js_agent_wire::{NetworkCeiling, Outcome, PortRange, refusal};
use js_types::{JobNetwork, PortMap};

use super::{Harness, job, names};

fn ceiling(level: JobNetwork, ports: &[&str]) -> NetworkCeiling {
    NetworkCeiling {
        level,
        ports: ports
            .iter()
            .map(|text| PortRange::parse(text).expect("a range"))
            .collect(),
    }
}

/// The text the agent keeps for the refusal of `run_id`.
fn why(events: &[Event], run_id: &str) -> String {
    events
        .iter()
        .find_map(|event| match event {
            Event::Refused {
                run_id: run, why, ..
            } if run == run_id => Some(why.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no refusal of {run_id}: {events:?}"))
}

/// Proves a job above the ceiling is refused, not lowered: under the ceilings `none` and
/// `allow-list`, starts that ask for `allow-list` and `open` above them are answered
/// `failed_to_start` with `network-above-ceiling`, the text names the level asked for and the
/// ceiling, and the runtime made nothing (no create; the ceiling was asked, nothing else). A
/// job at or under the ceiling is admitted and created at the level it asked for. The
/// comparison comes after the mounts and before the egress check: a bad mount is refused for
/// the mount, and a failing egress check is not reached. Catches a job lowered and run, a
/// generic code, and a check made after a create.
#[test]
fn a_job_above_the_ceiling_is_refused_with_a_code_naming_it() {
    let cases = [
        (JobNetwork::None, JobNetwork::AllowList),
        (JobNetwork::None, JobNetwork::Open),
        (JobNetwork::AllowList, JobNetwork::Open),
    ];
    for (n, (top, asked)) in cases.into_iter().enumerate() {
        let mut h = Harness::new(&format!("net-above-{n}"));
        h.fake.set_ceiling(ceiling(top, &[]));
        h.fake.fail("egress", 1);
        let mut spec = job("r1");
        spec.network = asked;
        let events = h.start(spec);
        assert_eq!(
            names(&events),
            [format!("refused r1 {}", refusal::NETWORK_ABOVE_CEILING)]
        );
        let said = why(&events, "r1");
        assert_eq!(
            said,
            format!("network {asked} asked, agent ceiling is {top}")
        );
        assert!(
            h.fake.created().is_empty(),
            "{asked} under {top} was created"
        );
        assert!(
            h.log
                .borrow()
                .iter()
                .all(|call| !call.starts_with("create"))
        );
        assert_eq!(h.fake.ceiling_asks(), 1);
        let results = h.life.results();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].outcome, Outcome::FailedToStart);
        assert_eq!(
            results[0].detail.as_deref(),
            Some(refusal::NETWORK_ABOVE_CEILING)
        );
        // The egress check was not reached: its failure is still owed to the next start.
        let mut within = job("r2");
        within.network = top;
        let names_of = names(&h.start(within));
        assert_eq!(names_of, ["refused r2 egress-unchecked"]);
    }

    for (n, top) in [JobNetwork::None, JobNetwork::AllowList, JobNetwork::Open]
        .into_iter()
        .enumerate()
    {
        let mut h = Harness::new(&format!("net-within-{n}"));
        h.fake.set_ceiling(ceiling(top, &[]));
        let levels = [JobNetwork::None, JobNetwork::AllowList, JobNetwork::Open];
        let admitted: Vec<JobNetwork> = levels.into_iter().filter(|l| *l <= top).collect();
        for (k, level) in admitted.iter().enumerate() {
            let mut spec = job(&format!("r{k}"));
            spec.network = *level;
            assert_eq!(names(&h.start(spec)), [format!("started r{k}")]);
        }
        let created: Vec<JobNetwork> = h.fake.created().iter().map(|run| run.network).collect();
        assert_eq!(created, admitted, "each runs at the level it asked for");
    }

    let mut h = Harness::new("net-mount-first");
    let mut spec = job("r1");
    spec.network = JobNetwork::Open;
    spec.mounts = vec![js_agent_wire::Mount {
        root_name: "nowhere".into(),
        rel: "x".into(),
        ro: true,
    }];
    assert_eq!(
        names(&h.start(spec)),
        [format!("refused r1 {}", refusal::INVALID_MOUNT)]
    );
    assert_eq!(h.fake.ceiling_asks(), 0, "the mounts are checked first");
}

/// Proves declared ports are carried and refused, not forwarded or dropped: under an `open`
/// ceiling that lists `80` and `8000-8010`, a job that declares an unlisted host port is
/// refused `port-above-ceiling` naming it (before the ports are found unsupported), one that
/// declares only listed ports is refused `ports-not-supported`, and neither creates
/// anything; a job above the level is `network-above-ceiling` whatever its ports. Catches a
/// job run with its ports silently ignored, and an unlisted port reported as merely
/// unsupported.
#[test]
fn a_job_with_ports_is_refused_until_forwarding_exists() {
    let mut h = Harness::new("net-ports");
    h.fake
        .set_ceiling(ceiling(JobNetwork::Open, &["80", "8000-8010"]));
    let with = |run: &str, level: JobNetwork, hosts: &[u16]| {
        let mut spec = job(run);
        spec.network = level;
        spec.ports = hosts
            .iter()
            .map(|host| PortMap {
                host: *host,
                container: 8080,
            })
            .collect();
        spec
    };
    let events = h.start(with("r1", JobNetwork::Open, &[80, 9000]));
    assert_eq!(
        names(&events),
        [format!("refused r1 {}", refusal::PORT_ABOVE_CEILING)]
    );
    assert_eq!(
        why(&events, "r1"),
        "host port 9000 asked, agent ceiling forwards 80,8000-8010"
    );
    let events = h.start(with("r2", JobNetwork::AllowList, &[80, 8005]));
    assert_eq!(
        names(&events),
        [format!("refused r2 {}", refusal::PORTS_NOT_SUPPORTED)]
    );
    assert!(h.fake.created().is_empty(), "a job with ports was created");
    let mut h = Harness::new("net-ports-above");
    h.fake.set_ceiling(ceiling(JobNetwork::AllowList, &["80"]));
    let events = h.start(with("r3", JobNetwork::Open, &[80]));
    assert_eq!(
        names(&events),
        [format!("refused r3 {}", refusal::NETWORK_ABOVE_CEILING)]
    );
    assert!(h.fake.created().is_empty());
}

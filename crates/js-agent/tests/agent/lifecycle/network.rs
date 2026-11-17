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

/// The one event of a start refused with `code` and `why`.
fn refused(run_id: &str, code: &'static str, why: &str) -> Vec<Event> {
    vec![Event::Refused {
        run_id: run_id.to_owned(),
        code,
        why: why.to_owned(),
    }]
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
        let said = format!("network {asked} asked, agent ceiling is {top}");
        assert_eq!(events, refused("r1", refusal::NETWORK_ABOVE_CEILING, &said));
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
        // The egress check was not reached: its failure is still owed to the next start
        // the firewall carries. Under the ceiling `none` there is none: a `none` job is never
        // checked, and starts.
        assert_eq!(h.fake.egress_asks(), 0);
        let mut within = job("r2");
        within.network = top;
        let names_of = names(&h.start(within));
        let expected = match top {
            JobNetwork::None => "started r2",
            _ => "refused r2 egress-unchecked",
        };
        assert_eq!(names_of, [expected]);
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

/// Proves a job's declared ports are admitted only where they can be forwarded, and reach
/// the runtime when they are: under an `open` ceiling that lists `80` and `8000-8010`, a job
/// that declares an unlisted host port is refused `port-above-ceiling` naming it and the list;
/// a `none` job that declares listed ports is refused `ports-not-supported` (nothing wires
/// it); an `allow-list` job that declares only listed ports is admitted and created with its
/// ports, as given, at its level. Under the ceiling `none` (mode none, which lists no port),
/// and under a wired ceiling that lists none, any port is `ports-not-supported`; a job above
/// the level is `network-above-ceiling` whatever its ports. A refused job creates nothing.
/// Catches the refusal of every port surviving the forwarding, a job run with its ports
/// dropped, an unlisted port admitted, and a port admitted where nothing forwards it.
#[test]
fn a_job_with_a_listed_port_is_admitted_and_forwarded() {
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
    let mut h = Harness::new("net-ports");
    h.fake
        .set_ceiling(ceiling(JobNetwork::Open, &["80", "8000-8010"]));
    let events = h.start(with("r1", JobNetwork::Open, &[80, 9000]));
    assert_eq!(
        events,
        refused(
            "r1",
            refusal::PORT_ABOVE_CEILING,
            "host port 9000 asked, agent ceiling forwards 80,8000-8010"
        )
    );
    let events = h.start(with("r2", JobNetwork::None, &[80]));
    assert_eq!(
        events,
        refused(
            "r2",
            refusal::PORTS_NOT_SUPPORTED,
            "1 declared port(s) asked for a job with network none, which has no address to \
             forward to"
        )
    );
    assert!(h.fake.created().is_empty(), "a refused job was created");
    let admitted = with("r3", JobNetwork::AllowList, &[80, 8005]);
    assert_eq!(names(&h.start(admitted.clone())), ["started r3"]);
    let created = h.fake.created();
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].network, JobNetwork::AllowList);
    assert_eq!(
        created[0].ports, admitted.ports,
        "the ports reach the runtime"
    );

    for (n, top, listed) in [
        (0, JobNetwork::None, &[][..]),
        (1, JobNetwork::Open, &[][..]),
    ] {
        let mut h = Harness::new(&format!("net-ports-unforwarded-{n}"));
        h.fake.set_ceiling(ceiling(top, listed));
        let events = h.start(with("r4", top, &[80]));
        assert_eq!(
            events,
            refused(
                "r4",
                refusal::PORTS_NOT_SUPPORTED,
                "1 declared port(s) asked, and this agent forwards no declared port"
            ),
            "{top}"
        );
        assert!(h.fake.created().is_empty());
    }

    let mut h = Harness::new("net-ports-above");
    h.fake.set_ceiling(ceiling(JobNetwork::AllowList, &["80"]));
    let events = h.start(with("r5", JobNetwork::Open, &[80]));
    assert_eq!(
        names(&events),
        [format!("refused r5 {}", refusal::NETWORK_ABOVE_CEILING)]
    );
    assert!(h.fake.created().is_empty());
}

/// Proves the firewall is checked before each create of a job it carries and never for a
/// `none` job: under the ceiling `open`, with every check failing, a `none` job starts and
/// the check was not asked; an `allow-list` and an `open` job are each refused
/// `egress-unchecked` after one check each, creating nothing; once the check passes again an
/// `allow-list` job is checked once more and starts. Catches a `none` job that pays for (or is
/// refused by) a firewall it does not use, and a wired job started without its check.
#[test]
fn the_firewall_is_checked_before_a_wired_jobs_create_and_never_for_a_none_job() {
    let mut h = Harness::new("net-check-wired");
    h.fake.set_ceiling(ceiling(JobNetwork::Open, &[]));
    h.fake.fail("egress", u32::MAX);
    assert_eq!(names(&h.start(job("r1"))), ["started r1"]);
    assert_eq!(h.fake.egress_asks(), 0);
    for (run, level) in [("r2", JobNetwork::AllowList), ("r3", JobNetwork::Open)] {
        let mut spec = job(run);
        spec.network = level;
        assert_eq!(
            names(&h.start(spec)),
            [format!("refused {run} egress-unchecked")]
        );
    }
    assert_eq!(h.fake.egress_asks(), 2);
    h.fake.fail("egress", 0);
    let mut spec = job("r4");
    spec.network = JobNetwork::AllowList;
    assert_eq!(names(&h.start(spec)), ["started r4"]);
    assert_eq!(h.fake.egress_asks(), 3);
    let created: Vec<String> = h.fake.created().iter().map(|r| r.run_id.clone()).collect();
    assert_eq!(created, ["r1", "r4"]);
}

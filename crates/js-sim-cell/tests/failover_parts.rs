//! The machinery the failover family added, each part shown to do what it says: a server of
//! `ServerSpec::ordered` with a cloud for each of its launching providers, named in the
//! trace and numbering its own machines; what the recording wrapper sees at each acquire;
//! the actions on one named cloud (`exhaust_provider`, `quota_refuse`,
//! `script_provider_by_name`, `preempt_machine_of`), on a cloud the cell has and on one it
//! has not; and the expectations about a named cloud.
//!
//! Sizes: two cells driven by hand, at most one registered host and three machines each
//! for at most [`LOOK_AT`] of virtual time, and one plain cell with no host.

#[path = "cell/failover.rs"]
#[allow(
    dead_code,
    reason = "the failover family's scenarios are not run here: these tests take only its providers, offers and server"
)]
mod failover;

use std::path::Path;

use js_elastic::RefusalCode;
use js_elastic_fake::{Call, Step};
use js_sim::Faults;
use js_sim_cell::actions::{
    exhaust_provider, preempt_machine_of, quota_refuse, script_provider_by_name,
};
use js_sim_cell::{
    AcquireAnswer, Action, Cell, CloudEvent, Counter, Expect, ServerSpec, Setup, Shape,
};
use js_testkit::dir::TestDir;
use js_types::sched::FarmTime;

/// Past the first machine's launch, its boot (120 s) and its hello.
const LOOK_AT: FarmTime = FarmTime::from_millis(200_000);

/// A cell of seed 1 keeping its trace lines, over [`failover::server`] of `statics`
/// registered hosts (each added) and clouds of at most two machines each.
fn ordered_cell(name: &str, statics: usize) -> Cell {
    let spec = failover::server(statics, 2, 2);
    let setup = Setup::new(1).with_trace_lines();
    let mut cell =
        Cell::new(setup, spec, scratch(name), Faults::default()).expect("the cell starts");
    for _ in 0..statics {
        cell.add_agent().expect("a host id");
    }
    cell
}

/// A cell of a pool of hosts that register themselves: no elastic pool.
fn plain_cell(name: &str) -> Cell {
    let spec = ServerSpec::whole_node_pool("burst", Shape::gpu_host(), 1);
    let setup = Setup::new(1).with_trace_lines();
    Cell::new(setup, spec, scratch(name), Faults::default()).expect("the cell starts")
}

/// A scratch directory of its own.
fn scratch(name: &str) -> TestDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("js-sim-cell");
    TestDir::under(root, name)
}

/// The cell's trace lines that start `<t> cell cloud `, without the time and `cell`.
fn lines(cell: &mut Cell) -> Vec<String> {
    let lines = cell.trace_lines().expect("lines are kept");
    let cell_lines = lines.iter().filter_map(|line| line.split_once(" cell "));
    let kept = cell_lines.filter(|(_, rest)| rest.starts_with("cloud "));
    kept.map(|(_, rest)| rest.to_owned()).collect()
}

/// Asserts that a server of `ServerSpec::ordered` has a cloud for each launching provider,
/// first to last, each observed with its name, its place, its share and its offers; that
/// with the first exhausted the job's machine is launched by the second, which numbers it
/// from 0 and names it, and its node, by the provider; that the second cloud's wrapper saw
/// the first at each of its acquires, with every offer refused; and that a preemption of the
/// second cloud's machine by name is traced under that name; and the expectations of a named
/// cloud's acquires and joined machines, met and unmet. Catches clouds that share one
/// count of machines, a trace that names a later cloud's machine as the first cloud's, a
/// wrapper that does not see the clouds before it, and a by-name action
/// that acts on the first cloud.
#[test]
fn an_ordered_server_has_a_named_cloud_for_each_provider() {
    let mut cell = ordered_cell("failover-ordered", 1);
    exhaust_provider(failover::EC2).run(&mut cell);
    cell.submit(&failover::job());
    cell.submit(&failover::job());
    cell.run_until(LOOK_AT).expect("no violation");

    let names: Vec<&str> = cell
        .clouds()
        .iter()
        .map(|cloud| cloud.pool().name())
        .collect();
    assert_eq!(names, [failover::EC2, failover::LAMBDA]);
    let clouds = &cell.observations().clouds;
    let ranks: Vec<(&str, usize, u64)> = clouds
        .iter()
        .map(|facts| (facts.pool.as_str(), facts.rank, facts.max_instances))
        .collect();
    assert_eq!(ranks, [(failover::EC2, 0, 2), (failover::LAMBDA, 1, 2)]);
    assert_eq!(clouds[0].offers.len(), failover::EC2_OFFERS);
    assert_eq!(clouds[1].offers.len(), failover::LAMBDA_OFFERS);
    let seen = &clouds[1].seen;
    assert!(!seen.is_empty(), "the second cloud was asked");
    for each in seen {
        let earlier: Vec<&str> = each.earlier.iter().map(|e| e.provider.as_str()).collect();
        assert_eq!(earlier, [failover::EC2]);
        assert!(each.earlier[0].open.is_empty(), "{each:?}");
    }
    assert!(clouds[0].seen.iter().all(|each| each.earlier.is_empty()));

    let traced = lines(&mut cell);
    assert!(
        traced.contains(&"cloud ec2 exhaust applied".to_owned()),
        "{traced:?}"
    );
    assert!(
        traced.contains(&"cloud lambda launch m000".to_owned()),
        "{traced:?}"
    );
    assert!(
        traced.contains(&"cloud lambda boot m000".to_owned()),
        "{traced:?}"
    );
    assert!(
        cell.machine(0).is_none(),
        "the first cloud launched nothing"
    );
    assert!(cell.cloud_named(failover::LAMBDA).is_some());

    preempt_machine_of(failover::LAMBDA, 0).run(&mut cell);
    let traced = lines(&mut cell);
    assert!(
        traced.contains(&"cloud lambda preempt m000".to_owned()),
        "{traced:?}"
    );
    assert_eq!(cell.total_counters().get(Counter::MachinePreempted), 1);
    assert_eq!(Expect::machines_joined(failover::EC2, 0).unmet(&cell), None);
    let over = Expect::acquires_at_most(failover::EC2, 0).unmet(&cell);
    let over = over.expect("the first cloud was asked");
    assert!(
        over.starts_with("acquires-at-most(ec2): wanted at most 0, got "),
        "{over}"
    );
    assert_eq!(
        Expect::acquires_at_most(failover::LAMBDA, 99).unmet(&cell),
        None
    );
}

/// Asserts that an offer the cloud refuses for quota in one zone is refused, by the fake
/// itself (it counts the refusal), at every acquire of it, and that the cloud's other offers
/// are asked and launch; and that a step queued for one named cloud's calls is taken by that
/// cloud only. Catches a quota rule the wrapper answers itself, one that refuses every offer
/// of the cloud, and a by-name script that lands on another cloud.
#[test]
fn a_quota_refused_offer_is_refused_and_the_clouds_other_offers_launch() {
    let mut cell = ordered_cell("failover-quota", 0);
    quota_refuse(failover::EC2, failover::EC2_SMALL, failover::ZONE_A).run(&mut cell);
    let lost = [Step::Lost { landed: false }];
    script_provider_by_name(failover::LAMBDA, Call::Acquire, &lost).run(&mut cell);
    cell.submit(&failover::job());
    cell.run_until(LOOK_AT).expect("no violation");

    let calls = cell
        .cloud_named(failover::EC2)
        .expect("the first cloud")
        .calls();
    let acquires: Vec<(String, Option<String>, AcquireAnswer)> = calls
        .into_iter()
        .filter_map(|call| match call.event {
            CloudEvent::Acquire {
                offer,
                zone,
                answer,
                ..
            } => Some((offer, zone, answer)),
            _ => None,
        })
        .collect();
    let refused = |(offer, zone, _): &&(String, Option<String>, AcquireAnswer)| {
        (offer == failover::EC2_SMALL) & (zone.as_deref() == Some(failover::ZONE_A))
    };
    let quota = AcquireAnswer::Refused(RefusalCode::Quota);
    let small_a: Vec<_> = acquires.iter().filter(refused).collect();
    assert!(!small_a.is_empty(), "{acquires:?}");
    assert!(
        small_a.iter().all(|(_, _, answer)| *answer == quota),
        "{acquires:?}"
    );
    let launched = acquires
        .iter()
        .filter(|(_, _, answer)| matches!(answer, AcquireAnswer::Accepted { .. }));
    assert_eq!(launched.count(), 1, "{acquires:?}");
    assert!(cell.total_counters().get(Counter::AcquireRefused) >= 1);
    assert_eq!(Expect::acquires(failover::LAMBDA, 0).unmet(&cell), None);
    let traced = lines(&mut cell);
    assert!(
        traced.contains(&"cloud ec2 quota-refuse ec2_small zone-a applied".to_owned()),
        "{traced:?}"
    );
    assert!(
        traced.contains(&"cloud lambda script acquire lost-not-landed applied".to_owned()),
        "{traced:?}"
    );
}

/// Asserts that each action on a named cloud, on a cell with no cloud of that name, writes
/// its line saying so and changes nothing, and that each expectation about such a cloud got
/// `no-pool`. Catches an action that acts on another cloud when the name is unknown, and an
/// expectation that reads one.
#[test]
fn a_change_to_a_cloud_the_cell_has_not_changes_nothing() {
    let mut cell = plain_cell("failover-none");
    exhaust_provider("nope").run(&mut cell);
    quota_refuse("nope", failover::EC2_SMALL, failover::ZONE_A).run(&mut cell);
    script_provider_by_name("nope", Call::Acquire, &[Step::Pass]).run(&mut cell);
    preempt_machine_of("nope", 0).run(&mut cell);
    assert_eq!(
        lines(&mut cell),
        [
            "cloud nope exhaust none",
            "cloud nope quota-refuse ec2_small zone-a none",
            "cloud nope script acquire pass none",
        ]
    );
    assert!(cell.clouds().is_empty());
    let unmet = [
        Expect::acquires("nope", 0),
        Expect::acquires_at_most("nope", 0),
        Expect::machines_joined("nope", 0),
        Expect::machines_launched("nope", 0),
    ]
    .map(|expect| expect.unmet(&cell));
    assert_eq!(
        unmet,
        [
            Some("acquires(nope): wanted 0, got no-pool".to_owned()),
            Some("acquires-at-most(nope): wanted at most 0, got no-pool".to_owned()),
            Some("machines-joined(nope): wanted 0, got no-pool".to_owned()),
            Some("machines-launched(nope): wanted 0, got no-pool".to_owned()),
        ]
    );
}

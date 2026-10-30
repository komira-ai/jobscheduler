//! The abort: a dry run stops nothing, an apply stops exactly what is wholly the
//! scheduler's, and nothing here dials anything (fakes only).

use std::sync::Arc;

use js_elastic::{AcquireId, HostName, HostStatus, SchedulerId};
use js_provider_lambdalabs::{
    ACQUIRE_TAG, Aborter, ApiError, Architecture, Call, FakeLambda, Instance, InstanceId,
    InstanceTypeOffer, ManualClock, Region, SCHEDULER_TAG, Tag, TerminateEffect,
};
use js_watch::abort::{LambdaTarget, Plan, Target, abort};
use js_watch::fake::{FakeTarget, doomed};

const SCHEDULER: &str = "sched-one";

fn plan() -> Plan {
    Plan {
        doomed: vec![
            doomed("i-1", "region-a", Some(129)),
            doomed("i-2", "region-b", None),
        ],
        left_alone: vec![doomed("i-3", "region-a", Some(75))],
    }
}

/// Asserts that a dry run prints what it would stop, with the hourly cost of each and in
/// total, lists what it leaves alone, and makes exactly one call to the target: the plan.
/// The target records every call, so a stop sent anyway fails the comparison. It catches
/// an abort whose default is to act, which is the one thing the CEO's command must not be.
#[test]
fn a_dry_run_prints_and_stops_nothing() {
    let target = FakeTarget::new(plan(), &[]);
    let outcome = abort(&target, "js", false).unwrap();
    assert_eq!(target.calls(), ["plan"]);
    assert_eq!((outcome.stopped, outcome.failed), (0, 0));
    assert_eq!(
        outcome.lines,
        [
            "abort (dry run): cloud fakecloud, scheduler js",
            "would stop i-1 gpu_1x_fake region-a 1.29 USD/h",
            "would stop i-2 gpu_1x_fake region-b price unknown",
            "left alone (some marks, not all; never touched): i-3 gpu_1x_fake region-a 0.75 USD/h",
            "total: 2 instance(s), 1.29 USD/hour (1 without a price)",
            "dry run: nothing was stopped; run again with --apply to stop them",
        ]
    );
}

/// Asserts that an apply stops each instance of the plan, in order, and none of the ones
/// left alone; that a refused stop is counted and printed and does not end the run (the
/// next instance is still stopped); and that the summary says how many went and how many
/// did not. It catches an apply that stops on the first failure (leaving machines billing)
/// and one that reaches the partially marked instances.
#[test]
fn an_apply_stops_the_plan_only_and_carries_on_past_a_refusal() {
    let target = FakeTarget::new(plan(), &["i-1"]);
    let outcome = abort(&target, "js", true).unwrap();
    assert_eq!(target.calls(), ["plan", "stop i-1", "stop i-2"]);
    assert_eq!((outcome.stopped, outcome.failed), (1, 1));
    assert_eq!(
        outcome.lines,
        [
            "abort (APPLY): cloud fakecloud, scheduler js",
            "stopping i-1 gpu_1x_fake region-a 1.29 USD/h",
            "  FAILED i-1: the fake refuses this one",
            "stopping i-2 gpu_1x_fake region-b price unknown",
            "left alone (some marks, not all; never touched): i-3 gpu_1x_fake region-a 0.75 USD/h",
            "total: 2 instance(s), 1.29 USD/hour (1 without a price)",
            "stopped 1, failed 1; list again to see them gone",
        ]
    );
}

fn tag(key: &str, value: &str) -> Tag {
    Tag {
        key: key.to_owned(),
        value: value.to_owned(),
    }
}

/// An instance as `GET /instances` shows it, launched by `scheduler` for `acquire`.
fn launched(
    scheduler: &str,
    acquire: &str,
    id: &str,
    region: &str,
    status: HostStatus,
) -> Instance {
    let name = HostName::derive(
        &SchedulerId::new(scheduler).unwrap(),
        &AcquireId::new(acquire).unwrap(),
    );
    let tags = vec![tag(SCHEDULER_TAG, scheduler), tag(ACQUIRE_TAG, acquire)];
    instance(id, Some(name.as_str()), region, status, tags)
}

fn instance(
    id: &str,
    name: Option<&str>,
    region: &str,
    status: HostStatus,
    tags: Vec<Tag>,
) -> Instance {
    Instance {
        id: InstanceId::new(id).unwrap(),
        name: name.map(str::to_owned),
        status,
        region: region.to_owned(),
        instance_type: "gpu_1x_sample".to_owned(),
        ip: None,
        private_ip: None,
        hostname: None,
        first_healthy: None,
        tags,
    }
}

/// An account with two of the scheduler's instances in two regions, one already
/// terminating, one terminated, another scheduler's, a half-marked one and a stranger's.
fn account() -> Arc<FakeLambda> {
    let fake = Arc::new(FakeLambda::new());
    fake.set_offers(vec![InstanceTypeOffer {
        name: "gpu_1x_sample".to_owned(),
        description: String::new(),
        gpu_description: String::new(),
        price_cents_per_hour: 75,
        vcpus: 8,
        memory_gib: 64,
        storage_gib: 100,
        gpus: 1,
        architecture: Architecture::X86_64,
        regions: vec![Region {
            name: "region-a".to_owned(),
            description: String::new(),
        }],
    }]);
    let active = HostStatus::Active;
    fake.add_instance(launched(
        SCHEDULER,
        "a",
        "ours-a",
        "region-a",
        active.clone(),
    ));
    fake.add_instance(launched(
        SCHEDULER,
        "b",
        "ours-b",
        "region-z",
        HostStatus::Booting,
    ));
    fake.add_instance(launched(
        SCHEDULER,
        "c",
        "ending",
        "region-a",
        HostStatus::Terminating,
    ));
    fake.add_instance(launched(
        SCHEDULER,
        "d",
        "done",
        "region-a",
        HostStatus::Terminated,
    ));
    fake.add_instance(launched(
        "other-sched",
        "e",
        "theirs",
        "region-a",
        active.clone(),
    ));
    let half = vec![tag(SCHEDULER_TAG, SCHEDULER)];
    fake.add_instance(instance(
        "half",
        Some("renamed"),
        "region-a",
        active.clone(),
        half,
    ));
    fake.add_instance(instance("plain", None, "region-a", active, Vec::new()));
    fake
}

fn target(fake: &Arc<FakeLambda>) -> LambdaTarget {
    LambdaTarget::new(Aborter::new(
        fake.clone(),
        SchedulerId::new(SCHEDULER).unwrap(),
        Arc::new(ManualClock::new()),
    ))
}

fn status_of(fake: &FakeLambda, id: &str) -> HostStatus {
    let wanted = InstanceId::new(id).unwrap();
    fake.instances()
        .into_iter()
        .find(|i| i.id == wanted)
        .unwrap()
        .status
}

/// Asserts the dry run over Lambda's real provider code and its in-memory account: the
/// plan names the scheduler's running instances in every region with the catalog's price,
/// leaves the half-marked one alone, skips the terminating and terminated ones and every
/// other tenant's, and the account has had no terminate (and no launch) call. It catches a
/// dry run that terminates, and a plan that reaches another scheduler's instances.
#[test]
fn the_lambda_dry_run_lists_ours_in_every_region_and_calls_no_terminate() {
    let fake = account();
    let outcome = abort(&target(&fake), SCHEDULER, false).unwrap();
    assert_eq!(fake.count(Call::Terminate), 0);
    assert_eq!(fake.count(Call::Launch), 0);
    assert_eq!(
        outcome.lines,
        [
            format!("abort (dry run): cloud lambda, scheduler {SCHEDULER}"),
            "would stop ours-a gpu_1x_sample region-a 0.75 USD/h".to_owned(),
            "would stop ours-b gpu_1x_sample region-z 0.75 USD/h".to_owned(),
            "left alone (some marks, not all; never touched): half gpu_1x_sample region-a 0.75 USD/h"
                .to_owned(),
            "total: 2 instance(s), 1.50 USD/hour (0 without a price)".to_owned(),
            "dry run: nothing was stopped; run again with --apply to stop them".to_owned(),
        ]
    );
}

/// Asserts the apply over the same account: terminate is called for exactly the two
/// instances of the scheduler that run, each through the provider's own release (which
/// reads the instance back), and every other instance keeps its status. It catches an apply
/// that terminates a neighbour's, a half-marked or an already ending instance, or that
/// reports success for an instance whose status did not change.
#[test]
fn the_lambda_apply_terminates_exactly_the_tagged_running_ones() {
    let fake = account();
    let outcome = abort(&target(&fake), SCHEDULER, true).unwrap();
    assert_eq!((outcome.stopped, outcome.failed), (2, 0));
    assert_eq!(fake.count(Call::Terminate), 2);
    assert_eq!(status_of(&fake, "ours-a"), HostStatus::Terminating);
    assert_eq!(status_of(&fake, "ours-b"), HostStatus::Terminating);
    for untouched in ["theirs", "half", "plain"] {
        assert_eq!(
            status_of(&fake, untouched),
            HostStatus::Active,
            "{untouched}"
        );
    }
    assert_eq!(status_of(&fake, "done"), HostStatus::Terminated);
}

/// Asserts the four answers a stop can get: an instance that is gone or terminating is a
/// stop; one the provider refuses to touch (not wholly ours, or not an id at all) is an
/// error that says so; one that does not change after the terminate is "no answer, run
/// again", never a success. It catches an unknown outcome counted as stopped.
#[test]
fn a_stop_is_a_success_only_when_the_provider_confirms() {
    let fake = account();
    let lambda = target(&fake);
    assert!(lambda.stop("ours-a").is_ok());
    fake.set_terminate_effect(TerminateEffect::Removed);
    assert!(lambda.stop("ours-b").is_ok());
    let refused = lambda.stop("plain").unwrap_err().to_string();
    assert!(refused.starts_with("refused:"), "{refused}");
    assert!(lambda.stop("not an id!").is_err());

    let fake = account();
    fake.set_terminate_effect(TerminateEffect::Nothing);
    let unknown = target(&fake).stop("ours-a").unwrap_err().to_string();
    assert_eq!(unknown, "no answer; run the abort again");
    assert_eq!(status_of(&fake, "ours-a"), HostStatus::Active);
}

/// Asserts that a list that fails ends the abort before anything is stopped, even with
/// `--apply`, and that a catalog that fails only costs the prices. It catches an abort
/// that proceeds on a partial list, and one that gives up on a missing price.
#[test]
fn a_failed_list_stops_the_abort_and_a_failed_catalog_only_loses_prices() {
    let fake = account();
    fake.fail_next(Call::ListInstances, ApiError::Transport("down".to_owned()));
    assert!(abort(&target(&fake), SCHEDULER, true).is_err());
    assert_eq!(fake.count(Call::Terminate), 0);

    fake.fail_next(Call::InstanceTypes, ApiError::Transport("down".to_owned()));
    let outcome = abort(&target(&fake), SCHEDULER, false).unwrap();
    assert!(
        outcome.lines[1].ends_with("region-a price unknown"),
        "{:?}",
        outcome.lines
    );
    assert_eq!(
        outcome.lines[4],
        "total: 2 instance(s), 0.00 USD/hour (2 without a price)"
    );
}

//! Scratch probes of the spend caps.

mod common;

use std::time::Duration;

use common::*;
use js_elastic::Acquired;
use js_scaler::{LaunchMode, Phase};

/// Two launches whose least cost together is over the day cap: the first was told to wait.
#[test]
fn a_deferred_launch_still_counts_against_the_day_cap() {
    let mut limits = settings("gpu");
    limits.max_spend_per_day = 20;
    let mut rig = Rig::new(limits);
    rig.input.waiting = vec![request("r1")];
    let first = acquires(&rig.tick(at(0))).remove(0);
    rig.input.acquired = vec![answered(
        &first,
        Acquired::Deferred {
            wait: Duration::from_secs(600),
        },
    )];
    rig.input.waiting = vec![request("r1"), request("r2")];
    let second = acquires(&rig.tick(at(5 * SECOND)));
    let retry = acquires(&rig.tick(at(5 * SECOND + 600 * SECOND)));
    let mut ids: Vec<_> = second.iter().chain(&retry).map(|p| p.acquire.clone()).collect();
    ids.push(first.acquire.clone());
    ids.sort_by_key(|id| id.as_str().to_owned());
    ids.dedup();
    assert!(ids.len() <= 1, "{} machines acquired against a cap of one", ids.len());
}

/// A pool put in plan mode must not send a retry.
#[test]
fn a_planned_retry_is_not_sent_in_plan_mode() {
    let mut rig = Rig::new(settings("gpu"));
    rig.input.waiting = vec![request("r1")];
    let first = acquires(&rig.tick(at(0))).remove(0);
    rig.input.acquired = vec![answered(
        &first,
        Acquired::Deferred {
            wait: Duration::from_secs(10),
        },
    )];
    rig.tick(at(SECOND));
    assert_eq!(rig.phase(0), Phase::Planned);
    rig.pool().settings.launch = LaunchMode::Plan;
    let actions = rig.tick(at(20 * SECOND));
    assert_eq!(acquires(&actions).len(), 0);
}

/// A machine launched 25h ago and still running is spend inside the last 24h.
#[test]
fn a_long_running_machine_counts_for_its_day_inside_the_window() {
    let mut limits = settings("gpu");
    limits.max_spend_per_day = 2000;
    let mut rig = Rig::new(limits);
    let mut a = record(Phase::Registered, 60, 0);
    let mut b = record(Phase::Registered, 60, 2 * HOUR);
    for each in [&mut a, &mut b] {
        each.registered = true;
        each.offer.cpu_millis = 1000;
    }
    rig.state.records.instances = vec![a, b];
    rig.input.waiting = vec![request("r1")];
    // last 24h: A 24h x 60 = 1440, B 23h x 60 = 1380, total 2820 > 2000.
    assert_eq!(acquires(&rig.tick(at(25 * HOUR))).len(), 0);
}

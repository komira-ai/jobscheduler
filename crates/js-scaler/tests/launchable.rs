//! The answer the scaler gives the registry for a request that finds no host: could a
//! machine be launched for it.

mod common;

use common::*;
use js_elastic::Offer;
use js_scaler::{
    Cooldown, CooldownReason, Health, HostState, LaunchMode, Mark, Phase, Settings, launchable,
};

fn can(rig: &Rig) -> bool {
    launchable(&rig.state, &rig.input, at(0), &demand())
}

/// Asserts the plain cases: a live pool that is available and has an offer that fits
/// could launch; a pool in plan mode, one whose checks are unfinished or failing, one
/// with no offer that fits, and one whose only fitting offer is unpriced could not; and
/// a pool cooling off after a failure still could, so the request waits.
#[test]
fn a_request_is_launchable_when_a_live_available_pool_has_a_fitting_offer() {
    assert!(can(&Rig::new(settings("gpu"))));

    let plan_only = Settings {
        launch: LaunchMode::Plan,
        ..settings("gpu")
    };
    assert!(!can(&Rig::new(plan_only)));

    for health in [Health::Checking, Health::Failing] {
        let mut rig = Rig::new(settings("gpu"));
        rig.pool().health = health;
        assert!(!can(&rig));
    }

    let mut other_os = Rig::new(settings("gpu"));
    other_os.pool().offers = vec![Offer {
        os: "macos".to_owned(),
        ..offer("m", 60)
    }];
    assert!(!can(&other_os));

    let mut unpriced = Rig::new(settings("gpu"));
    unpriced.pool().offers = vec![Offer {
        price_cents_per_hour: None,
        ..offer("free", 0)
    }];
    assert!(!can(&unpriced));

    let mut cooling = Rig::new(settings("gpu"));
    cooling.state.records.cooldowns.insert(
        "gpu".to_owned(),
        Cooldown {
            until: at(HOUR),
            reason: CooldownReason::BootFailed,
        },
    );
    assert!(can(&cooling));
}

/// Asserts that any pool may answer: a plan pool followed by a live one is launchable.
#[test]
fn any_pool_may_answer_yes() {
    let mut rig = Rig::new(Settings {
        launch: LaunchMode::Plan,
        ..settings("first")
    });
    rig.input.pools.push(pool(settings("second")));
    assert!(can(&rig));
}

/// Asserts the caps: a pool at `max_instances` could not launch; if the machine it holds
/// is an idle registered host (or one draining) it could, since that host would be
/// released; a busy host, one set aside, and one the registry cannot hear do not free the
/// slot. The hourly cap frees the same way, and the day cap does not free at all.
#[test]
fn a_full_pool_is_launchable_only_if_an_idle_host_would_free_a_slot() {
    let mut full = settings("gpu");
    full.max_instances = 1;
    let held = record(Phase::Registered, 60, 0);
    let named = held.name.clone();

    let make = |change: fn(&mut js_scaler::HostFacts), phase: Phase| {
        let mut rig = Rig::new(full.clone());
        let mut machine = record(phase, 60, 0);
        machine.registered = true;
        rig.state.records.instances = vec![machine];
        let mut facts = host(&named, "gpu");
        change(&mut facts);
        rig.input.hosts = vec![facts];
        rig
    };
    assert!(can(&make(|_| {}, Phase::Registered)));
    assert!(can(&make(
        |h| h.mark = Some(Mark::Draining),
        Phase::Draining
    )));
    assert!(!can(&make(|h| h.busy = true, Phase::Registered)));
    assert!(!can(&make(
        |h| h.mark = Some(Mark::Held),
        Phase::Registered
    )));
    assert!(!can(&make(
        |h| h.state = HostState::Lost,
        Phase::Registered
    )));
    assert!(!can(&make(
        |h| h.mark = Some(Mark::Releasing),
        Phase::Releasing
    )));

    let mut no_host = Rig::new(full);
    no_host.state.records.instances = vec![held];
    assert!(!can(&no_host));

    let mut hourly = Rig::new(Settings {
        max_spend_per_hour: 120,
        ..settings("gpu")
    });
    let mut costly = record(Phase::Registered, 100, 0);
    costly.registered = true;
    hourly.input.hosts = vec![host(&costly.name, "gpu")];
    hourly.state.records.instances = vec![costly];
    assert!(can(&hourly));
    hourly.input.hosts[0].busy = true;
    assert!(!can(&hourly));

    let day = Rig::new(Settings {
        max_spend_per_day: 14,
        ..settings("gpu")
    });
    assert!(!can(&day));
}

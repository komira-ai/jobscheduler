//! The go of a pool and `live_until`: when a go begins and ends, that the step writes it only
//! when it changes, that a pool past `live_until` launches nothing and drains nothing, and
//! that the go and its expiry hold across a restart. Each test drives the pure `step` on a
//! time it names; none reads a clock.

mod common;

use common::*;
use js_scaler::{Blocked, Go, LaunchMode, Records, Settings, State, launchable};

/// Live settings whose go ends an hour in.
fn until_an_hour() -> Settings {
    Settings {
        live_until: Some(at(HOUR)),
        ..settings("gpu")
    }
}

/// A scaler started from the records file that `rig` has written, given the input `rig` was
/// last given.
fn restart(rig: &Rig, settings: Settings) -> Rig {
    let text = rig.state.records.encode().unwrap();
    let mut fresh = Rig::new(settings);
    fresh.input = rig.input.clone();
    fresh.state = State::new(Records::decode(&text).unwrap());
    fresh
}

/// Asserts that the go begins with the pool's first launch (written with that launch, at
/// its time), that a live pool with nothing to launch has none and writes nothing, that a
/// later launch leaves the go where it was, and that a pool in plan mode never has one. It
/// catches a go written on every tick, a go that moves, and a go for a pool that launched
/// nothing.
#[test]
fn the_first_launch_writes_the_go_and_quiet_steps_write_nothing() {
    let mut rig = Rig::new(settings("gpu"));
    assert!(rig.tick(at(0)).is_empty());
    assert!(rig.state.records.go.is_empty());
    rig.input.waiting = vec![request("r1")];
    assert_eq!(kinds(&rig.tick(at(MINUTE))), ["Save", "Mint", "Acquire"]);
    let go = Go {
        at: at(MINUTE),
        settled: 0,
    };
    assert_eq!(rig.state.records.go["gpu"], go);
    rig.input.waiting = vec![request("r1"), request("r2")];
    rig.tick(at(HOUR));
    assert_eq!(rig.state.records.go["gpu"], go);

    let mut plan = Rig::new(Settings {
        launch: LaunchMode::Plan,
        ..settings("gpu")
    });
    plan.input.waiting = vec![request("r1")];
    assert!(plan.tick(at(MINUTE)).is_empty());
    assert!(plan.state.records.go.is_empty());
}

/// Asserts that a pool switched to plan loses its go in the step that sees it (written), and
/// that its next launch after being switched live again begins a new go at that time, with
/// nothing settled from the first: a human's second go is a new budget. It catches a go that
/// outlives the switch.
#[test]
fn a_pool_switched_to_plan_loses_its_go_and_the_next_go_begins_again() {
    let mut rig = Rig::new(settings("gpu"));
    rig.input.waiting = vec![request("r1")];
    rig.tick(at(0));
    rig.state.records.go.get_mut("gpu").unwrap().settled = 500;
    rig.pool().settings.launch = LaunchMode::Plan;
    assert_eq!(kinds(&rig.tick(at(HOUR))), ["Save"]);
    assert!(rig.state.records.go.is_empty());
    rig.pool().settings.launch = LaunchMode::Live;
    rig.input.waiting = vec![request("r1"), request("r2")];
    rig.tick(at(2 * HOUR));
    let go = Go {
        at: at(2 * HOUR),
        settled: 0,
    };
    assert_eq!(rig.state.records.go["gpu"], go);
}

/// Asserts that a pool past `live_until` launches nothing, to the millisecond: a
/// millisecond short of it a request gets a machine; at it, and long after, the request
/// gets none, the pool shows `Plan` with the launch it would make, and the registry's
/// `Launchable` says no. It catches a go that never ends, and an end one step late.
#[test]
fn a_pool_past_live_until_launches_nothing() {
    for (now, launches, mode) in [
        (HOUR - 1, 1, LaunchMode::Live),
        (HOUR, 0, LaunchMode::Plan),
        (HOUR + DAY, 0, LaunchMode::Plan),
    ] {
        let limits = until_an_hour();
        assert_eq!(limits.mode(at(now)), mode, "{now}");
        let mut rig = Rig::new(limits);
        rig.input.waiting = vec![request("r1")];
        let allowed = launchable(&rig.state, &rig.input, at(now), &demand());
        assert_eq!(allowed, launches == 1, "launchable at {now}");
        let actions = rig.tick(at(now));
        assert_eq!(acquires(&actions).len(), launches, "launches at {now}");
        if launches == 0 {
            assert_eq!(rig.state.blocked["gpu"], Blocked::Plan);
            assert_eq!(rig.state.plan.len(), 1);
            assert!(rig.state.records.go.is_empty());
        }
    }
}

/// Asserts that the end of a go removes launches and nothing more: a registered host that is
/// busy when the go ends is not drained, retired or released by the step that sees the end,
/// which writes only the end of the go (the pool's launch had begun it). It catches an expiry that cuts work.
#[test]
fn the_end_of_a_go_drains_and_releases_nothing() {
    let (mut rig, _) = registered(until_an_hour(), "i-1", 2 * SECOND);
    rig.input.hosts[0].busy = true;
    assert!(rig.state.records.go.contains_key("gpu"));
    let actions = rig.tick(at(2 * HOUR));
    assert_eq!(kinds(&actions), ["Save"]);
    assert!(rig.state.records.go.is_empty());
    assert_eq!(rig.phase(0), js_scaler::Phase::Registered);
    assert!(rig.tick(at(3 * HOUR)).is_empty());
}

/// Asserts, across restarts, that the go is the first step's and not the restart's, and that
/// `live_until` holds: a scaler that comes back before the end finds its go where it was
/// (the time of the first step, not of the restart) and launches for a second request; one
/// that comes back after the end launches nothing and ends the go. It catches a go begun again at every restart (a budget that a crash resets)
/// and an expiry the restart forgets.
#[test]
fn the_go_and_its_end_hold_across_a_restart() {
    let mut rig = Rig::new(until_an_hour());
    rig.input.waiting = vec![request("r1")];
    assert_eq!(acquires(&rig.tick(at(MINUTE))).len(), 1);

    let mut back = restart(&rig, until_an_hour());
    back.input.waiting = vec![request("r1"), request("r2")];
    let launched = acquires(&back.tick(at(HOUR - MINUTE)));
    assert_eq!(launched.len(), 1);
    assert_eq!(back.state.records.go["gpu"].at, at(MINUTE));

    let mut late = restart(&back, until_an_hour());
    late.input.waiting = vec![request("r1"), request("r2"), request("r3")];
    let actions = late.tick(at(HOUR + MINUTE));
    assert!(acquires(&actions).is_empty());
    assert_eq!(late.state.blocked["gpu"], Blocked::Plan);
    assert!(late.state.records.go.is_empty());
}

/// Asserts the file: records with no go write no `go` key (so a reader of the earlier
/// version reads them), and a go is read back as written. It catches a go that is lost in
/// the file, and one written into every file.
#[test]
fn the_go_is_in_the_records_file_only_when_there_is_one() {
    let mut records = Records::default();
    assert!(!records.encode().unwrap().contains("\"go\""));
    records.go.insert(
        "gpu".to_owned(),
        Go {
            at: at(5),
            settled: 7,
        },
    );
    let text = records.encode().unwrap();
    assert!(text.contains("\"go\""));
    assert_eq!(Records::decode(&text).unwrap(), records);
}

/// Asserts the names `GET /v1/pools` shows for the caps, and that no other reason has one:
/// the status already says `unavailable` for the checks, and a pool in plan mode, cooling
/// off or waiting is not "blocked" by a cap.
#[test]
fn only_the_caps_have_a_name_on_the_status() {
    let caps = [
        (Blocked::MaxInstances, "max_instances"),
        (Blocked::MaxSpend, "max_spend"),
        (Blocked::MaxSpendDay, "max_spend_day"),
        (Blocked::MaxSpendTotal, "max_spend_total"),
    ];
    for (blocked, name) in caps {
        assert_eq!(blocked.cap_name(), Some(name));
    }
    for other in [Blocked::Plan, Blocked::BootFailed, Blocked::Unpriced] {
        assert_eq!(other.cap_name(), None);
    }
}

//! While a silent or missing time is pending, the scaler writes its records now and then
//! even when nothing else changed, so a restart loses a bounded part of the time it
//! observed. With nothing pending it writes nothing.

mod common;

use common::*;
use js_scaler::{HostState, Records, State};

/// The most observed time a restart may lose with the default settings: the interval
/// between saves, a sixth of `unknown_hold` (30 minutes) capped at five minutes.
const INTERVAL: u64 = 5 * MINUTE;

/// A scaler started from the records file that `rig` has written, given the input `rig`
/// was last given.
fn restart(rig: &Rig) -> Rig {
    let text = rig.state.records.encode().unwrap();
    let mut fresh = Rig::new(settings("gpu"));
    fresh.input = rig.input.clone();
    fresh.state = State::new(Records::decode(&text).unwrap());
    fresh
}

/// A rig whose registered host went silent, its leases given up, at the first step.
fn silent_since_start() -> Rig {
    let (mut rig, _) = registered(settings("gpu"), "i-1", 2 * SECOND);
    rig.input.hosts[0].state = HostState::Lost;
    rig.input.hosts[0].lease_given_up = true;
    assert_eq!(kinds(&rig.tick(at(3 * SECOND))), ["Save"]);
    rig
}

/// Asserts that with a silent host pending the records are written once every interval of
/// observed time, to the millisecond, and at no step in between.
#[test]
fn a_pending_hold_is_saved_once_every_interval() {
    let mut rig = silent_since_start();
    assert!(rig.tick(at(3 * SECOND + INTERVAL - 1)).is_empty());
    assert_eq!(kinds(&rig.tick(at(3 * SECOND + INTERVAL))), ["Save"]);
    assert_eq!(rig.state.records.last_step, Some(at(3 * SECOND + INTERVAL)));
    for minute in 1..=4 {
        let now = 3 * SECOND + INTERVAL + minute * MINUTE;
        assert!(rig.tick(at(now)).is_empty(), "minute {minute}");
    }
    assert_eq!(kinds(&rig.tick(at(3 * SECOND + 2 * INTERVAL))), ["Save"]);
}

/// Asserts the same for a machine missing from complete views.
#[test]
fn a_pending_missing_hold_is_saved_once_every_interval() {
    let (mut rig, _) = registered(settings("gpu"), "i-1", 2 * SECOND);
    rig.input.hosts = Vec::new();
    assert_eq!(kinds(&rig.tick(at(3 * SECOND))), ["Save"]);
    assert!(rig.tick(at(3 * SECOND + INTERVAL - 1)).is_empty());
    assert_eq!(kinds(&rig.tick(at(3 * SECOND + INTERVAL))), ["Save"]);
}

/// Asserts that a restart after 25 quiet minutes keeps at least 25 minutes less one
/// interval of what the scaler observed.
#[test]
fn a_restart_after_quiet_minutes_keeps_all_but_one_interval() {
    let mut rig = silent_since_start();
    for minute in 1..=25 {
        rig.tick(at(3 * SECOND + minute * MINUTE));
    }
    let mut again = restart(&rig);
    let back = 3 * HOUR;
    again.tick(at(back));
    let name = again.state.silent.keys().next().unwrap().clone();
    let age = at(back).since(again.state.silent[&name]);
    assert!(
        age.as_millis() >= u128::from(25 * MINUTE - INTERVAL),
        "{age:?}"
    );
    assert!(age.as_millis() <= u128::from(25 * MINUTE), "{age:?}");
}

/// Asserts that a scaler restarted every ten minutes, with hours of downtime between the
/// runs, still releases the silent host once it has observed the hold, at most one
/// interval per restart late and never early.
#[test]
fn restarts_every_ten_minutes_still_reach_the_hold() {
    let mut rig = silent_since_start();
    let mut now = 3 * SECOND;
    let mut released = None;
    for run in 1..=8u64 {
        for _ in 1..=10 {
            now += MINUTE;
            if count(&rig.tick(at(now)), "Drain") > 0 {
                released = Some(run);
            }
            if released.is_some() {
                break;
            }
        }
        if released.is_some() {
            break;
        }
        rig = restart(&rig);
        now += HOUR;
        rig.tick(at(now));
    }
    let run = released.expect("the host is released");
    assert!((3..=6).contains(&run), "released in run {run}");
}

/// Asserts that nothing is written on a step that changes nothing when no silent or
/// missing time is pending: a busy live host over an hour of steps.
#[test]
fn nothing_pending_writes_nothing() {
    let (mut rig, _) = registered(settings("gpu"), "i-1", 2 * SECOND);
    rig.input.hosts[0].busy = true;
    for minute in 1..=60 {
        let actions = rig.tick(at(2 * SECOND + minute * MINUTE));
        assert!(actions.is_empty(), "minute {minute}: {:?}", kinds(&actions));
    }
    assert_eq!(rig.state.records.last_step, None);
}

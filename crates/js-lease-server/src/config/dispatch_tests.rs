//! The keys `dispatch_lease_secs` and `dispatch_recheck_ms`: their defaults, what the file sets,
//! and the values it refuses.

use std::time::Duration;

use super::*;
use crate::config::clients_rig::ClientsRig;

/// The `[server]` lines of a file that sets the keys, built into a configuration.
fn dispatch_of(rig: &ClientsRig, lines: &str) -> Result<DispatchTimes, String> {
    let config = rig.config_of_file(&rig.file(lines, ""));
    config
        .map(|config| config.agents.dispatch)
        .map_err(|refused| format!("{refused}"))
}

/// Catches a lease or a recheck that differs from the design's (10 s and 500 ms), a hold that
/// is not the held beat's, and a flag-started server that reads no defaults. A server started
/// on flags, and a file that leaves both keys out, have the defaults; the recheck is below
/// the hold, so a held poll always rechecks.
#[test]
fn the_defaults_are_ten_seconds_five_hundred_milliseconds_and_the_held_beats_hold() {
    let defaults = DispatchTimes::default();
    assert_eq!(defaults.lease, Duration::from_secs(10));
    assert_eq!(defaults.recheck, Duration::from_millis(500));
    assert_eq!(defaults.hold, MAX_HOLD);
    assert!(defaults.recheck < defaults.hold);
    assert!(Duration::from_millis(MOST_RECHECK_MS) < MAX_HOLD);
    let rig = ClientsRig::new("dispatch_defaults");
    let flags = rig.config_of_flags(&[]).unwrap();
    assert_eq!(flags.agents.dispatch, defaults);
    assert_eq!(dispatch_of(&rig, ""), Ok(defaults));
}

/// Catches a key the file sets that does not reach the poll, and one that moves the other.
/// Each key sets its own timing and leaves the other's default; the bounds are accepted.
#[test]
fn the_file_sets_each_timing() {
    let rig = ClientsRig::new("dispatch_file");
    let lease = dispatch_of(&rig, "dispatch_lease_secs = 3").unwrap();
    assert_eq!(lease.lease, Duration::from_secs(3));
    assert_eq!(lease.recheck, Duration::from_millis(DEFAULT_RECHECK_MS));
    let recheck = dispatch_of(&rig, "dispatch_recheck_ms = 40").unwrap();
    assert_eq!(recheck.recheck, Duration::from_millis(40));
    assert_eq!(recheck.lease, Duration::from_secs(DEFAULT_LEASE_SECS));
    let bounds = format!("dispatch_lease_secs = {MOST_LEASE_SECS}\ndispatch_recheck_ms = 1");
    let bounds = dispatch_of(&rig, &bounds).unwrap();
    assert_eq!(bounds.lease, Duration::from_secs(MOST_LEASE_SECS));
    assert_eq!(bounds.recheck, Duration::from_millis(1));
    let widest = format!("dispatch_recheck_ms = {MOST_RECHECK_MS}");
    let widest = dispatch_of(&rig, &widest).unwrap();
    assert_eq!(widest.recheck, Duration::from_millis(MOST_RECHECK_MS));
}

/// Catches a zero lease (a message handed out again at once, so never held), a lease so long a
/// lost reply strands a message, a recheck that is zero (a loop that never waits) or not below
/// the hold (a recheck that never runs), and a text where a number belongs. Each is refused at
/// start, naming the key.
#[test]
fn a_zero_a_too_long_or_a_mistyped_timing_is_refused() {
    let rig = ClientsRig::new("dispatch_refused");
    let cases = [
        (
            "dispatch_lease_secs = 0".to_owned(),
            "server.dispatch_lease_secs: must be from 1 to 3600".to_owned(),
        ),
        (
            "dispatch_lease_secs = 3601".to_owned(),
            "server.dispatch_lease_secs: must be from 1 to 3600".to_owned(),
        ),
        (
            "dispatch_recheck_ms = 0".to_owned(),
            "server.dispatch_recheck_ms: must be from 1 to 4999".to_owned(),
        ),
        (
            "dispatch_recheck_ms = 5000".to_owned(),
            "server.dispatch_recheck_ms: must be from 1 to 4999".to_owned(),
        ),
        (
            "dispatch_recheck_ms = \"fast\"".to_owned(),
            "server.dispatch_recheck_ms: must be an integer".to_owned(),
        ),
    ];
    for (line, said) in cases {
        let refused = dispatch_of(&rig, &line).unwrap_err();
        assert!(refused.contains(&said), "{line}: {refused}");
    }
}

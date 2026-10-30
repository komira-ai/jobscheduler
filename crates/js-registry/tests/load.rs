//! The load a host reports: the conversion to the scheduler's type, and what the registry
//! keeps of it per host. The registry is driven directly on a manual clock.

mod support;

use js_agent_wire::limits::MAX_MEMORY_BYTES;
use js_agent_wire::{LeaseClock, LeaseMinute, LoadDrops, NodeLoad as Wire};
use js_registry::{BadLease, HostState, node_load};
use js_types::sched::usage::NodeLoad;
use support::*;

/// A wire load that sets every field to a value of its own.
fn full() -> Wire {
    Wire {
        seq: 17,
        mono_usec: 61_000_123,
        cpu_usage_usec: 125_000_456,
        working_set_bytes: 7_000_000_789,
        active_file_bytes: 250_001,
        actions_cpu_limited: true,
        covered: vec!["3-7".into(), "0-0".into()],
        clocks: vec![
            LeaseClock {
                lease_id: "3-7".into(),
                started_mono_usec: 500_000,
            },
            LeaseClock {
                lease_id: "3-9".into(),
                started_mono_usec: 61_000_123,
            },
        ],
        minutes: vec![
            LeaseMinute {
                lease_id: "3-7".into(),
                minute: 0,
                cpu_mean_millis: 1500,
                cpu_peak_window_millis: 2000,
                ws_max_bytes: 3_000_000_000,
            },
            LeaseMinute {
                lease_id: "3-7".into(),
                minute: 63,
                cpu_mean_millis: 0,
                cpu_peak_window_millis: 0,
                ws_max_bytes: 0,
            },
        ],
        dropped: LoadDrops::default(),
    }
}

/// The wire form of a scheduler load: the inverse of [`node_load`] on the fields the wire
/// carries.
fn back(load: &NodeLoad) -> Wire {
    let text = |id: &js_types::sched::LeaseId| format!("{}-{}", id.term, id.seq);
    Wire {
        seq: load.seq,
        mono_usec: load.mono_usec,
        cpu_usage_usec: load.cpu_usage_usec,
        working_set_bytes: load.memory_working_set_bytes,
        active_file_bytes: load.memory_active_file_bytes,
        actions_cpu_limited: load.actions_cpu_limited,
        covered: load.covered.iter().map(text).collect(),
        clocks: load
            .lease_clocks
            .iter()
            .map(|c| LeaseClock {
                lease_id: text(&c.lease),
                started_mono_usec: c.started_mono_usec,
            })
            .collect(),
        minutes: load
            .minutes
            .iter()
            .map(|m| LeaseMinute {
                lease_id: text(&m.lease),
                minute: u16::try_from(m.minute).unwrap(),
                cpu_mean_millis: m.cpu_mean_millis,
                cpu_peak_window_millis: m.cpu_peak_window_millis,
                ws_max_bytes: m.ws_max_bytes,
            })
            .collect(),
        dropped: LoadDrops::default(),
    }
}

/// Asserts the conversion of a load with a distinct value in every field, field by field
/// (the working set stays bytes, the active file lands in the active file, the clocks and
/// minutes keep their order and lease), and that converting back gives the wire load again.
/// The two fields only the scheduler's type has read 0. It catches two fields swapped, a
/// unit scaled on the way, and a record dropped or reordered.
#[test]
fn a_wire_load_converts_field_for_field_and_back() {
    let wire = full();
    let load = node_load(&wire).unwrap();
    assert_eq!(
        (load.seq, load.mono_usec, load.cpu_usage_usec),
        (17, 61_000_123, 125_000_456)
    );
    assert_eq!(load.memory_working_set_bytes, 7_000_000_789);
    assert_eq!(load.memory_active_file_bytes, 250_001);
    assert!(load.actions_cpu_limited);
    assert_eq!(
        (load.cpu_throttled_usec, load.cpu_pressure_some_total_usec),
        (0, 0)
    );
    assert_eq!(load.covered.len(), 2);
    assert_eq!((load.covered[1].term, load.covered[1].seq), (0, 0));
    assert_eq!(load.lease_clocks[0].started_mono_usec, 500_000);
    assert_eq!(load.lease_clocks[1].lease.seq, 9);
    assert_eq!(load.minutes[0].cpu_mean_millis, 1500);
    assert_eq!(load.minutes[0].cpu_peak_window_millis, 2000);
    assert_eq!(load.minutes[0].ws_max_bytes, 3_000_000_000);
    assert_eq!(load.minutes[1].minute, 63);
    assert_eq!(back(&load), wire);
}

/// Asserts the round trip at the extremes: every counter at `u64::MAX`, the largest
/// memory the wire allows, the largest lease ids and millicore figures; and a load with
/// no lists at all. It catches a narrowing cast and a saturating conversion that would
/// change a figure silently.
#[test]
fn the_round_trip_holds_at_the_extremes_and_when_empty() {
    let big = format!("{}-{}", u64::MAX, u64::MAX);
    let wire = Wire {
        seq: u64::MAX,
        mono_usec: u64::MAX,
        cpu_usage_usec: u64::MAX,
        working_set_bytes: MAX_MEMORY_BYTES,
        active_file_bytes: MAX_MEMORY_BYTES,
        actions_cpu_limited: false,
        covered: vec![big.clone()],
        clocks: vec![LeaseClock {
            lease_id: big.clone(),
            started_mono_usec: u64::MAX,
        }],
        minutes: vec![LeaseMinute {
            lease_id: big,
            minute: 63,
            cpu_mean_millis: u32::MAX,
            cpu_peak_window_millis: u32::MAX,
            ws_max_bytes: MAX_MEMORY_BYTES,
        }],
        dropped: LoadDrops::default(),
    };
    assert_eq!(back(&node_load(&wire).unwrap()), wire);
    let bare = Wire {
        covered: Vec::new(),
        clocks: Vec::new(),
        minutes: Vec::new(),
        seq: 0,
        mono_usec: 0,
        cpu_usage_usec: 0,
        working_set_bytes: 0,
        active_file_bytes: 0,
        ..wire
    };
    assert_eq!(back(&node_load(&bare).unwrap()), bare);
}

/// Asserts that a lease id that is not `term-seq` is refused by the conversion (in any of
/// the three lists), not rewritten. It catches a lease read as another one.
#[test]
fn a_lease_that_is_not_term_dash_seq_is_refused() {
    for bad in ["01-2", "3.7", "-1", "x", "3-7 ", ""] {
        let mut wire = full();
        wire.covered = vec![bad.into()];
        assert_eq!(node_load(&wire), Err(BadLease), "covered {bad:?}");
        let mut wire = full();
        wire.clocks[0].lease_id = bad.into();
        assert_eq!(node_load(&wire), Err(BadLease), "clock {bad:?}");
        let mut wire = full();
        wire.minutes[1].lease_id = bad.into();
        assert_eq!(node_load(&wire), Err(BadLease), "minute {bad:?}");
    }
}

fn stored(fx: &Fixture, name: &str) -> Option<NodeLoad> {
    let hosts = fx.registry.hosts();
    hosts.into_iter().find(|h| h.id == host(name)).unwrap().load
}

fn beat_with(fx: &Fixture, name: &str, epoch: u64, load: Option<Wire>) {
    let beat = js_agent_wire::Heartbeat {
        load,
        ..fx.heartbeat(name, epoch)
    };
    fx.registry.heartbeat(&beat, &cred("t")).unwrap();
}

fn at(seq: u64) -> Wire {
    Wire { seq, ..full() }
}

/// Asserts what the registry keeps per host: nothing before a load, the latest after one,
/// a repeated or older reading (a delayed heartbeat) changing nothing, a newer one
/// replacing it, a heartbeat with no load clearing it (the agent could not measure, and a
/// stale reading would read as current), and the other host unaffected. It catches a
/// reading kept after the agent stopped sending, an old reading overwriting a new one, and
/// one host's load shown on another.
#[test]
fn the_latest_load_is_kept_per_host_and_cleared_by_a_beat_without_one() {
    let fx = fixture(2);
    let a = fx.join("a", "t");
    let b = fx.join("b", "t");
    assert_eq!(stored(&fx, "a"), None);
    beat_with(&fx, "a", a, Some(at(5)));
    assert_eq!(stored(&fx, "a").unwrap().seq, 5);
    assert_eq!(stored(&fx, "b"), None);
    beat_with(&fx, "a", a, Some(at(5)));
    beat_with(&fx, "a", a, Some(at(4)));
    assert_eq!(stored(&fx, "a").unwrap().seq, 5);
    beat_with(&fx, "a", a, Some(at(6)));
    assert_eq!(stored(&fx, "a").unwrap().seq, 6);
    beat_with(&fx, "b", b, Some(at(1)));
    assert_eq!(stored(&fx, "b").unwrap().seq, 1);
    beat_with(&fx, "a", a, None);
    assert_eq!(stored(&fx, "a"), None);
    assert_eq!(stored(&fx, "b").unwrap().seq, 1);
    beat_with(&fx, "a", a, Some(at(2)));
    assert_eq!(
        stored(&fx, "a"),
        None,
        "2 is below the 6 seen before the clear"
    );
    beat_with(&fx, "a", a, Some(at(7)));
    assert_eq!(stored(&fx, "a").unwrap().seq, 7);
}

/// Asserts that a load that breaks a rule of the wire costs the load and not the beat: a
/// working set above the cap leaves the host with no load while the heartbeat is
/// accepted, and a record naming a bad lease is dropped while the rest is kept. It
/// catches a refused heartbeat for a telemetry fault, and a bad record that reaches the
/// scheduler's type.
#[test]
fn a_bad_load_is_dropped_and_the_heartbeat_is_taken() {
    let fx = fixture(1);
    let a = fx.join("a", "t");
    let mut huge = at(3);
    huge.working_set_bytes = MAX_MEMORY_BYTES + 1;
    beat_with(&fx, "a", a, Some(huge));
    assert_eq!(stored(&fx, "a"), None);
    let mut partial = at(4);
    partial.covered.push("not-a-lease".into());
    partial.minutes[0].lease_id = "01-1".into();
    beat_with(&fx, "a", a, Some(partial));
    let load = stored(&fx, "a").unwrap();
    assert_eq!(load.covered.len(), 2);
    assert_eq!(load.minutes.len(), 1);
    assert_eq!(load.minutes[0].minute, 63);
}

/// Asserts the host-state rules: a new hello clears the load and its sequence restarts (a
/// reading numbered 1 is taken again), and a lost host shows none until it beats again.
/// It catches a reading of a dead session shown as current, and a restarted agent whose
/// first readings are all discarded as old.
#[test]
fn a_hello_clears_the_load_and_a_lost_host_shows_none() {
    let fx = fixture(1);
    let a = fx.join("a", "t");
    beat_with(&fx, "a", a, Some(at(40)));
    fx.pass_grace();
    let view = fx.registry.hosts().remove(0);
    assert_eq!((view.state, view.load), (HostState::Lost, None));
    beat_with(&fx, "a", a, Some(at(41)));
    assert_eq!(stored(&fx, "a").unwrap().seq, 41);
    fx.pass_grace();
    let again = fx.join("a", "t");
    assert_eq!(stored(&fx, "a"), None);
    beat_with(&fx, "a", again, Some(at(1)));
    assert_eq!(stored(&fx, "a").unwrap().seq, 1);
}

/// Asserts that the highest `seq` seen survives a clear: after a beat with no load (or a
/// load that fails vetting) removed the stored one, a delayed or retried older beat does not
/// bring a stale load back, and a newer one is taken. A hello starts the count again. It
/// catches a guard that only holds while a load is stored.
#[test]
fn an_older_beat_does_not_come_back_after_a_clear() {
    let fx = fixture(1);
    let a = fx.join("a", "t");
    beat_with(&fx, "a", a, Some(at(7)));
    beat_with(&fx, "a", a, None);
    assert_eq!(stored(&fx, "a"), None);
    beat_with(&fx, "a", a, Some(at(6)));
    assert_eq!(stored(&fx, "a"), None, "a late beat 6 after the clear");
    beat_with(&fx, "a", a, Some(at(7)));
    assert_eq!(stored(&fx, "a"), None, "a repeat of 7");
    let mut huge = at(9);
    huge.working_set_bytes = MAX_MEMORY_BYTES + 1;
    beat_with(&fx, "a", a, Some(huge));
    beat_with(&fx, "a", a, Some(at(8)));
    assert_eq!(
        stored(&fx, "a"),
        None,
        "8 is below the 9 that failed vetting"
    );
    beat_with(&fx, "a", a, Some(at(10)));
    assert_eq!(stored(&fx, "a").unwrap().seq, 10);
    fx.pass_grace();
    let again = fx.join("a", "t");
    beat_with(&fx, "a", again, Some(at(1)));
    assert_eq!(stored(&fx, "a").unwrap().seq, 1);
}

/// Asserts the two edges of the session rules. After a lost gap a reviving beat that repeats
/// the old `seq` leaves the old reading as the host's load (stale, not wrong: it carries its
/// own `mono_usec`), and a beat of the previous session after a re-hello is refused as
/// stale-epoch and leaves the new session's load as it was. It catches a refused beat that
/// still changes the load, and a revival that clears or replaces the reading by a repeat.
#[test]
fn a_repeat_after_a_gap_and_a_beat_of_an_old_session_leave_the_load() {
    let fx = fixture(1);
    let first = fx.join("a", "t");
    beat_with(&fx, "a", first, Some(at(8)));
    fx.pass_grace();
    assert_eq!(stored(&fx, "a"), None, "hidden while lost");
    beat_with(&fx, "a", first, Some(at(8)));
    assert_eq!(
        stored(&fx, "a").unwrap().seq,
        8,
        "the old reading is shown again"
    );
    fx.pass_grace();
    let second = fx.join("a", "t");
    beat_with(&fx, "a", second, Some(at(3)));
    let old = js_agent_wire::Heartbeat {
        load: Some(at(99)),
        ..fx.heartbeat("a", first)
    };
    let refused = fx.registry.heartbeat(&old, &cred("t"));
    assert!(matches!(refused, Err(js_registry::Refusal::StaleEpoch)));
    assert_eq!(stored(&fx, "a").unwrap().seq, 3);
}

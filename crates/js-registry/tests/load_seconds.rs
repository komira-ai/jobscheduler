//! The per-second lists of a host's load, which the registry keeps beside the scheduler's
//! load for the hosts listing: stored with the reading they came in, replaced and cleared
//! with it, and never part of the scheduler's type.

mod support;

use js_agent_wire::{Heartbeat, NodeLoad as Wire};
use js_registry::{HostView, LoadSeconds};
use support::*;

fn wire(seq: u64, millis: Vec<u32>, pressure: Vec<u16>) -> Wire {
    Wire {
        seq,
        mono_usec: 1_000_000 + seq,
        cpu_usage_usec: 0,
        host_cpu_usec: None,
        host_cpu_pressure_some_usec: None,
        actions_oom_events: None,
        working_set_bytes: 1 << 20,
        active_file_bytes: 0,
        host_nonfile_bytes: None,
        actions_cpu_limited: false,
        covered: Vec::new(),
        clocks: Vec::new(),
        minutes: Vec::new(),
        second_cpu_millis: millis,
        second_cpu_pressure_some_permille: pressure,
        dropped: Default::default(),
    }
}

fn beat_with(fx: &Fixture, epoch: u64, load: Option<Wire>) {
    let beat = Heartbeat {
        load,
        ..fx.heartbeat("a", epoch)
    };
    fx.registry.heartbeat(&beat, &cred("t")).unwrap();
}

fn view(fx: &Fixture) -> HostView {
    fx.registry.hosts().remove(0)
}

fn seconds(millis: &[u32], pressure: &[u16]) -> LoadSeconds {
    LoadSeconds {
        cpu_millis: millis.to_vec(),
        cpu_pressure_some_permille: pressure.to_vec(),
    }
}

/// Asserts what the registry keeps of the lists: nothing before a load, the lists of the
/// latest reading after one, replaced by a newer reading's (including one with none), kept
/// by an older or repeated reading that changes nothing, and cleared by a beat without a
/// load. It catches lists that outlive their reading and lists of an older reading shown
/// beside a newer load.
#[test]
fn the_seconds_follow_the_latest_reading() {
    let fx = fixture(1);
    let a = fx.join("a", "t");
    assert_eq!(view(&fx).load_seconds, LoadSeconds::default());
    beat_with(&fx, a, Some(wire(5, vec![1500, 0], vec![10, 1000])));
    assert_eq!(view(&fx).load_seconds, seconds(&[1500, 0], &[10, 1000]));
    beat_with(&fx, a, Some(wire(4, vec![9], vec![9])));
    assert_eq!(view(&fx).load_seconds, seconds(&[1500, 0], &[10, 1000]));
    beat_with(&fx, a, Some(wire(6, vec![250], vec![])));
    assert_eq!(view(&fx).load_seconds, seconds(&[250], &[]));
    beat_with(&fx, a, Some(wire(7, vec![], vec![])));
    assert_eq!(view(&fx).load_seconds, LoadSeconds::default());
    beat_with(&fx, a, Some(wire(8, vec![3], vec![4])));
    beat_with(&fx, a, None);
    assert_eq!(view(&fx).load, None);
    assert_eq!(view(&fx).load_seconds, LoadSeconds::default());
}

/// Asserts that a pressure list with a value above 1000 is emptied whole while the busy list
/// and the load stay, and that a load that fails vetting (a working set past the cap) takes
/// its lists with it. It catches an impossible share listed, and lists kept for a load that
/// was dropped.
#[test]
fn a_bad_pressure_list_or_a_dropped_load_leaves_no_seconds() {
    let fx = fixture(1);
    let a = fx.join("a", "t");
    beat_with(&fx, a, Some(wire(1, vec![7], vec![5, 1001])));
    assert_eq!(view(&fx).load.unwrap().seq, 1);
    assert_eq!(view(&fx).load_seconds, seconds(&[7], &[]));
    let mut huge = wire(2, vec![8], vec![8]);
    huge.working_set_bytes = js_agent_wire::limits::MAX_MEMORY_BYTES + 1;
    beat_with(&fx, a, Some(huge));
    assert_eq!(view(&fx).load, None);
    assert_eq!(view(&fx).load_seconds, LoadSeconds::default());
}

/// Asserts that a lost host lists no seconds, as it lists no load, and that they are listed
/// again with the old reading when a beat that repeats its `seq` revives the host. It catches
/// seconds of a dead session shown as current.
#[test]
fn a_lost_host_lists_no_seconds() {
    let fx = fixture(1);
    let a = fx.join("a", "t");
    beat_with(&fx, a, Some(wire(3, vec![11], vec![12])));
    fx.pass_grace();
    let lost = view(&fx);
    assert_eq!(lost.load, None);
    assert_eq!(lost.load_seconds, LoadSeconds::default());
    beat_with(&fx, a, Some(wire(3, vec![11], vec![12])));
    assert_eq!(view(&fx).load_seconds, seconds(&[11], &[12]));
}

fn with_oom(seq: u64, oom: Option<u64>) -> Wire {
    Wire {
        actions_oom_events: oom,
        ..wire(seq, Vec::new(), Vec::new())
    }
}

/// Asserts that the actions cgroup's OOM count is listed with the reading it came in: replaced
/// by a newer reading's (including one with none), kept by an older or repeated reading, 0
/// listed as 0, and cleared by a beat without a load and by a load that fails vetting. It
/// catches a count that outlives its reading and one of an older reading beside a newer load.
#[test]
fn the_oom_count_follows_the_latest_reading() {
    let fx = fixture(1);
    let a = fx.join("a", "t");
    assert_eq!(view(&fx).actions_oom_events, None);
    beat_with(&fx, a, Some(with_oom(5, Some(3))));
    assert_eq!(view(&fx).actions_oom_events, Some(3));
    beat_with(&fx, a, Some(with_oom(4, Some(9))));
    assert_eq!(view(&fx).actions_oom_events, Some(3));
    beat_with(&fx, a, Some(with_oom(6, Some(0))));
    assert_eq!(view(&fx).actions_oom_events, Some(0));
    beat_with(&fx, a, Some(with_oom(7, None)));
    assert_eq!(view(&fx).load.unwrap().seq, 7);
    assert_eq!(view(&fx).actions_oom_events, None);
    beat_with(&fx, a, Some(with_oom(8, Some(4))));
    beat_with(&fx, a, None);
    assert_eq!(view(&fx).load, None);
    assert_eq!(view(&fx).actions_oom_events, None);
    let mut huge = with_oom(9, Some(5));
    huge.working_set_bytes = js_agent_wire::limits::MAX_MEMORY_BYTES + 1;
    beat_with(&fx, a, Some(with_oom(9, Some(5))));
    assert_eq!(view(&fx).actions_oom_events, Some(5));
    beat_with(&fx, a, Some(Wire { seq: 10, ..huge }));
    assert_eq!(view(&fx).load, None);
    assert_eq!(view(&fx).actions_oom_events, None);
}

/// Asserts that a lost host lists no OOM count, as it lists no load, and that it is listed
/// again with the old reading when a beat that repeats its `seq` revives the host.
#[test]
fn a_lost_host_lists_no_oom_count() {
    let fx = fixture(1);
    let a = fx.join("a", "t");
    beat_with(&fx, a, Some(with_oom(3, Some(2))));
    fx.pass_grace();
    assert_eq!(view(&fx).actions_oom_events, None);
    beat_with(&fx, a, Some(with_oom(3, Some(2))));
    assert_eq!(view(&fx).actions_oom_events, Some(2));
}

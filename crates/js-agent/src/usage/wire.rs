//! The usage as the heartbeat says it.

use js_agent_wire::{LeaseClock, LeaseMinute, LoadDrops, NodeLoad};

use super::outbox::Reading;

/// The wire form of a reading: the node's counters, the leases that ran the whole window,
/// the start of each running lease and the closed minutes, each field as it is, in the
/// order the reading holds them.
pub fn to_wire(reading: &Reading) -> NodeLoad {
    let sample = &reading.sample;
    NodeLoad {
        seq: reading.seq,
        mono_usec: sample.mono_usec,
        cpu_usage_usec: sample.cpu_usage_usec,
        working_set_bytes: sample.working_set_bytes,
        active_file_bytes: sample.active_file_bytes,
        actions_cpu_limited: sample.cpu_limited,
        covered: reading.covered.clone(),
        clocks: reading
            .clocks
            .iter()
            .map(|(lease_id, started)| LeaseClock {
                lease_id: lease_id.clone(),
                started_mono_usec: *started,
            })
            .collect(),
        minutes: reading
            .minutes
            .iter()
            .map(|minute| LeaseMinute {
                lease_id: minute.lease_id.clone(),
                minute: minute.minute,
                cpu_mean_millis: minute.cpu_mean_millis,
                cpu_peak_window_millis: minute.cpu_peak_window_millis,
                ws_max_bytes: minute.ws_max_bytes,
            })
            .collect(),
        dropped: LoadDrops::default(),
    }
}

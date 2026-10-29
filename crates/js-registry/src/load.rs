//! The load a host reports in its heartbeat, as the scheduler reads it.
//!
//! [`node_load`] turns the wire's [`WireLoad`] into `js_types::sched::usage::NodeLoad`,
//! the type `js-sched` consumes. Every number is copied unchanged, in the units both
//! types state: CPU time in microseconds, memory in bytes, per-lease CPU in thousandths of
//! a core, times in microseconds on the agent's monotonic clock. The only change is the
//! form of a lease: the wire's `term-seq` text becomes a `sched::LeaseId`.
//!
//! Two fields exist only on the scheduler's side, `cpu_throttled_usec` and
//! `cpu_pressure_some_total_usec` (telemetry the wire does not carry yet); they read 0
//! here, and nothing in the conversion invents any other value.

use js_agent_wire::{NodeLoad as WireLoad, lease_parts};
use js_types::sched::LeaseId;
use js_types::sched::usage::{LeaseClock, LeaseMinute, NodeLoad};

/// A lease id of the load is not of the form `term-seq`. A load that
/// [`vet`](WireLoad::vet) passed has none.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the load names a lease that is not of the form term-seq")]
pub struct BadLease;

fn lease(text: &str) -> Result<LeaseId, BadLease> {
    let (term, seq) = lease_parts(text).ok_or(BadLease)?;
    Ok(LeaseId::new(term, seq))
}

/// The wire's load as the scheduler's, field for field.
///
/// # Errors
///
/// [`BadLease`] when a lease id of the load is not `term-seq`.
pub fn node_load(wire: &WireLoad) -> Result<NodeLoad, BadLease> {
    Ok(NodeLoad {
        seq: wire.seq,
        mono_usec: wire.mono_usec,
        cpu_usage_usec: wire.cpu_usage_usec,
        memory_working_set_bytes: wire.working_set_bytes,
        memory_active_file_bytes: wire.active_file_bytes,
        covered: wire
            .covered
            .iter()
            .map(|id| lease(id))
            .collect::<Result<_, _>>()?,
        actions_cpu_limited: wire.actions_cpu_limited,
        cpu_throttled_usec: 0,
        cpu_pressure_some_total_usec: 0,
        lease_clocks: wire
            .clocks
            .iter()
            .map(|clock| {
                Ok(LeaseClock {
                    lease: lease(&clock.lease_id)?,
                    started_mono_usec: clock.started_mono_usec,
                })
            })
            .collect::<Result<_, _>>()?,
        minutes: wire
            .minutes
            .iter()
            .map(|minute| {
                Ok(LeaseMinute {
                    lease: lease(&minute.lease_id)?,
                    minute: u32::from(minute.minute),
                    cpu_mean_millis: minute.cpu_mean_millis,
                    cpu_peak_window_millis: minute.cpu_peak_window_millis,
                    ws_max_bytes: minute.ws_max_bytes,
                })
            })
            .collect::<Result<_, _>>()?,
    })
}

/// What a heartbeat's load leaves of the host's stored one.
///
/// - No load, or one that does not pass [`vet`](WireLoad::vet) or convert: the host has no
///   load now (a stale reading would read as current).
/// - A reading whose `seq` is not above the stored one: the stored one stays (a delayed or
///   repeated heartbeat).
/// - Otherwise the new reading.
pub(crate) fn after_beat(stored: Option<NodeLoad>, wire: Option<&WireLoad>) -> Option<NodeLoad> {
    let wire = wire?;
    if stored.as_ref().is_some_and(|last| wire.seq <= last.seq) {
        return stored;
    }
    let mut vetted = wire.clone();
    vetted.vet().ok()?;
    node_load(&vetted).ok()
}

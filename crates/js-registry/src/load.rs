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
//! here, and 0 there means **not measured**, not "no throttling". Nothing else in the
//! conversion invents a value. The working set is the agent's `memory.current` less
//! `inactive_file`, clamped at 0 when the inactive file is larger.
//!
//! # What is kept
//!
//! Only the latest accepted beat's load is kept for a host. Closed minutes the beat
//! carries that a consumer has not read before the next beat replaces it are lost to
//! that consumer, and a beat the registry took whose reply was lost is sent again by the
//! agent, so a consumer that drains and sums the minutes must dedupe them by
//! `(lease, minute)`. Nothing here says how much the agent dropped: the wire carries no
//! drop counts (the agent's sampler counts gaps, clock steps and aged-out minutes, and a
//! lease it cannot read is left out), so "no minutes" does not tell "nothing was
//! dropped".

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

/// The wire's load as the scheduler's, field for field. The lease server's usage path
/// converts the same load with `NodeLoad::from(LoadIn)`, which sets the active file pages to
/// 0 on purpose; this one keeps them. A test in the lease server pins that this is the only
/// difference.
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

/// What the registry keeps of a host's load: the latest reading, and the highest `seq` it
/// has seen in this session (kept apart from the reading, so a clear does not let an older
/// beat back in). A hello makes a new, empty one.
#[derive(Debug, Default)]
pub(crate) struct Kept {
    pub(crate) latest: Option<NodeLoad>,
    seen: Option<u64>,
}

impl Kept {
    /// Takes in the load of a heartbeat.
    ///
    /// - No load: the host has none now (a stale reading would read as current); the
    ///   high-water `seq` stays.
    /// - A reading whose `seq` is not above the highest seen, cleared or not: nothing
    ///   changes (a delayed or repeated heartbeat).
    /// - Otherwise the `seq` is recorded, and the reading replaces the stored one; one
    ///   that does not pass [`vet`](WireLoad::vet) or convert leaves the host with none.
    pub(crate) fn take(&mut self, wire: Option<&WireLoad>) {
        let Some(wire) = wire else {
            self.latest = None;
            return;
        };
        if self.seen.is_some_and(|seen| wire.seq <= seen) {
            return;
        }
        self.seen = Some(wire.seq);
        let mut vetted = wire.clone();
        self.latest = vetted.vet().ok().and_then(|()| node_load(&vetted).ok());
    }
}

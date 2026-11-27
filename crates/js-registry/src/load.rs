//! The load a host reports in its heartbeat, as the scheduler reads it.
//!
//! [`node_load`] turns the wire's [`WireLoad`] into `js_types::sched::usage::NodeLoad`,
//! the type `js-sched` consumes. Every number is copied unchanged, in the units both
//! types state: CPU time in microseconds, memory in bytes, per-lease CPU in thousandths of
//! a core, times in microseconds on the agent's monotonic clock. The node's CPU counter is
//! the whole host's busy time when the agent sends it (`host_cpu_usec`), else its actions
//! cgroup's ([`WireLoad::node_cpu_usec`]). The only other change is the form of a lease:
//! the wire's `term-seq` text becomes a `sched::LeaseId`.
//!
//! The host's CPU pressure, `host_cpu_pressure_some_usec`, becomes
//! `cpu_pressure_some_total_usec`, and 0 when the agent does not send it. One field exists
//! only on the scheduler's side, `cpu_throttled_usec` (telemetry the wire does not carry
//! yet); it reads 0 here. 0 in either means **not measured**, not "no throttling" or "no
//! waiting". Nothing else in the conversion invents a value. The working set is the whole
//! host's non-file memory when the agent sends it (`host_nonfile_bytes`, with no active file
//! pages: it holds no page cache), else its actions cgroup's `memory.current` less
//! `inactive_file`, clamped at 0 when the inactive file is larger, with the group's active
//! file pages ([`WireLoad::node_memory_bytes`]).
//!
//! The agent's count of OOM events at the actions cgroup's own limit
//! (`actions_oom_events`) is not part of the conversion either: it is kept beside the
//! converted load for the hosts listing, as the lists are.
//!
//! The wire's two per-second lists (`second_cpu_millis`, `second_cpu_pressure_some_permille`)
//! are not part of that conversion: they are kept beside the converted load as
//! [`LoadSeconds`], for the hosts listing, and nothing in placement or sizing reads them.
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
        cpu_usage_usec: wire.node_cpu_usec(),
        memory_working_set_bytes: wire.node_memory_bytes().0,
        memory_active_file_bytes: wire.node_memory_bytes().1,
        covered: wire
            .covered
            .iter()
            .map(|id| lease(id))
            .collect::<Result<_, _>>()?,
        actions_cpu_limited: wire.actions_cpu_limited,
        cpu_throttled_usec: 0,
        cpu_pressure_some_total_usec: wire.host_cpu_pressure_some_usec.unwrap_or(0),
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

/// The per-second lists of a host's latest load, as the wire sent them: busy CPU of the
/// whole host in thousandths of a core and the share of each interval with a task waiting
/// for a CPU in thousandths, oldest first. They sit beside [`NodeLoad`], not in it: they are
/// for the hosts listing and nothing in placement or sizing reads them. Empty when the
/// agent sent none (it was not asked to, or the host has no pressure files).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoadSeconds {
    /// `second_cpu_millis` of the load.
    pub cpu_millis: Vec<u32>,
    /// `second_cpu_pressure_some_permille` of the load, at most 1000 each.
    pub cpu_pressure_some_permille: Vec<u16>,
}

/// What the registry keeps of a host's load: the latest reading, and the highest `seq` it
/// has seen in this session (kept apart from the reading, so a clear does not let an older
/// beat back in). A hello makes a new, empty one.
#[derive(Debug, Default)]
pub(crate) struct Kept {
    pub(crate) latest: Option<NodeLoad>,
    /// The per-second lists of the same reading; empty whenever `latest` is `None`.
    pub(crate) seconds: LoadSeconds,
    /// The actions cgroup's OOM count of the same reading; `None` whenever `latest` is `None`
    /// and when the agent sent none.
    pub(crate) actions_oom_events: Option<u64>,
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
    ///
    /// Answers the reading this heartbeat brought, as stored: `None` for no load, a
    /// repeated one, or one dropped.
    pub(crate) fn take(&mut self, wire: Option<&WireLoad>) -> Option<&NodeLoad> {
        let Some(wire) = wire else {
            self.latest = None;
            self.seconds = LoadSeconds::default();
            self.actions_oom_events = None;
            return None;
        };
        if self.seen.is_some_and(|seen| wire.seq <= seen) {
            return None;
        }
        self.seen = Some(wire.seq);
        let mut vetted = wire.clone();
        let kept = vetted.vet().ok().and_then(|()| {
            let load = node_load(&vetted).ok()?;
            let seconds = LoadSeconds {
                cpu_millis: vetted.second_cpu_millis,
                cpu_pressure_some_permille: vetted.second_cpu_pressure_some_permille,
            };
            Some((load, seconds, vetted.actions_oom_events))
        });
        self.actions_oom_events = kept.as_ref().and_then(|(_, _, oom)| *oom);
        let (latest, seconds) = kept.map(|(load, seconds, _)| (load, seconds)).unzip();
        self.latest = latest;
        self.seconds = seconds.unwrap_or_default();
        self.latest.as_ref()
    }
}

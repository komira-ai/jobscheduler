//! The server's side of the usage hub: what turns a heartbeat into the plain values
//! [`js_usage::UsageHub`] reads, and runs it where the server needs it to run.
//!
//! The hub itself (what is kept between heartbeats, whose minutes, the schedule of flushes
//! and prunes) is `js-usage`'s, and its rules are written there. [`Usage`] is the part that
//! knows the wire:
//!
//! * the heartbeat's load is checked (`NodeLoad::vet`) and mapped to plain values, and the
//!   records that did not map are counted;
//! * the results a heartbeat carried are not handed on: no lease is held, so none is
//!   accepted, and the hub learns nothing from a run;
//! * a panic in the sink is caught and counted as a fault, so it never costs the heartbeat
//!   its reply;
//! * the hub blocks on the sink, so `tick` and `finish` run it on the pool for blocking
//!   calls, and what a flush found is logged here (`node minute`, dropped readings).
//!
//! The registry's lock is not held when these methods run.

use std::num::NonZeroUsize;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use js_agent_wire::{Heartbeat, NodeLoad};
use js_types::Clock;
use js_types::HostId;
pub use js_usage::UsageCounters;
use js_usage::{Beat, BeatLoad, DroppedReadings, Flushed, NoKeys, UsageHub};
use tracing::{error, info, warn};

use crate::clock::SharedClock;
use crate::usage::{
    HostLimits, KeyProfile, NodeRow, ProfileCurves, ProfileKey, UsageFault, UsageSink,
};
use crate::usage_map::{host_of, load_in};
use crate::util::blocking;

/// The usage side of the server: the hub, the sink it writes to, and the clock it is run by.
pub(crate) struct Usage {
    hub: Arc<UsageHub>,
    /// The sink the hub writes to, which the job service's `GetUsage` reads.
    sink: Arc<dyn UsageSink>,
    clock: SharedClock,
}

impl Usage {
    /// The usage side over `sink`, for hosts the pool books as `limits`. The first flush
    /// falls due [`js_usage::FLUSH_EVERY_MINUTES`] after this.
    pub(crate) fn new(sink: Arc<dyn UsageSink>, clock: SharedClock, limits: HostLimits) -> Self {
        let started = clock.now().millis() / 60_000;
        Self {
            hub: Arc::new(UsageHub::new(
                Arc::clone(&sink),
                Arc::new(NoKeys),
                limits,
                started,
            )),
            sink,
            clock,
        }
    }

    /// The sink the hub writes to, for a reader of what it holds.
    pub(crate) fn sink(&self) -> Arc<dyn UsageSink> {
        Arc::clone(&self.sink)
    }

    /// The server's clock in wall minutes since the epoch.
    pub(crate) fn now_minute(&self) -> u64 {
        self.clock.now().millis() / 60_000
    }

    /// The sink's learned curves, for admission.
    pub(crate) fn curves(&self) -> Option<Arc<ProfileCurves>> {
        self.hub.curves()
    }

    /// What has been counted so far, with the sink's own counts.
    pub(crate) fn counters(&self) -> UsageCounters {
        self.hub.counters()
    }

    /// What a heartbeat that the registry accepted tells the usage side. `load` is the
    /// heartbeat's load, cloned from it (the registry keeps its own copy for the hosts
    /// view). Blocks on the sink: call it on the pool for blocking calls. A panic in the
    /// sink is caught and counted as a fault: it must not cost the beat its reply.
    pub(crate) fn on_beat(&self, beat: &Heartbeat, load: Option<NodeLoad>) {
        let ran = catch_unwind(AssertUnwindSafe(|| self.beat(beat, load)));
        if ran.is_err() {
            self.hub.note_fault();
            error!(host = %beat.host_id, "the usage side panicked on a heartbeat; the beat is served");
        }
    }

    fn beat(&self, beat: &Heartbeat, load: Option<NodeLoad>) {
        let Some(host) = host_of(&beat.host_id) else {
            self.hub.note_records_dropped(1);
            return;
        };
        let hub_beat = Beat {
            host: &host,
            epoch: beat.epoch,
            load: beat_load(load),
            results: &[],
        };
        // No lease holds a host: the registry queues no run.
        self.hub.on_beat(hub_beat, self.now_minute(), &|_, _| false);
    }

    /// What every tick of the server does for usage: flush when
    /// [`js_usage::FLUSH_EVERY_MINUTES`] have passed since the last flush, and prune at
    /// the first tick and when the day changes. A failure is counted and logged.
    pub(crate) async fn tick(&self) {
        let (hub, now) = (Arc::clone(&self.hub), self.now_minute());
        let ticked = blocking(move || hub.tick(now)).await;
        if let Some(flushed) = ticked.flush {
            report(flushed);
        }
        if let Some(Err(fault)) = ticked.prune {
            error!(error = %fault, "cannot prune the usage store");
        }
    }

    /// The flush at shutdown, whatever the schedule says.
    pub(crate) async fn finish(&self) {
        let (hub, now) = (Arc::clone(&self.hub), self.now_minute());
        let flushed = blocking(move || hub.finish(now)).await;
        report(flushed);
    }

    /// The rows of `host` from minute `from` to minute `to`, at most `limit`. Blocks.
    pub(crate) fn node_rows(
        &self,
        host: &HostId,
        from: u64,
        to: u64,
        limit: NonZeroUsize,
    ) -> Result<Vec<NodeRow>, UsageFault> {
        self.hub.node_rows(host, from, to, limit)
    }

    /// What is stored for `key`. Blocks.
    pub(crate) fn key_profile(&self, key: &ProfileKey) -> Result<Option<KeyProfile>, UsageFault> {
        self.hub.key_profile(key)
    }
}

/// The load of a beat as the hub reads it: checked, and as plain values. A load with a
/// scalar out of range is rejected whole; one with records left out (by the wire's
/// decoding and checks, or an id that does not parse) is read and the drops counted.
fn beat_load(load: Option<NodeLoad>) -> BeatLoad {
    let Some(mut load) = load else {
        return BeatLoad::Absent;
    };
    if load.vet().is_err() {
        return BeatLoad::Rejected;
    }
    let mapped = load_in(&load);
    BeatLoad::Read {
        load: mapped.load,
        records_dropped: u64::from(load.dropped.total()) + mapped.unparsable,
    }
}

/// Logs what a flush found: one line for each node minute it closed, one warning for each
/// node and minute that had readings dropped, and the failure if the sink had one.
fn report(flushed: Flushed) {
    match &flushed.closed {
        Ok(closed) => closed.iter().for_each(log_minute),
        Err(fault) => error!(error = %fault, "cannot write the usage rows"),
    }
    for DroppedReadings {
        minute,
        host,
        reasons,
    } in &flushed.dropped
    {
        let total: u32 = reasons.values().sum();
        let why: Vec<String> = reasons
            .iter()
            .map(|(reason, count)| format!("{}={count}", crate::usage::drop_name(*reason)))
            .collect();
        warn!(host = %host, minute, dropped = total, reasons = %why.join(","), "node readings dropped");
    }
}

/// The info line of one closed node minute.
fn log_minute((host, row): &(HostId, NodeRow)) {
    info!(
        host = %host,
        minute = row.minute,
        cpu_millis = row.cpu_millis,
        ws_bytes = row.ws_max_bytes,
        leases = row.leases,
        "node minute"
    );
}

#[cfg(test)]
mod tests;

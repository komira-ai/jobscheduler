//! `--placement` and `server.placement`, kept for one release so that a command line or
//! file that names `js-sched` still starts: `js-sched` is the only placement, the value
//! is a no-op that leaves a warning, and `registry` is refused. Also the check of the pool
//! the engine places on, and the placer the registry is given.

use std::sync::Arc;
use std::time::Duration;

use clap::ValueEnum;
use js_registry::PoolConfig;
use js_sched::{Backfill, MemoryPercent};

use super::CpuSlack;
use crate::adapt::SharedClock;
use crate::error::StartupError;
use crate::file::Registering;
use crate::sched::engine::Settings;
use crate::sched::map;
use crate::sched::place::SchedPlacer;

/// The values `--placement` and `server.placement` still parse. Neither chooses anything:
/// `js-sched` places every lease on a registered host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Placement {
    /// Removed: the registry's own first-free placement. Refused at start, naming the
    /// flag or key. Parsed only so that the refusal says why.
    #[value(hide = true)]
    Registry,
    /// The placement engine, the only one: accepted and ignored, with a warning that the
    /// flag or key is deprecated.
    JsSched,
}

impl Placement {
    /// What a `given` value of `place` (`--placement`, or the key `server.placement`)
    /// leaves: nothing when it is absent, the warning that `js-sched` is deprecated, or
    /// the refusal of `registry`. Each names the flag or the key to drop.
    pub(crate) fn given(given: Option<Self>, place: &str) -> Result<Option<String>, StartupError> {
        let drop = if place.starts_with("--") { "flag" } else { "key" };
        match given {
            None => Ok(None),
            Some(Self::JsSched) => Ok(Some(format!(
                "{place} is deprecated and does nothing: js-sched is the only placement (drop the {drop})"
            ))),
            Some(Self::Registry) => Err(StartupError::at(
                place,
                format!(
                    "the registry placement mode was removed; js-sched is the only placement (drop the {drop})"
                ),
            )),
        }
    }
}

/// Refuses a pool whose shape the placement engine cannot take as a node (a shape with no
/// `arch`): no lease could be placed on it. `place` names the pool's flag or file.
pub(crate) fn check_pool(pool: &PoolConfig, place: &str) -> Result<(), StartupError> {
    map::host(pool.shape()).map(|_| ()).map_err(|err| {
        let reason = format!(
            "the pool's shape cannot be taken as a node by the scheduler ({err}); \
             give the pool capabilities with an arch"
        );
        StartupError::at(place, reason)
    })
}

/// `pool` as the registry books it, and the share of each host's memory the placement
/// engine books: the registry books each host's whole memory
/// ([`PoolConfig::booking_whole_memory`]; the percent still decides what fits the pool,
/// and the hosts still report it), and the engine is given the pool's percent. The pool
/// holds a percent from 1 to 100 only (its parse refuses any other), so every percent the
/// engine is given is one js-sched takes.
pub(crate) fn split(pool: PoolConfig) -> (PoolConfig, MemoryPercent) {
    let percent = MemoryPercent::clamped(u32::from(pool.memory_percent().get()));
    (pool.booking_whole_memory(), percent)
}

/// The placer the registry takes: its one engine is built with `cpu_slack`, the share
/// `memory` of each host's memory, and backfill: a lease may backfill a host a whole-node
/// request has set aside, since the lease table ends every lease at its wall time, so
/// every bound is enforced, up to `max_lease_wall` (`server.max_lease_wall`; none: any
/// wall time the table took). `clock` is the registry's.
pub(crate) fn placer(
    max_lease_wall: Option<Duration>,
    cpu_slack: CpuSlack,
    memory: MemoryPercent,
    clock: &SharedClock,
) -> Arc<SchedPlacer> {
    let ceiling = max_lease_wall.unwrap_or(Duration::MAX);
    let settings = Settings {
        backfill: Backfill::enforced(ceiling),
        load: cpu_slack.load_config(),
        memory,
    };
    Arc::new(SchedPlacer::new(settings, clock.clone()))
}

/// `head` (`pool "<name>"`), once the pool it names is one the engine can place on
/// ([`check_pool`]); the refusal names the pool. A joined pool's shape is what every host
/// of it offers, so it must name an `arch`. An elastic pool's shape is the floor its
/// launches meet, which names no capability: it is not checked.
pub(crate) fn checked(head: String, pool: &Registering) -> Result<String, StartupError> {
    match pool.plan {
        Some(_) => Ok(head),
        None => check_pool(&pool.pool, &head).map(|()| head),
    }
}

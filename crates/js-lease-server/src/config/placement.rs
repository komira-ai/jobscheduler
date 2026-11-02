//! `--placement`: who places a lease on a registered host, the check of the pool it needs,
//! and the placer the registry is given for it.

use std::sync::Arc;

use clap::ValueEnum;
use js_registry::PoolConfig;

use crate::error::StartupError;
use crate::sched::map;
use crate::sched::place::SchedPlacer;

/// Who places a lease on a registered host (`--placement`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum Placement {
    /// The registry books the first free host by id; a lease holds a whole host.
    #[default]
    Registry,
    /// `js-sched` picks the host among those with room, and leases that share a host may.
    JsSched,
}

impl Placement {
    /// Refuses `js-sched` for a pool whose shape the scheduler cannot take as a node (a
    /// shape with no `arch`): every start would fall back to the first host, and the flag
    /// would do nothing it says. `place` names the flag or key for the message.
    pub(crate) fn check(self, pool: &PoolConfig, place: &str) -> Result<(), StartupError> {
        match self {
            Self::Registry => Ok(()),
            Self::JsSched => map::host(pool.shape()).map(|_| ()).map_err(|err| {
                let reason = format!(
                    "the pool's shape cannot be taken as a node by the scheduler ({err}); \
                     give the pool capabilities with an arch"
                );
                StartupError::at(place, reason)
            }),
        }
    }

    /// The placer the registry takes for this choice; none for `registry`.
    pub(crate) fn placer(self) -> Option<Arc<dyn js_registry::Placer>> {
        match self {
            Self::Registry => None,
            Self::JsSched => Some(Arc::new(SchedPlacer::default())),
        }
    }
}

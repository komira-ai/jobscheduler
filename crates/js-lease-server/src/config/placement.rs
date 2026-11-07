//! `--placement`: who places a lease on a registered host, the check of the pool it needs,
//! and the placer the registry is given for it.

use std::sync::Arc;
use std::time::Duration;

use clap::ValueEnum;
use js_registry::PoolConfig;
use js_sched::{Backfill, MemoryPercent};

use super::CpuSlack;
use crate::adapt::SharedClock;
use crate::error::StartupError;
use crate::sched::engine::Settings;
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
    /// would do nothing it says. A pool that places each host by what its hello reported
    /// ([`PoolConfig::placing_by_hello`], an elastic pool) is not refused for its shape: its
    /// hosts are nodes by the `arch` they report. Refuses `registry` for a pool with a
    /// `share_class`: the registry places no lease of a class that books part of a host, so
    /// every one would be refused. `place` names the flag or key for the message.
    pub(crate) fn check(self, pool: &PoolConfig, place: &str) -> Result<(), StartupError> {
        match self {
            Self::Registry if pool.share_class().is_some() => Err(StartupError::at(
                place,
                "the pool's share_class needs js-sched: the registry places no lease that shares a host",
            )),
            Self::Registry => Ok(()),
            Self::JsSched if pool.places_by_hello() => Ok(()),
            Self::JsSched => map::host(pool.shape()).map(|_| ()).map_err(|err| {
                let reason = format!(
                    "the pool's shape cannot be taken as a node by the scheduler ({err}); \
                     give the pool capabilities with an arch"
                );
                StartupError::at(place, reason)
            }),
        }
    }

    /// The CPU slack the placement engine is built with: `given`, or the engine's
    /// default. A slack given for any placement but `js-sched` is refused at `place`,
    /// naming `needs`: no other placement reads it, so it would do nothing.
    pub(crate) fn slack(
        self,
        given: Option<CpuSlack>,
        place: &str,
        needs: &str,
    ) -> Result<CpuSlack, StartupError> {
        match (self, given) {
            (Self::JsSched, given) => Ok(given.unwrap_or_default()),
            (Self::Registry, None) => Ok(CpuSlack::default()),
            (Self::Registry, Some(_)) => Err(StartupError::at(place, format!("requires {needs}"))),
        }
    }

    /// `pool` as the registry books it under this choice, and the share of each host's
    /// memory the placement engine books. Under `registry` the registry cuts the shape it
    /// books to the pool's `memory_schedulable_percent`, as it always did, and the engine
    /// is not built. Under `js-sched` the share is the engine's rule: the registry books
    /// each host's whole memory ([`PoolConfig::booking_whole_memory`]; the percent still
    /// decides what fits the pool, and the hosts still report it), and the engine is given
    /// the pool's percent. The pool holds a percent from 1 to 100 only (its parse refuses
    /// any other), so every percent the engine is given is one js-sched takes.
    pub(crate) fn split(self, pool: PoolConfig) -> (PoolConfig, MemoryPercent) {
        match self {
            Self::Registry => (pool, MemoryPercent::FULL),
            Self::JsSched => {
                let percent = MemoryPercent::clamped(u32::from(pool.memory_percent().get()));
                (pool.booking_whole_memory(), percent)
            }
        }
    }

    /// The placer the registry takes for this choice; none for `registry`. Under
    /// `js-sched` the placer's one engine is built with `cpu_slack`, the share `memory` of
    /// each host's memory, and backfill: a lease may backfill a host a whole-node request
    /// has set aside, since the lease table ends every lease at its wall time, so every
    /// bound is enforced, up to `max_lease_wall` (`server.max_lease_wall`; none: any wall
    /// time the table took). `clock` is the registry's.
    pub(crate) fn placer(
        self,
        max_lease_wall: Option<Duration>,
        cpu_slack: CpuSlack,
        memory: MemoryPercent,
        clock: &SharedClock,
    ) -> Option<Arc<SchedPlacer>> {
        match self {
            Self::Registry => None,
            Self::JsSched => {
                let ceiling = max_lease_wall.unwrap_or(Duration::MAX);
                let settings = Settings {
                    backfill: Backfill::enforced(ceiling),
                    load: cpu_slack.load_config(),
                    memory,
                };
                Some(Arc::new(SchedPlacer::new(settings, clock.clone())))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use js_types::{Admission, ClassId, ProviderId, ResourceClass, Resources, Shape};

    use super::*;

    /// A pool of `capabilities`, of a class that books whole hosts.
    fn pool(capabilities: &[(&str, &str)]) -> PoolConfig {
        let caps: BTreeMap<String, String> = capabilities
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        let shape = Shape::new(Resources::new(4000, 8 << 30, 0, None).unwrap(), caps);
        let id = ProviderId::new("burst").unwrap();
        PoolConfig::new(id, ResourceClass::gpu_host(), shape, 2)
    }

    /// Asserts each answer of the check, by the whole message where it refuses: under
    /// `js-sched` a pool whose shape names an `arch` is taken, one that names none is
    /// refused naming `arch`, and the same arch-less shape is taken when the pool places
    /// each host by its hello (an elastic pool, whose hosts report their `arch`); under
    /// `registry` a pool is taken unless it has a `share_class`. Catches an elastic pool
    /// refused for the floor its hosts are not placed by, and an agent or joined pool with
    /// no `arch` let through.
    #[test]
    fn each_placement_takes_or_refuses_the_pools_it_should() {
        let place = "server.placement";
        let named = pool(&[("arch", "x86_64")]);
        assert!(Placement::JsSched.check(&named, place).is_ok());
        let bare = pool(&[]);
        let refused = Placement::JsSched.check(&bare, place).err().unwrap();
        assert_eq!(
            refused.to_string(),
            "server.placement: the pool's shape cannot be taken as a node by the scheduler \
             (shape does not describe a node: the node report has no \"arch\" entry); \
             give the pool capabilities with an arch"
        );
        let elastic = pool(&[]).placing_by_hello();
        assert!(Placement::JsSched.check(&elastic, place).is_ok());
        assert!(Placement::Registry.check(&elastic, place).is_ok());
        let share = ResourceClass::new(
            ClassId::new("share").unwrap(),
            Admission::Book(Default::default()),
            0,
        );
        let sharing = pool(&[("arch", "x86_64")]).with_share_class(share).unwrap();
        let refused = Placement::Registry.check(&sharing, place).err().unwrap();
        assert_eq!(
            refused.to_string(),
            "server.placement: the pool's share_class needs js-sched: the registry places no \
             lease that shares a host"
        );
    }
}

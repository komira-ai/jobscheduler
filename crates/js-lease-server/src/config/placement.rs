//! `--placement` and `server.placement`, kept for one release so that a command line or
//! file that names `js-sched` still starts: `js-sched` is the only placement, the value
//! is a no-op that leaves a warning, and `registry` is refused. Also the check of the pool
//! the engine places on, and the placer the registry is given.

use std::sync::Arc;
use std::time::Duration;

use clap::ValueEnum;
use js_registry::PoolConfig;
use js_sched::{Backfill, MemoryPercent};
use js_sched_placer::sizes::{ColdMemoryOn, ColdSize, Curves};

use super::{CpuSlack, PackLimit};
use crate::clock::SharedClock;
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
        let drop = if place.starts_with("--") {
            "flag"
        } else {
            "key"
        };
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
/// `arch`): no lease could be placed on it. A pool that places each host by what its hello
/// reported ([`PoolConfig::placing_by_hello`], an elastic pool) is not refused for its
/// shape: its hosts are nodes by the `arch` they report. `place` names the pool's flag or
/// file.
pub(crate) fn check_pool(pool: &PoolConfig, place: &str) -> Result<(), StartupError> {
    if pool.places_by_hello() {
        return Ok(());
    }
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

/// The operator's two load settings of the placement engine: how far above a host's cores it
/// may be charged, and how full a host is packed before the least full one is chosen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LoadSetting {
    /// `--cpu-slack`.
    pub(crate) cpu_slack: CpuSlack,
    /// `--pack-limit`.
    pub(crate) pack_limit: PackLimit,
}

impl LoadSetting {
    /// js-sched's load settings with both values and every other setting at its default.
    fn load_config(self) -> js_sched::LoadConfig {
        self.pack_limit.apply(self.cpu_slack.load_config())
    }
}

/// The placer the registry takes: its one engine is built with `load` (the CPU slack and
/// the pack limit), the share
/// `memory` of each host's memory, the `cold` size of a job class with no estimate (and
/// `cold_on`, the cold memory of the jobs of each operating system the server names), the
/// `curves` mode (`--placement-curves`), and backfill: work may backfill a host a
/// whole-node request has set aside, up to `max_lease_wall` (`server.max_lease_wall`;
/// none: any wall time). `clock` is the registry's.
pub(crate) fn placer(
    max_lease_wall: Option<Duration>,
    load: LoadSetting,
    (memory, cold, curves): (MemoryPercent, ColdSize, Curves),
    cold_on: ColdMemoryOn,
    clock: &SharedClock,
) -> Arc<SchedPlacer> {
    let settings = settings_of(max_lease_wall, load, (memory, cold, curves));
    Arc::new(SchedPlacer::new(settings, clock.clone()).with_cold_memory_on(cold_on))
}

/// The engine's settings from the server's configuration, as [`placer`] takes them.
fn settings_of(
    max_lease_wall: Option<Duration>,
    load: LoadSetting,
    (memory, cold, curves): (MemoryPercent, ColdSize, Curves),
) -> Settings {
    let ceiling = max_lease_wall.unwrap_or(Duration::MAX);
    Settings {
        backfill: Backfill::enforced(ceiling),
        load: load.load_config(),
        memory,
        cold,
        curves,
    }
}

/// `head` (`pool "<name>"`), once the pool it names is one the engine can place on
/// ([`check_pool`]); the refusal names the pool.
pub(crate) fn checked(head: String, pool: &Registering) -> Result<String, StartupError> {
    check_pool(&pool.pool, &head).map(|()| head)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use js_types::{ProviderId, ResourceClass, Resources, Shape};

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

    /// Asserts each answer of the check, by the whole message where it refuses: a pool
    /// whose shape names an `arch` is taken, one that names none is refused naming `arch`,
    /// and the same arch-less shape is taken when the pool places each host by its hello
    /// (an elastic pool, whose hosts report their `arch`). Catches an elastic pool refused
    /// for the floor its hosts are not placed by, and an agent or joined pool with no
    /// `arch` let through.
    #[test]
    fn the_check_takes_or_refuses_the_pools_it_should() {
        let place = "--agent-pool-config";
        assert!(check_pool(&pool(&[("arch", "x86_64")]), place).is_ok());
        let refused = check_pool(&pool(&[]), place).err().unwrap();
        assert_eq!(
            refused.to_string(),
            "--agent-pool-config: the pool's shape cannot be taken as a node by the scheduler \
             (shape does not describe a node: the node report has no \"arch\" entry); \
             give the pool capabilities with an arch"
        );
        assert!(check_pool(&pool(&[]).placing_by_hello(), place).is_ok());
    }

    /// Asserts that the settings the engine is built with carry the pack limit and the CPU
    /// slack given, each beside the other's value and js-sched's defaults for the rest.
    /// Catches a limit dropped on the way to the engine, one swapped for the default, and
    /// the slack lost when the limit is set.
    #[test]
    fn the_pack_limit_and_slack_reach_the_engine_settings() {
        let (memory, cold) = (MemoryPercent::clamped(100), ColdSize::default());
        let load = LoadSetting {
            cpu_slack: CpuSlack::parse("1.5").unwrap(),
            pack_limit: PackLimit::parse("0.65").unwrap(),
        };
        let made = settings_of(None, load, (memory, cold, Curves::Off));
        assert_eq!(made.load.pack_limit_permille, 650);
        assert_eq!(made.load.s_cpu_permille, 500);
        let default = js_sched::LoadConfig::default();
        assert_eq!(made.load.s_mem_permille, default.s_mem_permille);
        let plain = settings_of(None, LoadSetting::default(), (memory, cold, Curves::Off));
        assert_eq!(plain.load, default);
    }

    /// Asserts that the settings the engine is built with carry the `curves` mode given,
    /// `shadow` and `off` each, with the other settings as given. Catches a mode dropped
    /// on the way to the engine and one swapped for the default.
    #[test]
    fn the_curves_mode_reaches_the_engine_settings() {
        let slack = LoadSetting::default();
        let memory = MemoryPercent::clamped(100);
        let cold = ColdSize::default();
        for curves in [Curves::Shadow, Curves::Off] {
            let made = settings_of(None, slack, (memory, cold, curves));
            assert_eq!(made.curves, curves);
            assert_eq!(made.cold, cold);
            assert_eq!(made.memory, memory);
        }
    }

    /// Asserts the three answers of a given value, each message in full: none leaves
    /// nothing, `js-sched` leaves the warning naming the flag or key to drop, and
    /// `registry` is refused naming the flag or key. Catches a warning or refusal that
    /// names the wrong thing to drop.
    #[test]
    fn a_given_value_leaves_nothing_a_warning_or_a_refusal() {
        assert_eq!(Placement::given(None, "--placement").unwrap(), None);
        assert_eq!(
            Placement::given(Some(Placement::JsSched), "server.placement").unwrap(),
            Some(
                "server.placement is deprecated and does nothing: js-sched is the only \
                 placement (drop the key)"
                    .to_owned()
            )
        );
        let refused = Placement::given(Some(Placement::Registry), "--placement");
        assert_eq!(
            refused.err().unwrap().to_string(),
            "--placement: the registry placement mode was removed; js-sched is the only \
             placement (drop the flag)"
        );
    }
}

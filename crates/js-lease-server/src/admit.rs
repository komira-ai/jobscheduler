//! Admission of a submitted job (design 5.6, step 2): the views of the pool that
//! [`js_sched::admission_ever::can_ever_serve`] decides over, built from the registry's hosts
//! and the pool's launching machine providers, and the one decision a job submit takes from
//! it before anything is recorded.
//!
//! The hosts are read from the registry (drain, cordon and silence; a taint is not read:
//! a tainted host is capable) and from the `node/` rows of the job store ([`crate::node_rows`]),
//! so a submit to a server that did not take a host's hello still sees the host. A launching
//! provider's catalog is read from the server's cache ([`Catalogs::view`]), which the tick
//! fills: the read calls no provider and answers at once. A catalog the cache has never
//! fetched is [`CatalogView::Unread`], which counts as "may offer".

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use js_agent_wire::{
    DISPATCH_CAPABILITY, HELD_BEAT_CAPABILITY, HOST_JOBS_CAPABILITY, LOAD_ADMISSION_CAPABILITY,
};
use js_caps::{FLAG_SET, LAYERED_ROOTFS, XCODE, is_mount_provider_key};
use js_elastic::CapacityProvider;
use js_jobs::{Isolation, ScaleMark};
use js_registry::{HostMark, HostState as RegistryState, HostView as RegistryView, Registry};
use js_sched::admission_ever::{
    Admission, CatalogView, HostState, HostView, PoolLimits, ProviderView, can_ever_serve,
};
use js_types::{HostId, Requirements, Shape};
use tracing::warn;

use crate::catalogs::{Cached, Catalogs};
use crate::job_store::ServerJobs;
use crate::node_rows::{self, Seen};
use crate::util::blocking;

/// A machine provider that launches for the pool, with what the pool lets it launch.
#[derive(Clone)]
pub(crate) struct Launching {
    /// The provider.
    pub(crate) provider: Arc<dyn CapacityProvider>,
    /// How many machines it may hold for the pool: its `provider_limits` `max_machines`, else
    /// the pool's.
    pub(crate) share: u32,
    /// The pool's `provider_limits` on it.
    pub(crate) limits: PoolLimits,
}

/// The pool's side of admission: its launching machine providers and the cache of their
/// catalogs. The hosts are read at each decision.
#[derive(Clone, Default)]
pub(crate) struct Admit {
    launching: Vec<Launching>,
    /// The cache of the launching providers' catalogs; `None` with none.
    cached: Option<Cached>,
}

impl Admit {
    /// Admission over `launching`, the pool's launching machine providers, whose catalogs
    /// are read from `cached`.
    pub(crate) fn new(launching: Vec<Launching>, cached: Cached) -> Self {
        Self {
            launching,
            cached: Some(cached),
        }
    }

    /// The views of the launching providers, each catalog as the cache holds it now.
    fn providers(&self) -> Vec<ProviderView> {
        self.cached
            .iter()
            .flat_map(|cached| {
                let now = cached.now();
                self.launching
                    .iter()
                    .map(move |one| provider_view(one, cached.catalogs(), now))
            })
            .collect()
    }
}

/// Admission of a submitted job (design section 5.6, step 2, `SubmitJob` step 3): the hosts
/// that register (a job runs on no other host), and the pool's launching machine providers.
/// A job with host isolation is offered only the registered hosts whose hello reported
/// `host-jobs`, and no provider: a machine not launched yet has reported nothing.
///
/// The hosts are the registry's (its lock is taken and let go at once) and every host whose
/// `node/` row the job store holds, a host in both counted once as the registry sees it. The
/// rows are read on the pool for blocking calls. When they cannot be read the job is admitted
/// and the failure logged: a refusal is never taken on less than the rows, and a job admitted
/// that no host can take waits in the queue until its limits end it, as one for a host that has
/// not said hello yet does. The launching providers' catalogs are read from the
/// server's cache, as a lease's are ([`Admit::decide`]): the read calls no provider and answers
/// at once. A catalog the cache has never fetched may offer.
pub(crate) struct JobAdmission {
    admit: Admit,
    registry: Registry,
    jobs: Arc<ServerJobs>,
}

impl JobAdmission {
    /// Admission over the hosts of `registry` and the `node/` rows of `jobs`, and the
    /// launching providers of `admit`.
    pub(crate) fn new(admit: Admit, registry: Registry, jobs: Arc<ServerJobs>) -> Self {
        Self {
            admit,
            registry,
            jobs,
        }
    }
}

#[tonic::async_trait]
impl crate::job_api::submit::Admission for JobAdmission {
    async fn can_ever_serve(&self, wanted: &Requirements, isolation: Isolation) -> bool {
        let on_the_machine = isolation == Isolation::Host;
        let (registry, jobs) = (self.registry.clone(), Arc::clone(&self.jobs));
        let rows = blocking(move || node_rows::read(&registry, &jobs)).await;
        let rows = match rows {
            Ok(rows) => rows,
            Err(error) => {
                warn!(%error, "the hosts' rows could not be read: the job is admitted unjudged");
                return true;
            }
        };
        let held = self.registry.hosts();
        let held_here: BTreeSet<&HostId> = held.iter().map(|host| &host.id).collect();
        let by_row = rows
            .iter()
            .filter(|seen| !held_here.contains(seen.host()))
            .filter(|seen| !on_the_machine | seen.reports_host_jobs())
            .map(row_host);
        let hosts: Vec<HostView> = held
            .iter()
            .filter(|host| !on_the_machine | host.claim.as_ref().is_some_and(reports))
            .map(registered)
            .chain(by_row)
            .collect();
        let providers = match on_the_machine {
            true => Vec::new(),
            false => self.admit.providers(),
        };
        can_ever_serve(wanted, &hosts, &providers) != Admission::Refuse
    }
}

/// Whether a host's hello claim reports that its agent runs host jobs.
fn reports(claim: &Shape) -> bool {
    reports_host_jobs(claim.capabilities())
}

/// Whether `capabilities`, those a host's hello reported, say its agent runs host jobs
/// ([`HOST_JOBS_CAPABILITY`] is exactly `"true"`).
pub(crate) fn reports_host_jobs(capabilities: &BTreeMap<String, String>) -> bool {
    capabilities.get(HOST_JOBS_CAPABILITY).map(String::as_str) == Some("true")
}

/// Whether `capabilities`, those a host's hello reported, say its agent beats again at once
/// after a held reply ([`HELD_BEAT_CAPABILITY`] is exactly [`FLAG_SET`]), so its heartbeat's
/// reply may be held open ([`crate::held_beat`]).
pub(crate) fn reports_held_beat(capabilities: &BTreeMap<String, String>) -> bool {
    capabilities.get(HELD_BEAT_CAPABILITY).map(String::as_str) == Some(FLAG_SET)
}

/// Whether `capabilities`, those a host's hello reported, say its agent admits a job start by
/// the job service's load placement for CPU and memory ([`LOAD_ADMISSION_CAPABILITY`] is
/// exactly [`FLAG_SET`]), so the host is packed by measured load, not by summed bookings.
pub(crate) fn reports_load_admission(capabilities: &BTreeMap<String, String>) -> bool {
    capabilities
        .get(LOAD_ADMISSION_CAPABILITY)
        .map(String::as_str)
        == Some(FLAG_SET)
}

/// Whether `capabilities`, those a host's hello reported, say its agent polls for its starts
/// and stops ([`DISPATCH_CAPABILITY`] is exactly [`FLAG_SET`]), so a server that serves the
/// poll leaves out of its heartbeat replies what the dispatch queue carries.
pub(crate) fn reports_dispatch_poll(capabilities: &BTreeMap<String, String>) -> bool {
    capabilities.get(DISPATCH_CAPABILITY).map(String::as_str) == Some(FLAG_SET)
}

/// Whether `capabilities`, those a host's hello reported, say its agent gives a job layers,
/// mounts and host ports ([`LAYERED_ROOTFS`] is exactly [`FLAG_SET`]).
pub(crate) fn reports_layered_rootfs(capabilities: &BTreeMap<String, String>) -> bool {
    capabilities.get(LAYERED_ROOTFS).map(String::as_str) == Some(FLAG_SET)
}

/// The keys `mount-provider.<name>` of `capabilities`, those a host's hello reported, that
/// hold exactly [`FLAG_SET`], in key order: the node-local providers its agent was given.
pub(crate) fn reported_mount_providers(capabilities: &BTreeMap<String, String>) -> Vec<String> {
    capabilities
        .iter()
        .filter(|(key, value)| is_mount_provider_key(key) & (value.as_str() == FLAG_SET))
        .map(|(key, _)| key.clone())
        .collect()
}

/// The builds of the Xcodes `capabilities`, those a host's hello reported, list under
/// [`XCODE`]: the value split on `,`, empty pieces skipped, in the order given.
pub(crate) fn reported_xcodes(capabilities: &BTreeMap<String, String>) -> Vec<String> {
    capabilities
        .get(XCODE)
        .map(|builds| builds.split(','))
        .into_iter()
        .flatten()
        .filter(|build| !build.is_empty())
        .map(str::to_owned)
        .collect()
}

/// `one`'s provider as admission sees it at `now`: enabled (no key turns a provider off),
/// with its share and limits, its capabilities, and its catalog as `catalogs` holds it. A
/// provider that launches nothing (`static`) has no catalog, and none is read.
pub(crate) fn provider_view(
    one: &Launching,
    catalogs: &Catalogs,
    now: js_elastic::Instant,
) -> ProviderView {
    let provider = one.provider.as_ref();
    let launches = provider.can_launch();
    let catalog = match launches {
        true => catalogs.view(&provider.id(), now),
        false => CatalogView::Offers(Vec::new()),
    };
    ProviderView {
        kind_is_static: !launches,
        enabled: true,
        share: one.share,
        capabilities: provider.capabilities(),
        catalog,
        limits: one.limits.clone(),
    }
}

/// A host of the registry as admission sees it, on the shape the registry places it on
/// ([`RegistryView::placement_shape`]: the pool's booking with the host's validated
/// accelerator facts). A taint, reported or announced, does not show: a tainted host is
/// capable, as its taint ends (the agent clears it, or an operator does) and placement alone
/// skips it meanwhile. It is draining when it is being released ([`HostMark::Releasing`]:
/// it is leaving the pool, and nothing ends the mark); lost when it is not live (silent
/// past its grace, or not heard since a restart). A drain mark ([`HostMark::Draining`]) is
/// not leaving: [`Registry::undrain`] ends it, as the scaler does for a request that needs
/// the host. Nor is the phase an agent announces when it is told to stop (`draining`): the
/// agent comes back, as after a restart. Such a host stays capable.
pub(crate) fn registered(host: &RegistryView) -> HostView {
    let placed = host.placement_shape();
    HostView {
        id: host.id.clone(),
        state: state_of(
            host.state != RegistryState::Live,
            host.cordon_until.is_some(),
            !host.leases.is_empty(),
        ),
        draining: host.mark == Some(HostMark::Releasing),
        total: placed.resources().clone(),
        facts: placed.capabilities().clone(),
    }
}

/// A host known only by its row as admission sees it: lost when it was last heard longer ago
/// than the registry's longest grace, serving otherwise; draining when its row carries the mark
/// of a release ([`ScaleMark::Releasing`], which nothing ends); on the shape the registry would
/// place it on. A draining mark ([`ScaleMark::Draining`]) and a taint are not leaving, as for a
/// host the registry holds.
fn row_host(seen: &Seen) -> HostView {
    HostView {
        id: seen.host().clone(),
        state: state_of(seen.lost, false, false),
        draining: seen.mark == Some(ScaleMark::Releasing),
        total: seen.placed.resources().clone(),
        facts: seen.placed.capabilities().clone(),
    }
}

/// The state of a host that is `lost`, `cordoned` and `busy`, in that order of precedence.
fn state_of(lost: bool, cordoned: bool, busy: bool) -> HostState {
    match (lost, cordoned, busy) {
        (true, _, _) => HostState::Lost,
        (false, true, _) => HostState::Cordoned,
        (false, false, true) => HostState::Busy,
        (false, false, false) => HostState::Serving,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts the precedence of a host's state: silence first, then a cordon, then work.
    #[test]
    fn a_host_state_is_lost_then_cordoned_then_busy() {
        assert_eq!(state_of(true, true, true), HostState::Lost);
        assert_eq!(state_of(false, true, true), HostState::Cordoned);
        assert_eq!(state_of(false, false, true), HostState::Busy);
        assert_eq!(state_of(false, false, false), HostState::Serving);
    }

    /// Asserts that a hello reports load admission only with the capability `load-admission`
    /// set to exactly `true`. Catches a host that reports nothing, or something else, packed
    /// by measured load though its agent refuses by the summed bookings.
    #[test]
    fn load_admission_is_reported_only_by_the_value_true() {
        let said = |value: Option<&str>| {
            let caps: BTreeMap<String, String> = value
                .map(|value| (LOAD_ADMISSION_CAPABILITY.to_owned(), value.to_owned()))
                .into_iter()
                .collect();
            reports_load_admission(&caps)
        };
        assert!(said(Some("true")));
        for other in [None, Some("false"), Some("True"), Some("true ")] {
            assert!(!said(other), "{other:?}");
        }
    }

    /// Asserts that a hello reports host jobs only with the capability `host-jobs` set to
    /// exactly `true`: absent, `false`, another case or a padded value do not. Catches a
    /// host that reports nothing, or something else, offered a host job.
    #[test]
    fn host_jobs_are_reported_only_by_the_value_true() {
        let said = |value: Option<&str>| {
            let caps: BTreeMap<String, String> = value
                .map(|value| (HOST_JOBS_CAPABILITY.to_owned(), value.to_owned()))
                .into_iter()
                .collect();
            reports_host_jobs(&caps)
        };
        assert!(said(Some("true")));
        for other in [None, Some("false"), Some("True"), Some("true ")] {
            assert!(!said(other), "{other:?}");
        }
    }

    /// Catches an agent that never said it polls being left without its starts in the
    /// heartbeat's reply. A hello reports the dispatch poll only with the capability
    /// `dispatch-poll` set to exactly `true`: absent, `false`, another case or a padded value
    /// do not, and the `held-beat` capability does not stand for it.
    #[test]
    fn the_dispatch_poll_is_reported_only_by_the_value_true() {
        let said = |value: Option<&str>| {
            let caps: BTreeMap<String, String> = value
                .map(|value| (DISPATCH_CAPABILITY.to_owned(), value.to_owned()))
                .into_iter()
                .collect();
            reports_dispatch_poll(&caps)
        };
        assert!(said(Some("true")));
        for other in [None, Some("false"), Some("TRUE"), Some("true ")] {
            assert!(!said(other), "{other:?}");
        }
        let held = BTreeMap::from([(HELD_BEAT_CAPABILITY.to_owned(), "true".to_owned())]);
        assert!(!reports_dispatch_poll(&held));
    }

    /// Asserts that a hello reports layered rootfs only with the capability `layered-rootfs`
    /// set to exactly `true`: absent, `false`, another case, `1` or a padded value do not, and
    /// the `host-jobs` capability does not stand for it. Catches a host that reports nothing,
    /// or something else, sent a job with layers, mounts or host ports.
    #[test]
    fn layered_rootfs_is_reported_only_by_the_value_true() {
        let said = |value: Option<&str>| {
            let caps: BTreeMap<String, String> = value
                .map(|value| (LAYERED_ROOTFS.to_owned(), value.to_owned()))
                .into_iter()
                .collect();
            reports_layered_rootfs(&caps)
        };
        assert!(said(Some("true")));
        for other in [None, Some("false"), Some("TRUE"), Some("1"), Some("true ")] {
            assert!(!said(other), "{other:?}");
        }
        let host_jobs = BTreeMap::from([(HOST_JOBS_CAPABILITY.to_owned(), "true".to_owned())]);
        assert!(!reports_layered_rootfs(&host_jobs));
    }

    /// Asserts that a hello reports a mount provider only by a `mount-provider.<name>` key
    /// whose value is exactly `true`, the keys come back in key order, and every other
    /// capability (the key with no name, `layered-rootfs`, a near-miss prefix) is not one.
    /// Catches a host sent a job for a provider it did not report, and one that reports a
    /// provider as `false` taken for having it.
    #[test]
    fn mount_providers_are_reported_only_by_keys_of_the_value_true() {
        let caps: BTreeMap<String, String> = [
            ("mount-provider.tierstore", "true"),
            ("mount-provider.cache", "true"),
            ("mount-provider.off", "false"),
            ("mount-provider.padded", "true "),
            ("mount-provider.", "true"),
            ("mount-providers.other", "true"),
            ("layered-rootfs", "true"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
        assert_eq!(
            reported_mount_providers(&caps),
            ["mount-provider.cache", "mount-provider.tierstore"]
        );
        assert!(reported_mount_providers(&BTreeMap::new()).is_empty());
    }

    /// Asserts that a hello's `xcode` capability is read as its builds split on `,` in the order
    /// given, with empty pieces skipped, one build staying one, and that a hello without it (or
    /// with an empty value) reports none. Catches a joined value read as one build, which no job
    /// names.
    #[test]
    fn xcodes_are_reported_as_the_builds_of_the_joined_value() {
        let with = |value: &str| BTreeMap::from([("xcode".to_owned(), value.to_owned())]);
        assert_eq!(
            reported_xcodes(&with("16C5032a,16E140")),
            ["16C5032a", "16E140"]
        );
        assert_eq!(reported_xcodes(&with(",16E140,,")), ["16E140"]);
        assert_eq!(reported_xcodes(&with("16E140")), ["16E140"]);
        assert!(reported_xcodes(&with("")).is_empty());
        assert!(reported_xcodes(&BTreeMap::new()).is_empty());
    }
}

#[cfg(test)]
#[path = "admit_rows_tests.rs"]
mod rows_tests;

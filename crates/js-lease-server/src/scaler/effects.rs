//! What the scaler's shell reads and does to the world, behind one trait, so the shell's
//! rules (what it writes first, what it does when a call fails, what it keeps for the next
//! tick) are tested with a fake that records every call and the real thing is tested
//! through a server.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js_elastic::{
    AcquireId, Acquired, Demand, EventStream, Held, HostRef, Offer, ProviderError, Released, Secret,
};
use js_jobs::{AllocationRecord, JobId, JobRecord, JobStoreError};
use js_registry::{Busy, HostView};
use js_scaler::{AcquirePlan, Health, Millis, Records};
use js_sched::queue_status::Outcome;
use js_types::ProviderId;

/// What the scaler reads of the job tables at a tick: the demand and the hosts in use.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobTables {
    /// Every queued job, in queue order. A job read here that is also among `live` (it was
    /// placed between the two reads) is taken as placed.
    pub queued: Vec<JobRecord>,
    /// Every live (booked or releasing) allocation, a whole-host one included, read after
    /// `queued`.
    pub live: Vec<AllocationRecord>,
}

/// A call on the registry that names one host, other than the removal of a host. None of
/// them returns anything the scaler acts on: it reads the registry's marks back at its next
/// step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Call {
    /// `Registry::drain`.
    Drain,
    /// `Registry::retire`.
    Retire,
    /// `Registry::undrain`.
    Undrain,
    /// `Registry::release_host`.
    ReleaseHost,
    /// `Registry::mark_ending`.
    MarkEnding,
    /// `Registry::cordon`, for the server's default cordon length: the host of a machine
    /// the provider reclaimed takes no new work.
    Cordon,
}

/// The shell's whole interface to the registry, the join tokens, the provider, the network
/// and the disk. Every method blocks and none holds a lock another part of the server
/// waits on across a provider call.
pub trait Effects: Send + Sync {
    /// The hosts the registry holds, as of one moment (one lock of the registry). Read
    /// beside the job tables ([`Effects::jobs`]): demand is the queued jobs, and a host is
    /// busy with a live allocation.
    fn hosts(&self) -> Vec<HostView>;

    /// The queued jobs and then the live allocations of the job tables; both empty for a
    /// server that holds no job tables.
    ///
    /// # Errors
    ///
    /// The [`JobStoreError`] of a read that failed: the tables are never read as empty.
    fn jobs(&self) -> Result<JobTables, JobStoreError>;

    /// What a queued job that books the whole host asks a machine to bring, besides its own
    /// constraints: the least every host of the pool brings (its floor).
    fn whole_host(&self) -> Demand;

    /// Whether the host `host` holds work its machine must not be released under, read at
    /// the moment of the release: a live job allocation. A read of the job tables that fails counts as work.
    fn holds_work(&self, host: &str) -> bool;

    /// What the pool's checks say, and the offers they read: the scaler launches only while
    /// the checks pass, and only from these offers.
    fn checks(&self) -> (Health, Vec<Offer>);

    /// The machines the pool's providers list as ours, every provider's.
    ///
    /// # Errors
    ///
    /// [`ProviderError`] when the list cannot be had whole; it is never read as empty.
    fn list(&self) -> Result<Vec<Held>, ProviderError>;

    /// The provider that listed or launched each machine of a provider after the pool's
    /// first, by the provider's id for the machine ([`js_scaler::PoolInput::listed_by`]), as
    /// of the last [`Effects::list`]. None by default: the pool has one provider.
    fn listed_by(&self) -> BTreeMap<String, ProviderId> {
        BTreeMap::new()
    }

    /// The acquire ids the pool's providers hold an unresolved call for.
    fn unresolved(&self) -> Vec<AcquireId>;

    /// The offers each of the pool's providers after the first may launch at `now`, by
    /// provider ([`js_scaler::PoolInput::later_offers`]); the first's are those of
    /// [`Effects::checks`]. None by default: the pool has one provider.
    fn later_offers(&self, _now: Millis) -> BTreeMap<ProviderId, Vec<Offer>> {
        BTreeMap::new()
    }

    /// How many fetches of the pool's providers' catalogs have completed in this process
    /// ([`crate::Catalogs::fetches`]). A count that moved since the last tick tells the
    /// scaler a catalog refresh completed, which ends the `acquire_retry` wait of a request
    /// every offer was refused for. Zero by default: no catalog is fetched.
    fn catalog_fetches(&self) -> u64 {
        0
    }

    /// Hands what a step reached for queued jobs (a refused acquire made for a job's request,
    /// a machine launched for one) to the placement pass, which records it on each job's
    /// queue status (`docs/design/queue-status.md`, section 2). Called once a tick, after the
    /// step. Nothing by default: a server that places no jobs records none.
    fn pass_two(&self, outcomes: Vec<(JobId, Outcome)>) {
        drop(outcomes);
    }

    /// Whether a read of the machine `id` by id says the provider no longer has it.
    fn is_gone(&self, id: &HostRef) -> bool;

    /// Writes `records`, all of them, so that they survive a crash.
    ///
    /// # Errors
    ///
    /// Why the file could not be written.
    fn save(&self, records: &Records) -> Result<(), String>;

    /// Mints the one-time join token of the host `host`, valid for `valid_for`; `None`
    /// when it could not be made.
    fn mint(&self, host: &str, valid_for: Duration) -> Option<Secret>;

    /// Launches the machine `plan` describes with the join token `token`, through the
    /// provider it names.
    fn acquire(&self, plan: &AcquirePlan, token: Secret) -> Acquired;

    /// Terminates the machine `id` and reads it back.
    fn release(&self, id: &HostRef) -> Released;

    /// Revokes every join token and credential of the host `host`; whether it did. A
    /// host with none is revoked.
    fn revoke(&self, host: &str) -> bool;

    /// Writes the hosts the registry refused to remove, all of them, so that they survive
    /// a restart.
    ///
    /// # Errors
    ///
    /// Why the file could not be written.
    fn save_unremoved(&self, hosts: &BTreeSet<String>) -> Result<(), String>;

    /// Takes the host `host` out of the registry, for a machine that is confirmed gone: the
    /// host is marked ending first, since nothing is left to protect, and then removed.
    ///
    /// # Errors
    ///
    /// Why the registry refused ([`Busy`]); the host is asked again at the next tick.
    fn remove(&self, host: &str) -> Result<(), Busy>;

    /// Forgets the host's network device, if its network mode has one. Best effort.
    fn forget(&self, host: &str);

    /// Makes `call` on the registry for `host`.
    fn call(&self, call: Call, host: &str);

    /// The provider's notices ([`js_elastic::CapacityProvider::events`]): `None` (the
    /// default) for a provider that sends none.
    fn events(&self) -> Option<EventStream> {
        None
    }
}

//! What the scaler's shell reads and does to the world, behind one trait, so the shell's
//! rules (what it writes first, what it does when a call fails, what it keeps for the next
//! tick) are tested with a fake that records every call and the real thing is tested
//! through a server.

use std::time::Duration;

use js_elastic::{AcquireId, Acquired, Held, HostRef, Offer, ProviderError, Released, Secret};
use js_registry::{HostView, Waiting};
use js_scaler::{AcquirePlan, Health, Records};

/// A call on the registry that names one host, other than the removal of a host. None of
/// them returns anything the scaler acts on: it reads the registry's marks back at its next
/// step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Call {
    /// `Registry::drain`.
    Drain,
    /// `Registry::undrain`.
    Undrain,
    /// `Registry::release_host`.
    ReleaseHost,
    /// `Registry::mark_ending`.
    MarkEnding,
}

/// The shell's whole interface to the registry, the join tokens, the provider, the network
/// and the disk. Every method blocks and none holds a lock another part of the server
/// waits on across a provider call.
pub trait Effects: Send + Sync {
    /// The hosts the registry holds and the whole-node requests that found no host, as of
    /// one moment (one lock of the registry).
    fn registry(&self) -> (Vec<HostView>, Vec<Waiting>);

    /// Whether the registry books a lease on the host `host`.
    fn leased(&self, host: &str) -> bool;

    /// What the pool's checks say, and the offers they read: the scaler launches only while
    /// the checks pass, and only from these offers.
    fn checks(&self) -> (Health, Vec<Offer>);

    /// The machines the provider lists as ours.
    ///
    /// # Errors
    ///
    /// [`ProviderError`] when the list cannot be had whole; it is never read as empty.
    fn list(&self) -> Result<Vec<Held>, ProviderError>;

    /// The acquire ids the provider holds an unresolved call for.
    fn unresolved(&self) -> Vec<AcquireId>;

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

    /// Launches the machine `plan` describes with the join token `token`.
    fn acquire(&self, plan: &AcquirePlan, token: Secret) -> Acquired;

    /// Terminates the machine `id` and reads it back.
    fn release(&self, id: &HostRef) -> Released;

    /// Revokes every join token and credential of the host `host`; whether it did. A
    /// host with none is revoked.
    fn revoke(&self, host: &str) -> bool;

    /// Takes the host `host` out of the registry, for a machine that is confirmed gone: the
    /// host is marked ending first, since nothing is left to protect, and then removed.
    ///
    /// # Errors
    ///
    /// Why the registry refused: a lease is booked on the host. It is asked again at the
    /// next tick.
    fn remove(&self, host: &str) -> Result<(), String>;

    /// Forgets the host's network device, if its network mode has one. Best effort.
    fn forget(&self, host: &str);

    /// Makes `call` on the registry for `host`.
    fn call(&self, call: Call, host: &str);
}

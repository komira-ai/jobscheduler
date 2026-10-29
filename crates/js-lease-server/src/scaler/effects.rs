//! What the scaler's shell reads and does to the world, behind one trait, so the shell's
//! rules (what it writes first, what it does when a call fails, what it keeps for the next
//! tick) are tested with a fake that records every call and the real thing is tested
//! through a server.

use std::time::Duration;

use js_elastic::{Acquired, Held, HostRef, Offer, ProviderError, Released, Secret};
use js_registry::{HostView, Waiting};
use js_scaler::{AcquirePlan, Records};

/// A call on the registry that names one host. None of them returns anything the scaler
/// acts on: it reads the registry's marks back at its next step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Call {
    /// `Registry::drain`.
    Drain,
    /// `Registry::undrain`.
    Undrain,
    /// `Registry::release_host`.
    ReleaseHost,
    /// `Registry::remove`.
    Remove,
    /// `Registry::mark_ending`.
    MarkEnding,
}

/// The shell's whole interface to the registry, the join tokens, the provider, the network
/// and the disk. Every method blocks and none holds a lock another part of the server
/// waits on across a provider call.
pub trait Effects: Send + Sync {
    /// The whole-node requests that found no host.
    fn waiting(&self) -> Vec<Waiting>;

    /// The hosts the registry holds.
    fn hosts(&self) -> Vec<HostView>;

    /// What the provider could launch now.
    ///
    /// # Errors
    ///
    /// [`ProviderError`] when the catalog cannot be had whole.
    fn catalog(&self) -> Result<Vec<Offer>, ProviderError>;

    /// The machines the provider lists as ours.
    ///
    /// # Errors
    ///
    /// [`ProviderError`] when the list cannot be had whole; it is never read as empty.
    fn list(&self) -> Result<Vec<Held>, ProviderError>;

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

    /// Forgets the host's network device, if its network mode has one. Best effort.
    fn forget(&self, host: &str);

    /// Makes `call` on the registry for `host`.
    fn call(&self, call: Call, host: &str);
}

//! How the provider is allowed to spend. Every default is the safe one.

use std::num::NonZeroUsize;
use std::time::Duration;

use crate::own::SchedulerId;

/// Whether launches are sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A launch is built and logged and not sent.
    DryRun,
    /// A launch is sent, if the provider is also enabled.
    Live,
}

/// Why a configuration was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// The ssh key name is empty, longer than 64 characters or not printable ASCII.
    #[error("the ssh key name must be 1 to 64 printable ASCII characters")]
    SshKey,
}

/// The settings of a provider. Launches happen only when it is [enabled](Self::enable)
/// and [live](Self::live); it starts disabled and in dry-run mode.
#[derive(Debug, Clone)]
pub struct ElasticConfig {
    pub(crate) scheduler: SchedulerId,
    pub(crate) ssh_key: String,
    pub(crate) max_instances: NonZeroUsize,
    pub(crate) enabled: bool,
    pub(crate) mode: Mode,
    pub(crate) launch_interval: Duration,
    pub(crate) unknown_hold: Duration,
}

impl ElasticConfig {
    /// A configuration for the scheduler `scheduler`, which puts the account's ssh key
    /// `ssh_key` on what it launches and never holds more than `max_instances`
    /// instances, counting the ones it cannot place and the launches it has no answer
    /// for. There is no default cap.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] when `ssh_key` is not a key name.
    pub fn new(
        scheduler: SchedulerId,
        ssh_key: &str,
        max_instances: NonZeroUsize,
    ) -> Result<Self, ConfigError> {
        let named =
            (1..=64).contains(&ssh_key.len()) && ssh_key.bytes().all(|b| b.is_ascii_graphic());
        match named {
            false => Err(ConfigError::SshKey),
            true => Ok(Self {
                scheduler,
                ssh_key: ssh_key.to_owned(),
                max_instances,
                enabled: false,
                mode: Mode::DryRun,
                // The API allows one launch per 12 seconds.
                launch_interval: Duration::from_secs(12),
                unknown_hold: Duration::from_secs(600),
            }),
        }
    }

    /// Enables the provider. Without this every acquire is refused.
    #[must_use]
    pub fn enable(mut self) -> Self {
        self.enabled = true;
        self
    }

    /// Sends launches. Without this a launch is built and logged and refused.
    #[must_use]
    pub fn live(mut self) -> Self {
        self.mode = Mode::Live;
        self
    }

    /// Sets the least time between two launches (12 seconds by default, the API's
    /// limit).
    #[must_use]
    pub fn launch_interval(mut self, interval: Duration) -> Self {
        self.launch_interval = interval;
        self
    }

    /// Sets how long a launch that got no answer counts toward the cap while no list
    /// shows it (ten minutes by default). After that a list is trusted to show it.
    #[must_use]
    pub fn unknown_hold(mut self, hold: Duration) -> Self {
        self.unknown_hold = hold;
        self
    }
}

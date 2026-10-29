//! How the provider is allowed to spend. Every default is the safe one.

use std::num::NonZeroUsize;
use std::time::Duration;

use js_elastic::SchedulerId;

/// The scratch file system's size when the configuration gives none, in GiB. Lambda's
/// smaller GPU instances have a 512 GiB disk: 200 GiB for job scratch leaves the rest for
/// the system, the container images and the agent's state.
pub const DEFAULT_SCRATCH_GIB: u32 = 200;

/// The smallest scratch file system, in GiB.
pub const MIN_SCRATCH_GIB: u32 = 10;

/// The largest scratch file system, in GiB.
pub const MAX_SCRATCH_GIB: u32 = 4096;

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
    /// The scratch size is outside [`MIN_SCRATCH_GIB`] to [`MAX_SCRATCH_GIB`].
    #[error("the scratch size must be {MIN_SCRATCH_GIB} to {MAX_SCRATCH_GIB} GiB, not {gib}")]
    ScratchSize {
        /// The size asked for.
        gib: u32,
    },
    /// A region is not a region code: 1 to 64 lower case letters, digits or `-`.
    #[error("a region is 1 to 64 lower case letters, digits or '-'")]
    Region,
}

/// The settings of a provider. Launches happen only when it is [enabled](Self::enable)
/// and [live](Self::live); it starts disabled and in dry-run mode.
#[derive(Debug, Clone)]
pub struct ElasticConfig {
    pub(crate) scheduler: SchedulerId,
    pub(crate) ssh_key: Option<String>,
    pub(crate) scratch_gib: u32,
    pub(crate) regions: Option<Vec<String>>,
    pub(crate) max_machines: NonZeroUsize,
    pub(crate) enabled: bool,
    pub(crate) mode: Mode,
    pub(crate) launch_interval: Duration,
    pub(crate) unknown_hold: Duration,
}

impl ElasticConfig {
    /// A configuration for the scheduler `scheduler`, which puts the account's ssh key
    /// `ssh_key` on what it launches and never holds more than `max_machines`
    /// machines, counting the ones it cannot place and the launches it has no answer
    /// for. There is no default.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] when `ssh_key` is not a key name.
    pub fn new(
        scheduler: SchedulerId,
        ssh_key: &str,
        max_machines: NonZeroUsize,
    ) -> Result<Self, ConfigError> {
        let named =
            (1..=64).contains(&ssh_key.len()) && ssh_key.bytes().all(|b| b.is_ascii_graphic());
        match named {
            false => Err(ConfigError::SshKey),
            true => Ok(Self::with(
                scheduler,
                Some(ssh_key.to_owned()),
                max_machines,
            )),
        }
    }

    /// A configuration as [`Self::new`] with no ssh key named. Lambda takes exactly one
    /// key name at launch and the key is never used to log in (the firewall lets nothing
    /// in), so the provider settles on one itself: the account's key named
    /// `js-<scheduler>` if it is there, else the account's only key, else (an account
    /// with no key) a key it has the API make under that name and keeps neither half of.
    /// An account with several keys and none of that name is refused until one is named
    /// with [`Self::new`]. Making a key writes to the account, and happens at the first
    /// live launch, never in a dry run.
    pub fn with_account_key(scheduler: SchedulerId, max_machines: NonZeroUsize) -> Self {
        Self::with(scheduler, None, max_machines)
    }

    fn with(scheduler: SchedulerId, ssh_key: Option<String>, max_machines: NonZeroUsize) -> Self {
        Self {
            scheduler,
            ssh_key,
            scratch_gib: DEFAULT_SCRATCH_GIB,
            regions: None,
            max_machines,
            enabled: false,
            mode: Mode::DryRun,
            // The API allows one launch per 12 seconds.
            launch_interval: Duration::from_secs(12),
            unknown_hold: Duration::from_secs(600),
        }
    }

    /// Sets the size of the scratch file system the host is prepared with, in GiB
    /// ([`DEFAULT_SCRATCH_GIB`] by default).
    ///
    /// # Errors
    ///
    /// [`ConfigError::ScratchSize`] when `gib` is outside [`MIN_SCRATCH_GIB`] to
    /// [`MAX_SCRATCH_GIB`].
    pub fn scratch_gib(mut self, gib: u32) -> Result<Self, ConfigError> {
        match (MIN_SCRATCH_GIB..=MAX_SCRATCH_GIB).contains(&gib) {
            false => Err(ConfigError::ScratchSize { gib }),
            true => {
                self.scratch_gib = gib;
                Ok(self)
            }
        }
    }

    /// Limits the regions offered and launched into to these (every region by default).
    ///
    /// # Errors
    ///
    /// [`ConfigError::Region`] when `regions` is empty or one is not a region code.
    pub fn regions(mut self, regions: &[&str]) -> Result<Self, ConfigError> {
        let code = |r: &&str| {
            (1..=64).contains(&r.len())
                && r.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        };
        match !regions.is_empty() && regions.iter().all(code) {
            false => Err(ConfigError::Region),
            true => {
                self.regions = Some(regions.iter().map(|r| (*r).to_owned()).collect());
                Ok(self)
            }
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

    /// Sets how long a launch that got no answer counts toward `max_machines` while no list
    /// shows it (ten minutes by default). After that a list is trusted to show it.
    #[must_use]
    pub fn unknown_hold(mut self, hold: Duration) -> Self {
        self.unknown_hold = hold;
        self
    }
}

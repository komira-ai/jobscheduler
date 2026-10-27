use std::time::Duration;

use js_types::{HostId, Labels, LeaseId};

use crate::clock::Timestamp;
use crate::error::{SaveError, SettingsError};
use crate::state::LeaseState;

/// How long a lease lives when the client does not renew it.
///
/// A ttl the client asks for is clamped to `max_ttl`; a client that names none gets
/// `default_ttl`. A zero ttl is refused. Neither bound replaces the request's maximum
/// wall time, which bounds the whole life of a lease whatever the ttl (as the wall clock
/// reads it; see the crate docs on a clock that steps).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    default_ttl: Duration,
    max_ttl: Duration,
}

impl Settings {
    /// Settings with these bounds. `default_ttl` must be positive and at most
    /// `max_ttl`, which makes `max_ttl` positive as well.
    pub fn new(default_ttl: Duration, max_ttl: Duration) -> Result<Self, SettingsError> {
        if default_ttl.is_zero() {
            return Err(SettingsError::DefaultNotPositive);
        }
        if default_ttl > max_ttl {
            return Err(SettingsError::DefaultAboveMax);
        }
        Ok(Self {
            default_ttl,
            max_ttl,
        })
    }

    /// The ttl a client that names none gets.
    pub fn default_ttl(&self) -> Duration {
        self.default_ttl
    }

    /// The longest ttl granted at one time.
    pub fn max_ttl(&self) -> Duration {
        self.max_ttl
    }

    /// The ttl to grant for what the client asked: the default for `None`, the ask
    /// clamped to the maximum otherwise, and nothing for a zero ask.
    pub(crate) fn grant(&self, asked: Option<Duration>) -> Option<Duration> {
        let ttl = asked.unwrap_or(self.default_ttl);
        (!ttl.is_zero()).then(|| ttl.min(self.max_ttl))
    }
}

/// A lease the provider confirmed, as the client gets it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    /// The lease id.
    pub id: LeaseId,
    /// The host it holds.
    pub host: HostId,
    /// When it lapses unless renewed.
    pub expires_at: Timestamp,
    /// The labels written onto every billable resource of the lease.
    pub labels: Labels,
}

/// One lease of the table with its state, as [`crate::LeaseTable::list`] shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseInfo {
    /// The lease id.
    pub id: LeaseId,
    /// Where the lease is in its life.
    pub state: LeaseState,
    /// The host, once the provider has named it.
    pub host: Option<HostId>,
    /// When the lease was acquired.
    pub acquired_at: Timestamp,
    /// When it lapses unless renewed.
    pub expires_at: Timestamp,
    /// When its maximum wall time ends it, whatever renewals say.
    pub ends_at: Timestamp,
    /// The labels written onto every billable resource of the lease.
    pub labels: Labels,
}

/// What a release did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseOutcome {
    /// The provider confirmed the stop and the lease is gone from the table.
    Freed,
    /// The provider did not confirm the stop. The lease stays, releasing, with its
    /// booking counted, and [`crate::LeaseTable::tick`] retries it.
    Releasing,
}

/// What one [`crate::LeaseTable::tick`] did, for the server to log and count.
#[derive(Debug, Default)]
pub struct TickReport {
    /// Leases whose expiry had passed and whose stop was started this tick.
    pub expired: Vec<LeaseId>,
    /// Leases the provider confirmed gone this tick, expired or being released.
    pub freed: Vec<LeaseId>,
    /// Pending leases the provider confirmed present: now held.
    pub confirmed: Vec<LeaseId>,
    /// Pending leases the provider said do not exist: dropped.
    pub dropped: Vec<LeaseId>,
    /// Leases still pending after the tick.
    pub pending: usize,
    /// Leases still releasing after the tick.
    pub releasing: usize,
    /// Set when the tick wrote the file (it changed the table, or an earlier save had
    /// failed) and could not.
    pub save_error: Option<SaveError>,
}

impl TickReport {
    /// Whether the tick changed any lease.
    pub fn changed(&self) -> bool {
        let lists = [&self.expired, &self.freed, &self.confirmed, &self.dropped];
        lists.iter().any(|list| !list.is_empty())
    }
}

/// What a missing lease file means when a table is opened.
///
/// The file is the only record of the ids already issued and of the leases the
/// provider still holds. A table that started empty whenever the file was missing would
/// restart at term 1 after a lost or unmounted state volume, reissue old ids and forget
/// leases it never stopped. So creating the file is something the operator asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Boot {
    /// The file must exist: a missing file is an error.
    Resume,
    /// The first boot: a missing file starts an empty table. An existing file is
    /// loaded as for [`Boot::Resume`].
    Initialise,
}

use std::time::Duration;

use js_types::{HostId, Labels, LeaseId, Resources};

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
    prepare_timeout: Duration,
    boot_grace: Duration,
    regain_grace: Duration,
    max_wall: Option<Duration>,
}

/// How long a whole-node lease may wait to be observed ready when nothing says
/// otherwise: 600 s, the default of [`Settings::prepare_timeout`].
pub const DEFAULT_PREPARE_TIMEOUT: Duration = Duration::from_secs(600);

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
            prepare_timeout: DEFAULT_PREPARE_TIMEOUT,
            boot_grace: Duration::ZERO,
            regain_grace: Duration::ZERO,
            max_wall: None,
        })
    }

    /// The same settings with `max_wall` as the longest maximum wall time a request may
    /// ask for. A lease is bounded by its maximum wall time whatever the renewals, so this
    /// bounds the whole life of every lease the table grants after it is set, and with it
    /// the time a draining host takes to empty. A request above it is refused with
    /// [`AcquireError::WallAboveCeiling`](crate::AcquireError::WallAboveCeiling). It must be
    /// positive; with none set, only a positive wall time is required. A lease already in
    /// the file keeps the wall time it was granted.
    pub fn with_max_wall(mut self, max_wall: Duration) -> Result<Self, SettingsError> {
        if max_wall.is_zero() {
            return Err(SettingsError::MaxWallNotPositive);
        }
        self.max_wall = Some(max_wall);
        Ok(self)
    }

    /// The ceiling on a request's maximum wall time, if one is set.
    pub fn max_wall(&self) -> Option<Duration> {
        self.max_wall
    }

    /// The same settings with `prepare_timeout` as the longest a whole-node lease waits,
    /// pending, to be observed ready. A whole lease's ttl and maximum wall time count from
    /// the moment it is observed ready, so this is the only bound on the wait. It must be
    /// positive; it defaults to [`DEFAULT_PREPARE_TIMEOUT`] (600 s). The request's maximum
    /// wall time does not shorten the wait.
    pub fn with_prepare_timeout(
        mut self,
        prepare_timeout: Duration,
    ) -> Result<Self, SettingsError> {
        if prepare_timeout.is_zero() {
            return Err(SettingsError::PrepareNotPositive);
        }
        self.prepare_timeout = prepare_timeout;
        Ok(self)
    }

    /// The same settings with the restart grace of reservation records: on open, every
    /// held record's deadline becomes the later of the stored deadline and the boot plus
    /// `boot_grace`, and every record may be claimed until `regain_grace` after the boot
    /// ([`ReservationRecord::claim_by`]). Both default to zero. A restored
    /// deadline is `max(stored, boot + boot_grace)`, so a restart loop holds it at
    /// `now + boot_grace` while the loop lasts and never grows it beyond that.
    ///
    /// This grace applies to reservation records only. Lease records have no restart
    /// grace in this crate; a lease-side grace is a separate setting, not yet wired into the
    /// server.
    pub fn with_restart_grace(mut self, boot_grace: Duration, regain_grace: Duration) -> Self {
        self.boot_grace = boot_grace;
        self.regain_grace = regain_grace;
        self
    }

    /// The floor of a restored reservation's deadline: the boot plus this.
    pub fn boot_grace(&self) -> Duration {
        self.boot_grace
    }

    /// How long after the boot a restored reservation record may still be claimed.
    pub fn regain_grace(&self) -> Duration {
        self.regain_grace
    }

    /// The longest a whole-node lease waits, pending, to be observed ready.
    pub fn prepare_timeout(&self) -> Duration {
        self.prepare_timeout
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

/// A host a lease file says is booked, as [`crate::read_bookings`] reports it.
///
/// A provider that keeps its bookings in memory (the static pool) loses them when the
/// process ends; the file is the record from which a restart books them again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Booking {
    /// The lease holding the host.
    pub lease: LeaseId,
    /// The host the file names for it.
    pub host: HostId,
    /// The state the file has the lease in: a host is named for a held lease and for a
    /// releasing one, whose booking stays counted until the provider confirms the stop.
    pub state: LeaseState,
    /// What the lease books on the host when its class lets leases share one: its own
    /// resources. `None` for a lease that books the whole host, and for a file written
    /// before the file kept them, which a provider puts back as booking the whole host.
    pub shares: Option<Resources>,
}

/// Until when a host is excluded for a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExclusionEnd {
    /// Until this instant.
    At(Timestamp),
    /// For the life of the request.
    Forever,
}

/// A host a request must not be placed on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exclusion {
    /// The excluded host.
    pub host: HostId,
    /// Until when.
    pub until: ExclusionEnd,
}

/// The part of a reservation record that exists while a host is reserved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    /// The reserved host.
    pub host: HostId,
    /// When it was reserved.
    pub reserved_at: Timestamp,
    /// When the reservation lapses. After an open, the stored deadline plus the boot
    /// grace.
    pub deadline: Timestamp,
    /// Whether the host was quiet when it was set aside: it drains nothing and waits for
    /// the request's next start, and does not count toward the draining limit.
    pub assigned: bool,
}

/// A request's place on the wait list when the ledger was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Queued {
    /// Where the request waited from the front (zero is next). An open returns the
    /// records as written; the caller restores them in this order.
    pub place: u32,
    /// When the request was first presented, which its wait is counted from.
    pub since: Timestamp,
}

/// One row of the reservation ledger, kept in the lease file. The table stores and
/// returns these and decides nothing about them: a record naming a host or request that
/// no longer exists is dropped by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservationRecord {
    /// The request the record is for; one record per request.
    pub request_id: String,
    /// The reservation, or `None` for a request between reservations, which keeps only
    /// its counters and exclusions.
    pub held: Option<Held>,
    /// The request's place on the wait list, or `None` for a request that is not waiting
    /// (it keeps only its counters and exclusions).
    pub queued: Option<Queued>,
    /// How many reservations of the request were given up.
    pub give_ups: u32,
    /// How many prepares of the request failed.
    pub prepare_failures: u32,
    /// The hosts excluded for the request.
    pub exclusions: Vec<Exclusion>,
    /// Set on a record restored by an open, never written: the boot plus the regain
    /// grace, after which the caller drops the record if nobody claimed it. `None` on
    /// a record the caller made.
    pub claim_by: Option<Timestamp>,
}

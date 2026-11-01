//! The table's decisions, with no provider and no clock in them. The generic
//! [`crate::LeaseTable`] calls the provider and hands each answer here, so every branch
//! is one non-generic function.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use js_provider::{Observed, StartAck, StartSpec, StopAck};
use js_types::{Admission, HostId, LabelError, Labels, LeaseId, ReservedKey, Resources};

use crate::clock::{Monotonic, SystemMonotonic, Timestamp};
use crate::error::{AcquireError, OpenError, RenewError, SaveError};
use crate::reservations;
use crate::state::LeaseState;
use crate::store::{self, Document, FORMAT_VERSION, SharesRow, StoredLease, StoredShares};
use crate::timings::Timings;
use crate::types::{Booking, Boot, Lease, LeaseInfo, ReservationRecord, Settings};

/// What a tick does with one lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// Nothing: held and not due.
    Idle,
    /// The expiry has passed: start the stop.
    Expire,
    /// Releasing: stop again.
    Retry,
    /// Pending: observe (a pending whole lease at its deadline too, once).
    Resolve,
}

/// What an observation of a pending or reloaded lease found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Seen {
    /// The provider has the lease.
    Confirmed,
    /// The provider said it does not exist, and the record is gone.
    Gone,
    /// No answer; the record is kept as it was.
    Unsure,
}

/// What a pending whole-node lease keeps until it is observed ready: the ttl granted
/// and the maximum wall time asked, both counted from that moment.
#[derive(Debug, Clone, Copy)]
struct Token {
    ttl: Duration,
    wall: Duration,
}

/// Millis of `duration` as the file holds them, saturating.
fn millis_of(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[derive(Debug, Clone)]
pub(crate) struct Record {
    id: LeaseId,
    state: LeaseState,
    host: Option<HostId>,
    acquired_at: Timestamp,
    expires_at: Timestamp,
    ends_at: Timestamp,
    /// Whether the lease is of a whole-node class.
    whole: bool,
    /// Set while a whole lease is pending: what its clock starts from when it is ready.
    token: Option<Token>,
    /// Set (in memory only) once a pending whole lease has been observed at or after its
    /// prepare deadline and the provider still had no answer. Until then the deadline
    /// observes the lease instead of expiring it; see [`Record::step`].
    deadline_observed: bool,
    request_labels: BTreeMap<String, String>,
    validation_run: Option<String>,
    labels: Labels,
    /// What the lease books on its host when its class lets leases share one.
    shares: Option<Resources>,
    extra: toml::Table,
}

/// The labels of a stored lease: the caller's, plus the validation run when it has one.
fn labels_of(
    caller: &BTreeMap<String, String>,
    validation_run: Option<&str>,
) -> Result<Labels, LabelError> {
    let labels = Labels::from_caller(caller)?;
    match validation_run {
        None => Ok(labels),
        Some(run) => labels.with_reserved(ReservedKey::ValidationRun, run),
    }
}

impl Record {
    fn restore(stored: &StoredLease) -> Result<Self, String> {
        let host = stored.host.as_deref().map(HostId::new).transpose();
        let host = host.map_err(|err| err.to_string())?;
        let labels = labels_of(&stored.labels, stored.validation_run.as_deref());
        let labels = labels.map_err(|err| err.to_string())?;
        let token = Self::restore_token(stored)?;
        Ok(Self {
            id: LeaseId::new(stored.term, stored.sequence),
            state: stored.state,
            host,
            acquired_at: Timestamp::from_millis(stored.acquired_at_ms),
            expires_at: Timestamp::from_millis(stored.expires_at_ms),
            ends_at: Timestamp::from_millis(stored.ends_at_ms),
            whole: stored.whole,
            token,
            deadline_observed: false,
            request_labels: stored.labels.clone(),
            validation_run: stored.validation_run.clone(),
            labels,
            shares: Self::restore_shares(stored),
            extra: stored.extra.clone(),
        })
    }

    /// What the stored lease books when its class lets leases share a host. A row that is
    /// not valid resources (it is not a table, lacks a count, or names GPUs without a class)
    /// reads as none, so the lease is put back booking the whole host, and is warned about.
    fn restore_shares(stored: &StoredLease) -> Option<Resources> {
        let found = stored.shares.as_ref().and_then(|row| match row {
            SharesRow::Good(row) => row.resources(),
            SharesRow::Bad => None,
        });
        if stored.shares.is_some() && found.is_none() {
            tracing::warn!(
                lease = %LeaseId::new(stored.term, stored.sequence),
                "the lease file's shares row is not valid resources; the lease is put back booking the whole host"
            );
        }
        found
    }

    /// The token of a pending whole lease: both of its fields are required, because a
    /// pending lease without them could never start its clock.
    fn restore_token(stored: &StoredLease) -> Result<Option<Token>, String> {
        let waiting = stored.whole && stored.state == LeaseState::Pending;
        if !waiting {
            return Ok(None);
        }
        let (Some(ttl), Some(wall)) = (stored.ttl_ms, stored.wall_ms) else {
            let reason = "a pending whole lease lacks ttl_ms or wall_ms";
            return Err(format!(
                "lease {}.{}: {reason}",
                stored.term, stored.sequence
            ));
        };
        Ok(Some(Token {
            ttl: Duration::from_millis(ttl),
            wall: Duration::from_millis(wall),
        }))
    }

    /// The lease is ready at `now`: a whole lease's clock starts here, so it expires at
    /// `now + ttl` (at most its wall time) and ends at `now + wall`. A lease with no
    /// token is left as it is.
    fn start_clock(&mut self, now: Timestamp) {
        let Some(token) = self.token.take() else {
            return;
        };
        self.ends_at = now.plus(token.wall);
        self.expires_at = now.plus(token.ttl).min(self.ends_at);
    }

    fn stored(&self) -> StoredLease {
        StoredLease {
            term: self.id.term(),
            sequence: self.id.sequence(),
            state: self.state,
            host: self.host.as_ref().map(|host| host.as_str().to_string()),
            acquired_at_ms: self.acquired_at.millis(),
            expires_at_ms: self.expires_at.millis(),
            ends_at_ms: self.ends_at.millis(),
            whole: self.whole,
            ttl_ms: self.token.map(|token| millis_of(token.ttl)),
            wall_ms: self.token.map(|token| millis_of(token.wall)),
            validation_run: self.validation_run.clone(),
            labels: self.request_labels.clone(),
            shares: self
                .shares
                .as_ref()
                .map(|shares| SharesRow::Good(StoredShares::of(shares))),
            extra: self.extra.clone(),
        }
    }

    fn info(&self) -> LeaseInfo {
        LeaseInfo {
            id: self.id,
            state: self.state,
            host: self.host.clone(),
            acquired_at: self.acquired_at,
            expires_at: self.expires_at,
            ends_at: self.ends_at,
            labels: self.labels.clone(),
        }
    }

    /// Notes an observation that found no answer: at or after the deadline of a pending
    /// whole lease it is the one the provider is given before the table expires the lease.
    fn note_unknown_at(&mut self, now: Timestamp) {
        let pending_whole = self.whole && self.state == LeaseState::Pending;
        self.deadline_observed |= pending_whole && self.expires_at <= now;
    }

    /// A pending whole lease's deadline is the provider's prepare deadline too, and the
    /// provider decides what a missed one means (a failed prepare, which it counts and
    /// holds against the host). So a pending whole lease that has reached its deadline is
    /// observed first; only when that observation still has no answer does the next tick
    /// expire it (the table's own stop, which the provider does not count as a failure).
    fn step(&self, now: Timestamp) -> Step {
        if self.state == LeaseState::Releasing {
            return Step::Retry;
        }
        let unasked = [
            self.whole,
            self.state == LeaseState::Pending,
            !self.deadline_observed,
        ];
        let unasked = unasked.into_iter().all(|yes| yes);
        if self.expires_at <= now && !unasked {
            return Step::Expire;
        }
        if self.state == LeaseState::Pending {
            return Step::Resolve;
        }
        Step::Idle
    }
}

/// The leases of `document`, checked: the count matches, every lease is valid, no id
/// twice. Any failure is an [`OpenError::Corrupt`] naming `path`.
fn restore_all(path: &Path, document: &Document) -> Result<BTreeMap<LeaseId, Record>, OpenError> {
    let counted = u64::try_from(document.lease.len()).ok();
    if counted != Some(document.lease_count) {
        let reason = format!(
            "the file counts {} leases and holds {}",
            document.lease_count,
            document.lease.len()
        );
        return Err(store::corrupt(path, reason));
    }
    let restored = document.lease.iter().map(Record::restore);
    let restored = restored.collect::<Result<Vec<_>, _>>();
    let restored = restored.map_err(|reason| store::corrupt(path, reason))?;
    let mut leases = BTreeMap::new();
    for record in restored {
        let id = record.id;
        if leases.insert(id, record).is_some() {
            return Err(store::corrupt(path, format!("lease {id} is listed twice")));
        }
    }
    Ok(leases)
}

/// The hosts the file at `path` says are booked, one per lease that names a host. A
/// missing file books nothing. The file is read and checked as [`Book::load`] does and
/// is not locked, written or changed.
pub(crate) fn bookings(path: &Path) -> Result<Vec<Booking>, OpenError> {
    let document = store::read(path)?;
    let leases = document.map(|document| restore_all(path, &document));
    let leases = leases.transpose()?.unwrap_or_default();
    let named = leases.values().filter_map(|record| {
        Some(Booking {
            lease: record.id,
            host: record.host.clone()?,
            state: record.state,
            shares: record.shares.clone(),
        })
    });
    Ok(named.collect())
}

#[derive(Debug)]
pub(crate) struct Book {
    settings: Settings,
    path: PathBuf,
    /// Held for the life of the book: the exclusive lock on the file.
    _lock: store::Lock,
    term: u64,
    next_sequence: u64,
    leases: BTreeMap<LeaseId, Record>,
    /// The reservation ledger by request id, and the fields of it this build does not
    /// know, by request id.
    ledger: BTreeMap<String, ReservationRecord>,
    kept: BTreeMap<String, toml::Table>,
    /// The version the file had, and the fields of it this build does not know.
    version: u32,
    extra: toml::Table,
    /// Set when a save failed and not yet cleared by one that worked: the file is
    /// behind the table.
    unsaved: bool,
    /// Times each save of the file, for the slow-step warning.
    timings: Timings,
}

impl Book {
    /// Loads the table over `path`. A missing file is an empty table only when `boot`
    /// is [`Boot::Initialise`]; otherwise it is [`OpenError::Missing`], so a lost state
    /// volume is never mistaken for a first boot. Every boot takes the next term, so an
    /// id of an earlier boot is never issued again even if the file lost its last
    /// writes. The file is locked first, and a second table over it is refused.
    pub(crate) fn load(
        settings: Settings,
        path: PathBuf,
        boot: Boot,
        now: Timestamp,
    ) -> Result<Self, OpenError> {
        let lock = store::lock(&path)?;
        let empty = Document {
            version: FORMAT_VERSION,
            term: 0,
            next_sequence: 1,
            lease_count: 0,
            lease: Vec::new(),
            reservation_count: 0,
            reservation: Vec::new(),
            extra: toml::Table::new(),
        };
        let document = match (store::read(&path)?, boot) {
            (Some(document), _) => document,
            (None, Boot::Initialise) => empty,
            (None, Boot::Resume) => return Err(OpenError::Missing { path }),
        };
        let leases = restore_all(&path, &document)?;
        let (ledger, kept) = reservations::restore_all(&path, &document, &settings, now)?;
        let term = document.term.checked_add(1);
        let term = term.filter(|term| *term <= store::LARGEST);
        let term = term.ok_or_else(|| OpenError::TermExhausted { path: path.clone() })?;
        Ok(Self {
            settings,
            path,
            _lock: lock,
            term,
            next_sequence: document.next_sequence.max(1),
            leases,
            ledger: ledger
                .into_iter()
                .map(|r| (r.request_id.clone(), r))
                .collect(),
            kept,
            version: document.version,
            extra: document.extra,
            unsaved: false,
            timings: Timings::new(SystemMonotonic::default()),
        })
    }

    /// Times the saves that follow with `clock`, into a new record.
    pub(crate) fn watch_saves(&mut self, clock: impl Monotonic + 'static) {
        self.timings = Timings::new(clock);
    }

    /// The record of the saves.
    pub(crate) fn timings(&self) -> Timings {
        self.timings.clone()
    }

    /// Writes the table to its file, and remembers whether that worked. A write that took
    /// longer than a second is logged as a warning naming `operation`, the step that saved.
    pub(crate) fn save(&mut self, operation: &'static str) -> Result<(), SaveError> {
        let document = Document {
            version: self.version.max(FORMAT_VERSION),
            term: self.term,
            next_sequence: self.next_sequence,
            lease_count: u64::try_from(self.leases.len()).unwrap_or(u64::MAX),
            lease: self.leases.values().map(Record::stored).collect(),
            reservation_count: u64::try_from(self.ledger.len()).unwrap_or(u64::MAX),
            reservation: (self.ledger.values())
                .map(|record| reservations::stored(record, &self.kept))
                .collect(),
            extra: self.extra.clone(),
        };
        let timer = self.timings.begin(operation);
        let saved = store::write(&self.path, &document);
        timer.finish();
        self.unsaved = saved.is_err();
        saved
    }

    /// Replaces the ledger, the later of two records of one request winning, and marks
    /// the file behind the table.
    pub(crate) fn set_reservations(&mut self, records: Vec<ReservationRecord>) {
        self.ledger = records
            .into_iter()
            .map(|r| (r.request_id.clone(), r))
            .collect();
        self.kept.retain(|id, _| self.ledger.contains_key(id));
        self.unsaved = true;
    }

    pub(crate) fn reservations(&self) -> Vec<ReservationRecord> {
        self.ledger.values().cloned().collect()
    }

    /// Whether the last save failed, so the file is behind the table.
    pub(crate) fn unsaved(&self) -> bool {
        self.unsaved
    }

    /// The ceiling on a request's maximum wall time, if the settings have one.
    pub(crate) fn max_wall(&self) -> Option<Duration> {
        self.settings.max_wall()
    }

    pub(crate) fn grant(&self, asked: Option<Duration>) -> Option<Duration> {
        self.settings.grant(asked)
    }

    /// The next lease id of this boot; `None` when the sequence after it could not be
    /// stored (the file holds integers up to [`store::LARGEST`]), in which case nothing
    /// changes.
    pub(crate) fn next_id(&mut self) -> Option<LeaseId> {
        let after = self.next_sequence.checked_add(1);
        let after = after.filter(|after| *after <= store::LARGEST)?;
        let id = LeaseId::new(self.term, self.next_sequence);
        self.next_sequence = after;
        Some(id)
    }

    /// Records a lease before the provider is told to start it, and returns the record.
    pub(crate) fn insert_pending(
        &mut self,
        spec: &StartSpec,
        now: Timestamp,
        ttl: Duration,
    ) -> Record {
        let request = spec.request();
        let ends_at = now.plus(request.max_wall);
        let whole = *spec.class().admission() == Admission::Whole;
        // A whole lease waits, pending, for at most the prepare timeout and is not charged
        // for the wait: its own ttl and wall time start when it is ready, so its maximum
        // wall time does not clamp the wait.
        let expires_at = if whole {
            now.plus(self.settings.prepare_timeout())
        } else {
            now.plus(ttl).min(ends_at)
        };
        let token = whole.then_some(Token {
            ttl,
            wall: request.max_wall,
        });
        let record = Record {
            id: spec.lease(),
            state: LeaseState::Pending,
            host: None,
            acquired_at: now,
            expires_at,
            ends_at,
            whole,
            token,
            deadline_observed: false,
            request_labels: request.labels.clone(),
            validation_run: request.validation_run.clone(),
            labels: spec.labels().clone(),
            shares: (!spec.class().admission().books_whole_host())
                .then(|| request.resources.clone()),
            extra: toml::Table::new(),
        };
        self.leases.insert(record.id, record.clone());
        record
    }

    /// Applies the provider's answer to a start of `record`.
    pub(crate) fn settle_start(
        &mut self,
        mut record: Record,
        ack: StartAck,
    ) -> Result<Lease, AcquireError> {
        match ack {
            StartAck::Started { host } => {
                record.state = LeaseState::Held;
                record.start_clock(record.acquired_at);
                record.host = Some(host.clone());
                let lease = Lease {
                    id: record.id,
                    host,
                    expires_at: record.expires_at,
                    labels: record.labels.clone(),
                };
                self.leases.insert(record.id, record);
                Ok(lease)
            }
            StartAck::Refused(code) => {
                self.leases.remove(&record.id);
                Err(AcquireError::Refused(code))
            }
            StartAck::Unknown => Err(AcquireError::Unconfirmed(record.id)),
        }
    }

    /// Extends a held lease: to `now + ttl` at most, never past its maximum wall time,
    /// never earlier than it already was.
    pub(crate) fn renew(
        &mut self,
        id: &LeaseId,
        asked: Option<Duration>,
        now: Timestamp,
    ) -> Result<Timestamp, RenewError> {
        let ttl = self.grant(asked).ok_or(RenewError::TtlNotPositive)?;
        let record = self.leases.get_mut(id).ok_or(RenewError::Unknown(*id))?;
        if record.state != LeaseState::Held {
            return Err(RenewError::NotHeld {
                id: *id,
                state: record.state,
            });
        }
        if record.expires_at <= now {
            return Err(RenewError::Expired(*id));
        }
        let extended = now.plus(ttl).min(record.ends_at);
        record.expires_at = record.expires_at.max(extended);
        Ok(record.expires_at)
    }

    /// Marks a lease releasing; `None` when the table has no such lease.
    pub(crate) fn mark_releasing(&mut self, id: &LeaseId) -> Option<()> {
        let record = self.leases.get_mut(id);
        record.map(|record| record.state = LeaseState::Releasing)
    }

    /// Applies the provider's answer to a stop. True when the lease is freed: only a
    /// stop that read back `Absent` frees it, so a stop whose read-back still shows the
    /// lease, or shows nothing, keeps it.
    pub(crate) fn settle_stop(&mut self, id: &LeaseId, ack: StopAck) -> bool {
        match ack {
            StopAck::Stopped { read_back } => self.settle_absence(id, &read_back),
            StopAck::Unknown => false,
        }
    }

    /// Removes the lease when `seen` is an explicit absence. True when it did.
    pub(crate) fn settle_absence(&mut self, id: &LeaseId, seen: &Observed) -> bool {
        let gone = seen.frees_booking();
        if gone {
            self.leases.remove(id);
        }
        gone
    }

    /// Applies an observation of a pending or reloaded lease: present makes it held on
    /// the host named (a whole lease's ttl and wall time start now), absent drops it,
    /// unknown keeps it as it was.
    pub(crate) fn settle_observe(&mut self, id: &LeaseId, seen: Observed, now: Timestamp) -> Seen {
        match seen {
            Observed::Present { host } => {
                let record = self.leases.get_mut(id);
                let _ = record.map(|record| {
                    record.state = LeaseState::Held;
                    record.host = Some(host);
                    record.start_clock(now);
                });
                Seen::Confirmed
            }
            Observed::Absent => {
                self.leases.remove(id);
                Seen::Gone
            }
            Observed::Unknown => {
                let record = self.leases.get_mut(id);
                let _ = record.map(|record| record.note_unknown_at(now));
                Seen::Unsure
            }
        }
    }

    /// What a tick does with the lease at `now`.
    pub(crate) fn step(&self, id: &LeaseId, now: Timestamp) -> Step {
        self.leases
            .get(id)
            .map_or(Step::Idle, |record| record.step(now))
    }

    /// Every lease id, in order.
    pub(crate) fn ids(&self) -> Vec<LeaseId> {
        self.leases.keys().copied().collect()
    }

    /// The ids of the leases loaded as held.
    pub(crate) fn held_ids(&self) -> Vec<LeaseId> {
        let held = self.leases.values();
        let held = held.filter(|record| record.state == LeaseState::Held);
        held.map(|record| record.id).collect()
    }

    /// How many leases are in `state`.
    pub(crate) fn count(&self, state: LeaseState) -> usize {
        let all = self.leases.values();
        all.filter(|record| record.state == state).count()
    }

    pub(crate) fn term(&self) -> u64 {
        self.term
    }

    pub(crate) fn list(&self) -> Vec<LeaseInfo> {
        self.leases.values().map(Record::info).collect()
    }
}

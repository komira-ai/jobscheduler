use std::path::{Path, PathBuf};
use std::time::Duration;

use js_provider::{Provider, StartSpec};
use js_types::{LeaseId, Request};

use crate::book::{self, Book, Seen, Step};
use crate::clock::{Clock, Timestamp};
use crate::error::{AcquireError, OpenError, ReleaseError, RenewError, SaveError};
use crate::state::LeaseState;
use crate::types::{Booking, Boot, Lease, LeaseInfo, ReleaseOutcome, Settings, TickReport};

/// The lease table over one provider. See the crate docs for the rules.
///
/// The table decides nothing itself: it calls the provider and passes each answer to
/// the non-generic book that applies it, so the rules are the same whatever the
/// provider or clock. It is not thread-safe; a server wraps it in a lock.
#[derive(Debug)]
pub struct LeaseTable<P, C> {
    provider: P,
    clock: C,
    book: Book,
}

impl<P: Provider, C: Clock> LeaseTable<P, C> {
    /// Opens the table over the file at `path`, holding an exclusive lock beside it for
    /// as long as the table lives (a second table over the same file is an
    /// [`OpenError::Lock`]). A missing file is an [`OpenError::Missing`] unless `boot` is
    /// [`Boot::Initialise`].
    ///
    /// Every lease in the file is restored with its state and the next sequence is
    /// kept; the table takes a new term for this boot. A lease stored as held is
    /// observed once: present stays held, absent is dropped, unknown is kept. The file
    /// is written before this returns.
    pub fn open(
        provider: P,
        clock: C,
        settings: Settings,
        path: impl Into<PathBuf>,
        boot: Boot,
    ) -> Result<Self, OpenError> {
        let book = Book::load(settings, path.into(), boot)?;
        let mut table = Self {
            provider,
            clock,
            book,
        };
        for id in table.book.held_ids() {
            let seen = table.provider.observe(&id);
            table.book.settle_observe(&id, seen);
        }
        table.save()?;
        Ok(table)
    }

    /// Acquires a lease for `request` on the provider, for `ttl` (the default when
    /// `None`, at most the maximum, never zero).
    ///
    /// The lease is written to the file before the provider is called. A refusal, by
    /// the start contract or the provider, records nothing. An Unknown answer leaves
    /// the lease pending and is an [`AcquireError::Unconfirmed`]; [`Self::tick`]
    /// resolves it. The lease expires at the earlier of `now + ttl` and `now` plus the
    /// request's maximum wall time.
    pub fn acquire(
        &mut self,
        request: Request,
        ttl: Option<Duration>,
    ) -> Result<Lease, AcquireError> {
        let ttl = self.book.grant(ttl).ok_or(AcquireError::TtlNotPositive)?;
        let now = self.clock.now();
        let id = self.book.next_id().ok_or(AcquireError::IdsExhausted)?;
        let known = self.provider.classes();
        let spec = StartSpec::new(id, request, &known).map_err(AcquireError::Refused)?;
        let record = self.book.insert_pending(&spec, now, ttl);
        self.save()?;
        let ack = self.provider.start(spec);
        let settled = self.book.settle_start(record, ack);
        let _ = self.save();
        settled
    }

    /// Extends a held lease and returns its new expiry: `now + ttl` clamped as in
    /// [`Self::acquire`], never past the lease's maximum wall time and never earlier
    /// than the expiry it has. A lease that is unknown, pending, releasing or past its
    /// expiry is an error and is not extended.
    pub fn renew(&mut self, id: &LeaseId, ttl: Option<Duration>) -> Result<Timestamp, RenewError> {
        let now = self.clock.now();
        let expires_at = self.book.renew(id, ttl, now)?;
        self.save()?;
        Ok(expires_at)
    }

    /// Releases a lease: marks it releasing, stops it on the provider and, when the
    /// provider reads back that it is gone, removes it. A stop without that answer
    /// leaves the lease releasing, with its booking counted, for [`Self::tick`] to
    /// retry. Releasing a lease that is already releasing stops it again.
    pub fn release(&mut self, id: &LeaseId) -> Result<ReleaseOutcome, ReleaseError> {
        let marked = self.book.mark_releasing(id);
        marked.ok_or(ReleaseError::Unknown(*id))?;
        self.save()?;
        let freed = self.stop(id);
        let _ = self.save();
        match freed {
            true => Ok(ReleaseOutcome::Freed),
            false => Ok(ReleaseOutcome::Releasing),
        }
    }

    /// The one place time acts. For every lease: a releasing one is stopped again, and
    /// observed when the stop does not free it; a pending or held one whose expiry has
    /// passed is marked releasing and stopped as a release does; a pending one still
    /// in time is observed (present makes it held, absent drops it, unknown keeps it).
    /// The file is written once at the end when anything changed or an earlier save failed,
    /// and a failure is reported in the result.
    pub fn tick(&mut self) -> TickReport {
        let now = self.clock.now();
        let mut report = TickReport::default();
        for id in self.book.ids() {
            match self.book.step(&id, now) {
                Step::Idle => {}
                Step::Expire => {
                    self.book.mark_releasing(&id);
                    report.expired.push(id);
                    if self.stop(&id) {
                        report.freed.push(id);
                    }
                }
                Step::Retry => {
                    if self.stop_or_observe(&id) {
                        report.freed.push(id);
                    }
                }
                Step::Resolve => {
                    let seen = self.provider.observe(&id);
                    match self.book.settle_observe(&id, seen) {
                        Seen::Confirmed => report.confirmed.push(id),
                        Seen::Gone => report.dropped.push(id),
                        Seen::Unsure => {}
                    }
                }
            }
        }
        report.pending = self.book.count(LeaseState::Pending);
        report.releasing = self.book.count(LeaseState::Releasing);
        if report.changed() | self.book.unsaved() {
            report.save_error = self.save().err();
        }
        report
    }

    /// Every lease of the table with its state, in id order.
    pub fn list(&self) -> Vec<LeaseInfo> {
        self.book.list()
    }

    /// The term of this boot: the first boot of a file takes 1 and every later boot the
    /// next. A caller that mints its own ids puts the term in them, so an id of an earlier
    /// boot is never issued again even though the caller keeps nothing across a restart.
    pub fn term(&self) -> u64 {
        self.book.term()
    }

    /// The provider, for reads such as its capacity.
    pub fn provider(&self) -> &P {
        &self.provider
    }

    /// The provider, mutably. Tests use it to script the fake.
    pub fn provider_mut(&mut self) -> &mut P {
        &mut self.provider
    }

    fn stop(&mut self, id: &LeaseId) -> bool {
        let ack = self.provider.stop(id);
        self.book.settle_stop(id, ack)
    }

    fn stop_or_observe(&mut self, id: &LeaseId) -> bool {
        match self.stop(id) {
            true => true,
            false => self.gone(id),
        }
    }

    fn gone(&mut self, id: &LeaseId) -> bool {
        let seen = self.provider.observe(id);
        self.book.settle_absence(id, &seen)
    }

    fn save(&mut self) -> Result<(), SaveError> {
        self.book.save()
    }
}

impl<P, C> LeaseTable<P, C> {
    /// Gives the provider back, dropping the table and its clock.
    pub fn into_provider(self) -> P {
        self.provider
    }
}

/// The bookings the lease file at `path` records: every lease that names a host, in id
/// order. A missing file has none. This reads the file as [`LeaseTable::open`] does
/// (a file that is not valid is an [`OpenError::Corrupt`]) but takes no lock, calls no
/// provider and writes nothing, so a caller can seat the bookings in a provider that
/// forgot them before it opens the table over that provider.
pub fn read_bookings(path: impl AsRef<Path>) -> Result<Vec<Booking>, OpenError> {
    book::bookings(path.as_ref())
}

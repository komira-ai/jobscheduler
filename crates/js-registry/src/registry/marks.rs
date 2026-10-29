//! The marks of scale-down: a host taken out of use so that it can be given back
//! (design `pools-providers-network.md`, sections 6.2 and 6.3).
//!
//! A host goes `unmarked -> Draining -> Releasing -> removed`. `Draining` and `Releasing`
//! are the two marks. A marked host takes no start of any kind: no lease is booked on it
//! (ordinary or whole-node), no reservation is set on it, and no run is queued on it
//! ([`crate::Registry::queue_run`]). They are not the whole-node marks (`Reserved`, `Held`),
//! which serve one request and have a deadline; these serve no request and have none.
//!
//! # The quiet rule
//!
//! [`Registry::drain`] and [`Registry::release_host`] set their mark only if the host is
//! quiet at that moment, and they check and set under the one lock that books hosts, so no
//! start can land between the check and the mark. Quiet means: no lease booked, no run
//! the host lists or the registry holds live, no whole-node lease committed or being
//! restored, and no whole-node reservation on the host. The registry has no other
//! notion of work on a host (it holds no stream), so these are all the ways a host can
//! be busy. A busy host answers [`Busy`] and keeps the mark it had. So a host that is
//! `Releasing` was quiet when it was marked and has been unbookable since: nothing can
//! have started on it that the release would cut. Work that a host reports afterwards (a
//! run listed in a later heartbeat, which the registry cannot prevent) is not cut by the
//! registry; the caller reads it as the host going busy before it calls `release_host`.
//!
//! # Persistence
//!
//! The marks are held in memory only, like the whole-node marks. A restart forgets them: a
//! host that was draining is a plain idle host again, and the scaler that owns the
//! decision drains it again.

use js_pool_static::HostBook;
use js_types::HostId;

use super::{HostMark, Record, Registry, State};
use crate::runs::Runs;
use crate::whole::reserve::Marks;

/// A host mark of scale-down, held in a host's record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Leaving {
    Draining,
    Releasing,
}

impl Leaving {
    pub(super) fn mark(self) -> HostMark {
        match self {
            Self::Draining => HostMark::Draining,
            Self::Releasing => HostMark::Releasing,
        }
    }
}

/// Why a mark call was refused. The first four say the host is busy, or not the host the
/// call is for; the rest say the call does not apply to the mark the host has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Busy {
    /// The registry does not hold the host.
    #[error("the registry does not hold the host")]
    UnknownHost,
    /// A lease is booked on the host.
    #[error("a lease is booked on the host")]
    Lease,
    /// The host lists a run, or the registry holds a live run of the host.
    #[error("the host has runs")]
    Runs,
    /// The host is set aside for a waiting whole-node request, holds a whole-node lease
    /// or is restoring after one.
    #[error("the host is set aside for, or restoring from, a whole-node lease")]
    Whole,
    /// [`Registry::release_host`] on a host that is not draining.
    #[error("the host is not draining")]
    NotDraining,
    /// [`Registry::remove`] on a host that is not releasing.
    #[error("the host is not releasing")]
    NotReleasing,
    /// [`Registry::drain`] or [`Registry::undrain`] on a host that is releasing: the mark
    /// is not undone.
    #[error("the host is releasing")]
    Releasing,
}

impl Registry {
    /// Marks `host` [`HostMark::Draining`] if it is quiet (see the module documentation
    /// of the marks): from then on it takes no start. Draining a draining host is `Ok`
    /// and changes nothing. The mark has no deadline; only [`Registry::undrain`] and
    /// [`Registry::release_host`] end it, and no placement path does.
    ///
    /// # Errors
    ///
    /// [`Busy`] when the host is unknown, busy, or already releasing. A busy host keeps
    /// the state it had.
    pub fn drain(&self, host: &HostId) -> Result<(), Busy> {
        let mut guard = self.lock();
        let State {
            book,
            records,
            runs,
            marks,
            ..
        } = &mut *guard;
        let record = records.get_mut(host).ok_or(Busy::UnknownHost)?;
        match record.leaving {
            Some(Leaving::Releasing) => return Err(Busy::Releasing),
            Some(Leaving::Draining) => return Ok(()),
            None => {}
        }
        quiet(book, runs, marks, host, record)?;
        record.leaving = Some(Leaving::Draining);
        Ok(())
    }

    /// Clears the draining mark of `host`, so it takes starts again. A host with no mark
    /// is left as it is.
    ///
    /// # Errors
    ///
    /// [`Busy::UnknownHost`] for a host the registry does not hold, and
    /// [`Busy::Releasing`] for a host that is releasing: that mark is not undone.
    pub fn undrain(&self, host: &HostId) -> Result<(), Busy> {
        let mut guard = self.lock();
        let record = guard.records.get_mut(host).ok_or(Busy::UnknownHost)?;
        match record.leaving {
            Some(Leaving::Releasing) => Err(Busy::Releasing),
            Some(Leaving::Draining) | None => {
                record.leaving = None;
                Ok(())
            }
        }
    }

    /// Marks the draining `host` [`HostMark::Releasing`], only if it is still quiet: the
    /// check and the mark are made under the lock that books hosts. After this the
    /// registry books nothing on the host, so a release that follows cuts no work.
    /// Releasing a releasing host is `Ok`. A busy host stays draining, and the caller
    /// undrains it or tries again.
    ///
    /// # Errors
    ///
    /// [`Busy::NotDraining`] for a host with no mark, [`Busy::UnknownHost`] for a host the
    /// registry does not hold, and the reason it is busy otherwise.
    pub fn release_host(&self, host: &HostId) -> Result<(), Busy> {
        let mut guard = self.lock();
        let State {
            book,
            records,
            runs,
            marks,
            ..
        } = &mut *guard;
        let record = records.get_mut(host).ok_or(Busy::UnknownHost)?;
        match record.leaving {
            None => return Err(Busy::NotDraining),
            Some(Leaving::Releasing) => return Ok(()),
            Some(Leaving::Draining) => {}
        }
        quiet(book, runs, marks, host, record)?;
        record.leaving = Some(Leaving::Releasing);
        Ok(())
    }

    /// Drops the releasing `host`, binding and runs included, and frees its place in the
    /// pool's machine limit. A host the registry does not hold is already removed, which
    /// is `Ok`. Called after the machine is read back gone, so it asks nothing of the
    /// host's runs: the registry's own record of them goes with it.
    ///
    /// # Errors
    ///
    /// [`Busy::NotReleasing`] for a host with another mark or none, and [`Busy::Lease`]
    /// for a host with a lease booked (a lease booked on a releasing host would be lost
    /// with it).
    pub fn remove(&self, host: &HostId) -> Result<(), Busy> {
        let mut guard = self.lock();
        let State {
            book,
            records,
            runs,
            ..
        } = &mut *guard;
        let Some(record) = records.get(host) else {
            return Ok(());
        };
        if record.leaving != Some(Leaving::Releasing) {
            return Err(Busy::NotReleasing);
        }
        if !book.remove_free(host) {
            return Err(Busy::Lease);
        }
        records.remove(host);
        runs.forget(host);
        Ok(())
    }
}

/// Whether `host` is quiet, or the first way it is not.
fn quiet(
    book: &HostBook,
    runs: &Runs,
    marks: &Marks,
    host: &HostId,
    record: &Record,
) -> Result<(), Busy> {
    if book.booked().contains_key(host) {
        return Err(Busy::Lease);
    }
    let whole = [marks.holder(host).is_some(), !record.exclusive.is_free()];
    if whole.into_iter().any(|set| set) {
        return Err(Busy::Whole);
    }
    match record.quiet(runs, host) {
        true => Ok(()),
        false => Err(Busy::Runs),
    }
}

#[cfg(test)]
mod tests;

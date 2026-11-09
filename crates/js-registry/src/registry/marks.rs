//! The marks of scale-down: a host taken out of use so that it can be given back
//! (design `pools-providers-network.md`, sections 6.2 and 6.3).
//!
//! A host goes `unmarked -> Draining -> Releasing -> removed`. `Draining` and `Releasing`
//! are the two marks. A marked host takes no new job; the job engine reads the mark in the
//! host's view ([`crate::HostView::mark`]).
//!
//! # The quiet rule
//!
//! [`Registry::drain`] and [`Registry::release_host`] set their mark only if the host is
//! quiet at that moment, and they check and set under the registry's lock. Quiet means: the
//! host lists no run (the previous release's work). The registry has no other notion of work
//! on a host (it holds no stream; the jobs on a host are the job tables'), so that is all
//! it can tell. A busy host answers [`Busy`] and keeps the mark it had. Work that a host
//! reports afterwards (a run listed in a later heartbeat, which the registry cannot
//! prevent) is not cut by the registry; the caller reads it as the host going busy before
//! it calls `release_host`.
//!
//! # Persistence
//!
//! The marks are held in memory only. A restart forgets them: a host that was draining is a
//! plain idle host again, and the scaler that owns the decision drains it again. A host
//! whose release was already under way is the exception that cannot wait for the scaler's
//! next step: the scaler's own records say which hosts those are, and it marks them with
//! [`Registry::mark_ending`] before any hello is admitted, so such a host that says hello
//! again is admitted already `Releasing`.
//!
//! So after a restart a host the scaler had retired ([`Registry::retire`]) is bookable again
//! until the scaler's next tick retires it again: a job may be placed on it in that gap,
//! bounded by one tick plus the margin of the cloud lifetime.

use js_types::HostId;

use super::{HostMark, Record, Registry, State};

/// A host mark of scale-down, held in a host's record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Leaving {
    /// Set by [`Registry::drain`] on a quiet host, or by [`Registry::retire`] on any host.
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

/// Why a mark call was refused. The first two say the host is busy, or not the host the
/// call is for; the rest say the call does not apply to the mark the host has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Busy {
    /// The registry does not hold the host.
    #[error("the registry does not hold the host")]
    UnknownHost,
    /// The host lists a run.
    #[error("the host has runs")]
    Runs,
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
    /// of the marks): from then on it takes no new job. Draining a draining host is `Ok`
    /// and changes nothing. The mark has no deadline; only [`Registry::undrain`] and
    /// [`Registry::release_host`] end it, and no placement path does.
    ///
    /// # Errors
    ///
    /// [`Busy`] when the host is unknown, busy, or already releasing. A busy host keeps
    /// the state it had.
    pub fn drain(&self, host: &HostId) -> Result<(), Busy> {
        let mut guard = self.lock();
        let record = guard.records.get_mut(host).ok_or(Busy::UnknownHost)?;
        match record.leaving {
            Some(Leaving::Releasing) => return Err(Busy::Releasing),
            Some(Leaving::Draining) => return Ok(()),
            None => {}
        }
        quiet(record)?;
        record.leaving = Some(Leaving::Draining);
        Ok(())
    }

    /// Marks `host` [`HostMark::Draining`] whether or not it is quiet, for a host that has
    /// reached the scaler's `max_age`. From then on it takes no new job, and no work it has
    /// is cut. The release still goes through [`Registry::release_host`], which asks the host
    /// to be quiet. Retiring a draining host is `Ok` and changes nothing.
    ///
    /// # Errors
    ///
    /// [`Busy::UnknownHost`] when the registry does not hold the host, and
    /// [`Busy::Releasing`] when it is releasing: that mark is not undone.
    pub fn retire(&self, host: &HostId) -> Result<(), Busy> {
        let mut guard = self.lock();
        let record = guard.records.get_mut(host).ok_or(Busy::UnknownHost)?;
        match record.leaving {
            Some(Leaving::Releasing) => Err(Busy::Releasing),
            _ => {
                record.leaving = Some(Leaving::Draining);
                Ok(())
            }
        }
    }

    /// Clears the draining mark of `host`, so it takes jobs again. A host with no mark is
    /// left as it is.
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
    /// check and the mark are made under the registry's lock. Releasing a releasing host is
    /// `Ok`. A busy host stays draining, and the caller undrains it or tries again.
    ///
    /// # Errors
    ///
    /// [`Busy::NotDraining`] for a host with no mark, [`Busy::UnknownHost`] for a host the
    /// registry does not hold, and [`Busy::Runs`] for a host that lists a run.
    pub fn release_host(&self, host: &HostId) -> Result<(), Busy> {
        let mut guard = self.lock();
        let record = guard.records.get_mut(host).ok_or(Busy::UnknownHost)?;
        match record.leaving {
            None => return Err(Busy::NotDraining),
            Some(Leaving::Releasing) => return Ok(()),
            Some(Leaving::Draining) => {}
        }
        quiet(record)?;
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
    /// [`Busy::NotReleasing`] for a host with another mark or none.
    pub fn remove(&self, host: &HostId) -> Result<(), Busy> {
        let mut guard = self.lock();
        let State {
            book,
            records,
            ending,
            ..
        } = &mut *guard;
        let Some(record) = records.get(host) else {
            ending.remove(host);
            return Ok(());
        };
        if record.leaving != Some(Leaving::Releasing) {
            return Err(Busy::NotReleasing);
        }
        book.remove_free(host);
        records.remove(host);
        ending.remove(host);
        self.tell_gone(host);
        Ok(())
    }

    /// Marks `host` [`HostMark::Releasing`] whether or not it is quiet, and keeps the mark
    /// for a host the registry does not hold yet: its first hello is admitted already
    /// marked. For a caller that has decided the machine is being given back and must not
    /// take work, however the registry came to hold it: a scaler that restarts with a
    /// release in flight calls this for every such machine before the server admits a
    /// hello, because the marks are held in memory and a host whose release was under way
    /// would otherwise say hello again unmarked and be given work before its machine is
    /// gone. Work the host already has is not cut; the mark only stops new work.
    /// Marking a releasing host again changes nothing. [`Registry::remove`] ends it.
    pub fn mark_ending(&self, host: &HostId) {
        let mut guard = self.lock();
        guard.ending.insert(host.clone());
        if let Some(record) = guard.records.get_mut(host) {
            record.leaving = Some(Leaving::Releasing);
        }
    }
}

/// Whether `record`'s host is quiet, or the way it is not.
fn quiet(record: &Record) -> Result<(), Busy> {
    match record.quiet() {
        true => Ok(()),
        false => Err(Busy::Runs),
    }
}

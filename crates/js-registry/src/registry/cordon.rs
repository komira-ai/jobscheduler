//! The cordon of a host: a host takes itself out of placement for a while without
//! stopping its work, as an update of its agent needs.
//!
//! A cordoned host is offered no new lease, no whole-node commit and no reservation's
//! start; the lease it holds, and the runs on it, go on. A cordon is a lease of its own:
//! whoever sets it says how long it lasts, [`Registry::cordon`] renews it by setting it
//! again, and it lifts by itself at the time it lapses, so a requester that dies costs the
//! time it asked for and no more. A cordon never shortens when it is set again; only
//! [`Registry::uncordon`] ends it early.
//!
//! The cordon is the host's record and is read at the time of each placement, so nothing
//! ticks. It is in the binding the caller persists ([`Registry::bindings`]), so a restart
//! keeps it ([`Registry::restore_tainted`]); a cordon that has lapsed is not kept. The
//! registry does not tell who set a cordon: the one route that sets it is the host's own,
//! and the requester that must lift only a cordon it placed asks [`Cordoned::placed`].

use std::time::Duration;

use js_lease::Timestamp;
use js_types::HostId;

use super::{HostView, Record, Registry, State};

/// The registry does not hold the host: it never said hello since the registry started,
/// and it holds no lease, no taint and no cordon that a restart put back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the registry does not hold the host")]
pub struct NotHeld;

/// One host as it is read about itself: what the update hook of its agent waits on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnHost {
    /// The host as the listing shows it.
    pub host: HostView,
    /// When the cordon lapses, when the host is cordoned.
    pub cordon_until: Option<Timestamp>,
    /// When the current session began (the time of its hello); `None` for a host that
    /// has not said hello since the registry started. With [`HostView::epoch`] it tells
    /// a session that went silent from the one a new hello began.
    pub session_started: Option<Timestamp>,
    /// The `runtime` the host reported at its last hello, when it reported one.
    pub runtime: Option<String>,
    /// The time the registry read these at.
    pub now: Timestamp,
}

impl OwnHost {
    fn of(host: HostView, record: &Record, now: Timestamp) -> Self {
        Self {
            host,
            cordon_until: record.cordon.filter(|until| *until > now),
            session_started: record.hello_at,
            runtime: record.runtime.clone(),
            now,
        }
    }
}

/// What [`Registry::cordon`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cordoned {
    /// The host as it is now.
    pub host: OwnHost,
    /// Whether this call placed the cordon, as opposed to renewing one the host had.
    pub placed: bool,
}

/// What [`Registry::uncordon`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Uncordoned {
    /// The host as it is now.
    pub host: OwnHost,
    /// Whether a cordon was in force and this call ended it.
    pub lifted: bool,
}

impl Registry {
    /// Cordons `host` until `lease` after now, or until later than that if it is cordoned
    /// for longer already: a repeat renews, and never shortens. From then on the host
    /// takes no new lease (a lease it holds is untouched) until the cordon lapses or
    /// [`Registry::uncordon`] ends it.
    ///
    /// # Errors
    ///
    /// [`NotHeld`] for a host the registry does not hold.
    pub fn cordon(&self, host: &HostId, lease: Duration) -> Result<Cordoned, NotHeld> {
        let now = self.shared.clock.now();
        let mut guard = self.lock_at(now);
        let State {
            book,
            records,
            runs,
            marks,
            ..
        } = &mut *guard;
        let record = records.get_mut(host).ok_or(NotHeld)?;
        let placed = !record.cordoned(now);
        let wanted = now.plus(lease);
        record.cordon = Some(
            record
                .cordon
                .filter(|held| *held > wanted)
                .unwrap_or(wanted),
        );
        let view = self.viewer((&*book, &*runs, &*marks), now);
        let host = OwnHost::of(view(host, &*record), &*record, now);
        Ok(Cordoned { host, placed })
    }

    /// Ends the cordon of `host`, if it has one. Ending a cordon that is not there is not
    /// an error.
    ///
    /// # Errors
    ///
    /// [`NotHeld`] for a host the registry does not hold.
    pub fn uncordon(&self, host: &HostId) -> Result<Uncordoned, NotHeld> {
        let now = self.shared.clock.now();
        let mut guard = self.lock_at(now);
        let State {
            book,
            records,
            runs,
            marks,
            ..
        } = &mut *guard;
        let record = records.get_mut(host).ok_or(NotHeld)?;
        let lifted = record.cordoned(now);
        record.cordon = None;
        let view = self.viewer((&*book, &*runs, &*marks), now);
        let host = OwnHost::of(view(host, &*record), &*record, now);
        Ok(Uncordoned { host, lifted })
    }

    /// `host` as it is read about itself, or `None` for a host the registry does not hold.
    pub fn own_host(&self, host: &HostId) -> Option<OwnHost> {
        let now = self.shared.clock.now();
        let guard = self.lock_at(now);
        let record = guard.records.get(host)?;
        let view = self.viewer((&guard.book, &guard.runs, &guard.marks), now);
        Some(OwnHost::of(view(host, record), record, now))
    }
}

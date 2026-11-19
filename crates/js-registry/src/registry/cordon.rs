//! The cordon of a host: a host takes itself out of placement for a while without
//! stopping its work, as an update of its agent needs.
//!
//! A cordoned host is offered no new job; the work it has goes on. A cordon is a lease of its
//! own: whoever sets it says how long it lasts, [`Registry::cordon`] renews it by setting it
//! again, and it lifts by itself at the time it lapses, so a requester that dies costs the
//! time it asked for and no more. A cordon never shortens when it is set again; only
//! [`Registry::uncordon`] ends it early.
//!
//! The cordon is the host's record and is read at the time of each placement, so nothing
//! ticks. It is in the binding the caller persists ([`Registry::bindings`]), so a restart
//! keeps it ([`Registry::restore_tainted`]); a cordon that has lapsed is not kept. The
//! holder of a cordon ([`CordonHolder`]) is kept in memory with it and is not persisted: a
//! server with the job service keeps it in the job store's cordon row, and one without it
//! loses the name over a restart, so the cordon is then unnamed. Who may renew, take over or
//! lift a cordon is the rule of [`js_types::cordon`]. A server with the job service decides
//! by the job store's rows and mirrors them here ([`Registry::mirror_cordons`]), so a cordon
//! placed or lifted through another server is seen by the placement of this one.

use std::collections::BTreeMap;
use std::time::Duration;

use js_types::cordon::{self, Asker, Cordon};
use js_types::{CordonHolder, HostId, Timestamp};

use super::{HostView, Record, Registry};

/// The registry does not hold the host: it never said hello since the registry started,
/// and it holds no taint and no cordon that a restart put back.
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
    /// Who holds the cordon, when the host is cordoned and the cordon names a holder.
    pub cordon_holder: Option<CordonHolder>,
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
            cordon_holder: record.cordon_holder_at(now),
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
    /// Whether the cordon in force is another holder's and was left as it was.
    pub held_by_other: bool,
}

/// What [`Registry::uncordon`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Uncordoned {
    /// The host as it is now.
    pub host: OwnHost,
    /// Whether a cordon was in force and this call ended it.
    pub lifted: bool,
    /// Whether the cordon in force is another holder's and was left as it was.
    pub held_by_other: bool,
}

impl Registry {
    /// Cordons `host` until `lease` after now, by the rule of [`js_types::cordon::cordon`]
    /// for the asker `by`: with no cordon in force it places one, held by the holder `by`
    /// names; an unnamed asker or the cordon's own holder renews it, never shortening it; a
    /// holder that is not the cordon's leaves it as it is (`held_by_other`); a take-over
    /// makes it the asker's. From then on the host takes no new job (the work it has is
    /// untouched) until the cordon lapses or [`Registry::uncordon`] ends it.
    ///
    /// # Errors
    ///
    /// [`NotHeld`] for a host the registry does not hold.
    pub fn cordon(&self, host: &HostId, lease: Duration, by: &Asker) -> Result<Cordoned, NotHeld> {
        let now = self.shared.clock.now();
        let mut guard = self.lock_at(now);
        let record = guard.records.get_mut(host).ok_or(NotHeld)?;
        let held = record.cordon_at(now);
        let done = cordon::cordon(held.as_ref(), lease, by);
        record.set_cordon(Some(&done.cordon), now);
        let view = self.viewer(now);
        let host = OwnHost::of(view(host, &*record), &*record, now);
        Ok(Cordoned {
            host,
            placed: !done.already,
            held_by_other: done.held_by_other,
        })
    }

    /// Ends the cordon of `host`, if it has one, by the rule of [`js_types::cordon::uncordon`]:
    /// a lift that names a holder ends only that holder's cordon and leaves another's
    /// (`held_by_other`); one that names none ends any. Ending a cordon that is not there is
    /// not an error.
    ///
    /// # Errors
    ///
    /// [`NotHeld`] for a host the registry does not hold.
    pub fn uncordon(
        &self,
        host: &HostId,
        holder: Option<&CordonHolder>,
    ) -> Result<Uncordoned, NotHeld> {
        let now = self.shared.clock.now();
        let mut guard = self.lock_at(now);
        let record = guard.records.get_mut(host).ok_or(NotHeld)?;
        let held = record.cordon_at(now);
        let done = cordon::uncordon(held.as_ref(), holder);
        if !done.held_by_other {
            record.set_cordon(None, now);
        }
        let view = self.viewer(now);
        let host = OwnHost::of(view(host, &*record), &*record, now);
        Ok(Uncordoned {
            host,
            lifted: done.lifted,
            held_by_other: done.held_by_other,
        })
    }

    /// Sets the cordon of `host` exactly to `cordon`, which a store decided: it lapses its
    /// `until` after now and is held by its holder; `None` clears it.
    ///
    /// # Errors
    ///
    /// [`NotHeld`] for a host the registry does not hold.
    pub fn set_cordon(
        &self,
        host: &HostId,
        cordon: Option<&Cordon<Duration>>,
    ) -> Result<OwnHost, NotHeld> {
        let now = self.shared.clock.now();
        let mut guard = self.lock_at(now);
        let record = guard.records.get_mut(host).ok_or(NotHeld)?;
        record.set_cordon(cordon, now);
        let view = self.viewer(now);
        Ok(OwnHost::of(view(host, &*record), &*record, now))
    }

    /// Sets the cordon of every host the registry holds to the entry `cordons` has for it
    /// (lapsing its time left after now, with its holder), and clears the cordon of a host
    /// it has none for; a host in `cordons` that the registry does not hold is ignored. The
    /// server with the job service calls it at each tick with the job store's cordon rows,
    /// which are the cordons' record then, so a cordon placed or lifted through another
    /// server is seen by this registry's placement.
    pub fn mirror_cordons(&self, cordons: &BTreeMap<HostId, Cordon<Duration>>) {
        let now = self.shared.clock.now();
        let mut guard = self.lock_at(now);
        for (host, record) in &mut guard.records {
            record.set_cordon(cordons.get(host), now);
        }
    }

    /// Every cordon in force, by host, with the time it has left and its holder.
    pub fn cordons(&self) -> BTreeMap<HostId, Cordon<Duration>> {
        let now = self.shared.clock.now();
        let guard = self.lock_at(now);
        guard
            .records
            .iter()
            .filter_map(|(host, record)| Some((host.clone(), record.cordon_at(now)?)))
            .collect()
    }

    /// `host` as it is read about itself, or `None` for a host the registry does not hold.
    pub fn own_host(&self, host: &HostId) -> Option<OwnHost> {
        let now = self.shared.clock.now();
        let guard = self.lock_at(now);
        let record = guard.records.get(host)?;
        let view = self.viewer(now);
        Some(OwnHost::of(view(host, record), record, now))
    }
}

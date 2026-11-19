//! The cordon table (`cordon/<host>`): a host taken out of job placement. The row is written
//! with a time to live, so it lapses when the cordon does, and holds the cordon's holder
//! ([`CordonHolder`]) when the request that placed it named one. Every placement's
//! transaction checks the row absent ([`JobStore::place`], [`JobStore::place_with`] and
//! [`JobStore::place_claimed`]), so a host a cordon is in force on is placed on by no server
//! that shares the store, whichever server took the cordon; [`JobStore::place_claimed`] also
//! reads the row and answers [`JobStoreError::HostCordoned`].
//!
//! [`JobStore::cordon_host`] and [`JobStore::uncordon_host`] decide by the rule of
//! `js_types::cordon` (who may renew, extend, take over or lift a cordon), on the row as they
//! read it, and commit at the version they read, so two requests that race commit one and the
//! other is a [`JobStoreError::CordonConflict`], which writes nothing and its caller asks
//! again. A request that the
//! rule leaves as it is (the cordon is another holder's) writes nothing.
//!
//! The table is additive: a store written before it has no `cordon/` rows and places as
//! before; no `idx/` space names a cordon, so the index backfill neither reads nor writes
//! one. A row that lapsed is absent to every read, and the retention tick does not read the
//! table.

use std::collections::BTreeMap;
use std::time::Duration;

use js_state::{Cond, Store, Txn, Versioned};
use js_types::cordon::{Asker, Cordon, cordon, uncordon};
use js_types::{CordonHolder, HostId};

use super::JobStore;
use crate::codec::{DecodeError, decode_cordon, encode_cordon};
use crate::error::JobStoreError;
use crate::ids::IdSource;
use crate::keys;

/// What [`JobStore::cordon_host`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CordonWrite {
    /// The cordon in force after the call, with the time it has left by the store's clock.
    pub cordon: Cordon<Duration>,
    /// Whether a cordon was in force before the call.
    pub already: bool,
    /// Whether the cordon in force is another holder's and was left as it was: nothing was
    /// written.
    pub held_by_other: bool,
}

/// What [`JobStore::uncordon_host`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UncordonWrite {
    /// Whether a cordon was in force and the call lifted it.
    pub lifted: bool,
    /// Whether a cordon is in force that another holder holds, so the call left it.
    pub held_by_other: bool,
    /// The cordon in force after the call, with the time it has left; `None` when there is
    /// none.
    pub cordon: Option<Cordon<Duration>>,
}

/// The cordon the live row `row` of `key` holds, with the time it has left at `now`.
fn cordon_in(
    key: &str,
    row: &Versioned,
    now: js_state::StoreTime,
) -> Result<Cordon<Duration>, JobStoreError> {
    let holder = decode_cordon(&row.value).map_err(|why| JobStoreError::row(key, why))?;
    // A cordon row is always put with a time to live; tag 0 stands for the expiry, which is
    // not a field of the row.
    let expires = row
        .expires_at
        .ok_or_else(|| JobStoreError::row(key, DecodeError::Missing { tag: 0 }))?;
    Ok(Cordon {
        until: expires.since(now),
        holder,
    })
}

impl<S: Store, I: IdSource> JobStore<S, I> {
    /// The cordon row of `host` as it stands: the cordon with the time it has left, and the
    /// version to commit against; `None` when there is no live row.
    fn read_cordon(
        &self,
        host: &HostId,
    ) -> Result<Option<(Cordon<Duration>, js_state::Version)>, JobStoreError> {
        let key = keys::cordon(host);
        let now = self.store.now()?;
        self.store
            .get(&key)?
            .map(|row| Ok((cordon_in(&key, &row, now)?, row.version)))
            .transpose()
    }

    /// The condition a write of the cordon row of `host` carries, given what was read.
    fn cordon_cond(read: Option<&(Cordon<Duration>, js_state::Version)>) -> Cond {
        read.map_or(Cond::Absent, |(_, version)| Cond::Version(*version))
    }

    /// Cordons `host` for `lease` from the store's time now, as `by` asks, by the rule of
    /// `js_types::cordon`: with no cordon in force it places one held by the holder `by`
    /// names; a request that names none, and the holder's own, extend the cordon in force and
    /// never shorten it; a take-over makes it the asker's; any other holder's request leaves
    /// it as it is and writes nothing ([`CordonWrite::held_by_other`]). The row is put with
    /// the time left as its time to live, at its absence or at the version read.
    ///
    /// # Errors
    /// [`JobStoreError::CordonConflict`] when the row changed before the commit (nothing is
    /// written; the caller asks again); [`JobStoreError::Corrupt`] or [`JobStoreError::UnknownRow`] when the row
    /// cannot be read, or is live with no expiry; [`JobStoreError::Store`].
    pub fn cordon_host(
        &self,
        host: &HostId,
        lease: Duration,
        by: &Asker,
    ) -> Result<CordonWrite, JobStoreError> {
        let read = self.read_cordon(host)?;
        let decided = cordon(read.as_ref().map(|(held, _)| held), lease, by);
        let result = CordonWrite {
            already: decided.already,
            held_by_other: decided.held_by_other,
            cordon: decided.cordon,
        };
        if result.held_by_other {
            return Ok(result);
        }
        let txn = Txn::new().put_ttl(
            keys::cordon(host),
            encode_cordon(result.cordon.holder.as_ref())?,
            result.cordon.until,
            Self::cordon_cond(read.as_ref()),
        );
        self.commit_cordon(host, txn)?;
        Ok(result)
    }

    /// Lifts the cordon of `host` as `holder` asks: with a holder, only that holder's cordon is
    /// lifted and another's is left ([`UncordonWrite::held_by_other`]); with none, any cordon
    /// is. The row is deleted at the version read. Nothing is written when no cordon is in
    /// force or the cordon is left.
    ///
    /// # Errors
    /// As [`JobStore::cordon_host`].
    pub fn uncordon_host(
        &self,
        host: &HostId,
        holder: Option<&CordonHolder>,
    ) -> Result<UncordonWrite, JobStoreError> {
        let read = self.read_cordon(host)?;
        let decided = uncordon(read.as_ref().map(|(held, _)| held), holder);
        if decided.lifted {
            let txn = Txn::new().delete(keys::cordon(host), Self::cordon_cond(read.as_ref()));
            self.commit_cordon(host, txn)?;
        }
        Ok(UncordonWrite {
            lifted: decided.lifted,
            held_by_other: decided.held_by_other,
            cordon: read.map(|(held, _)| held).filter(|_| !decided.lifted),
        })
    }

    /// Every cordon in force, by host, each with the time it has left by the store's clock:
    /// what every pass leaves out and every tick puts on the registry. It fails closed, so one
    /// row it cannot read keeps no other host from placement: a live row that does not parse,
    /// or that a later build wrote, is a cordon in force with no holder this build can name,
    /// for the time the row has left (for ever, for a row with no expiry, which every
    /// placement's check finds in force too); a key that names no host is no host's cordon
    /// and is passed over.
    ///
    /// # Errors
    /// [`JobStoreError::Store`].
    pub fn cordons(&self) -> Result<BTreeMap<HostId, Cordon<Duration>>, JobStoreError> {
        let now = self.store.now()?;
        let rows = self.store.scan_prefix(keys::CORDON)?.into_iter();
        let in_force = |(key, row): (String, Versioned)| {
            let host = HostId::new(key.trim_start_matches(keys::CORDON)).ok()?;
            let holder = decode_cordon(&row.value).unwrap_or_default();
            let until = row.expires_at.map_or(Duration::MAX, |end| end.since(now));
            Some((host, Cordon { until, holder }))
        };
        Ok(rows.filter_map(in_force).collect())
    }

    /// Puts the cordon `held` on `host` for the time it has left, if no cordon is in force
    /// there, and never overwrites one: a server carries the cordons an earlier release kept
    /// only in its host bindings into the table once, at its start. `true` when the row was
    /// written, `false` when a cordon was already in force (the one there stands).
    ///
    /// # Errors
    /// [`JobStoreError::TooLarge`]; [`JobStoreError::Store`].
    pub fn restore_cordon(
        &self,
        host: &HostId,
        held: &Cordon<Duration>,
    ) -> Result<bool, JobStoreError> {
        let txn = Txn::new().put_ttl(
            keys::cordon(host),
            encode_cordon(held.holder.as_ref())?,
            held.until,
            Cond::Absent,
        );
        Ok(self.write(txn)?.is_ok())
    }

    /// Puts each of `held`, the cordons a server's host bindings brought back at its start,
    /// into the table as [`JobStore::restore_cordon`] does, once for the store: the first
    /// server to do so writes [`keys::CORDONS_RESTORED`] after them, and every later call
    /// writes nothing and answers `false`. An earlier release kept the cordons only in the
    /// bindings, so the first start of this one carries them over; after it, the table is the
    /// cordons' record, and a cordon lifted through any server is not written back from a
    /// binding that still holds it.
    ///
    /// # Errors
    /// [`JobStoreError::TooLarge`]; [`JobStoreError::Store`]. A failure before the marker is
    /// written leaves it absent, so the next start carries the cordons over again.
    pub fn restore_cordons(
        &self,
        held: &BTreeMap<HostId, Cordon<Duration>>,
    ) -> Result<bool, JobStoreError> {
        if self.store.get(keys::CORDONS_RESTORED)?.is_some() {
            return Ok(false);
        }
        for (host, cordon) in held {
            self.restore_cordon(host, cordon)?;
        }
        // Another server marking it first is the same end: the commit's answer is not needed.
        let mark = Txn::new().put(keys::CORDONS_RESTORED, Vec::new(), Cond::Absent);
        let _marked = self.write(mark)?;
        Ok(true)
    }

    /// Commits `txn`, a write of the cordon row of `host` at the version read: a conflict is
    /// [`JobStoreError::CordonConflict`].
    fn commit_cordon(&self, host: &HostId, txn: Txn) -> Result<(), JobStoreError> {
        self.write(txn)?
            .map(|_| ())
            .map_err(|_| JobStoreError::CordonConflict { host: host.clone() })
    }
}

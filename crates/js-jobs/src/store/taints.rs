//! The taint table (`taint/<host>`, [`HostTaint`]): the mark a host-isolation job's placement
//! puts on its host. [`JobStore::place`] writes it in the transaction that places the job and
//! books its allocation, with the row's absence as a precondition, so a host is tainted by the
//! first host job placed on it and never by a second, and a placement that fails writes no
//! taint. The row holds the taint until the caller keeps it where it keeps every host's taint
//! (the server's host binding), and then deletes it ([`JobStore::drop_taints`]): the row is
//! the taint's record between the two writes, so a crash between them loses nothing. The drop
//! deletes a row at the version it read and only while it names the job the caller kept, so a
//! taint put since is not lost. Nothing else deletes or rewrites a taint row, and the retention
//! tick does not read the table. Every placement's transaction refuses a host whose row is there
//! (`host_rows`), whichever server placed the host job.
//!
//! The table is additive: a store written before it has no `taint/` rows and reads and writes
//! its jobs and allocations as before; no `idx/` space names a taint, so the index backfill
//! neither reads nor writes one. There is one row per tainted host, so the table is read whole,
//! by a scan of `taint/`.

use std::collections::BTreeMap;

use js_state::{Cond, Store, Txn};
use js_types::HostId;

use super::JobStore;
use crate::codec::{DecodeError, decode_taint, encode_taint};
use crate::error::JobStoreError;
use crate::ids::{IdSource, JobId};
use crate::keys;
use crate::record::{HostTaint, Isolation, JobRecord};

/// `txn` with the taint of `host` by `job` put beside it when the job runs on the host itself
/// (the row must be absent), and with the row checked absent for a contained job: either way a
/// host that is tainted when the commit lands makes it conflict.
pub(super) fn with_taint(txn: Txn, job: &JobRecord, host: &HostId) -> Result<Txn, JobStoreError> {
    if job.spec.constraints.isolation != Isolation::Host {
        return Ok(txn.check(keys::taint(host), Cond::Absent));
    }
    let taint = HostTaint {
        job: job.id.clone(),
    };
    Ok(txn.put(keys::taint(host), encode_taint(&taint)?, Cond::Absent))
}

/// The host and taint the row `key` holds.
fn taint_at(key: &str, value: &[u8]) -> Result<(HostId, HostTaint), JobStoreError> {
    let host = HostId::new(key.trim_start_matches(keys::TAINT))
        .map_err(|_| JobStoreError::row(key, DecodeError::WrongKey))?;
    let taint = decode_taint(value).map_err(|why| JobStoreError::row(key, why))?;
    Ok((host, taint))
}

impl<S: Store, I: IdSource> JobStore<S, I> {
    /// Every taint a host-isolation placement put on a host, by host.
    ///
    /// # Errors
    /// [`JobStoreError::Corrupt`] when a row does not parse or is under a key that names no
    /// host; [`JobStoreError::UnknownRow`] when a later build wrote one; [`JobStoreError::Store`].
    pub fn host_taints(&self) -> Result<BTreeMap<HostId, HostTaint>, JobStoreError> {
        self.store
            .scan_prefix(keys::TAINT)?
            .into_iter()
            .map(|(key, row)| taint_at(&key, &row.value))
            .collect()
    }

    /// Deletes the taint rows of `kept`, each a host with the job whose placement tainted it,
    /// once the caller holds each host's taint elsewhere (the server's host binding): in one
    /// transaction, each row at the version read and only while it still names that job, so a
    /// taint a later host job put on the host after this one's ended stays. A host whose row is
    /// absent or names another job is passed over; no transaction is made when none is left. A
    /// commit that loses a row's version to another write reads the rows again.
    ///
    /// # Errors
    /// [`JobStoreError::Corrupt`] or [`JobStoreError::UnknownRow`] when a row cannot be read
    /// (it is not deleted); [`JobStoreError::Store`]: nothing was deleted.
    pub fn drop_taints(&self, kept: &[(HostId, JobId)]) -> Result<(), JobStoreError> {
        loop {
            let mut txn = Txn::new();
            let mut dropped = 0_usize;
            for (host, job) in kept {
                let key = keys::taint(host);
                let Some(row) = self.store.get(&key)? else {
                    continue;
                };
                if taint_at(&key, &row.value)?.1.job == *job {
                    txn = txn.delete(key, Cond::Version(row.version));
                    dropped += 1;
                }
            }
            if dropped == 0 || self.write(txn)?.is_ok() {
                return Ok(());
            }
        }
    }
}

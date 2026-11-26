//! The delivery of jobs' completion callbacks (`docs/design/job-retry-and-completion-callback.md`,
//! section 5.5), kept in the row `callback/<job id>` ([`CallbackRecord`]):
//!
//! - **Armed** in the transaction that records a job that asks for a callback
//!   ([`JobStore::insert_queued`]): the row takes the signing key, which the job row replaces by
//!   its digest, and a `webhook-id` minted from the store's id source.
//! - **Due** in the transaction that ends the job, whichever way it ends: the first call is due
//!   at the end, and the deadline is the end plus the one the job asked for, cut to its
//!   retention end. A pending row has one entry under `idx/callback/due/`, at the instant a claim
//!   may next take it.
//! - **Claimed** by one instance at a time ([`JobStore::claim_callbacks`]): each due row in its
//!   own claimed transaction, so two instances never hold one row, for
//!   [`CALLBACK_CLAIM_LEASE`](crate::callback::CALLBACK_CLAIM_LEASE); a claim not answered by then
//!   lapses and the row is handed out again. A row past its deadline is expired instead.
//! - **Answered** by the claim that holds it ([`JobStore::answer_callback`]): delivered,
//!   abandoned, or due again after a doubling wait. A final state erases the key.
//! - **Raised** at boot ([`JobStore::raise_at_boot`]): a pending row's deadline is raised to at
//!   least the boot plus [`BOOT_GRACE`](crate::BOOT_GRACE), and a row whose job row is gone,
//!   which only an older build's sweep leaves, is deleted.
//! - **Deleted** with its job's row by the retention sweep.

use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use js_state::{Claim, Cond, ResumeToken, Store, StoreTime, Txn, TxnRead, Version, Versioned};
use js_types::CallbackAnswer;

use super::JobStore;
use super::index::{Span, put_job};
use crate::callback::{CallbackClaim, CallbackKey, CallbackRecord, CallbackStatus};
use crate::codec::callback_fields::{decode_callback, encode_callback};
use crate::error::JobStoreError;
use crate::ids::{IdSource, InstanceId, JobId};
use crate::keys;
use crate::record::{JobRecord, JobSpec};
use crate::result::{BOOT_GRACE, SWEEP_BATCH};

/// A callback row as read: the record and its version.
type Read = (CallbackRecord, Version);

/// The alphabet of base64url (RFC 4648, section 5).
const BASE64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// `bits` in base64url without padding: 22 characters, as a `webhook-id` is written.
pub(super) fn base64url(bits: [u8; 16]) -> String {
    let mut out = String::with_capacity(22);
    let sextet = |value: u32| char::from(BASE64URL[(value & 63) as usize]);
    for chunk in bits.chunks(3) {
        let [a, b, c] = [0, 1, 2].map(|i| u32::from(chunk.get(i).copied().unwrap_or(0)));
        let group = (a << 16) | (b << 8) | c;
        let shifts = [18, 12, 6, 0];
        let len = chunk.len() + 1;
        out.extend(shifts[..len].iter().map(|shift| sextet(group >> shift)));
    }
    out
}

/// The callback the row `key` holds.
fn callback_at(key: &str, value: &[u8]) -> Result<CallbackRecord, JobStoreError> {
    decode_callback(value)
        .and_then(|callback| super::same_key(key, keys::callback(&callback.job), callback))
        .map_err(|why| JobStoreError::row(key, why))
}

/// The due entry `callback` calls for: one while it is pending, none otherwise.
fn entry_of(callback: &CallbackRecord) -> Option<String> {
    callback
        .due_at()
        .map(|at| keys::callback_due(at, &callback.job))
}

/// The row a due entry points at: `callback/<id>`; `None` for an entry that names no job.
fn row_of(entry: &str) -> Option<String> {
    let rest = entry.strip_prefix(keys::CALLBACK_DUE)?;
    let (_, id) = rest.split_once('/')?;
    JobId::parse(id).ok().map(|id| keys::callback(&id))
}

/// `txn` with a put of `after` on `cond`, its due entry moved from that of `before`.
fn put_callback(
    txn: Txn,
    before: Option<&CallbackRecord>,
    after: &CallbackRecord,
    cond: Cond,
) -> Result<Txn, JobStoreError> {
    let txn = txn.put(keys::callback(&after.job), encode_callback(after)?, cond);
    let gone = before.and_then(entry_of);
    let kept = entry_of(after);
    let txn = match gone.filter(|gone| Some(gone) != kept.as_ref()) {
        Some(gone) => txn.delete(gone, Cond::Any),
        None => txn,
    };
    Ok(match kept {
        Some(kept) => txn.put(kept, Vec::new(), Cond::Any),
        None => txn,
    })
}

/// `txn` with a delete of the callback row `key`, as `read` holds it, on `cond`, and of its due
/// entry when `read` is one this build reads.
fn delete_callback(txn: Txn, key: String, read: Option<&CallbackRecord>, cond: Cond) -> Txn {
    let txn = txn.delete(key, cond);
    match read.and_then(entry_of) {
        Some(entry) => txn.delete(entry, Cond::Any),
        None => txn,
    }
}

/// The decision of one claim of the row `key`, which the due entry `entry` points at, for `by`
/// at `now`, on what `read` shows: the transaction that claims (or expires) it, and the claim it
/// makes; `None` when the row is not claimable at `now`, or cannot be read.
fn decide_claim(
    read: &mut dyn TxnRead,
    key: &str,
    by: &InstanceId,
    now: StoreTime,
) -> Result<Option<(Txn, Option<CallbackClaim>)>, JobStoreError> {
    let row = read.get(key)?;
    let callback = row
        .as_ref()
        .and_then(|row| callback_at(key, &row.value).ok());
    // A claimed key with no live row never reaches a decision (the store answers `Held`); a
    // row that went, or that this build cannot read, is declined as one that is not due is.
    let (Some(row), Some(callback)) = (row, callback.filter(|callback| callback.claimable(now)))
    else {
        return Ok(None);
    };
    let (after, claim) = callback.clone().claimed(by, now);
    let txn = put_callback(
        Txn::new(),
        Some(&callback),
        &after,
        Cond::Version(row.version),
    )?;
    Ok(Some((txn, claim)))
}

impl<S: Store, I: IdSource> JobStore<S, I> {
    /// The transaction that records `record`, a new queued job, on `cond` absent, and the
    /// record as it is stored. A job that asks for a callback has its key replaced by the key's
    /// digest, and the same transaction writes its armed `callback/` row, with the key and a
    /// `webhook-id` minted from the store's id source, whatever the key held (a row of an earlier
    /// job of the id that an older build's sweep left).
    ///
    /// # Errors
    /// [`JobStoreError::CallbackKeyMissing`] when the callback holds its key's digest, not the
    /// key; [`JobStoreError::Mint`]; [`JobStoreError::TooLarge`].
    pub(super) fn recorded(&self, record: JobRecord) -> Result<(Txn, JobRecord), JobStoreError> {
        let Some(callback) = record.spec.callback.clone() else {
            let txn = put_job(Txn::new(), None, &record, Cond::Absent)?;
            return Ok((txn, record));
        };
        let CallbackKey::Key(key) = callback.key.clone() else {
            return Err(JobStoreError::CallbackKeyMissing { id: record.id });
        };
        let webhook_id = base64url(self.ids.mint()?);
        let armed = CallbackRecord::armed(record.id.clone(), &callback, key, webhook_id);
        let stored = JobRecord {
            spec: JobSpec {
                callback: Some(callback.digested()),
                ..record.spec
            },
            ..record
        };
        let txn = put_job(Txn::new(), None, &stored, Cond::Absent)?;
        let txn = put_callback(txn, None, &armed, Cond::Any)?;
        Ok((txn, stored))
    }

    /// `txn`, which writes `ended` (the job as it is written) at `now`, with the job's callback
    /// made due at its version read when that write ends the job: the first call due at `now`,
    /// and the deadline set. A write that does not end the job, a job without a callback, and
    /// one whose row is gone or not armed (none this build writes), is `txn` as it is.
    ///
    /// # Errors
    /// [`JobStoreError::UnknownRow`] or [`JobStoreError::Corrupt`] when the row cannot be
    /// read; [`JobStoreError::Store`].
    pub(super) fn due_callback(
        &self,
        txn: Txn,
        ended: &JobRecord,
        now: StoreTime,
    ) -> Result<Txn, JobStoreError> {
        if (!ended.state.is_ended()) | ended.spec.callback.is_none() {
            return Ok(txn);
        }
        let Some((armed, version)) = self.read_callback(&ended.id)? else {
            return Ok(txn);
        };
        let retention_ends = ended.retention_ends.unwrap_or(now);
        match armed.due(now, retention_ends) {
            Some(due) => put_callback(txn, Some(&armed), &due, Cond::Version(version)),
            None => Ok(txn),
        }
    }

    /// The callback row of the job `id`, with its version; `None` when there is none.
    fn read_callback(&self, id: &JobId) -> Result<Option<Read>, JobStoreError> {
        let key = keys::callback(id);
        self.store
            .get(&key)?
            .map(|row| Ok((callback_at(&key, &row.value)?, row.version)))
            .transpose()
    }

    /// Claims up to `limit` callbacks due at `now` for the instance `by`, each until
    /// [`CALLBACK_CLAIM_LEASE`](crate::callback::CALLBACK_CLAIM_LEASE) past `now`, so no other
    /// instance sends it meanwhile. The due entries are read in order of the instant each row
    /// became claimable; each row is claimed in a transaction of its own on its locked row
    /// (`FOR UPDATE SKIP LOCKED` on Postgres) and read again there, so a row another instance
    /// holds or has just claimed is left to it. A claim counts a try at `now`. A row whose
    /// deadline is at or before `now` is expired instead, its key erased, and not handed out.
    /// An entry whose row is gone, or no longer calls for it, is deleted. A row this build
    /// cannot read is left as it is and logged. Returns the claims, in the order of the entries.
    ///
    /// # Errors
    /// A store error when the due callbacks cannot be read or claimed; what was claimed before
    /// the failure stays claimed until its lease ends.
    pub fn claim_callbacks(
        &self,
        by: &InstanceId,
        now: StoreTime,
        limit: usize,
    ) -> Result<Vec<CallbackClaim>, JobStoreError> {
        let mut claims = Vec::new();
        let Some(page) = NonZeroUsize::new(limit.min(SWEEP_BATCH.get())) else {
            return Ok(claims);
        };
        let due = Span {
            prefix: keys::CALLBACK_DUE,
            below: Some(keys::callback_due_below(
                now.saturating_add(Duration::from_nanos(1)),
            )),
        };
        let mut resume: Option<ResumeToken> = None;
        loop {
            let (entries, next) = self.entry_page(&due, resume.as_ref(), page)?;
            for entry in entries {
                if claims.len() == limit {
                    return Ok(claims);
                }
                claims.extend(self.claim_entry(&entry, by, now)?);
            }
            match next {
                Some(token) => resume = Some(token),
                None => return Ok(claims),
            }
        }
    }

    /// One claim of [`JobStore::claim_callbacks`], of the row the due entry `entry` points at:
    /// the claim it made, or `None` when it made none.
    fn claim_entry(
        &self,
        entry: &str,
        by: &InstanceId,
        now: StoreTime,
    ) -> Result<Option<CallbackClaim>, JobStoreError> {
        // An entry that names no job is a later build's: it is left as it is.
        let Some(key) = row_of(entry) else {
            return Ok(None);
        };
        let Some(row) = self.store.get(&key)? else {
            self.drop_entry(entry, None)?;
            return Ok(None);
        };
        let Ok(callback) = callback_at(&key, &row.value) else {
            tracing::warn!(%key, "a callback row this build cannot read is left as it is");
            return Ok(None);
        };
        if entry_of(&callback).as_deref() != Some(entry) {
            self.drop_entry(entry, Some((&key, row.version)))?;
            return Ok(None);
        }
        let mut failed = None;
        let mut made = None;
        let claim = [key.clone()];
        let outcome =
            self.store.claim_decide(
                &claim,
                &mut |read| match decide_claim(read, &key, by, now) {
                    Ok(decided) => decided.map(|(txn, claim)| {
                        made = claim;
                        txn
                    }),
                    Err(error) => {
                        failed = Some(error);
                        None
                    }
                },
            );
        self.writes.fetch_add(1, Ordering::SeqCst);
        match (outcome?, failed) {
            (_, Some(error)) => Err(error),
            (Claim::Committed(_), None) => Ok(made),
            _ => Ok(None),
        }
    }

    /// Deletes the due entry `entry`, which its row (`at`, its key and the version read) no
    /// longer calls for, or which has no row: at that version, so a row written since keeps an
    /// entry it may call for again.
    fn drop_entry(&self, entry: &str, at: Option<(&str, Version)>) -> Result<(), JobStoreError> {
        let txn = match at {
            Some((key, version)) => Txn::new().check(key, Cond::Version(version)),
            None => Txn::new(),
        };
        // A row written since keeps its entry: the next claim decides it again.
        let _held = self.write(txn.delete(entry, Cond::Any))?;
        Ok(())
    }

    /// Records `answer` to the try `claim` made, at `now`, at the version of the callback's row
    /// read, when the row still holds `claim`: a delivery or an abandon ends the delivery and
    /// erases the key; a failure keeps its class, and the next try is due after a wait that
    /// starts at 5 s and doubles with each try to at most 10 minutes, never later than the
    /// deadline. A failure at or past the deadline expires the callback, and erases the key.
    ///
    /// # Errors
    /// [`JobStoreError::Stale`] when the callback's row no longer holds `claim` (it lapsed and
    /// another claim took the row, it was answered, or the row is gone) or changed before the
    /// commit, and nothing is written; [`JobStoreError::UnknownRow`] or
    /// [`JobStoreError::Corrupt`] when the row cannot be read; a store error when the answer
    /// cannot be written.
    pub fn answer_callback(
        &self,
        claim: CallbackClaim,
        answer: CallbackAnswer,
        now: StoreTime,
    ) -> Result<(), JobStoreError> {
        let stale = || JobStoreError::Stale {
            id: claim.job.clone(),
        };
        let (callback, version) = self.read_callback(&claim.job)?.ok_or_else(stale)?;
        if !callback.holds(&claim.held) {
            return Err(stale());
        }
        let answered = callback.clone().answered(answer, now);
        let txn = put_callback(
            Txn::new(),
            Some(&callback),
            &answered,
            Cond::Version(version),
        )?;
        self.commit(&claim.job, txn).map(|_| ())
    }

    /// Where the delivery of the callback of the job `id` stands. `None` when the job has no
    /// callback, or when its row is gone (none this build deletes before its job's).
    ///
    /// # Errors
    /// [`JobStoreError::NotFound`] when there is no job `id`; [`JobStoreError::UnknownRow`]
    /// or [`JobStoreError::Corrupt`] when the job's row or its callback's cannot be read; a store
    /// error when a row cannot be read.
    pub fn callback_status(&self, id: &JobId) -> Result<Option<CallbackStatus>, JobStoreError> {
        let (job, _) = self.job_row(id)?;
        if job.spec.callback.is_none() {
            return Ok(None);
        }
        Ok(self
            .read_callback(id)?
            .map(|(callback, _)| callback.status()))
    }

    /// `txn`, which deletes the rows of the jobs `jobs` past their retention end, with the
    /// callback row of each that has a callback deleted too, and its due entry. A row this build
    /// cannot read is deleted with its job all the same.
    pub(super) fn drop_callbacks(
        &self,
        txn: Txn,
        jobs: &[JobRecord],
    ) -> Result<Txn, JobStoreError> {
        let mut txn = txn;
        for job in jobs.iter().filter(|job| job.spec.callback.is_some()) {
            let key = keys::callback(&job.id);
            let read = self.store.get(&key)?;
            let read = read.and_then(|row| callback_at(&key, &row.value).ok());
            txn = delete_callback(txn, key, read.as_ref(), Cond::Any);
        }
        Ok(txn)
    }

    /// The boot's raise of the callback rows (the module docs), each in a transaction of its own
    /// at the version read: a pending row's deadline is raised to at least `boot` plus
    /// [`BOOT_GRACE`], and a row whose job is not among `jobs` (the keys of every job row the
    /// boot read) is deleted on its job row still being absent. A row another writer changed in
    /// between is read again, with its job row, and decided again. A row a later build wrote is
    /// left as it is. Returns how many rows it wrote, and how many it read again.
    pub(super) fn raise_callbacks_at_boot(
        &self,
        boot: StoreTime,
        jobs: &BTreeSet<String>,
    ) -> Result<(usize, usize), JobStoreError> {
        let floor = boot.saturating_add(BOOT_GRACE);
        let mut due = Vec::new();
        for (key, row) in self.store.scan_prefix(keys::CALLBACK)? {
            let job = key.replacen(keys::CALLBACK, keys::JOB, 1);
            due.extend(raised_callback(key, &row, floor, jobs.contains(&job))?);
        }
        let (mut written, mut again) = (0, 0);
        while !due.is_empty() {
            let (keys, txns): (Vec<String>, Vec<Txn>) = due.into_iter().unzip();
            let held = self.write_each(txns)?;
            due = Vec::new();
            for (key, held) in keys.into_iter().zip(held) {
                if held {
                    written += 1;
                    continue;
                }
                again += 1;
                let job = self
                    .store
                    .get(&key.replacen(keys::CALLBACK, keys::JOB, 1))?;
                if let Some(row) = self.store.get(&key)? {
                    due.extend(raised_callback(key, &row, floor, job.is_some())?);
                }
            }
        }
        Ok((written, again))
    }
}

/// The transaction that raises the callback row `row` at `key` to `floor`, or deletes it when
/// its job row is not `held` (on that row being absent still), at the version read, with its
/// key; `None` when the row needs neither or is one a later build wrote.
fn raised_callback(
    key: String,
    row: &Versioned,
    floor: StoreTime,
    held: bool,
) -> Result<Option<(String, Txn)>, JobStoreError> {
    let callback = match callback_at(&key, &row.value) {
        Ok(callback) => callback,
        Err(JobStoreError::UnknownRow { .. }) => return Ok(None),
        Err(error) => return Err(error),
    };
    let cond = Cond::Version(row.version);
    if !held {
        let job = keys::job(&callback.job);
        let txn = Txn::new().check(job, Cond::Absent);
        let txn = delete_callback(txn, key.clone(), Some(&callback), cond);
        return Ok(Some((key, txn)));
    }
    callback
        .raised_at_boot(floor)
        .map(|raised| {
            Ok((
                key.clone(),
                put_callback(Txn::new(), Some(&callback), &raised, cond)?,
            ))
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::base64url;

    /// Asserts base64url of 16 bytes against the reference encodings of RFC 4648's alphabet:
    /// all zeros, all ones, and bytes that use the last two characters (`-` and `_`), which
    /// standard base64 writes `+` and `/`. Catches the standard alphabet, padding written, and
    /// the last byte's two characters dropped or extended.
    #[test]
    fn a_webhook_id_is_base64url_without_padding() {
        assert_eq!(base64url([0; 16]), "AAAAAAAAAAAAAAAAAAAAAA");
        assert_eq!(base64url([0xff; 16]), "_____________________w");
        let mut bits = [0xfb; 16];
        bits[15] = 0xef;
        assert_eq!(base64url(bits), "-_v7-_v7-_v7-_v7-_v77w");
        let counted: [u8; 16] = std::array::from_fn(|i| i as u8);
        assert_eq!(base64url(counted), "AAECAwQFBgcICQoLDA0ODw");
    }
}

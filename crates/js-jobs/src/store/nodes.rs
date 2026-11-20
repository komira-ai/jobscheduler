//! The presence table (`node/<host>`, [`NodeRecord`]): a host's session and what its hello and
//! beats left, in the store, so every server that shares the store judges a beat on the same
//! row and no server holds a host's session of its own (design `multi-instance.md`, section 0
//! and G15).
//!
//! A hello writes the row at its absence or at the version read, with the next epoch; a beat
//! writes it at the version read, guarded by its (epoch, sequence). A write that loses its
//! version to another server reads the row again and judges the beat or hello afresh, so two
//! servers that take beats of one host at once apply each (epoch, sequence) once and never
//! move the row back. Every time the table holds or compares is the store's clock
//! ([`Store::now`]), read by the call that writes or compares it; nothing here reads a system
//! clock.
//!
//! The table is additive: a store written before it has no `node/` rows, and no `idx/` space
//! names one, so the index backfill and the retention tick neither read nor write it.

use std::collections::BTreeMap;
use std::time::Duration;

use js_state::{Cond, Store, StoreTime, Txn, Version, Versioned};
use js_types::HostId;

use super::JobStore;
use crate::codec::DecodeError;
use crate::codec::node_fields::{decode_node, encode_node};
use crate::error::JobStoreError;
use crate::ids::IdSource;
use crate::keys;
use crate::node::{AppliedBeat, Beat, NodeHello, NodeRecord, NodeRow, NodeState};

/// The host's record the row `key` holds, which must name the host of its key.
fn node_at(key: &str, row: &Versioned) -> Result<NodeRow, JobStoreError> {
    decode_node(&row.value)
        .and_then(|record| {
            (keys::node(&record.host) == key)
                .then_some(record)
                .ok_or(DecodeError::WrongKey)
        })
        .map(|record| NodeRow {
            record,
            version: row.version,
        })
        .map_err(|why| JobStoreError::row(key, why))
}

/// The condition a write of a host's row carries, given the row read.
fn node_cond(read: Option<&NodeRow>) -> Cond {
    read.map_or(Cond::Absent, |row| Cond::Version(row.version))
}

/// The beat that `last` (the row's last applied beat) and a beat of `epoch` with `seq` leave
/// applied: within one epoch the highest sequence applied stays, and a beat with none leaves
/// the one there.
fn applied(last: Option<AppliedBeat>, epoch: u64, seq: Option<u64>) -> AppliedBeat {
    let kept = last
        .filter(|last| last.epoch == epoch)
        .and_then(|last| last.seq);
    AppliedBeat {
        epoch,
        seq: seq.max(kept),
    }
}

/// The answer to a beat of `epoch` with `seq` that `record` does not take, made from the row;
/// `None` when the beat is to be applied.
fn refusal(record: &NodeRecord, epoch: u64, seq: Option<u64>) -> Option<fn(NodeRow) -> Beat> {
    if record.epoch > epoch {
        return Some(Beat::StaleEpoch);
    }
    if record.epoch < epoch {
        return Some(|row| Beat::NoSession(Some(row)));
    }
    let applied_seq = record
        .last_beat
        .filter(|last| last.epoch == epoch)
        .and_then(|last| last.seq);
    match (seq, applied_seq) {
        (Some(seq), Some(applied_seq)) if seq <= applied_seq => Some(Beat::Superseded),
        _ => None,
    }
}

impl<S: Store, I: IdSource> JobStore<S, I> {
    /// The row of `host` as it stands, with its version, or `None` when it has none.
    ///
    /// # Errors
    /// [`JobStoreError::UnknownRow`] when a later build wrote the row,
    /// [`JobStoreError::Corrupt`] when it does not parse or names another host;
    /// [`JobStoreError::Store`].
    pub fn read_node(&self, host: &HostId) -> Result<Option<NodeRow>, JobStoreError> {
        let key = keys::node(host);
        self.store
            .get(&key)?
            .map(|row| node_at(&key, &row))
            .transpose()
    }

    /// Records the hello of `host` as its row: the next epoch (one more than the row's, 1 for
    /// a host with none), what `hello` announces, no load, the first listing pending, no beat
    /// applied, no container missed, and `heard_at` the store's time now; the row's listing-clean
    /// flag and the fields a later build wrote are kept. The row is written at its absence or
    /// at the version read; a write that loses to another server's reads the row again and
    /// writes the epoch after that one's, so two hellos never take one epoch. Returns the row
    /// written, with its version.
    ///
    /// # Errors
    /// [`JobStoreError::UnknownRow`] or [`JobStoreError::Corrupt`] when the row there cannot be
    /// read (it is not written over: [`JobStore::forget_node`] deletes it at its version);
    /// [`JobStoreError::TooLarge`]; [`JobStoreError::Store`], its clock included. Nothing is
    /// written.
    pub fn hello_node(&self, host: &HostId, hello: &NodeHello) -> Result<NodeRow, JobStoreError> {
        loop {
            let read = self.read_node(host)?;
            let now = self.store.now()?;
            let before = read.as_ref().map(|row| &row.record);
            let record = NodeRecord {
                host: host.clone(),
                epoch: before.map_or(1, |before| before.epoch.saturating_add(1)),
                session: hello.session,
                pool: hello.pool.clone(),
                claim: hello.claim.clone(),
                shape: hello.shape.clone(),
                labels: hello.labels.clone(),
                agent_process: hello.agent_process.clone(),
                state: NodeState {
                    phase: hello.phase,
                    load: None,
                    first_listing_pending: true,
                    last_listing_clean: before.is_none_or(|before| before.state.last_listing_clean),
                    missing_in_a_row: BTreeMap::new(),
                },
                last_beat: None,
                heard_at: now,
                carried: before
                    .map(|before| before.carried.clone())
                    .unwrap_or_default(),
            };
            if let Some(row) = self.put_node(record, read.as_ref())? {
                return Ok(row);
            }
        }
    }

    /// Applies a beat of `host` in the session of `epoch`, with the sequence `seq` its agent
    /// gave it (`None` from an agent that sends none), if the row's session is that epoch and
    /// no beat of a sequence at least as new was applied in it: the row's state becomes what
    /// `update` makes of the row as read, its last applied beat this one (within the epoch the
    /// highest sequence stays), and `heard_at` the store's time now; the session, the hello's
    /// fields and the fields a later build wrote are kept. The row is written at the version
    /// read; a write that loses to another server's reads the row again and judges the beat on
    /// it, calling `update` again, so of two servers that apply one (epoch, sequence) at once
    /// one does and the other's is [`Beat::Superseded`]. A beat with no sequence is applied,
    /// one write after another, whatever its age.
    ///
    /// Every answer but [`Beat::Applied`] writes nothing: [`Beat::StaleEpoch`] for an epoch
    /// older than the row's, [`Beat::NoSession`] for no row or an epoch newer than the row's,
    /// [`Beat::Superseded`] for a sequence no newer than the one applied in the epoch.
    ///
    /// # Errors
    /// As [`JobStore::hello_node`].
    pub fn beat_node(
        &self,
        host: &HostId,
        epoch: u64,
        seq: Option<u64>,
        mut update: impl FnMut(&NodeRecord) -> NodeState,
    ) -> Result<Beat, JobStoreError> {
        loop {
            let Some(read) = self.read_node(host)? else {
                return Ok(Beat::NoSession(None));
            };
            if let Some(answer) = refusal(&read.record, epoch, seq) {
                return Ok(answer(read));
            }
            let now = self.store.now()?;
            let record = NodeRecord {
                state: update(&read.record),
                last_beat: Some(applied(read.record.last_beat, epoch, seq)),
                heard_at: now,
                ..read.record.clone()
            };
            if let Some(row) = self.put_node(record, Some(&read))? {
                return Ok(Beat::Applied(row));
            }
        }
    }

    /// Writes the row `row` again with its state replaced by `state`, at the version `row` was
    /// read or written at: the way a beat, after the writes of its listing (made through
    /// [`JobStore::guarded`] at that version), records in its own write that the first listing
    /// is applied, whether the listing was clean, and the missing counts. The epoch, session,
    /// the hello's fields (the agent process included), the last beat, `heard_at` and the
    /// fields a later build wrote are kept. Returns the row as written, with its new version,
    /// or `None` when the row is not at that version any more (another beat or hello was
    /// applied, or the row was forgotten) and nothing was written.
    ///
    /// # Errors
    /// [`JobStoreError::TooLarge`]; [`JobStoreError::Store`]. Nothing is written.
    pub fn settle_node(
        &self,
        row: &NodeRow,
        state: NodeState,
    ) -> Result<Option<NodeRow>, JobStoreError> {
        let record = NodeRecord {
            state,
            ..row.record.clone()
        };
        self.put_node(record, Some(row))
    }

    /// Every host's row heard within `grace` by the store's clock (its `heard_at` at most
    /// `grace` before the store's time now), with its version, in host order. A row this
    /// build cannot read (a later build's, one that does not parse, or one under another
    /// host's key) is left out and logged: a host whose row cannot be read is not live.
    ///
    /// # Errors
    /// [`JobStoreError::Store`], its clock included.
    pub fn live_nodes(&self, grace: Duration) -> Result<Vec<NodeRow>, JobStoreError> {
        let now = self.store.now()?;
        let mut live = Vec::new();
        for (key, row) in self.store.scan_prefix(keys::NODE)? {
            match node_at(&key, &row) {
                Ok(row) if heard_within(row.record.heard_at, grace, now) => live.push(row),
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(%key, %error, "a node row this build cannot read is not live");
                }
            }
        }
        Ok(live)
    }

    /// Deletes the row of `host` if it is still at `at`, whatever it holds (a row this build
    /// cannot read included): `true` when it was deleted, `false` when the row is absent or
    /// was written since (a hello or a beat landed), and nothing was deleted.
    ///
    /// # Errors
    /// [`JobStoreError::Store`].
    pub fn forget_node(&self, host: &HostId, at: Version) -> Result<bool, JobStoreError> {
        let txn = Txn::new().delete(keys::node(host), Cond::Version(at));
        let deleted = self.write_node(txn)?.is_ok();
        if deleted {
            self.note_node(host, None);
        }
        Ok(deleted)
    }

    /// Writes `record` as its host's row at the absence or version of `read`: the row as
    /// written, or `None` when the row changed since it was read.
    fn put_node(
        &self,
        record: NodeRecord,
        read: Option<&NodeRow>,
    ) -> Result<Option<NodeRow>, JobStoreError> {
        let txn = Txn::new().put(
            keys::node(&record.host),
            encode_node(&record)?,
            node_cond(read),
        );
        // The transaction writes the one row, so its revision is the row's new version.
        Ok(self.write_node(txn)?.ok().map(|commit| {
            let clean = record.state.last_listing_clean;
            self.note_node(&record.host, Some(clean));
            NodeRow {
                record,
                version: Version(commit.revision.0),
            }
        }))
    }
}

/// Whether a row heard at `heard_at` is heard within `grace` of `now`.
fn heard_within(heard_at: StoreTime, grace: Duration, now: StoreTime) -> bool {
    heard_at.saturating_add(grace) >= now
}

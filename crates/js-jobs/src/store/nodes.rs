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

use js_state::{Claim, Cond, Store, StoreTime, Txn, TxnRead, Version, Versioned};
use js_types::HostId;

use super::host_rows::binding_tainted;
use super::{JobStore, booked_in};
use crate::codec::DecodeError;
use crate::codec::node_fields::{decode_node, encode_node};
use crate::error::JobStoreError;
use crate::ids::IdSource;
use crate::keys;
use crate::node::{AppliedBeat, Beat, Kept, NodeHello, NodeRecord, NodeRow, NodeState, Retire};

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
            let record = hello_record(host, hello, read.as_ref(), now);
            if let Some(row) = self.put_node(record, read.as_ref())? {
                return Ok(row);
            }
        }
    }

    /// [`JobStore::hello_node`] for a host of a pool that holds at most `cap` hosts. A host that
    /// has a row is not new to the pool and is written as there. For a host that has none, the
    /// pool's admission row `pool/<pool>` is read first, then the pool's rows are counted (every
    /// row this build can read that names the pool, heard lately or not: a row is gone only
    /// once [`JobStore::forget_node`] or [`JobStore::forget_silent_nodes`] deleted it); when
    /// they are `cap` or more the hello is refused, and otherwise the host's row is put at its
    /// absence and the admission row at the version read, in one transaction. Two hellos of new
    /// hosts of one pool thus write one row at one version: the second's commit conflicts, and
    /// it counts again with the first's row there, so a pool one host from its cap admits one.
    ///
    /// # Errors
    /// [`JobStoreError::PoolFull`] when the pool holds `cap` hosts or more; as
    /// [`JobStore::hello_node`]. Nothing is written.
    pub fn hello_node_in_pool(
        &self,
        host: &HostId,
        hello: &NodeHello,
        cap: usize,
    ) -> Result<NodeRow, JobStoreError> {
        let pool_key = keys::pool(&hello.pool);
        loop {
            let read = self.read_node(host)?;
            if read.is_some() {
                return self.hello_node(host, hello);
            }
            // The admission row before the count: a hello that commits after this read moves
            // the row, and one that committed before it is counted.
            let admission = self.store.get(&pool_key)?;
            let held = self.rows_of_pool(&hello.pool)?;
            if held >= cap {
                return Err(JobStoreError::PoolFull {
                    host: host.clone(),
                    pool: hello.pool.clone(),
                });
            }
            let now = self.store.now()?;
            let record = hello_record(host, hello, None, now);
            let cond = admission.map_or(Cond::Absent, |row| Cond::Version(row.version));
            // The host's row is the transaction's last write, so its version is the commit's
            // revision, the highest one the transaction took.
            let txn = Txn::new().put(pool_key.clone(), Vec::new(), cond).put(
                keys::node(host),
                encode_node(&record)?,
                Cond::Absent,
            );
            // A write of the pool's row besides the node row: a write of the tables, counted.
            if let Ok(commit) = self.write(txn)? {
                let version = Version(commit.revision.0);
                self.note_node(host, Some(record.state.last_listing_clean));
                return Ok(NodeRow { record, version });
            }
        }
    }

    /// Whether a hello of `host` into the pool `pool`, which holds at most `cap` hosts, would be
    /// admitted by [`JobStore::hello_node_in_pool`] as the rows stand now: `host` has a row (it
    /// is not new to the pool), or the pool's rows are fewer than `cap`. A read that writes
    /// nothing, for judging a hello before anything of it is written; the hello's own write
    /// decides it again, so two hellos that both read room still admit one.
    ///
    /// # Errors
    /// [`JobStoreError::Store`].
    pub fn pool_admits(
        &self,
        host: &HostId,
        pool: &str,
        cap: usize,
    ) -> Result<bool, JobStoreError> {
        if self.read_node(host)?.is_some() {
            return Ok(true);
        }
        Ok(self.rows_of_pool(pool)? < cap)
    }

    /// How many rows this build can read name the pool `pool`.
    fn rows_of_pool(&self, pool: &str) -> Result<usize, JobStoreError> {
        let rows = self.store.scan_prefix(keys::NODE)?;
        let rows = rows.iter().filter_map(|(key, row)| node_at(key, row).ok());
        Ok(rows.filter(|row| row.record.pool == pool).count())
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

    /// Every host's row this build can read, heard lately or not, in host order, with the
    /// store's time when they were read. A row this build cannot read is left out and logged.
    ///
    /// # Errors
    /// [`JobStoreError::Store`], its clock included.
    pub fn nodes(&self) -> Result<(StoreTime, Vec<NodeRow>), JobStoreError> {
        let now = self.store.now()?;
        let mut rows = Vec::new();
        for (key, row) in self.store.scan_prefix(keys::NODE)? {
            match node_at(&key, &row) {
                Ok(row) => rows.push(row),
                Err(error) => {
                    tracing::warn!(%key, %error, "a node row this build cannot read is skipped");
                }
            }
        }
        Ok((now, rows))
    }

    /// Deletes the row of each host silent for longer than `silent_for` by the store's clock
    /// (its `heard_at` more than `silent_for` before the store's time now) that holds nothing:
    /// no live allocation, no `taint/<host>` row, no binding that holds a taint, no cordon and
    /// no scale-down mark, as the registry forgets a host lost past its retention. Each host is
    /// decided in a transaction of its own that claims its row and reads the others; the row is
    /// deleted at the version the scan read, so a hello or beat that lands in between keeps it.
    /// Returns the hosts whose rows were deleted, in host order. A host whose decision reads a
    /// row this build cannot read (its binding, or an allocation row) is logged and its row is
    /// kept, and the other hosts are decided as usual.
    ///
    /// # Errors
    /// [`JobStoreError::Store`], its clock included: the hosts after the failure are left to the
    /// next call.
    pub fn forget_silent_nodes(&self, silent_for: Duration) -> Result<Vec<HostId>, JobStoreError> {
        let (now, rows) = self.nodes()?;
        let mut forgotten = Vec::new();
        for row in rows {
            if heard_within(row.record.heard_at, silent_for, now) {
                continue;
            }
            let host = row.record.host.clone();
            let claim = [keys::node(&host)];
            let mut failed = None;
            let outcome = self
                .store
                .claim_decide(
                    &claim,
                    &mut |read| match holds_nothing(read, &host, row.version) {
                        Ok(true) => {
                            Some(Txn::new().delete(keys::node(&host), Cond::Version(row.version)))
                        }
                        Ok(false) => None,
                        Err(error) => {
                            failed = Some(error);
                            None
                        }
                    },
                )?;
            if let Some(error) = failed {
                tracing::warn!(%host, %error, "a silent host's row is kept: a row it holds cannot be read");
                continue;
            }
            if matches!(outcome, Claim::Committed(_)) {
                self.note_node(&host, None);
                forgotten.push(host);
            }
        }
        Ok(forgotten)
    }

    /// Writes the row `row` again with its state replaced by `state`, at the version `row` was
    /// read or written at: the way a beat, after the writes of its listing (made through
    /// [`JobStore::guarded`] at that version), records in its own write that the first listing
    /// is applied, whether the listing was clean, and the missing counts. The epoch, session,
    /// the hello's fields (the agent process and version included), the last beat, `heard_at` and the
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
        self.live_nodes_reusing(grace, &BTreeMap::new())
    }

    /// [`JobStore::live_nodes`], taking each row the store still holds at the version it has in
    /// `known` (rows read before, by host) from there, so that only a row written since is
    /// decoded. Whether a row is live is judged afresh either way.
    ///
    /// # Errors
    /// As [`JobStore::live_nodes`].
    pub fn live_nodes_reusing(
        &self,
        grace: Duration,
        known: &BTreeMap<HostId, NodeRow>,
    ) -> Result<Vec<NodeRow>, JobStoreError> {
        let now = self.store.now()?;
        let mut live = Vec::new();
        for (key, row) in self.store.scan_prefix(keys::NODE)? {
            let host = key
                .strip_prefix(keys::NODE)
                .and_then(|host| HostId::new(host).ok());
            let kept = host.and_then(|host| known.get(&host));
            let kept = kept.filter(|kept| kept.version == row.version).cloned();
            match kept.map_or_else(|| node_at(&key, &row), Ok) {
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

    /// Deletes the row of `host`, which the operator retires, when the host is silent and holds
    /// nothing: its `heard_at` is more than `grace` before the store's time now (so it is not
    /// live), and it holds no live allocation, no taint (its own row or its binding's), no
    /// cordon and no scale-down mark, as [`JobStore::forget_silent_nodes`] judges a row it
    /// forgets. It is that forget for one host, without waiting out the lost-host retention.
    /// The host's row is claimed and deleted at the version read; a hello or beat that lands
    /// between the read and the delete moves the row, and the row is read and judged again (so
    /// a host that came back is [`Kept::Live`]), as is a row another server deleted first
    /// ([`Retire::Absent`]).
    ///
    /// # Errors
    /// [`JobStoreError::UnknownRow`] or [`JobStoreError::Corrupt`] when the host's row, its
    /// binding or an allocation row it reads cannot be read; [`JobStoreError::Store`], its clock
    /// included. Nothing is deleted.
    pub fn retire_node(&self, host: &HostId, grace: Duration) -> Result<Retire, JobStoreError> {
        loop {
            let Some(row) = self.read_node(host)? else {
                return Ok(Retire::Absent);
            };
            let now = self.store.now()?;
            if heard_within(row.record.heard_at, grace, now) {
                return Ok(Retire::Refused(Kept::Live));
            }
            let claim = [keys::node(host)];
            let mut refused = None;
            let outcome = self
                .store
                .claim_decide(&claim, &mut |read| match kept_by(read, host).transpose() {
                    None => Some(Txn::new().delete(keys::node(host), Cond::Version(row.version))),
                    Some(kept) => {
                        refused = Some(kept);
                        None
                    }
                })?;
            match (outcome, refused) {
                (Claim::Committed(_), _) => {
                    self.note_node(host, None);
                    return Ok(Retire::Retired);
                }
                (Claim::Declined, Some(kept)) => return kept.map(Retire::Refused),
                // The row moved (a hello or a beat) or went (another server deleted it) since
                // it was read: it is read and judged again.
                _ => {}
            }
        }
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

/// The row of `host` that a hello writes over `before` (the row read, if any) at the store's
/// time `now`: the next epoch, what `hello` announces, no load, the first listing pending, no
/// beat applied, no container missed; the listing-clean flag and the fields a later build wrote
/// are kept.
fn hello_record(
    host: &HostId,
    hello: &NodeHello,
    before: Option<&NodeRow>,
    now: StoreTime,
) -> NodeRecord {
    let before = before.map(|row| &row.record);
    NodeRecord {
        host: host.clone(),
        epoch: before.map_or(1, |before| before.epoch.saturating_add(1)),
        session: hello.session,
        pool: hello.pool.clone(),
        claim: hello.claim.clone(),
        shape: hello.shape.clone(),
        labels: hello.labels.clone(),
        agent_process: hello.agent_process.clone(),
        agent_version: hello.agent_version.clone(),
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
    }
}

/// Whether the rows `read` shows let the row of `host`, read at `version`, be forgotten: it is
/// still at that version, and the host holds nothing ([`kept_by`]).
fn holds_nothing(
    read: &mut dyn TxnRead,
    host: &HostId,
    version: Version,
) -> Result<bool, JobStoreError> {
    let same = read
        .get(&keys::node(host))?
        .is_some_and(|row| row.version == version);
    Ok(same & kept_by(read, host)?.is_none())
}

/// What the rows `read` shows `host` still holds or is owed, which keeps its row: a live
/// allocation first, then a taint (its own row or its binding's), a cordon and a scale-down
/// mark; `None` when it holds none of them.
fn kept_by(read: &mut dyn TxnRead, host: &HostId) -> Result<Option<Kept>, JobStoreError> {
    let binding_key = keys::binding(host);
    let binding_taint = match read.get(&binding_key)? {
        Some(row) => binding_tainted(&binding_key, &row.value)?,
        None => false,
    };
    let taint_row = read.get(&keys::taint(host))?.is_some();
    let cordoned = read.get(&keys::cordon(host))?.is_some();
    let marked = read.get(&keys::hostmark(host))?.is_some();
    let booked = booked_in(read, host)?;
    let reasons = [
        (booked.allocations > 0, Kept::HoldsJobs),
        (binding_taint | taint_row, Kept::Tainted),
        (cordoned, Kept::Cordoned),
        (marked, Kept::Marked),
    ];
    Ok(reasons
        .into_iter()
        .find_map(|(holds, kept)| holds.then_some(kept)))
}

/// Whether a row heard at `heard_at` is heard within `grace` of `now`.
fn heard_within(heard_at: StoreTime, grace: Duration, now: StoreTime) -> bool {
    heard_at.saturating_add(grace) >= now
}

//! What a start reads of the state store, and what a look at one operation or at the queue
//! reads: the unfinished operations, each with its lease, loss count, fate, parcel and size
//! key, and the first ids to hand out.
//!
//! A `lease/` or `op/` row that holds a command this binary does not know was written by a
//! later binary: a scan skips it, and the rows it can read load. A start ([`Rows::load`])
//! logs the key of each row it skips; the scans a request makes skip them silently, so one
//! leftover row does not fill the log at request rate. A row that is malformed, or that
//! holds a known command of the wrong kind, still refuses the scan, and so does a single
//! row a write depends on, whatever it holds.
//!
//! A skipped `lease/` row leaves its operation with no lease: it loads queued, the
//! scheduler can grant it again, and the action may run twice. Nothing names the skipped
//! row's lease again, so the row is never deleted.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

use js_sched::{Fate, Recovered, RecoveredLease, Request};
use js_state::{Version, Versioned};
use js_types::sched::{LeaseGrant, OperationId, WaiterId, WorkerId};

use crate::cordon::StoredCordon;
use crate::{Rows, StateError, fate, keys, number, opkey};

/// What a store holds of the scheduler, as a start reads it.
#[derive(Debug, PartialEq, Eq)]
pub struct Stored {
    /// The first operation id no frontend has reserved.
    pub next_operation: u64,
    /// The first lease sequence no frontend has reserved.
    pub next_seq: u64,
    /// Every unfinished operation, in id order.
    pub operations: Vec<StoredOperation>,
    /// Every node an operator placed, in node-name order.
    pub cordons: Vec<(WorkerId, StoredCordon)>,
}

/// An unfinished operation of a store.
#[derive(Debug, PartialEq, Eq)]
pub struct StoredOperation {
    /// Its id.
    pub operation: OperationId,
    /// What it runs.
    pub request: Request,
    /// The committed lease it holds.
    pub lease: Option<LeaseGrant>,
    /// How many of its leases were lost.
    pub losses: u32,
    /// What its kills for memory and other faults changed, if it had any.
    pub fate: Option<Fate>,
    /// What the embedding server kept with it when it was submitted ([`crate::Ext`]), if
    /// the writer kept anything.
    pub parcel: Option<Vec<u8>>,
    /// Its size key, if the writer named one (`opkey/` row) and size keys are read.
    pub size_key: Option<u64>,
}

impl StoredOperation {
    /// This operation as the scheduler takes it up, held for `waiter`.
    #[must_use]
    pub fn recovered(self, waiter: WaiterId) -> Recovered {
        Recovered {
            operation: self.operation,
            request: self.request,
            waiter,
            lease: self.lease.map(|grant| RecoveredLease {
                lease: grant.lease,
                worker: grant.worker,
                // The store does not keep the admitted cost yet, so a recovered lease is
                // charged its request (the scheduler's meaning of `None`).
                cost: None,
            }),
            losses: self.losses,
            fate: self.fate,
        }
    }
}

/// The loss count a `loss/` row holds: eight bytes, big-endian, that fit a count.
fn loss_count(key: &str, row: &Versioned) -> Result<u32, StateError> {
    u32::try_from(number(key, row)?).map_err(|_| StateError::corrupt(key, "not a loss count"))
}

/// The fate a `fate/` row holds.
fn fate_row(key: &str, row: &Versioned) -> Result<Fate, StateError> {
    fate::decode(&row.value).map_err(|why| StateError::corrupt(key, why))
}

/// The size key an `opkey/` row holds.
fn size_key_row(key: &str, row: &Versioned) -> Result<u64, StateError> {
    opkey::decode(&row.value).map_err(|why| StateError::corrupt(key, why))
}

impl Rows {
    /// Whether the `opkey/` rows are read. A store that never had the learned sizes on
    /// holds none, and with them off a row an earlier run left is neither loaded nor
    /// allowed to fail a start. The transaction that ends an operation (it comes with the
    /// grant and finish rows) will delete its `opkey/` row whatever this says, so rows an
    /// earlier run left are cleaned up as their operations end. Reading is on until this says otherwise.
    pub fn set_read_size_keys(&self, read: bool) {
        self.size_keys.store(read, Ordering::SeqCst);
    }

    /// What the store holds: the unfinished operations, each with its lease, and the
    /// first ids to hand out.
    ///
    /// # Errors
    /// The store cannot be read, or a row is not one the scheduler wrote.
    pub fn load(&self) -> Result<Stored, StateError> {
        let mut leases = BTreeMap::new();
        for (key, row) in self.scan_known_logged(keys::LEASE)? {
            let grant = self.lease_row(&key, &row)?;
            leases.insert(grant.operation, grant);
        }
        let mut losses = BTreeMap::new();
        for (key, row) in self.store.scan_prefix(keys::LOSS)? {
            let operation = keys::operation_of(keys::LOSS, &key)?;
            losses.insert(operation, loss_count(&key, &row)?);
        }
        let mut fates = BTreeMap::new();
        for (key, row) in self.store.scan_prefix(keys::FATE)? {
            let operation = keys::operation_of(keys::FATE, &key)?;
            fates.insert(operation, fate_row(&key, &row)?);
        }
        let mut parcels = self.ext.parcels(&*self.store)?;
        let mut size_keys = BTreeMap::new();
        if self.size_keys.load(Ordering::SeqCst) {
            for (key, row) in self.store.scan_prefix(keys::OPKEY)? {
                let operation = keys::operation_of(keys::OPKEY, &key)?;
                size_keys.insert(operation, size_key_row(&key, &row)?);
            }
        }
        let mut operations = Vec::new();
        for (key, row) in self.scan_known_logged(keys::OPERATION)? {
            let request = self.request_row(&key, &row)?;
            let operation = keys::operation_of(keys::OPERATION, &key)?;
            operations.push(StoredOperation {
                operation,
                request,
                lease: leases.remove(&operation),
                losses: losses.remove(&operation).unwrap_or(0),
                fate: fates.remove(&operation),
                parcel: parcels.remove(&operation),
                size_key: size_keys.remove(&operation),
            });
        }
        Ok(Stored {
            next_operation: self.counter(keys::NEXT_OP)?,
            next_seq: self.counter(keys::NEXT_SEQ)?,
            operations,
            cordons: self.cordons()?,
        })
    }

    /// The operation `operation` as the store holds it, or `None` if it has no `op/` row.
    /// Its lease is found through its `lease-by-op/` row.
    ///
    /// # Errors
    /// The store cannot be read, or a row of the operation is not one the scheduler wrote.
    pub fn stored_operation(
        &self,
        operation: OperationId,
    ) -> Result<Option<StoredOperation>, StateError> {
        self.stored_with(operation, None)
    }

    /// [`Rows::stored_operation`], given the lease the caller has read as the operation's
    /// (`None` leaves the lookup to this call).
    pub(crate) fn stored_with(
        &self,
        operation: OperationId,
        known: Option<LeaseGrant>,
    ) -> Result<Option<StoredOperation>, StateError> {
        let key = keys::operation(operation);
        let Some(row) = self.store.get(&key)? else {
            return Ok(None);
        };
        let request = self.request_row(&key, &row)?;
        let lease = match known {
            Some(grant) => Some(grant),
            None => self.lease_of(operation)?,
        };
        let loss = keys::loss(operation);
        let losses = self
            .store
            .get(&loss)?
            .map(|row| loss_count(&loss, &row))
            .transpose()?
            .unwrap_or(0);
        let fate_key = keys::fate(operation);
        let fate = self
            .store
            .get(&fate_key)?
            .map(|row| fate_row(&fate_key, &row))
            .transpose()?;
        let parcel = self.ext.parcel(&*self.store, operation)?;
        let size_key = if self.size_keys.load(Ordering::SeqCst) {
            let opkey = keys::opkey(operation);
            let row = self.store.get(&opkey)?;
            row.map(|row| size_key_row(&opkey, &row)).transpose()?
        } else {
            None
        };
        Ok(Some(StoredOperation {
            operation,
            request,
            lease,
            losses,
            fate,
            parcel,
            size_key,
        }))
    }

    /// The number a counter row (`meta/next_op`, `meta/next_seq`, `meta/epoch`) holds; 0
    /// for a row that is not there.
    fn counter(&self, key: &str) -> Result<u64, StateError> {
        self.store.get(key)?.map_or(Ok(0), |row| number(key, &row))
    }

    /// The epoch row, as another frontend or an operator's bump may have raised it since this
    /// handle opened; 0 if the store has none.
    ///
    /// # Errors
    /// The store cannot be read, or the row is not a number.
    pub fn stored_epoch(&self) -> Result<u64, StateError> {
        self.counter(keys::EPOCH)
    }

    /// Every queued operation, with the version of its `queue/` row.
    ///
    /// # Errors
    /// The store cannot be read, or a key under `queue/` is not an operation id.
    pub fn queued(&self) -> Result<BTreeMap<OperationId, Version>, StateError> {
        let mut queued = BTreeMap::new();
        for (key, row) in self.store.scan_prefix(keys::QUEUE)? {
            queued.insert(keys::operation_of(keys::QUEUE, &key)?, row.version);
        }
        Ok(queued)
    }

    /// The version of `operation`'s `queue/` row, if the operation is queued.
    ///
    /// # Errors
    /// The store cannot be read.
    pub fn queue_version(&self, operation: OperationId) -> Result<Option<Version>, StateError> {
        let row = self.store.get(&keys::queue(operation))?;
        Ok(row.map(|row| row.version))
    }

    /// Whether a frontend of the store has ever owned `worker`: its `owner/` row is there.
    ///
    /// # Errors
    /// The store cannot be read.
    pub fn node_known(&self, worker: &WorkerId) -> Result<bool, StateError> {
        Ok(self.store.get(&keys::owner(worker))?.is_some())
    }

    /// The store's clock, in milliseconds: the one every frontend of the store shares, in
    /// which a drain's deadline is written.
    #[must_use]
    pub fn store_ms(&self) -> u64 {
        let now = self.store.now().as_duration();
        now.as_secs()
            .saturating_mul(1_000)
            .saturating_add(u64::from(now.subsec_millis()))
    }
}

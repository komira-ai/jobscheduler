//! A row codec ([`TestCodec`]) and a set of embedder rows ([`TestExt`]) for tests, behind
//! the `test-support` feature.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use js_sched::Request;
use js_state::{Cond, Store, Txn};
use js_types::sched::usage::LeaseCost;
use js_types::sched::{LeaseGrant, OperationId};

use crate::{Ext, RowCodec, StateError, Value, WorkerUp, keys};

/// The codec of tests: a row value is a line of text, a kind and a number, and the value
/// itself stays in the codec, which numbers each value it encodes. A row is therefore read
/// only by the codec that wrote it (several frontends of one test share one, in an `Arc`),
/// and a test reads what a store holds without a wire format to write. A value that starts
/// with `later` reads as [`Value::Unknown`], as one a later binary wrote does.
#[derive(Debug, Default)]
pub struct TestCodec {
    kept: Mutex<Vec<Value>>,
}

impl TestCodec {
    /// A codec that has encoded nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The value of the `lease/` row of `grant` as a binary that did not keep costs wrote
    /// it ([`Value::LegacyLease`]): the cost is not kept, and reads back as the default.
    #[must_use]
    pub fn legacy_lease(&self, grant: &LeaseGrant) -> Vec<u8> {
        let placeholder = LeaseGrant {
            cost: LeaseCost::default(),
            ..grant.clone()
        };
        self.encode("legacy-lease", Value::LegacyLease(placeholder))
    }

    /// The bytes of `value`, which the codec keeps to read back.
    fn encode(&self, kind: &str, value: Value) -> Vec<u8> {
        let mut kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
        kept.push(value);
        format!("{kind} {}", kept.len() - 1).into_bytes()
    }
}

impl RowCodec for TestCodec {
    fn submit(&self, request: &Request) -> Result<Vec<u8>, StateError> {
        Ok(self.encode("submit", Value::Submit(request.clone())))
    }

    fn lease(&self, grant: &LeaseGrant) -> Result<Vec<u8>, StateError> {
        Ok(self.encode("lease", Value::Lease(grant.clone())))
    }

    fn session(&self, up: &WorkerUp) -> Result<Vec<u8>, StateError> {
        Ok(self.encode("session", Value::Session(up.clone())))
    }

    fn read(&self, key: &str, bytes: &[u8]) -> Result<Value, StateError> {
        let text = std::str::from_utf8(bytes).map_err(|_| StateError::corrupt(key, "not text"))?;
        if text.starts_with("later") {
            return Ok(Value::Unknown);
        }
        let number = text
            .split_once(' ')
            .and_then(|(_, number)| number.parse().ok());
        let Some(number): Option<usize> = number else {
            return Err(StateError::corrupt(key, "not a row of the test codec"));
        };
        let kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
        kept.get(number)
            .cloned()
            .ok_or_else(|| StateError::corrupt(key, "a row of another codec"))
    }
}

/// `parcel/<operation>`: the parcel an operation was submitted with.
const PARCEL: &str = "parcel/";
/// `ended-parcel/<operation>`: the parcel kept with the result of an operation.
const ENDED_PARCEL: &str = "ended-parcel/";
/// The version byte every parcel row starts with.
const PARCEL_VERSION: u8 = 1;

/// The rows of tests ([`Ext`]): a parcel is kept as it is, behind a version byte, in a
/// `parcel/<operation>` row written with the submission and removed when the operation
/// ends, and an ended parcel in an `ended-parcel/<operation>` row that expires.
#[derive(Clone, Copy, Debug, Default)]
pub struct TestExt;

impl TestExt {
    /// The row value of `parcel`.
    fn value(parcel: &[u8]) -> Vec<u8> {
        [&[PARCEL_VERSION][..], parcel].concat()
    }

    /// The parcel the row `key` holds.
    fn read(key: &str, value: &[u8]) -> Result<Vec<u8>, StateError> {
        match value.split_first() {
            Some((&PARCEL_VERSION, parcel)) => Ok(parcel.to_vec()),
            _ => Err(StateError::corrupt(key, "not a parcel")),
        }
    }

    /// The key of `operation`'s row under `prefix`.
    fn key(prefix: &str, operation: OperationId) -> String {
        format!("{prefix}{:016x}", operation.0)
    }

    /// The parcel of the row under `prefix` of `operation`, if there is one.
    fn one(
        store: &dyn Store,
        prefix: &str,
        operation: OperationId,
    ) -> Result<Option<Vec<u8>>, StateError> {
        let key = Self::key(prefix, operation);
        let row = store.get(&key)?;
        row.map(|row| Self::read(&key, &row.value)).transpose()
    }
}

impl Ext for TestExt {
    fn submitted(&self, txn: Txn, operation: OperationId, parcel: &[u8]) -> Txn {
        let key = Self::key(PARCEL, operation);
        txn.put(key, Self::value(parcel), Cond::Absent)
    }

    fn settled(&self, txn: Txn, operation: OperationId) -> Txn {
        txn.delete(Self::key(PARCEL, operation), Cond::Any)
    }

    fn ended(&self, txn: Txn, operation: OperationId, parcel: &[u8], ttl: Duration) -> Txn {
        let key = Self::key(ENDED_PARCEL, operation);
        txn.put_ttl(key, Self::value(parcel), ttl, Cond::Any)
    }

    fn parcel(
        &self,
        store: &dyn Store,
        operation: OperationId,
    ) -> Result<Option<Vec<u8>>, StateError> {
        Self::one(store, PARCEL, operation)
    }

    fn parcels(&self, store: &dyn Store) -> Result<BTreeMap<OperationId, Vec<u8>>, StateError> {
        let mut parcels = BTreeMap::new();
        for (key, row) in store.scan_prefix(PARCEL)? {
            let operation = keys::operation_of(PARCEL, &key)?;
            parcels.insert(operation, Self::read(&key, &row.value)?);
        }
        Ok(parcels)
    }

    fn ended_parcel(
        &self,
        store: &dyn Store,
        operation: OperationId,
    ) -> Result<Option<Vec<u8>>, StateError> {
        Self::one(store, ENDED_PARCEL, operation)
    }
}

//! A row codec for tests ([`TestCodec`]), behind the `test-support` feature.

use std::sync::{Mutex, PoisonError};

use js_sched::Request;
use js_types::sched::LeaseGrant;

use crate::{RowCodec, StateError, Value, WorkerUp};

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

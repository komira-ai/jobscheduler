//! The calls that are out to providers, and the answers not yet handed to the lifecycle.
//!
//! One entry for each call key of each provider. An entry is `Out` from the moment the call
//! is sent until its task has the answer, then `Answered` until the lifecycle asks again and
//! is handed the answer, which removes the entry.

use std::collections::BTreeMap;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::lifecycle::ProviderAnswer;

/// What tells two calls apart: the provider, then `ProviderCall::key` (kind, run, index).
pub(super) type Key = (String, u8, String, u32);

/// One call's state.
enum Slot {
    /// Sent, not answered.
    Out,
    /// Answered, not yet handed over.
    Answered(ProviderAnswer),
}

/// The table of calls, shared by the client and the tasks that carry its calls.
#[derive(Default)]
pub(super) struct Flights {
    slots: Mutex<BTreeMap<Key, Slot>>,
    changed: Condvar,
}

impl Flights {
    fn lock(&self) -> MutexGuard<'_, BTreeMap<Key, Slot>> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// What the lifecycle's asking of `key` settles without sending: `Pending` while the call
    /// is out, or its answer, handed over once (the entry is removed). `None` when the call is
    /// to be sent: the entry is now `Out`.
    pub(super) fn begin(&self, key: Key) -> Option<ProviderAnswer> {
        let mut slots = self.lock();
        match slots.remove(&key) {
            Some(Slot::Out) => {
                slots.insert(key, Slot::Out);
                Some(ProviderAnswer::Pending)
            }
            Some(Slot::Answered(answer)) => Some(answer),
            None => {
                slots.insert(key, Slot::Out);
                None
            }
        }
    }

    /// Records the answer of the call `key` and wakes whoever waits for it.
    pub(super) fn finish(&self, key: Key, answer: ProviderAnswer) {
        self.lock().insert(key, Slot::Answered(answer));
        self.changed.notify_all();
    }

    /// Blocks until the answer of `key` is in, for at most `guard`; whether it is.
    pub(super) fn wait_answered(&self, key: &Key, guard: Duration) -> bool {
        let slots = self.lock();
        let (slots, result) = self
            .changed
            .wait_timeout_while(slots, guard, |slots| {
                !matches!(slots.get(key), Some(Slot::Answered(_)))
            })
            .unwrap_or_else(PoisonError::into_inner);
        drop(slots);
        !result.timed_out()
    }
}

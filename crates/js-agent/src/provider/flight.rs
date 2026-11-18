//! The calls that are out to providers, and the answers not yet handed to the lifecycle.
//!
//! One entry for each call key of each provider. An entry is `Out` from the moment the call
//! is sent until its task has the answer, then `Answered` until the lifecycle asks again and
//! is handed the answer, which removes the entry. A failed answer is `Rested` once its retry
//! pause has passed: the ask it is handed to sends the call again at once. Each call sent
//! has a number of its own, so the answer and the pause's end of a call sent earlier never
//! touch the entry of one sent after it.

use std::collections::BTreeMap;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::lifecycle::ProviderAnswer;

/// What tells two calls apart: the provider, then `ProviderCall::key` (kind, run, index).
pub(super) type Key = (String, u8, String, u32);

/// One call's state.
enum Slot {
    /// Sent (the call's number), not answered.
    Out(u64),
    /// Answered (the call's number), not yet handed over.
    Answered(ProviderAnswer, u64),
    /// A failed answer whose retry pause has passed, not yet handed over.
    Rested(ProviderAnswer),
}

/// What the lifecycle's asking of a key comes to.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Begun {
    /// The answer, sending nothing: `Pending` while the call is out, or its answer.
    Settled(ProviderAnswer),
    /// The call is to be sent, with this number: the entry is now `Out`.
    Send(u64),
    /// A call that was unavailable and has waited its pause: its answer is handed over and
    /// the call is to be sent again, with this number; the entry is now `Out`.
    Retry(ProviderAnswer, u64),
}

/// The table of calls, shared by the client and the tasks that carry its calls.
#[derive(Default)]
pub(super) struct Flights {
    slots: Mutex<Table>,
    changed: Condvar,
}

/// The entries, and the number of the last call sent.
#[derive(Default)]
struct Table {
    slots: BTreeMap<Key, Slot>,
    sent: u64,
}

impl Table {
    /// Marks `key` out for a new call, and gives the call's number.
    fn send(&mut self, key: Key) -> u64 {
        self.sent += 1;
        self.slots.insert(key, Slot::Out(self.sent));
        self.sent
    }
}

impl Flights {
    fn lock(&self) -> MutexGuard<'_, Table> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// What the lifecycle's asking of `key` comes to: `Pending` while the call is out; its
    /// answer, handed over once (the entry is removed); the call to be sent when there is no
    /// entry; and, for an unavailable answer that has waited its pause, the answer with the
    /// call to be sent again.
    pub(super) fn begin(&self, key: Key) -> Begun {
        let mut table = self.lock();
        match table.slots.remove(&key) {
            Some(Slot::Out(sent)) => {
                table.slots.insert(key, Slot::Out(sent));
                Begun::Settled(ProviderAnswer::Pending)
            }
            Some(Slot::Answered(answer, _)) => Begun::Settled(answer),
            Some(Slot::Rested(answer @ ProviderAnswer::Unavailable(_))) => {
                Begun::Retry(answer, table.send(key))
            }
            Some(Slot::Rested(answer)) => Begun::Settled(answer),
            None => Begun::Send(table.send(key)),
        }
    }

    /// Marks the failed answer of the call `sent` of `key` as having waited its pause. An
    /// entry that is not that call's answer (handed over during the pause, or a later call's)
    /// is left as it is.
    pub(super) fn rest(&self, key: &Key, sent: u64) {
        let mut table = self.lock();
        let kept = match table.slots.remove(key) {
            Some(Slot::Answered(answer, number)) if number == sent => Slot::Rested(answer),
            Some(other) => other,
            None => return,
        };
        table.slots.insert(key.clone(), kept);
    }

    /// Records the answer of the call `sent` of `key` and wakes whoever waits for it. The
    /// answer of a call whose entry is no longer out for it (a later call was sent) is
    /// dropped: the later call's answer is the one handed over.
    pub(super) fn finish(&self, key: Key, sent: u64, answer: ProviderAnswer) {
        let mut table = self.lock();
        if matches!(table.slots.get(&key), Some(Slot::Out(number)) if *number == sent) {
            table.slots.insert(key, Slot::Answered(answer, sent));
        }
        drop(table);
        self.changed.notify_all();
    }

    /// Blocks until the answer of `key` is in, for at most `guard`; whether it is.
    pub(super) fn wait_answered(&self, key: &Key, guard: Duration) -> bool {
        let table = self.lock();
        let (table, result) = self
            .changed
            .wait_timeout_while(table, guard, |table| {
                !matches!(
                    table.slots.get(key),
                    Some(Slot::Answered(..) | Slot::Rested(_))
                )
            })
            .unwrap_or_else(PoisonError::into_inner);
        drop(table);
        !result.timed_out()
    }
}

#[cfg(test)]
#[path = "flight_tests.rs"]
mod tests;

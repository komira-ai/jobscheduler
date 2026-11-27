//! The calls that are out to providers, and the answers not yet handed to the lifecycle.
//!
//! One entry for each call key of each provider. An entry is `Out` from the moment the call
//! is sent until its task has the answer, then `Answered` until the lifecycle asks again and
//! is handed the answer, which removes the entry. A failed answer is `Rested` once its retry
//! pause has passed: the ask it is handed to sends the call again at once. Each call sent
//! has a number of its own, so the answer and the pause's end of a call sent earlier never
//! touch the entry of one sent after it.
//!
//! A call may name the call that follows it ([`Flights::finish`]'s `then`): when it succeeds,
//! the following call is marked out in the same turn of the lock, and the task that carried
//! the first sends it. A source's `Mount` follows its `Prepare` so. [`Flights::forget`] drops
//! the entries of calls no one will ask for again (a released source's), so a call out then is
//! neither handed over nor followed.

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
    /// answer of a call whose entry is no longer out for it (a later call was sent, or it was
    /// forgotten) is dropped: the later call's answer is the one handed over.
    ///
    /// `then` names the call that follows this one when it succeeds. It is marked out, and
    /// its number answered for the caller to send it, when the answer is recorded and is a
    /// success, and `then` has no entry (a call of its own is not replaced).
    pub(super) fn finish(
        &self,
        key: Key,
        sent: u64,
        answer: ProviderAnswer,
        then: Option<Key>,
    ) -> Option<u64> {
        let mut table = self.lock();
        let mut follow = None;
        if matches!(table.slots.get(&key), Some(Slot::Out(number)) if *number == sent) {
            let succeeded = matches!(answer, ProviderAnswer::Done(_));
            table.slots.insert(key, Slot::Answered(answer, sent));
            follow = then
                .filter(|next| succeeded && !table.slots.contains_key(next))
                .map(|next| table.send(next));
        }
        drop(table);
        self.changed.notify_all();
        follow
    }

    /// Drops the entries of `keys`, whatever their state: a call out of one of them is then
    /// neither handed over nor followed when it answers, and asking it again sends it again.
    pub(super) fn forget(&self, keys: &[Key]) {
        let mut table = self.lock();
        for key in keys {
            table.slots.remove(key);
        }
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

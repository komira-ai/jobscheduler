//! The hand-over between the two threads of the agent: the worker publishes what it
//! measured, the session takes it into a heartbeat and says whether the heartbeat went
//! through.
//!
//! The outbox is a leaf: one mutex, held to copy or swap and never across a call out. The
//! worker alone owns the [`Binner`](super::Binner), so a minute that was taken but not
//! delivered goes back to the worker through [`Outbox::reclaim`] and not to the binner
//! directly.
//!
//! # What becomes of a taken load
//!
//! - [`Outbox::delivered`]: the server answered 2xx. The minutes are its now.
//! - [`Outbox::failed`]: the beat did not reach the server, or the server asked to be tried
//!   again later (a transport error, a conflict, 408, 429, 5xx). The minutes are kept and
//!   ride on a later beat, once.
//! - [`Outbox::refused`]: the server answered a 4xx that no retry changes (the message
//!   itself is refused, `invalid-heartbeat` among them). The minutes are dropped and
//!   counted: resending the same bytes would be refused again for ever, and the session
//!   ends on such an answer anyway.
//!
//! # At least once
//!
//! A beat that went out but whose answer never came back (a transport error after the
//! server processed it) is failed like any other, so its minutes ride on a later beat and
//! the server may see a minute twice. Delivery is at least once; the server dedupes by
//! (lease, minute). Only a 2xx answer settles a minute for good.
//!
//! The scalar part of a load (the counters and gauges) is never resent: the next sample
//! has newer ones.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use js_agent_wire::NodeLoad;

use super::binner::{ClosedMinute, MAX_UNSENT_AGE_USEC};
use super::sampler::Sample;
use super::wire::to_wire;

/// What one sample of the node amounts to, ready to be sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    /// Counts the samples of this process from 1.
    pub seq: u64,
    /// The node's reading; its `leases` are not sent.
    pub sample: Sample,
    /// The leases that ran the whole window.
    pub covered: Vec<String>,
    /// When each running lease started: `(lease id, microseconds on the agent's clock)`.
    pub clocks: Vec<(String, u64)>,
    /// The minutes closed since the last reading, oldest first.
    pub minutes: Vec<ClosedMinute>,
}

/// Minutes that left the worker, and the clock reading of the sample they rode with.
#[derive(Debug, Default)]
struct Held {
    at: u64,
    minutes: Vec<ClosedMinute>,
}

#[derive(Debug, Default)]
struct Slot {
    reading: Option<Reading>,
    in_flight: Option<Held>,
    returned: Vec<Held>,
    dropped: u64,
}

/// The mailbox. See the [module](self).
#[derive(Debug, Default)]
pub struct Outbox {
    window_usec: AtomicU64,
    slot: Mutex<Slot>,
}

impl Outbox {
    /// An empty outbox with no window: the worker samples nothing until the session has a
    /// heartbeat interval to give it.
    pub fn new() -> Self {
        Self::default()
    }

    /// The time between samples: the session's heartbeat interval. Called when a session
    /// is registered.
    pub fn set_window(&self, window: Duration) {
        let usec = u64::try_from(window.as_micros()).unwrap_or(u64::MAX);
        self.window_usec.store(usec, Ordering::SeqCst);
    }

    /// The time between samples in microseconds; 0 until the first [`Outbox::set_window`].
    pub fn window_usec(&self) -> u64 {
        self.window_usec.load(Ordering::SeqCst)
    }

    fn lock(&self) -> MutexGuard<'_, Slot> {
        self.slot.lock().unwrap_or_else(|e| { if true { panic!("poisoned") } PoisonError::into_inner(e) })
    }

    /// Puts a reading where the session takes it from. A reading nobody took is replaced,
    /// and its minutes are kept for the worker to take back.
    pub fn publish(&self, reading: Reading) {
        let mut slot = self.lock();
        if let Some(old) = slot.reading.replace(reading) {
            slot.returned.push(Held {
                at: old.sample.mono_usec,
                minutes: old.minutes,
            });
        }
    }

    /// The minutes that were not delivered, for the worker to put back in its binner: those
    /// of a failed beat, and those of a reading nobody took (which is discarded). Minutes
    /// that rode with a sample older than the binner's bound before `now_usec` are dropped
    /// and counted instead.
    pub fn reclaim(&self, now_usec: u64) -> Vec<ClosedMinute> {
        let mut slot = self.lock();
        let unsent = slot.reading.take().map(|reading| Held {
            at: reading.sample.mono_usec,
            minutes: reading.minutes,
        });
        let held: Vec<Held> = slot.returned.drain(..).chain(unsent).collect();
        let mut kept = Vec::new();
        for held in held {
            match now_usec.saturating_sub(held.at) <= MAX_UNSENT_AGE_USEC {
                true => kept.extend(held.minutes),
                false => slot.dropped += held.minutes.len() as u64,
            }
        }
        kept
    }

    /// Takes the published reading as the load of the next heartbeat; `None` when there is
    /// none. A load taken earlier that nobody answered for counts as failed.
    pub fn take(&self) -> Option<NodeLoad> {
        let mut slot = self.lock();
        let unanswered = slot.in_flight.take();
        slot.returned.extend(unanswered);
        let reading = slot.reading.take()?;
        slot.in_flight = Some(Held {
            at: reading.sample.mono_usec,
            minutes: reading.minutes.clone(),
        });
        Some(to_wire(&reading))
    }

    /// The heartbeat that carried the last load taken went through.
    pub fn delivered(&self) {
        self.lock().in_flight = None;
    }

    /// The heartbeat that carried the last load taken did not go through: keep its minutes.
    pub fn failed(&self) {
        let mut slot = self.lock();
        let unanswered = slot.in_flight.take();
        slot.returned.extend(unanswered);
    }

    /// The heartbeat that carried the last load taken was refused for good: drop its
    /// minutes.
    pub fn refused(&self) {
        let mut slot = self.lock();
        let lost = slot.in_flight.take().map_or(0, |held| held.minutes.len());
        slot.dropped += lost as u64;
    }

    /// Panics while holding the lock, so that the lock is poisoned: for the tests that the
    /// session and the worker go on after a thread died holding it.
    #[doc(hidden)]
    pub fn panic_while_locked(&self) {
        let _slot = self.lock();
        panic!("the thread holding the outbox died");
    }

    /// How many minutes were dropped for being too old or for a refused beat.
    pub fn dropped(&self) -> u64 {
        self.lock().dropped
    }
}

//! Delivery off the caller's path: alerts are queued and a thread of their own hands them
//! to the sink, so a receiver that refuses or hangs never delays the caller.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::said::Said;
use crate::sink::{Alert, AlertSink};

#[derive(Default)]
struct Queue {
    waiting: VecDeque<Alert>,
    /// Alerts queued or being sent.
    pending: usize,
    /// Answers not yet read: the key and whether the sink took it.
    answers: Vec<(String, bool)>,
    dropped: u64,
}

struct Shared {
    queue: Mutex<Queue>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The delivery thread for one sink. It lives for the rest of the process, parked when
/// the queue is empty.
struct Outbox {
    shared: Arc<Shared>,
    capacity: usize,
}

impl Outbox {
    fn spawn(sink: Arc<dyn AlertSink>, capacity: usize) -> Self {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue::default()),
            changed: Condvar::new(),
        });
        let worker = Arc::clone(&shared);
        std::thread::spawn(move || {
            loop {
                let alert = {
                    let mut queue = worker
                        .changed
                        .wait_while(worker.lock(), |queue| queue.waiting.is_empty())
                        .unwrap_or_else(PoisonError::into_inner);
                    queue
                        .waiting
                        .pop_front()
                        .expect("the wait ended on an alert")
                };
                let taken = sink.send(&alert).is_ok();
                let mut queue = worker.lock();
                queue.answers.push((alert.key, taken));
                queue.pending -= 1;
                worker.changed.notify_all();
            }
        });
        Self {
            shared,
            capacity: capacity.max(1),
        }
    }

    /// Queues `alert`; when the queue is full the oldest waiting alert is dropped, counted,
    /// and answered as refused so that it is sent again later.
    fn push(&self, alert: Alert) {
        let mut queue = self.shared.lock();
        if queue.waiting.len() >= self.capacity {
            let old = queue
                .waiting
                .pop_front()
                .expect("a full queue holds an alert");
            queue.dropped += 1;
            queue.answers.push((old.key, false));
            queue.pending -= 1;
        }
        queue.waiting.push_back(alert);
        queue.pending += 1;
        self.shared.changed.notify_all();
    }
}

/// Alerts to a sink, deduped by [`Said`] and delivered by a thread of their own.
///
/// [`Notifier::pass`] is cheap and never waits on the sink: it reads how earlier sends
/// went, decides what is due, and queues it. The queue holds at most `capacity` alerts;
/// beyond that the oldest is dropped (counted by [`Notifier::dropped`]) and sent again
/// later like a refused one.
pub struct Notifier {
    said: Said,
    outbox: Outbox,
}

impl Notifier {
    /// A notifier for `sink` that repeats a holding alert every `repeat_after` and queues
    /// at most `capacity` alerts.
    pub fn new(sink: Arc<dyn AlertSink>, repeat_after: Duration, capacity: usize) -> Self {
        Self {
            said: Said::new(repeat_after),
            outbox: Outbox::spawn(sink, capacity),
        }
    }

    /// One pass at time `now` over the alerts that hold (key to text).
    pub fn pass(&mut self, now: Duration, found: &BTreeMap<String, String>) {
        let answers = std::mem::take(&mut self.outbox.shared.lock().answers);
        for (key, taken) in answers {
            match taken {
                true => self.said.delivered(&key, now),
                false => self.said.refused(&key, now),
            }
        }
        for alert in self.said.pass(now, found) {
            self.outbox.push(alert);
        }
    }

    /// Waits until everything queued has been answered by the sink. For a program about
    /// to exit and for tests; a caller on a tick never calls it.
    pub fn flush(&self) {
        let shared = &self.outbox.shared;
        let _idle = shared
            .changed
            .wait_while(shared.lock(), |queue| queue.pending > 0)
            .unwrap_or_else(PoisonError::into_inner);
    }

    /// How many alerts the full queue has dropped.
    pub fn dropped(&self) -> u64 {
        self.outbox.shared.lock().dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sink::{FakeSink, SinkError};

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn holding(keys: &[&str]) -> BTreeMap<String, String> {
        keys.iter()
            .map(|key| ((*key).to_owned(), format!("about {key}")))
            .collect()
    }

    /// A sink that waits for a gate to open before it answers, and then takes everything.
    struct Gated {
        open: Mutex<bool>,
        opened: Condvar,
        taken: FakeSink,
    }

    impl Gated {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                open: Mutex::new(false),
                opened: Condvar::new(),
                taken: FakeSink::new(),
            })
        }

        fn open(&self) {
            *self.open.lock().unwrap() = true;
            self.opened.notify_all();
        }
    }

    impl AlertSink for Gated {
        fn send(&self, alert: &Alert) -> Result<(), SinkError> {
            let _open = self
                .opened
                .wait_while(self.open.lock().unwrap(), |open| !*open)
                .unwrap();
            self.taken.send(alert)
        }
    }

    /// Asserts the whole path with the fake sink: an alert is delivered once however many
    /// passes follow, once more after it clears and returns, and a refused one is sent
    /// again after the wait. It catches a notifier that loses the answer of a send or
    /// reads it at the wrong time.
    #[test]
    fn alerts_are_delivered_once_again_after_a_return_and_after_a_refusal() {
        let sink = Arc::new(FakeSink::new());
        let mut notifier = Notifier::new(sink.clone(), secs(3600), 8);
        let mut step = |at: u64, keys: &[&str]| {
            notifier.pass(secs(at), &holding(keys));
            notifier.flush();
        };
        step(0, &["a"]);
        step(1, &["a"]);
        step(2, &["a"]);
        assert_eq!(sink.keys(), ["a"]);
        step(3, &[]);
        step(4, &["a"]);
        assert_eq!(sink.keys(), ["a", "a"]);

        sink.refuse_next(1);
        step(10, &["b"]);
        step(11, &["b"]);
        assert_eq!(sink.keys(), ["a", "a"]);
        step(16, &["b"]);
        step(17, &["b"]);
        assert_eq!(sink.keys(), ["a", "a", "b"]);
    }

    /// Asserts that a sink that blocks does not block the pass: the pass returns while
    /// the sink is held shut, a full queue drops the oldest and counts it, and once the
    /// sink opens the alerts that were dropped are sent again later. It catches delivery
    /// on the caller's thread, where a hung receiver would stall every tick behind it.
    #[test]
    fn a_blocked_sink_does_not_block_the_pass_and_a_full_queue_drops_the_oldest() {
        let sink = Gated::new();
        let mut notifier = Notifier::new(sink.clone(), secs(3600), 2);
        notifier.pass(secs(0), &holding(&["a", "b", "c", "d"]));
        assert!(notifier.dropped() >= 1);
        sink.open();
        notifier.flush();
        // Whatever was dropped was answered as refused and comes back after the wait.
        notifier.pass(secs(1), &holding(&["a", "b", "c", "d"]));
        notifier.pass(secs(10), &holding(&["a", "b", "c", "d"]));
        notifier.flush();
        let mut keys = sink.taken.keys();
        keys.sort();
        keys.dedup();
        assert_eq!(keys, ["a", "b", "c", "d"]);
    }
}

//! Helpers shared by the placement-score simulations z and ad.

#![allow(dead_code, reason = "each simulation uses some of the helpers")]

use js_sched::{Event, Prediction};
use js_types::sched::{Resources, StartLease};

use crate::load_rig::{GIB, Rig, w};

/// An 88-thread host with `memory_gib` GiB of memory.
pub fn host(memory_gib: u64) -> Resources {
    Resources::new(88_000, memory_gib * GIB)
}

/// The names of the workers `starts` went to, in order.
pub fn on(starts: &[StartLease]) -> Vec<&str> {
    starts.iter().map(|s| s.worker.as_str()).collect()
}

/// Puts `count` actions charged `hint` on `worker`: the other workers are cordoned for
/// the round, so the predicate leaves one worker and the placement is forced. The
/// request limit is the predicted memory (at least one GiB).
pub fn hold(r: &mut Rig, worker: &str, others: &[&str], count: usize, hint: Prediction) {
    for other in others {
        r.feed(Event::Cordon { worker: w(other) });
    }
    let limit = hint.memory_bytes.max(GIB);
    for _ in 0..count {
        r.action(hint.cpu_millis, limit, Some(hint));
    }
    assert_eq!(on(&r.tick()), vec![worker; count], "forced onto {worker}");
    for other in others {
        r.feed(Event::Uncordon { worker: w(other) });
    }
}

/// Submits one action charged `hint` and runs a round; the worker it went to.
pub fn place(r: &mut Rig, hint: Prediction) -> String {
    r.action(hint.cpu_millis, hint.memory_bytes.max(GIB), Some(hint));
    let starts = r.tick();
    assert_eq!(starts.len(), 1, "the lease was placed, never refused");
    starts[0].worker.as_str().to_owned()
}

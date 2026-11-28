//! The families of the server's own work, read when a scrape is answered and written after
//! those of [`super::render`]: the tasks it ran on the runtime's pool for blocking calls, the
//! queue statuses its passes wrote one commit each and, over a Postgres store, the statements
//! the store sent and the threads it started for its calls. A rate of each, per second or per
//! tick, is what the server costs its host and its store.

use std::sync::atomic::Ordering;

use super::Recorder;
use super::render::counter;
use crate::api::Shared;
use crate::store::{ServerStore, StoreTally};

/// What the server has counted of its own work since it started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Work {
    /// The tasks run on the runtime's pool for blocking calls ([`crate::util::blocking`]).
    pub(crate) blocking_tasks: u64,
    /// The queue statuses of unplaced jobs the passes wrote ([`Recorder`]).
    pub(crate) queue_status_writes: u64,
    /// What a Postgres store counted; `None` for SQLite or no store.
    pub(crate) store: Option<StoreTally>,
}

impl Work {
    /// What the server of `shared`, whose events `recorder` counts, has counted so far.
    pub(crate) fn now(shared: &Shared, recorder: &Recorder) -> Work {
        Work {
            blocking_tasks: crate::util::blocking_tasks(),
            queue_status_writes: recorder.queue_status_writes.load(Ordering::Relaxed),
            store: shared.server_store().and_then(ServerStore::tally),
        }
    }
}

/// Writes the families of `work` to `out`: the blocking tasks and the queue status writes
/// always, the store's two only over a Postgres store.
pub(crate) fn write(out: &mut String, work: &Work) {
    let help = "Tasks the server ran on the runtime's pool for blocking calls: its store calls, file writes and syncs, each a hand-off to a thread of that pool and back.";
    counter(out, "js_blocking_tasks_total", help, work.blocking_tasks);
    let help = "Queue statuses of jobs a pass did not place that the pass wrote, each a read and a transaction of its own: a job's status changed (it waits for another reason, or the detail of its wait changed). A rate near the rate of passes times the queued jobs means every pass rewrites every queued job.";
    counter(
        out,
        "js_queue_status_writes_total",
        help,
        work.queue_status_writes,
    );
    if let Some(store) = work.store {
        let help = "Messages carrying statements the server's Postgres store sent, each a round trip: a read is one, a transaction one for each of its statements.";
        counter(out, "js_store_statements_total", help, store.statements);
        let help = "Threads the server's Postgres store started to make a call away from the runtime; none while every call is made on the runtime's pool for blocking calls. A rise means each call starts and ends a thread.";
        counter(out, "js_store_call_threads_total", help, store.threads);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts the whole text of the work families over a Postgres store and over none: the
    /// blocking tasks and the queue status writes always, the store's statements and threads
    /// only with the store's tally.
    /// It catches a family missing, renamed or out of order, a count read from the wrong
    /// field, and store families written for a store that counts nothing.
    #[test]
    fn the_work_is_written_with_the_store_families_only_over_postgres() {
        let tasks = "# HELP js_blocking_tasks_total Tasks the server ran on the runtime's pool for blocking calls: its store calls, file writes and syncs, each a hand-off to a thread of that pool and back.\n# TYPE js_blocking_tasks_total counter\njs_blocking_tasks_total 7\n# HELP js_queue_status_writes_total Queue statuses of jobs a pass did not place that the pass wrote, each a read and a transaction of its own: a job's status changed (it waits for another reason, or the detail of its wait changed). A rate near the rate of passes times the queued jobs means every pass rewrites every queued job.\n# TYPE js_queue_status_writes_total counter\njs_queue_status_writes_total 5\n";
        let mut alone = String::new();
        let none = Work {
            blocking_tasks: 7,
            queue_status_writes: 5,
            store: None,
        };
        write(&mut alone, &none);
        assert_eq!(alone, tasks);
        let mut over = String::new();
        let store = Some(StoreTally {
            statements: 41,
            threads: 3,
        });
        write(&mut over, &Work { store, ..none });
        let statements = "# HELP js_store_statements_total Messages carrying statements the server's Postgres store sent, each a round trip: a read is one, a transaction one for each of its statements.\n# TYPE js_store_statements_total counter\njs_store_statements_total 41\n";
        let threads = "# HELP js_store_call_threads_total Threads the server's Postgres store started to make a call away from the runtime; none while every call is made on the runtime's pool for blocking calls. A rise means each call starts and ends a thread.\n# TYPE js_store_call_threads_total counter\njs_store_call_threads_total 3\n";
        assert_eq!(over, format!("{tasks}{statements}{threads}"));
    }
}

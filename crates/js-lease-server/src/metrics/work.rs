//! The families of the server's own work, read when a scrape is answered and written after
//! those of [`super::render`]: the tasks it ran on the runtime's pool for blocking calls and,
//! over a Postgres store, the statements the store sent and the threads it started for its
//! calls. A rate of each, per second or per tick, is what the server costs its host.

use super::render::counter;
use crate::api::Shared;
use crate::store::{ServerStore, StoreTally};

/// What the server has counted of its own work since it started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Work {
    /// The tasks run on the runtime's pool for blocking calls ([`crate::util::blocking`]).
    pub(crate) blocking_tasks: u64,
    /// What a Postgres store counted; `None` for SQLite or no store.
    pub(crate) store: Option<StoreTally>,
}

impl Work {
    /// What the server of `shared` has counted so far.
    pub(crate) fn now(shared: &Shared) -> Work {
        Work {
            blocking_tasks: crate::util::blocking_tasks(),
            store: shared.server_store().and_then(ServerStore::tally),
        }
    }
}

/// Writes the families of `work` to `out`: the blocking tasks always, the store's two only
/// over a Postgres store.
pub(crate) fn write(out: &mut String, work: &Work) {
    let help = "Tasks the server ran on the runtime's pool for blocking calls: its store calls, file writes and syncs, each a hand-off to a thread of that pool and back.";
    counter(out, "js_blocking_tasks_total", help, work.blocking_tasks);
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
    /// blocking tasks always, the store's statements and threads only with the store's tally.
    /// It catches a family missing, renamed or out of order, a count read from the wrong
    /// field, and store families written for a store that counts nothing.
    #[test]
    fn the_work_is_written_with_the_store_families_only_over_postgres() {
        let tasks = "# HELP js_blocking_tasks_total Tasks the server ran on the runtime's pool for blocking calls: its store calls, file writes and syncs, each a hand-off to a thread of that pool and back.\n# TYPE js_blocking_tasks_total counter\njs_blocking_tasks_total 7\n";
        let mut alone = String::new();
        let none = Work {
            blocking_tasks: 7,
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

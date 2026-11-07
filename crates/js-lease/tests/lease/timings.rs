//! What the table keeps of how long its steps took, on clocks the tests drive.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_lease::{Monotonic, Timings};

use crate::support::{fresh, request};

/// A subscriber that discards what it is given, the default of this thread for as long as
/// the guard lives. A test whose steps are slow logs the slow-step warning; the first log
/// of that callsite registers it, and a registration seen by no subscriber that wants it
/// is cached as never enabled, which drops the first warning `slow_save` captures on its
/// own thread at the same time. Under this subscriber it is cached as enabled sometimes,
/// so each thread's subscriber decides.
fn subscribed() -> tracing::subscriber::DefaultGuard {
    let discard = tracing_subscriber::fmt()
        .with_writer(std::io::sink)
        .finish();
    tracing::subscriber::set_default(discard)
}

/// A clock that answers with the readings it was given, in order.
#[derive(Debug, Clone)]
struct Script(Arc<Mutex<VecDeque<Duration>>>);

impl Script {
    fn of(millis: &[u64]) -> Self {
        let readings = millis.iter().map(|ms| Duration::from_millis(*ms));
        Self(Arc::new(Mutex::new(readings.collect())))
    }
}

impl Monotonic for Script {
    fn reading(&self) -> Duration {
        self.0.lock().unwrap().pop_front().unwrap()
    }
}

#[derive(Debug)]
struct Step(Mutex<u64>, u64);

impl Monotonic for Step {
    fn reading(&self) -> Duration {
        let mut now = self.0.lock().unwrap();
        *now += self.1;
        Duration::from_millis(*now - self.1)
    }
}

/// Asserts the record: per operation, in name order, how many steps were timed, how many
/// took over a second (1000 ms does not, 1001 ms does) and the longest. Three `acquire`
/// steps of 1500, 500 and 1000 ms and one `release` of 20 ms read back as exactly that,
/// and clones share the record. It catches a record that counts every step as slow, one
/// that puts the bound on the wrong side, and one that keeps the last duration and not
/// the longest.
#[test]
fn the_record_keeps_count_slow_and_longest_per_operation() {
    let _log = subscribed();
    let timings = Timings::new(Script::of(&[0, 1500, 1500, 2000, 2000, 3000, 3000, 3020]));
    assert_eq!(timings.report(), "no lease steps timed");
    let again = timings.clone();
    timings.begin("acquire").finish();
    again.begin("acquire").finish();
    timings.begin("acquire").finish();
    again.begin("release").finish();
    assert_eq!(
        timings.report(),
        "acquire: 3 timed, 1 over 1 s, longest 1500 ms; release: 1 timed, 0 over 1 s, longest 20 ms"
    );
}

/// Asserts that the table's own record, read through `timings()`, holds its saves under
/// the step that made them: a table given a new clock starts a new record, and a release on
/// a clock that moves 20 ms per reading is one timed `release` of 20 ms and one timed
/// `release-settled` of 20 ms. It catches a table that times into a record the caller
/// cannot reach.
#[test]
fn the_table_records_its_saves_where_the_caller_can_read_them() {
    let (mut table, _clock, _scratch) = fresh("timings_table");
    let id = table.acquire(request(3600)).unwrap().id;
    table.watch_saves(Step(Mutex::new(0), 20));
    assert_eq!(table.timings().report(), "no lease steps timed");
    table.release(&id).unwrap();
    assert_eq!(
        table.timings().report(),
        "release: 1 timed, 0 over 1 s, longest 20 ms; \
         release-settled: 1 timed, 0 over 1 s, longest 20 ms"
    );
}

/// Asserts that the save made while the table opens is recorded as `open`, so a slow
/// first save is named like the others. The duration is whatever the disk took; only
/// the name and the count are asserted.
#[test]
fn the_open_save_is_recorded_as_open() {
    let (table, _clock, _scratch) = fresh("timings_open");
    let report = table.timings().report();
    assert!(
        report.starts_with("open: 1 timed, 0 over 1 s, longest "),
        "{report}"
    );
}

/// Asserts that a step that has begun and not finished is reported as in flight, under
/// its operation, with nothing yet timed; that finishing it moves it to the timed count
/// and the suffix goes; and that a timer dropped without finishing (a cancelled future)
/// does not stay in flight or count as timed. It catches a record that only knows
/// finished steps, which shows nothing about a step stuck at the moment of a hang, and
/// one that counts a cancelled step as in flight for ever.
#[test]
fn a_step_that_has_begun_is_in_flight_until_it_finishes_or_is_dropped() {
    let _log = subscribed();
    let timings = Timings::new(Script::of(&[0, 1500, 1500, 1500]));
    let waiting = timings.begin("wait-acquire");
    assert_eq!(
        timings.report(),
        "wait-acquire: 0 timed, 0 over 1 s, longest 0 ms, 1 in flight"
    );
    let second = timings.begin("wait-acquire");
    assert!(timings.report().ends_with("longest 0 ms, 2 in flight"));
    drop(second);
    assert!(timings.report().ends_with("longest 0 ms, 1 in flight"));
    waiting.finish();
    assert_eq!(
        timings.report(),
        "wait-acquire: 1 timed, 1 over 1 s, longest 1500 ms"
    );
    drop(timings.begin("release"));
    assert_eq!(
        timings.report(),
        "wait-acquire: 1 timed, 1 over 1 s, longest 1500 ms"
    );
}

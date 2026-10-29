//! What the table keeps of how long its steps took, on clocks the tests drive.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_lease::{Monotonic, Timings};

use crate::support::{fresh, request};

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
/// took over a second (1000 ms does not, 1001 ms does) and the longest. Three `renew`
/// steps of 1500, 500 and 1000 ms and one `release` of 20 ms read back as exactly that,
/// and clones share the record. It catches a record that counts every step as slow, one
/// that puts the bound on the wrong side, and one that keeps the last duration and not
/// the longest.
#[test]
fn the_record_keeps_count_slow_and_longest_per_operation() {
    let timings = Timings::new(Script::of(&[0, 1500, 1500, 2000, 2000, 3000, 3000, 3020]));
    assert_eq!(timings.report(), "no lease steps timed");
    let again = timings.clone();
    timings.begin("renew").finish();
    again.begin("renew").finish();
    timings.begin("renew").finish();
    again.begin("release").finish();
    assert_eq!(
        timings.report(),
        "release: 1 timed, 0 over 1 s, longest 20 ms; renew: 3 timed, 1 over 1 s, longest 1500 ms"
    );
}

/// Asserts that the table's own record, read through `timings()`, holds its saves under
/// the step that made them: a table given a new clock starts a new record, and a renew on a clock that moves 20 ms per reading is one
/// timed `renew` of 20 ms. It catches a table that times into a record the caller
/// cannot reach.
#[test]
fn the_table_records_its_saves_where_the_caller_can_read_them() {
    let (mut table, _clock, _scratch) = fresh("timings_table");
    let id = table.acquire(request(3600), None).unwrap().id;
    table.watch_saves(Step(Mutex::new(0), 20));
    assert_eq!(table.timings().report(), "no lease steps timed");
    table.renew(&id, None).unwrap();
    assert_eq!(
        table.timings().report(),
        "renew: 1 timed, 0 over 1 s, longest 20 ms"
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

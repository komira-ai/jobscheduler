//! The warning for a slow save of the lease file. The save is timed on a clock the
//! test drives, so no test here waits or reads the system clock.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_lease::Monotonic;
use js_types::LeaseId;

use crate::support::{Table, fresh, request};

/// A clock that moves by `step` on every reading, so a save, which reads it once
/// before and once after writing, is measured as exactly `step`.
#[derive(Debug, Clone)]
struct Stepping {
    micros: Arc<AtomicU64>,
    step: Duration,
}

impl Stepping {
    fn of(step: Duration) -> Self {
        Self {
            micros: Arc::new(AtomicU64::new(0)),
            step,
        }
    }
}

impl Monotonic for Stepping {
    fn reading(&self) -> Duration {
        let step = u64::try_from(self.step.as_micros()).unwrap();
        Duration::from_micros(self.micros.fetch_add(step, Ordering::SeqCst))
    }
}

/// What the table logged on this thread, as text.
struct Log {
    text: Arc<Mutex<Vec<u8>>>,
    _scope: tracing::subscriber::DefaultGuard,
}

struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Log {
    fn start() -> Self {
        let text = Arc::new(Mutex::new(Vec::new()));
        let sink = text.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || Sink(sink.clone()))
            .finish();
        Self {
            text,
            _scope: tracing::subscriber::set_default(subscriber),
        }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.text.lock().unwrap()).into_owned()
    }

    fn slow_saves(&self) -> Vec<String> {
        let text = self.text();
        let lines = text.lines().filter(|line| line.contains("slow"));
        lines.map(str::to_string).collect()
    }
}

fn millis(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn held(table: &mut Table) -> LeaseId {
    table.acquire(request(3600), None).unwrap().id
}

/// Asserts that a renew whose save the clock measures at 1.5 s logs one warning that
/// names the operation (`renew`) and the duration (1500 ms). It catches a save that is
/// never timed and a warning that does not say which step was slow.
#[test]
fn a_save_over_a_second_warns_with_the_operation_and_the_duration() {
    let (mut table, _clock, _scratch) = fresh("slow_save_warns");
    let id = held(&mut table);
    table.watch_saves(Stepping::of(millis(1500)));
    let log = Log::start();
    table.renew(&id, None).unwrap();
    let lines = log.slow_saves();
    assert_eq!(lines.len(), 1, "{}", log.text());
    assert!(lines[0].contains("WARN"), "{}", lines[0]);
    assert!(lines[0].contains("operation=renew"), "{}", lines[0]);
    assert!(lines[0].contains("took_ms=1500"), "{}", lines[0]);
}

/// Asserts that a save measured at 0.5 s logs nothing, so the warning is not a line per
/// save.
#[test]
fn a_save_under_a_second_logs_nothing() {
    let (mut table, _clock, _scratch) = fresh("slow_save_quiet");
    let id = held(&mut table);
    table.watch_saves(Stepping::of(millis(500)));
    let log = Log::start();
    table.renew(&id, None).unwrap();
    assert_eq!(log.text(), "");
}

/// Asserts which side of the bound a save of exactly 1 s is on: it is not slow (the
/// warning is for a save over a second), and 1 ms more is.
#[test]
fn a_save_of_exactly_a_second_is_not_slow_and_a_millisecond_more_is() {
    let (mut table, _clock, _scratch) = fresh("slow_save_edge");
    let id = held(&mut table);
    let log = Log::start();
    table.watch_saves(Stepping::of(millis(1000)));
    table.renew(&id, None).unwrap();
    assert_eq!(log.text(), "");
    table.watch_saves(Stepping::of(millis(1001)));
    table.renew(&id, None).unwrap();
    let lines = log.slow_saves();
    assert_eq!(lines.len(), 1, "{}", log.text());
    assert!(lines[0].contains("took_ms=1001"), "{}", lines[0]);
}

/// Asserts that every step that saves names itself: acquire writes twice (the pending
/// record before the start and the settled one after), release writes twice, and a tick
/// that changed something writes once. The names are what tell an operator which call
/// stalled.
#[test]
fn every_saving_step_names_itself() {
    let (mut table, clock, _scratch) = fresh("slow_save_names");
    table.watch_saves(Stepping::of(millis(2000)));
    let log = Log::start();
    let id = held(&mut table);
    table.release(&id).unwrap();
    held(&mut table);
    clock.advance(Duration::from_secs(3600));
    table.tick();
    let text = log.text();
    let names: Vec<&str> = text
        .lines()
        .filter_map(|line| line.split("operation=").nth(1))
        .filter_map(|rest| rest.split_whitespace().next())
        .collect();
    assert_eq!(
        names,
        [
            "acquire",
            "acquire-settled",
            "release",
            "release-settled",
            "acquire",
            "acquire-settled",
            "tick"
        ],
        "{text}"
    );
}

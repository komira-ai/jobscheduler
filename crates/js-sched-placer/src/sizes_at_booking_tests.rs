//! A success whose group's `memory.peak` came within 5% of its booking and stayed within it
//! reached its booking (a host that holds a job at its booking held it there), so its class
//! books a step above the booking next (`1.2 x` the booking plus 128 MiB, rounded up to
//! 256 MiB), up to the whole host, whatever its lower non-file peak. The runs here report a
//! non-file series peak of their own below the `memory.peak`, as a job whose page cache filled
//! the rest of its limit does. The host is 16 GiB.

use std::time::Duration;

use js_jobs::{Booked, JobEnd, JobRecord, ResourceUsage};
use js_types::MemorySeries;

use super::super::reached;
use super::{GIB, JobSizes, MIB, MODULE_COLD, Tables, booked, bounds, exited, needs, of_class};
use super::{queued, ran, size};

/// A success whose group peaked at `group` bytes (`memory.peak`) and whose non-file series
/// peaked at `series_mib` MiB.
fn held(group: u64, series_mib: u32) -> JobEnd {
    let mut end = exited(0, group, Duration::from_secs(30));
    if let JobEnd::Exited { output, .. } = &mut end {
        output.memory_series =
            Some(MemorySeries::new(1, vec![100, series_mib, 200], 3_000_000, false).unwrap());
    }
    end
}

/// The runs of one class, each with the booking it ran under, and the class's next booking.
struct Class {
    tables: Tables,
    history: Vec<(JobRecord, Booked)>,
}

impl Class {
    fn new() -> Self {
        Self {
            tables: Tables::new(),
            history: Vec::new(),
        }
    }

    /// `n` runs booked `memory` bytes that each end as `end` gives.
    fn run(&mut self, n: usize, memory: u64, end: impl Fn() -> JobEnd) -> &mut Self {
        for _ in 0..n {
            let id = format!("run-{}", self.history.len());
            let at = booked(2000, memory);
            let job = ran(
                &self.tables,
                (&id, None),
                of_class("compile", needs(0, 0)),
                at,
                end(),
            );
            self.history.push((job, at));
        }
        self
    }

    /// The memory the next job of the class books, taught every run so far.
    fn next(&self) -> u64 {
        let mut sizes = JobSizes::with_cold(MODULE_COLD);
        sizes.set_bounds(Some(bounds()));
        for (job, at) in &self.history {
            assert!(sizes.observe(job, *at), "{} teaches a run", job.id);
        }
        size(&sizes, &queued("next", of_class("compile", needs(0, 0)))).1
    }
}

/// Asserts that a run held at its booking books its class one step above that booking: five
/// runs of 6450 MiB book 7.75 GiB; a run under 7.75 GiB whose group peaked at the whole
/// booking while its non-file series peaked at 6450 MiB books the next job 9.5 GiB
/// (`1.2 x 7.75 GiB + 128 MiB`). Catches the class booked from the non-file peak of a run its
/// booking held (7.75 GiB again), which keeps it at the limit that holds it, run after run.
#[test]
fn a_run_held_at_its_booking_books_the_class_a_step_above_it() {
    let mut class = Class::new();
    class.run(5, 12 * GIB, || {
        exited(0, 6450 * MIB, Duration::from_secs(30))
    });
    assert_eq!(class.next(), 7936 * MIB);
    class.run(1, 7936 * MIB, || held(7936 * MIB, 6450));
    assert_eq!(class.next(), 9728 * MIB, "a step above the booking");
}

/// Asserts that a class that stops reaching its booking stays where it is: after a run held at
/// 7.75 GiB books 9.5 GiB, forty runs under 9.5 GiB whose group peaks at 7.8 GiB (82% of the
/// booking) keep it at 9.5 GiB: neither back down to 7.75 GiB nor further up. Catches a step
/// undone by the next run that fits (the class would be held again on the run after), and a
/// class that keeps climbing once its runs fit.
#[test]
fn a_class_that_stops_reaching_its_booking_stays_where_it_is() {
    let mut class = Class::new();
    class.run(5, 12 * GIB, || {
        exited(0, 6450 * MIB, Duration::from_secs(30))
    });
    class.run(1, 7936 * MIB, || held(7936 * MIB, 6450));
    class.run(40, 9728 * MIB, || held(7800 * MIB, 6450));
    assert_eq!(class.next(), 9728 * MIB);
}

/// Asserts the cap: a class whose runs keep reaching their booking near the host climbs to the
/// whole host and no further. Runs of 11 GiB book 13.5 GiB; a run held at 13.5 GiB books the
/// whole 16 GiB host (a step above, `1.2 x 13.5 GiB + 128 MiB`, would be 16.5 GiB); runs held
/// at the 16 GiB booking keep it at 16 GiB. Catches a step that books past the host, and a
/// class held at the host's whole memory dropped back to its non-file peaks.
#[test]
fn a_class_held_at_its_booking_near_the_host_stops_at_the_whole_host() {
    let mut class = Class::new();
    class.run(5, 16 * GIB, || exited(0, 11 * GIB, Duration::from_secs(30)));
    assert_eq!(class.next(), 13824 * MIB);
    class.run(1, 13824 * MIB, || held(13824 * MIB, 11 * 1024));
    assert_eq!(class.next(), 16 * GIB, "the whole host");
    class.run(10, 16 * GIB, || held(16 * GIB - 100 * MIB, 11 * 1024));
    assert_eq!(class.next(), 16 * GIB, "and no further");
}

/// Asserts where a run reaches its booking, against a 10 GiB booking with a 6 GiB non-file
/// peak: a group peak of exactly 95% of the booking reached it (12.25 GiB next, a step above
/// it), one byte under did not, and neither did a group peak one byte past the booking (a run
/// that used more than it booked, which no host held); both book from the non-file peak
/// (7.5 GiB). Catches the 95% edge off by one either way, and a run past its booking taken as
/// held there.
#[test]
fn a_run_reaches_its_booking_from_95_percent_of_it_to_the_booking_itself() {
    let booking = 10 * GIB;
    let edge = booking / 20 * 19;
    let next = |group: u64| {
        let mut class = Class::new();
        class.run(5, booking, || held(group, 6 * 1024));
        class.next()
    };
    assert_eq!(next(edge), 12544 * MIB, "at 95%");
    assert_eq!(next(booking), 12544 * MIB, "at the booking");
    assert_eq!(next(edge - 1), 7680 * MIB, "just under 95%");
    assert_eq!(next(booking + 1), 7680 * MIB, "past the booking");
}

/// Asserts that a booking of 0 is never reached, not even by a group peak of 0, while a
/// booking is reached by a group peak at it. Catches a run with no booking taken as held at
/// it.
#[test]
fn a_booking_of_0_is_never_reached() {
    let usage = |peak| ResourceUsage {
        cpu_time: Duration::ZERO,
        peak_memory_bytes: peak,
        cpu_throttle: None,
        cpu_pressure_some: None,
        cpu_peak: None,
        peak_swap_bytes: None,
    };
    assert!(!reached(usage(0), 0));
    assert!(!reached(usage(GIB), 0));
    assert!(reached(usage(GIB), GIB));
}

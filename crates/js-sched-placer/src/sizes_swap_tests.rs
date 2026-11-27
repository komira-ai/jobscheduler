//! A success whose host pushed part of it into swap held more than its peak shows: its class
//! books its peak plus its swap peak (with the margin) next. The runs here go through the
//! job store, so the swap peak is read back from the stored row before it is taught. The host
//! is 16 GiB.

use std::time::Duration;

use js_jobs::JobEnd;

use super::{GIB, JobSizes, MIB, MODULE_COLD, Tables, booked, bounds, needs, of_class, output};
use super::{queued, ran, size};

/// The memory the next job of a class books after five successes booked 8 GiB that each peaked
/// at 3 GiB and reported `swap` as their swap peak.
fn next_after(swap: Option<u64>) -> u64 {
    let tables = Tables::new();
    let mut sizes = JobSizes::with_cold(MODULE_COLD);
    sizes.set_bounds(Some(bounds()));
    for n in 0..5 {
        let mut out = output(3 * GIB, Duration::from_secs(30));
        if let Some(usage) = out.usage.as_mut() {
            usage.peak_swap_bytes = swap;
        }
        let at = booked(2000, 8 * GIB);
        let ended = ran(
            &tables,
            (&format!("run-{n}"), None),
            of_class("compile", needs(0, 0)),
            at,
            JobEnd::Exited {
                exit_code: 0,
                output: out,
            },
        );
        let job = tables.jobs.get_job(&ended.id).unwrap().unwrap();
        assert_eq!(
            job.result
                .as_ref()
                .and_then(|r| r.usage)
                .map(|u| u.peak_swap_bytes),
            Some(swap),
            "the stored run keeps its swap peak"
        );
        assert!(sizes.observe(&job, at), "run-{n} teaches a run");
    }
    size(&sizes, &queued("next", of_class("compile", needs(0, 0)))).1
}

/// Asserts that a class whose runs peaked at 3 GiB and swapped 1 GiB books 5 GiB next
/// (`1.2 x 4 GiB + 128 MiB`, rounded up to 256 MiB), while the same runs with a swap peak of 0,
/// or with none reported (an agent from before the field), book 3840 MiB, from the peak alone.
/// Catches a swap peak lost between the job's result and the estimator (the placer then books a
/// class squeezed into swap what its host could not hold, run after run), and a swap peak of 0
/// or an absent one counted as a swap.
#[test]
fn a_class_whose_runs_swapped_books_their_peak_plus_their_swap() {
    assert_eq!(next_after(Some(GIB)), 5 * GIB, "swapped 1 GiB");
    assert_eq!(next_after(Some(0)), 3840 * MIB, "a swap peak of 0");
    assert_eq!(next_after(None), 3840 * MIB, "no swap peak reported");
}

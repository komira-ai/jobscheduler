//! The swap peak of a success: a run that swapped is a held sample of its peak plus its swap
//! peak (the most it can have held at once), kept below the host.

use super::*;

/// A success that peaked at `peak` and held `swap` bytes of swap, booked high enough never to
/// have touched its booking.
fn swapped(peak: u64, swap: u64) -> Run {
    Run {
        swap_peak_bytes: swap,
        ..success(peak)
    }
}

/// What five runs of `run` book, by the estimate and by the high end of the recent peaks (the
/// placer's figure for a class with its own runs).
fn booked(run: &Run) -> (Result<u64, Fallback>, Option<u64>) {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, run, 5);
    (
        estimate(&s, &k).map(|e| e.memory_bytes),
        s.peak_memory(&k, DAY, &Bounds::default()),
    )
}

/// Rule: a success with a 3 GiB peak and 1 GiB of swap is a sample of 4 GiB, so it books
/// `1.2 x 4 GiB + 128 MiB` rounded up to 256 MiB (5 GiB), by the estimate and by the recent
/// peaks alike; the same peak with no swap, or with a swap peak of 0, books from 3 GiB alone
/// (3840 MiB). Catches a swap peak the estimator ignores (a class squeezed into swap keeps
/// booking what its host could not hold) and a swap peak of 0 counted as a swap.
#[test]
fn a_success_that_swapped_books_its_peak_plus_its_swap() {
    assert_eq!(
        booked(&swapped(3 * GIB, GIB)),
        (Ok(5 * GIB), Some(5 * GIB)),
        "peak plus swap"
    );
    assert_eq!(
        booked(&swapped(3 * GIB, 0)),
        (Ok(3840 * MIB), Some(3840 * MIB)),
        "no swap: the peak alone"
    );
    assert_eq!(booked(&swapped(3 * GIB, 0)), booked(&success(3 * GIB)));
}

/// Rule: a success that swapped and reached its booking needed at least the larger of its peak
/// plus its swap and its booking. 3996 MiB at a 4 GiB booking with 2 GiB of swap is 6044 MiB
/// (7424 MiB booked, not the 5 GiB of the booking alone); 7.8 GiB at an 8 GiB booking with
/// 100 MiB of swap is below the booking, so the booking (9984 MiB booked). Catches a run that
/// both swapped and reached its booking sized from one of them alone.
#[test]
fn a_success_that_swapped_at_its_booking_books_the_larger_of_the_two() {
    let at = |booked_memory_bytes, peak, swap| Run {
        booked_memory_bytes,
        limit_hits: 1,
        ..swapped(peak, swap)
    };
    assert_eq!(
        booked(&at(4 * GIB, 4 * GIB - 100 * MIB, 2 * GIB)).0,
        Ok(7424 * MIB)
    );
    assert_eq!(
        booked(&at(8 * GIB, 8 * GIB - 200 * MIB, 100 * MIB)).0,
        Ok(9984 * MIB)
    );
}

/// Rule: a held sample stays below the host. A 40 GiB peak with 30 GiB of swap on a 64 GiB host
/// is held just below the host, which books the whole host (`OverCap`), and a swap peak of
/// `u64::MAX` saturates to the same instead of overflowing. Catches a sum that wraps to a small
/// sample and one that books past the host.
#[test]
fn a_swap_past_the_host_is_held_below_it() {
    for swap in [30 * GIB, u64::MAX] {
        assert_eq!(
            booked(&swapped(40 * GIB, swap)).0,
            Err(Fallback::OverCap),
            "{swap}"
        );
    }
}

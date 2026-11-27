//! A burst of a heavy-tail class on the farm, modelled on what a farm's journals showed: a
//! compile class whose runs average 2.75 cores and burst to 16 to 70 cores for a few seconds,
//! learned one run at a time, then 24 of its jobs at once over six hosts of 88 cores.

use super::*;
use crate::trace::tests::job;

/// The five-second peaks of the class's runs, in millicores, taken in turn.
const PEAKS: [u32; 10] = [
    16_000, 16_000, 16_000, 17_000, 18_000, 20_000, 30_000, 40_000, 52_000, 70_000,
];

/// The jobs of the burst.
const BURST: u64 = 24;

/// Twenty runs of the class one minute apart (each alone on the farm), then [`BURST`] of them
/// at once: each runs a minute at 2.75 cores on average.
fn trace() -> Vec<Job> {
    let run = |at: u64, index: usize| Job {
        peak_milli: Some(PEAKS[index % PEAKS.len()]),
        ..job(at, 60_000, 165_000, 1)
    };
    let history = (0..20).map(|index| run(index as u64 * 120_000, index));
    let burst = (0..BURST as usize).map(|index| run(2_400_000, index));
    history.chain(burst).collect()
}

/// The replay of [`trace`] on the farm with its runs at their peak while it lasts.
fn replayed(bookings: Bookings) -> Outcome {
    let arm = Arm {
        name: "test",
        bookings,
        rule: Rule::Booked,
        limit_permille: 1100,
    };
    let setup = Setup {
        farm: Farm::OURS,
        shape: Shape::Burst,
        speed: 1,
        held_out_ms: None,
    };
    replay(&trace(), arm, setup)
}

/// Asserts that a burst of a heavy-tail class spreads over the farm when its runs are taught
/// with their peaks, and piles onto one host when they are not: at the median of its means
/// (2.75 cores) every one of the 24 jobs fits the first host (66 cores booked) and that host
/// is asked past its cores; at the 90th percentile of its peaks (held to a quarter of a host,
/// 22 cores) four go to each host, no host is booked past its cores, and the CPU asked past
/// the cores falls. Catches the heavy-tail booking missing from the replay's sizing.
#[test]
fn a_burst_of_a_heavy_tail_class_spreads_when_its_peaks_are_booked() {
    let median = replayed(Bookings::Learned);
    let tailed = replayed(Bookings::Tailed);
    assert_eq!(median.booked_peak, 66_000);
    assert_eq!(median.placed, [44, 0, 0, 0, 0, 0]);
    assert_eq!(tailed.booked_peak, 88_000);
    assert_eq!(tailed.placed, [24, 4, 4, 4, 4, 4]);
    assert!(median.over_excess > 0);
    assert!(tailed.over_excess < median.over_excess);
}

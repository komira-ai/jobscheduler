use super::*;
use crate::trace::tests::job;
use js_types::CpuSeries;

/// One host of 16 cores.
const FARM: Farm = Farm {
    hosts: 1,
    milli: 16_000,
    bytes: 251 << 30,
};

/// A compile of class 1 that arrives at `arrive_ms` and runs a minute and a half: 30 s at 4
/// cores (a quarter of the host, the most a slot books), then 60 s at a quarter core, 135 s of
/// CPU in all; with its series as its host recorded it in 2 s points when `recorded`.
fn compile(arrive_ms: u64, recorded: bool) -> Job {
    let points: Vec<u16> = [64; 15].into_iter().chain([4; 30]).collect();
    Job {
        peak_milli: Some(4000),
        cpu_series: recorded.then(|| CpuSeries::new(2, points, 90_000_000, false).unwrap()),
        ..job(arrive_ms, 90_000, 135_000, 1)
    }
}

/// Three days of compiles, a round every ten minutes: eight at once, whose bursts meet when
/// they share the host, and one alone five minutes later.
fn trace(recorded: bool) -> Vec<Job> {
    (0..3 * 144)
        .flat_map(move |round| {
            let at = round * 600_000;
            std::iter::repeat_n(at, 8)
                .chain([at + 300_000])
                .map(move |arrive| compile(arrive, recorded))
        })
        .collect()
}

/// The row of `text` that starts with `name`.
fn row(text: &str, name: &str) -> String {
    text.lines()
        .find(|line| line.starts_with(&format!("| {name} |")))
        .unwrap_or_else(|| panic!("no row {name:?} in\n{text}"))
        .to_owned()
}

/// The names of the gate's four rows.
const ROWS: [&str; 4] = [
    "total time (wait plus run), mean s",
    "throughput, jobs per hour",
    "core-s asked past the hosts' cores",
    "jobs booked by curve",
];

/// Asserts the gate on a shaped class: learned on two days of recorded series, the curve mode
/// books the compiles' bursts where they come, so on the held-out third day four share the host
/// at once and the other four wait for the bursts to pass, where the flat step starts all eight
/// and their bursts meet. It holds all four (no worse total time, the same throughput, fewer
/// core-seconds past the cores, jobs booked by curve) and passes. Catches a gate that judges
/// the curve mode on synthesised lives or on the days it learned on, and a curve mode that
/// cannot pay where the design says it should.
#[test]
fn a_shaped_class_learned_on_two_days_passes_the_gate_on_the_third() {
    let text = gate(&trace(true), FARM, 2);
    assert!(text.starts_with("## The gate on held-out days\n\n"), "{text}");
    assert!(
        text.contains(
            "held out: the 1296 jobs that arrived after them, 1296 with a CPU series that stands \
             for their run."
        ),
        "{text}"
    );
    for name in ROWS {
        assert!(row(&text, name).ends_with(" | yes |"), "{name}:\n{text}");
    }
    assert!(
        text.ends_with(
            "The curve mode passes the gate: no loss against the flat step on the held-out days.\n"
        ),
        "{text}"
    );
}

/// Asserts that the same trace without its series does not pass: no class learns a curve, so no
/// held-out job is booked by one, whatever the other three rows say. Catches a gate a trace
/// without series passes on the curve mode's other order of ranking hosts alone.
#[test]
fn a_trace_without_series_does_not_pass_the_gate() {
    let text = gate(&trace(false), FARM, 2);
    assert!(
        text.contains("held out: the 1296 jobs that arrived after them, 0 with a CPU series"),
        "{text}"
    );
    assert!(
        row(&text, ROWS[3]).ends_with("| - | 0 | be at least one | no |"),
        "{text}"
    );
    assert!(
        text.ends_with(
            "The curve mode does not pass the gate: the profiles stay in shadow \
             (`placement-profiles-live.md` section 5, stage 3).\n"
        ),
        "{text}"
    );
}

/// Asserts that a trace no job of which arrives after the learning days says that nothing is
/// held out, and that fewer learning days than two are read as two. Catches a gate that judges
/// an empty part (which every arm passes) and a spread taken from a day before the trace.
#[test]
fn a_trace_that_ends_within_its_learning_days_holds_nothing_out() {
    let jobs = trace(true);
    let before: Vec<Job> = jobs
        .iter()
        .filter(|job| job.arrive_ms < 2 * DAY_MS)
        .cloned()
        .collect();
    assert_eq!(before.len(), 2 * 144 * 9);
    let said = "## The gate on held-out days\n\n\
                No job arrived after the trace's first 2 days: nothing is held out.\n";
    assert_eq!(gate(&before, FARM, 2), said);
    assert_eq!(gate(&before, FARM, 0), said);
    assert_eq!(
        gate(&before, FARM, 3),
        said.replace("first 2 days", "first 3 days")
    );
}

/// Asserts each of the four at its boundary: a total time the spread above the flat step's
/// holds and a millisecond more does not; a span the flat step's over 0.98 holds and a
/// millisecond longer does not; core-seconds past cores hold only when fewer, not when equal;
/// one job booked by curve holds and none does not. Catches each comparison off by one way or
/// the other.
#[test]
fn each_of_the_four_holds_up_to_its_boundary_and_not_past_it() {
    let flat = Held {
        mean_ms: 100_000,
        span_ms: 98_000,
        over: 10,
        curved: 0,
    };
    let at = Held {
        mean_ms: 100_500,
        span_ms: 100_000,
        over: 9,
        curved: 1,
    };
    assert_eq!(holds(&flat, &at, 500), [true; 4]);
    let past = Held {
        mean_ms: 100_501,
        span_ms: 100_001,
        over: 10,
        curved: 0,
    };
    assert_eq!(holds(&flat, &past, 500), [false; 4]);
    assert_eq!((yes(true), yes(false)), ("yes", "no"));
}

/// Asserts what an arm did with the held-out part: the mean of its jobs' times from arrival to
/// end, its span from the part's start (at least a millisecond), its CPU past cores, and its
/// jobs booked by curve, the learning days' jobs left out of each; and that the mean of a part
/// with no job is 0. Catches a learning day's job counted in the held-out part.
#[test]
fn the_held_out_part_is_measured_on_its_own_jobs() {
    let jobs = [job(0, 1, 1, 1), job(10, 1, 1, 1), job(20, 1, 1, 1)];
    let outcome = Outcome {
        ended: vec![5000, 40, 60],
        curved: vec![true, false, true],
        held_over_excess: 7,
        ..Outcome::default()
    };
    assert_eq!(
        held(&jobs, &outcome, 10),
        Held {
            mean_ms: 35,
            span_ms: 50,
            over: 7,
            curved: 1,
        }
    );
    assert_eq!(mean_total(&jobs, &outcome, &(100..200)), 0);
    let instant = Outcome {
        ended: vec![0, 10, 10],
        ..outcome
    };
    assert_eq!(held(&jobs, &instant, 10).span_ms, 1);
}

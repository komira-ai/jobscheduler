use super::*;
use crate::trace::tests::job;

/// Nine jobs, one of each kind the report tells apart.
fn nine() -> Vec<Job> {
    let mut jobs = vec![
        // Short: ten seconds at two cores, peaking at four; no contention recorded.
        Job {
            peak_milli: Some(4000),
            ..job(0, 10_000, 20_000, 1)
        },
        // Long and bursty: two minutes at one core, peaking at four; none waited.
        Job {
            peak_milli: Some(4000),
            pressure_ms: Some(0),
            ..job(1000, 120_000, 120_000, 2)
        },
        // Long and steady: two cores, peaking at three; 5% waited.
        Job {
            peak_milli: Some(3000),
            pressure_ms: Some(6000),
            ..job(2000, 120_000, 240_000, 3)
        },
        // Long, no peak; 20% waited.
        Job {
            pressure_ms: Some(24_000),
            ..job(3000, 120_000, 120_000, 4)
        },
        // Failed: in no class.
        Job {
            end: End::Failed,
            ..job(4000, 10_000, 10_000, 5)
        },
    ];
    // Four jobs of 24 cores for a minute that booked nothing: 96 cores if on one host.
    for _ in 0..4 {
        jobs.push(Job {
            booked_milli: 0,
            ..job(5000, 60_000, 1_440_000, 6)
        });
    }
    jobs
}

#[test]
fn the_report_describes_the_trace_and_its_classes() {
    let text = report(&nine(), Farm::OURS);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "# CPU packing replay");
    assert_eq!(
        lines[2],
        "9 jobs over 0.0 h on 6 hosts of 88 cores: 2 core-hours of CPU, a mean of 51.0 cores \
         (9.7% of the farm). Recorded wait p50 0 s, p90 0 s, p99 0 s. The most CPU a run of 10 s \
         or more averaged: 24.0 cores (a shorter run is taken to average at most 24.0). Runs of 30 \
         s or more: 7, with 99.5% of the CPU, their mean CPU p25 1.0, p50 24.0, p75 24.0 cores."
    );
    for expected in [
        "5 classes, 8 runs that ended with 0. The 5 with the most CPU:",
        "| 6 | 4 | 92.0 | 60 | 24.0 | - | - | - |",
        "| 3 | 1 | 3.8 | 120 | 2.0 | 3.0 | 1.5 | 4.1% to 66.6% |",
        "| 2 | 1 | 1.9 | 120 | 1.0 | 4.0 | 4.0 | 4.1% to 25.0% |",
        "| 4 | 1 | 1.9 | 120 | 1.0 | - | - | - |",
        "| 1 | 1 | 0.3 | 10 | 2.0 | 4.0 | 2.0 | 50.0% to 50.0% |",
        "| short (wall p50 under 60 s) | 1 | 12.5 | 0.3 |",
        "| long and bursty (peak/mean p50 2 or more) | 1 | 12.5 | 1.9 |",
        "| long and steady | 1 | 12.5 | 3.8 |",
        "| long, no peak recorded | 2 | 62.5 | 93.9 |",
        "| recorded | 0 | 0 | 0 | 0 | 0 | 0 |",
    ] {
        assert!(lines.contains(&expected), "missing {expected:?} in\n{text}");
    }
    // The rows of the classes come largest first, the tie of classes 2 and 4 by index.
    let at = |row: &str| lines.iter().position(|line| *line == row).unwrap();
    assert!(
        at("| 6 | 4 | 92.0 | 60 | 24.0 | - | - | - |")
            < at("| 3 | 1 | 3.8 | 120 | 2.0 | 3.0 | 1.5 | 4.1% to 66.6% |")
    );
    assert!(
        at("| 2 | 1 | 1.9 | 120 | 1.0 | 4.0 | 4.0 | 4.1% to 25.0% |")
            < at("| 4 | 1 | 1.9 | 120 | 1.0 | - | - | - |")
    );
}

#[test]
fn the_report_checks_the_replay_by_band_of_recorded_contention() {
    let text = report(&nine(), Farm::OURS);
    let band = |name: &str| {
        text.lines()
            .find(|line| line.starts_with(&format!("| {name} |")))
            .unwrap_or_else(|| panic!("no band {name}"))
            .to_owned()
    };
    // Six runs recorded none (the failed one counts: it ran), one each of the others.
    assert!(band("none recorded").starts_with("| none recorded | 6 | - | "));
    assert!(band("under 1%").starts_with("| under 1% | 1 | 0.0 | "));
    assert!(band("1% to 10%").starts_with("| 1% to 10% | 1 | 5.0 | "));
    assert!(band("10% or more").starts_with("| 10% or more | 1 | 20.0 | "));
    // The four jobs of 24 cores on one host, as placed, are slowed.
    assert!(!band("none recorded").ends_with("| 0.0 | 0.0 |"));
    assert!(text.contains("| replayed, blip | "));
    assert!(text.contains("| replayed, burst | "));
}

#[test]
fn the_report_replays_every_arm_at_every_speed_and_shape() {
    let text = report(&nine(), Farm::OURS);
    for speed in SPEEDS {
        assert!(text.contains(&format!("## Options, arrivals at {speed}x\n")));
    }
    for arm in ARMS {
        for shape in SHAPES {
            let row = format!("| {} | {} | ", arm.name, shape.name());
            assert_eq!(text.matches(&row).count(), 2, "{row}");
        }
    }
    // The recorded bookings put the four 24-core jobs on one host; the curve at the cores
    // never puts more on one than it has.
    let row = |name: &str| {
        text.lines()
            .find(|line| line.starts_with(&format!("| {name} | blip | ")))
            .unwrap()
            .split(" | ")
            .nth(8)
            .unwrap()
            .to_owned()
    };
    assert_ne!(row("recorded bookings"), "-");
    assert_eq!(row("D curve, 1.0"), "-");
}

#[test]
fn a_row_says_each_figure_of_an_outcome() {
    let farm = Farm {
        hosts: 1,
        milli: 4000,
        bytes: 1 << 30,
    };
    let mut outcome = Outcome {
        waits: vec![2000, 0],
        last_end_ms: 10_000,
        own_ms: 20_000,
        slowed_ms: 2000,
        over_ms: 1000,
        over_excess: 4_000_000,
        used: 2e7,
        held_ms: 25_000,
        under_ms: 5000,
        plan_error: 4_000_000,
        booked: 30_000_000,
        above_booking: 5_000_000,
        ..Outcome::default()
    };
    let mut text = String::new();
    row(
        &mut text,
        ARMS[1],
        Shape::Blip,
        &outcome,
        (farm, 20_000_000),
    );
    assert_eq!(
        text,
        "| A median, 1.1 | blip | 720 | 0 | 2 | 2 | 2.5 | 10.0 | 4.0 | 10.0 | 12.0 | 50.0 | 1.50 | 25.0 | 10.0 | 50.0 |\n"
    );
    outcome.over_ms = 0;
    let mut text = String::new();
    row(
        &mut text,
        ARMS[1],
        Shape::Blip,
        &outcome,
        (farm, 20_000_000),
    );
    assert!(text.contains("| 0.0 | - | 10.0 | 12.0 |"), "{text}");
}

#[test]
fn percentiles_and_figures_are_rounded_as_the_report_says() {
    assert_eq!(percentile(&[], 50), 0);
    assert_eq!(percentile(&[1, 2, 3, 4], 50), 2);
    assert_eq!(percentile(&[1, 2, 3, 4], 99), 4);
    assert_eq!(percentile(&[1, 2, 3, 4], 0), 1);
    assert_eq!(percent(1.0, 0.0), "0.0");
    assert_eq!(percent(1.0, 3.0), "33.3");
    assert_eq!(secs(1), "1");
    assert_eq!(secs(1000), "1");
    assert_eq!(cores(1260), "1.3");
    let empty = report(&[], Farm::OURS);
    assert!(empty.contains("0 classes, 0 runs that ended with 0. The 0 with the most CPU:"));
}

#[test]
fn a_waits_row_gives_every_job_and_those_after_the_first_hour() {
    let jobs = [job(0, 1, 1, 0), job(3_600_000, 1, 1, 0)];
    let mut text = String::new();
    waits_row(&mut text, "x", &jobs, &[5000, 7000]);
    assert_eq!(text, "| x | 5 | 7 | 7 | 7 | 7 | 7 |\n");
}

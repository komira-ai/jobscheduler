//! The memory estimate: formula, statistic, samples, held samples, hierarchy, hosts, hints, CPU.

use super::*;

// ---- the formula and the statistic ------------------------------------------------

/// Rule: `E = ceil(1.2 x max + 128 MiB)` rounded up to 256 MiB, in integers.
#[test]
fn golden_window_formula() {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &success(3 * GIB), 5);
    // 3 GiB x 1.2 + 128 MiB = 3814.4 MiB -> 3840 MiB.
    assert_eq!(memory(&s, &k), 3840 * MIB);
}

/// Rule: a sum that is already a multiple of 256 MiB stays (no extra step).
#[test]
fn an_exact_multiple_is_not_rounded_further() {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &success(1600 * MIB), 5);
    // 1600 MiB x 1.2 + 128 MiB = 2048 MiB exactly.
    assert_eq!(memory(&s, &k), 2048 * MIB);
}

/// Rule: the statistic is the maximum, so one high sample among 31 low ones sizes the
/// estimate, and the maximum holds for at least 256 runs and then lapses.
#[test]
fn one_high_sample_raises_the_estimate_and_lapses_after_the_span() {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &success(3 * GIB), 1);
    feed(&mut s, &k, &success(GIB), 31);
    assert_eq!(memory(&s, &k), 3840 * MIB);
    feed(&mut s, &k, &success(GIB), 256 - 32);
    assert_eq!(memory(&s, &k), 3840 * MIB, "still in the last 256 runs");
    feed(&mut s, &k, &success(GIB), 288 - 256);
    assert_eq!(
        memory(&s, &k),
        3840 * MIB,
        "the oldest chunk is kept to 288 runs"
    );
    feed(&mut s, &k, &success(GIB), 1);
    assert_eq!(
        memory(&s, &k),
        1536 * MIB,
        "289 runs later the heavy mode is gone"
    );
}

/// Rule: raising is immediate: one sample above the estimate raises it at once.
#[test]
fn a_new_maximum_raises_at_once() {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &success(GIB), 40);
    assert_eq!(memory(&s, &k), 1536 * MIB);
    feed(&mut s, &k, &success(3 * GIB), 1);
    assert_eq!(memory(&s, &k), 3840 * MIB);
}

/// Rule: fewer than `N_MIN` samples can only raise: the estimate is at least the
/// default booking; no estimate below two samples without a floor.
#[test]
fn n_below_n_min_never_lowers_below_the_default() {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &success(500 * MIB), 1);
    assert_eq!(estimate(&s, &k), Err(Fallback::Cold));
    for n in 2..N_MIN {
        feed(&mut s, &k, &success(500 * MIB), 1);
        assert_eq!(memory(&s, &k), 4 * GIB, "n = {n}");
    }
    feed(&mut s, &k, &success(500 * MIB), 1);
    assert_eq!(memory(&s, &k), GIB, "n = {N_MIN} may lower to the minimum");
}

/// Rule: with few samples the estimate still raises above the default.
#[test]
fn few_samples_can_raise() {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &success(6 * GIB), 2);
    // 6 GiB x 1.2 + 128 MiB = 7503.2 MiB -> 7680 MiB.
    assert_eq!(memory(&s, &k), 7680 * MIB);
}

// ---- samples: which runs count -----------------------------------------------------

/// Rule 3.1: a failed (not memory-killed), lost or cancelled run is not a sample.
#[test]
fn a_failed_non_oom_run_is_not_a_sample() {
    for outcome in [Outcome::Failed, Outcome::Lost, Outcome::Cancelled] {
        let mut s = Summaries::default();
        let k = linux(1);
        let run = Run {
            outcome,
            ..success(20 * GIB)
        };
        let seen = s.observe(&k, &run, DAY, &Bounds::default());
        assert_eq!(
            seen,
            Observed {
                memory: Err(Discard::NotASample),
                cpu: CpuSample::Skipped
            }
        );
        assert_eq!(s.get(&k).unwrap().samples(), 0);
        assert_eq!(estimate(&s, &k), Err(Fallback::Cold));
    }
}

/// Rule 3.1: a success is a sample only with a non-zero measurement, a wall time of
/// one second, and a peak under the host's memory.
#[test]
fn a_success_needs_a_trusted_measurement() {
    let mut s = Summaries::default();
    let k = linux(1);
    let b = Bounds::default();
    let short = Run {
        wall_us: MIN_WALL_US - 1,
        ..success(GIB)
    };
    assert_eq!(
        s.observe(&k, &short, DAY, &b).memory,
        Err(Discard::Untrusted)
    );
    let unmeasured = Run {
        peak_memory_bytes: None,
        ..success(GIB)
    };
    assert_eq!(
        s.observe(&k, &unmeasured, DAY, &b).memory,
        Err(Discard::Untrusted)
    );
    let zero = Run {
        peak_memory_bytes: Some(0),
        ..success(GIB)
    };
    assert_eq!(
        s.observe(&k, &zero, DAY, &b).memory,
        Err(Discard::Untrusted)
    );
    let whole_host = success(b.host_memory);
    assert_eq!(
        s.observe(&k, &whole_host, DAY, &b).memory,
        Err(Discard::OverCap)
    );
    assert_eq!(s.get(&k).unwrap().samples(), 0);
    let exact = Run {
        wall_us: MIN_WALL_US,
        ..success(b.host_memory - 1)
    };
    assert_eq!(s.observe(&k, &exact, DAY, &b).memory, Ok(()));
    assert_eq!(s.get(&k).unwrap().samples(), 1);
}

/// M7: zero is not absent. A run with a zero or missing field never changes the
/// estimate or the stored summary, over arbitrary streams.
#[test]
fn a_zero_field_never_changes_the_estimate() {
    let b = Bounds::default();
    for seed in 1..=20u64 {
        let mut rng = Rng(seed * 0x2545_f491);
        let k = linux(1);
        let (mut plain, mut noisy) = (Summaries::default(), Summaries::default());
        for _ in 0..120 {
            let run = Run {
                cpu_usage_us: rng.below(4_000_000) + 1,
                node_cpu_pressure_permille: Some(0),
                ..success(rng.below(3 * GIB) + 1)
            };
            plain.observe(&k, &run, DAY, &b);
            noisy.observe(&k, &run, DAY, &b);
            let zeros = [
                Run {
                    peak_memory_bytes: Some(0),
                    cpu_usage_us: 0,
                    ..run
                },
                Run {
                    peak_memory_bytes: None,
                    cpu_usage_us: 0,
                    ..run
                },
                Run { wall_us: 0, ..run },
            ];
            for zero in zeros {
                let seen = noisy.observe(&k, &zero, DAY, &b);
                assert!(
                    seen.memory.is_err() && seen.cpu == CpuSample::Skipped,
                    "{zero:?}"
                );
            }
            assert_eq!(plain.get(&k), noisy.get(&k));
            assert_eq!(
                plain.estimate(&k, DAY, &b, &Hint::default()),
                noisy.estimate(&k, DAY, &b, &Hint::default())
            );
        }
    }
    // A zero CPU time alone drops the CPU sample but keeps the memory sample.
    let mut s = Summaries::default();
    let idle = Run {
        node_cpu_pressure_permille: Some(0),
        ..success(GIB)
    };
    let seen = s.observe(&linux(1), &idle, DAY, &b);
    assert!(seen.memory.is_ok() && seen.cpu == CpuSample::Skipped);
}

// ---- M2: a run that reached its booking needed at least the booking ---------------------

fn held_estimate(run: Run, times: usize) -> u64 {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &run, times);
    memory(&s, &k)
}

/// A success that used `peak`, booked at `booked`, that the caller does not count as having
/// reached its booking.
fn booked_at(booked: u64, peak: u64) -> Run {
    Run {
        booked_memory_bytes: booked,
        ..success(peak)
    }
}

/// A success that used `peak`, booked at `booked`, that the caller counts as having reached
/// its booking once.
fn at_booking(booked: u64, peak: u64) -> Run {
    Run {
        limit_hits: 1,
        ..booked_at(booked, peak)
    }
}

/// A run of a key that needs `need`, booked `booked`, as a host that holds a job at its
/// booking reports it: its peak is `min(need, booked)`, and the caller counts it as having
/// reached its booking when that peak came within 5% of the booking.
fn held_to(booked: u64, need: u64) -> Run {
    let peak = need.min(booked);
    Run {
        limit_hits: u64::from(peak * 100 >= booked * 95),
        ..booked_at(booked, peak)
    }
}

/// M2: a success that reached its booking ([`Run::limit_hits`]) needed at least the booking:
/// it is a held sample of the larger of its peak and its booking, sized with the usual
/// margin, so its key books a step above the booking. A success that swapped is a held sample
/// of its peak; any other success is an honest one, whatever its booking. Catches a run held
/// at its booking recorded as `min(peak, booked)` with no margin, which books its key at that
/// booking again, and a run that reached its booking sized from its lower peak alone.
#[test]
fn a_success_that_reached_its_booking_books_a_step_above_it() {
    // 4 GiB x 1.2 + 128 MiB = 5044 MiB -> 5 GiB.
    assert_eq!(
        held_estimate(at_booking(4 * GIB, 4 * GIB - 100 * MIB), 5),
        5 * GIB,
        "at its booking"
    );
    assert_eq!(
        held_estimate(at_booking(4 * GIB, GIB), 5),
        5 * GIB,
        "at least the booking, whatever the peak read"
    );
    assert_eq!(
        held_estimate(at_booking(4 * GIB, 5 * GIB), 5),
        6 * GIB + GIB / 4,
        "a peak above its booking: the peak (5 GiB x 1.2 + 128 MiB)"
    );
    let mut s = Summaries::default();
    feed(&mut s, &linux(1), &at_booking(100 * GIB, 4 * GIB), 5);
    assert_eq!(
        estimate(&s, &linux(1)),
        Err(Fallback::OverCap),
        "a booking past the host: held just below it, which books the whole host"
    );
    let swapped = Run {
        swap_peak_bytes: 1,
        ..booked_at(4 * GIB, 3 * GIB)
    };
    assert_eq!(held_estimate(swapped, 5), 3840 * MIB, "swap: its peak");
    assert_eq!(
        held_estimate(booked_at(4 * GIB, 3 * GIB), 5),
        3840 * MIB,
        "below the booking: headroom over the peak"
    );
    assert_eq!(
        held_estimate(booked_at(4 * GIB, 6 * GIB), 5),
        7680 * MIB,
        "a peak above the booking not counted: honest"
    );
}

/// M2: a key held at its booking leaves it on the next run and then stays: ten runs of
/// 6450 MiB book 7.75 GiB; one run that reached that booking with a peak of 6450 MiB books
/// 9.5 GiB, a step above the booking, not 7.75 GiB from its peak; runs of the same peak that
/// do not reach 9.5 GiB keep it there. Catches a key whose runs reach their booking booked
/// at that booking (or below it) run after run, and a key that falls back once it fits.
#[test]
fn a_key_held_at_its_booking_books_a_step_above_it_and_stays_there() {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &success(6450 * MIB), 10);
    assert_eq!(memory(&s, &k), 7936 * MIB);
    feed(&mut s, &k, &at_booking(7936 * MIB, 6450 * MIB), 1);
    assert_eq!(memory(&s, &k), 9728 * MIB, "a step above the booking");
    feed(&mut s, &k, &booked_at(9728 * MIB, 6450 * MIB), 40);
    assert_eq!(memory(&s, &k), 9728 * MIB, "it fits: no further step");
}

/// M2: what a key that keeps reaching its booking costs before it fits: a compile that needs
/// 10 GiB, booked 7.75 GiB from its earlier runs, is held at its booking on two runs (at
/// 7.75 GiB, then at 9.5 GiB) and fits on the third (11.75 GiB); its first measured peak
/// then books it 12.25 GiB, where it stays.
#[test]
fn a_key_that_needs_10_gib_from_7_75_is_held_on_two_runs_and_fits_on_the_third() {
    let b = Bounds::default();
    let (mut s, k) = (Summaries::default(), linux(1));
    feed(&mut s, &k, &success(6450 * MIB), 10);
    let mut bookings = Vec::new();
    let mut held = 0;
    for _ in 0..6 {
        let booked = memory(&s, &k);
        bookings.push(booked / MIB);
        let run = held_to(booked, 10 * GIB);
        held += run.limit_hits;
        s.observe(&k, &run, DAY, &b);
    }
    assert_eq!(held, 2, "runs held at their booking before one fits");
    assert_eq!(bookings, [7936, 9728, 12032, 12544, 12544, 12544]);
}

/// M2 closed loop: the booking follows the estimate and each run is held at its booking, so
/// its peak is `min(need, booking)`, 200 runs per key. A key that needs more than its booking
/// climbs a step per run that reaches the booking until it fits, then reaches it no more
/// (none of the last 100 runs does) and never books above a step over the largest booking a
/// run could still reach (`need / 0.95`).
#[test]
fn a_closed_loop_climbs_until_the_key_fits_and_then_settles() {
    let b = Bounds::default();
    for (seed, need) in [
        (1, 2 * GIB),
        (2, 4 * GIB),
        (3, 5 * GIB),
        (4, 6 * GIB),
        (5, 8 * GIB),
        (6, 10 * GIB),
        (7, 24 * GIB),
    ] {
        let mut rng = Rng(seed * 0x9e37_79b9);
        let (mut s, k) = (Summaries::default(), linux(1));
        let (mut top, mut late_hits) = (0, 0);
        for run in 0..200 {
            let booked = s
                .estimate(&k, DAY, &b, &Hint::default())
                .map_or(b.default_memory, |est| est.memory_bytes);
            top = top.max(booked);
            let held = held_to(booked, need - rng.below(need / 5));
            if run >= 100 {
                late_hits += held.limit_hits;
            }
            s.observe(&k, &held, DAY, &b);
        }
        let reach = (need * 100).div_ceil(95);
        let ceiling = (reach + reach.div_ceil(5) + 128 * MIB).div_ceil(256 * MIB) * 256 * MIB;
        assert_eq!(late_hits, 0, "need {need}: still at its booking");
        assert!(
            top <= ceiling.max(4 * GIB),
            "need {need}: top {top} > {ceiling}"
        );
    }
}

/// M2 cap: a key whose runs keep reaching their booking near the host climbs to the host and
/// no further: from 48.25 GiB under a 64 GiB host, a run held at a 56 GiB booking makes the estimate
/// [`Fallback::OverCap`] (the whole host), and more runs held at the host's whole memory keep
/// it there, each a held sample kept below the host. Catches a step that books past the host,
/// and a held sample at the host discarded, which would drop the key back to its peaks.
#[test]
fn a_key_held_at_its_booking_near_the_host_stops_at_the_host() {
    let b = Bounds::default();
    let (mut s, k) = (Summaries::default(), linux(1));
    feed(&mut s, &k, &success(40 * GIB), 10);
    assert_eq!(memory(&s, &k), 48 * GIB + GIB / 4);
    s.observe(&k, &at_booking(56 * GIB, 40 * GIB), DAY, &b);
    assert_eq!(
        estimate(&s, &k),
        Err(Fallback::OverCap),
        "a step reaches the host"
    );
    for _ in 0..40 {
        let seen = s.observe(&k, &at_booking(b.host_memory, 40 * GIB), DAY, &b);
        assert_eq!(seen.memory, Ok(()), "a held run at the host is kept");
    }
    assert_eq!(
        estimate(&s, &k),
        Err(Fallback::OverCap),
        "and stays at the host"
    );
    assert_eq!(
        s.get(&k).unwrap().long_max(),
        40 * GIB,
        "the held samples never become measured ones"
    );
}

/// S5: a kill is a sample of at least the booking it died at (a host that kills a job for
/// memory holds it at its booking), not the record's `used` (0 when the killed run carried no
/// usage), nor the build host's derived `1.5 x booking + 512 MiB`; a use above the booking
/// is the sample.
#[test]
fn a_kill_sample_is_at_least_the_booking_it_died_at() {
    let k = linux(1);
    let mut s = Summaries::default();
    s.observe(&k, &kill(4 * GIB, 0, None), DAY, &Bounds::default());
    assert_eq!(s.get(&k).unwrap().long_max(), 4 * GIB, "the booking");
    let mut s = Summaries::default();
    s.observe(&k, &kill(4 * GIB, 5 * GIB, None), DAY, &Bounds::default());
    assert_eq!(s.get(&k).unwrap().long_max(), 5 * GIB, "a use above it");
}

/// A kill with nothing booked and nothing measured has no sample to give and no
/// booking to raise from: it is not acted on.
#[test]
fn a_kill_with_nothing_booked_or_measured_is_ignored() {
    let mut s = Summaries::default();
    let k = linux(1);
    let nothing = Run {
        peak_memory_bytes: None,
        ..kill(0, 0, None)
    };
    let seen = s.observe(&k, &nothing, DAY, &Bounds::default());
    assert_eq!(seen.memory, Err(Discard::Untrusted));
    assert_eq!(s.get(&k).unwrap().samples(), 0);
    assert_eq!(floor(&s, &k), 0);
    assert_eq!(estimate(&s, &k), Err(Fallback::Cold));
}

/// Rule 3.4.2: a kill enters the maxima as a sample (with headroom), so the estimate
/// rises on it even when the floor is lower.
#[test]
fn a_kill_is_a_censored_sample() {
    let mut s = Summaries::default();
    let k = linux(1);
    s.observe(&k, &kill(4 * GIB, 5 * GIB, None), DAY, &Bounds::default());
    assert_eq!(s.get(&k).unwrap().samples(), 1);
    // 5 GiB x 1.2 + 128 MiB = 6.125 GiB -> 6.25 GiB, above the 4.25 GiB floor.
    assert_eq!(memory(&s, &k), 6 * GIB + GIB / 4);
}

/// Rule 3.4.2: when the measured use is above the limit the run died at, the use is the
/// sample; neither exceeds the host.
#[test]
fn the_kill_sample_is_the_larger_of_use_and_limit() {
    let bounds = Bounds::default();
    let sample = |used: Option<u64>, booked: u64| {
        let mut s = Summaries::default();
        let k = linux(1);
        let mut run = kill(booked, 0, None);
        run.peak_memory_bytes = used;
        s.observe(&k, &run, DAY, &bounds);
        s.get(&k).unwrap().long_max()
    };
    assert_eq!(sample(Some(7 * GIB), 4 * GIB), 7 * GIB);
    assert_eq!(sample(Some(3 * GIB), 4 * GIB), 4 * GIB);
    assert_eq!(sample(None, 4 * GIB), 4 * GIB);
    assert_eq!(sample(Some(100 * GIB), 4 * GIB), 64 * GIB - 1);
}

// ---- S4: mixed keys -------------------------------------------------------------------

/// S4: a key whose recent samples are far apart is mixed: never lowered, and raised
/// only by a kill.
#[test]
fn a_mixed_key_is_never_lowered_and_raised_only_by_kills() {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &success(500 * MIB), 10);
    feed(&mut s, &k, &success(5 * GIB / 2), 1);
    assert!(s.get(&k).unwrap().mixed());
    assert_eq!(
        memory(&s, &k),
        4 * GIB,
        "mixed: the default, not the sized 3.25 GiB"
    );
    feed(&mut s, &k, &success(5 * GIB), 1);
    assert_eq!(
        memory(&s, &k),
        6 * GIB + GIB / 4,
        "its own largest peak (5 GiB x 1.2 + 128 MiB) raises a mixed key above the default"
    );
    s.observe(&k, &heavy_kill(), DAY, &Bounds::default());
    assert!(memory(&s, &k) >= 16 * GIB, "a kill does");
}

/// S4: the dispersion test is strict: exactly four times the median is not mixed.
#[test]
fn the_mixed_boundary_is_exact() {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &success(GIB), 20);
    feed(&mut s, &k, &success(4 * GIB), 1);
    assert!(!s.get(&k).unwrap().mixed());
    assert_eq!(memory(&s, &k), 5 * GIB);
    feed(&mut s, &k, &success(4 * GIB + 1), 1);
    assert!(s.get(&k).unwrap().mixed());
    assert_eq!(memory(&s, &k), 5 * GIB, "mixed: sized from its own peak");
    assert!(
        !Summary::new(Basis::Cgroup, DAY).mixed(),
        "no samples is not mixed"
    );
}

// ---- 2.1 / 2.3 the key and the hierarchy ----------------------------------------------

/// Rule 2.1: Mac and Linux samples never mix.
#[test]
fn bases_never_mix() {
    let mut s = Summaries::default();
    let mac = key(Basis::MacFootprint, Some(1));
    let lin = key(Basis::Cgroup, Some(1));
    feed(&mut s, &mac, &success(3 * GIB), 5);
    s.observe(&mac, &heavy_kill(), DAY, &Bounds::default());
    assert_eq!(estimate(&s, &lin), Err(Fallback::Missing));
    assert_eq!(floor(&s, &lin), 0);
    assert!(memory(&s, &mac) >= 16 * GIB);
    feed(&mut s, &lin, &success(GIB), 5);
    assert_eq!(memory(&s, &lin), 1536 * MIB);
    assert_eq!(s.get(&mac).unwrap().basis(), Basis::MacFootprint);
}

/// Rule 2.1: instance and platform class separate keys.
#[test]
fn instances_and_platforms_never_mix() {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &success(3 * GIB), 5);
    s.observe(&k, &heavy_kill(), DAY, &Bounds::default());
    let other_instance = Key::new(
        Instance::new("other"),
        PlatformClass::new("linux-x86_64"),
        Basis::Cgroup,
        Shape(7),
        Some(Cfg(1)),
    );
    let other_platform = Key::new(
        Instance::new("main"),
        PlatformClass::new("linux-arm64"),
        Basis::Cgroup,
        Shape(7),
        Some(Cfg(1)),
    );
    let other_shape = Key::new(
        Instance::new("main"),
        PlatformClass::new("linux-x86_64"),
        Basis::Cgroup,
        Shape(8),
        Some(Cfg(1)),
    );
    for other in [other_instance, other_platform, other_shape] {
        assert_eq!(estimate(&s, &other), Err(Fallback::Missing));
        assert_eq!(floor(&s, &other), 0);
    }
}

/// Rule 2.3: the full key (L1) answers once it has `N_MIN` samples, otherwise the
/// shape-level summary (L2), which merges configurations.
#[test]
fn the_levels_answer_in_order() {
    let mut s = Summaries::default();
    feed(&mut s, &linux(1), &success(3 * GIB), 10);
    // A new configuration has one sample: L1 cannot answer, L2 (the shape) does.
    feed(&mut s, &linux(2), &success(GIB), 1);
    let est = estimate(&s, &linux(2)).unwrap();
    assert_eq!((est.level, est.memory_bytes), (Level::L2, 3840 * MIB));
    // Exactly N_MIN of its own: L1 answers with its own, smaller, maximum.
    feed(&mut s, &linux(2), &success(GIB), 4);
    let est = estimate(&s, &linux(2)).unwrap();
    assert_eq!((est.level, est.memory_bytes), (Level::L1, 1536 * MIB));
    // A key with no configuration has only L2.
    let none = key(Basis::Cgroup, None);
    let est = estimate(&s, &none).unwrap();
    assert_eq!((est.level, est.memory_bytes), (Level::L2, 3840 * MIB));
}

/// A kill under one configuration floors the whole shape: another configuration is
/// floored too (L2 merges, over-books, never under-books).
#[test]
fn a_shape_floor_applies_to_every_configuration() {
    let mut s = Summaries::default();
    s.observe(&linux(1), &heavy_kill(), DAY, &Bounds::default());
    feed(&mut s, &linux(2), &success(GIB), 5);
    assert_eq!(estimate(&s, &linux(2)).unwrap().level, Level::L1);
    assert!(memory(&s, &linux(2)) >= 16 * GIB);
    assert_eq!(floor(&s, &linux(2)), 16 * GIB);
}

// ---- M1: never a whole host ------------------------------------------------------------

fn on_host(host: u64) -> Bounds {
    Bounds {
        host_memory: host,
        ..Bounds::default()
    }
}

/// M1: a key floor stays strictly below the host's memory, on a small host as on a
/// large one.
#[test]
fn a_floor_stays_strictly_below_the_host() {
    for (host, expect) in [
        (16 * GIB, 16 * GIB - 256 * MIB),
        (8 * GIB + 1, 8 * GIB),
        (8 * GIB, 8 * GIB - 256 * MIB),
    ] {
        let mut s = Summaries::default();
        let k = linux(1);
        let b = on_host(host);
        s.observe(&k, &kill(4 * GIB, 6 * GIB, Some(64 * GIB)), DAY, &b);
        assert_eq!(s.key_floor(&k, &b), expect, "host {host}");
        assert!(s.key_floor(&k, &b) < host);
    }
}

/// M1: an estimate that would reach the host is `OverCap`, not a clamped whole-host
/// booking; one step below is fine.
#[test]
fn an_estimate_reaching_the_host_is_over_cap() {
    let b = on_host(8 * GIB);
    let ask = |peak| {
        let mut s = Summaries::default();
        let k = linux(1);
        for _ in 0..5 {
            s.observe(&k, &success(peak), DAY, &b);
        }
        s.estimate(&k, DAY, &b, &Hint::default())
    };
    // 6.5 GiB x 1.2 + 128 MiB = 7.925 GiB -> 8 GiB: the whole host.
    assert_eq!(ask(6 * GIB + GIB / 2), Err(Fallback::OverCap));
    assert_eq!(ask(6 * GIB).unwrap().memory_bytes, 7680 * MIB);
    // A host smaller than the minimum estimate cannot be sized at all.
    let tiny = on_host(GIB);
    let mut s = Summaries::default();
    let k = linux(1);
    for _ in 0..5 {
        s.observe(&k, &success(GIB / 4), DAY, &tiny);
    }
    assert_eq!(
        s.estimate(&k, DAY, &tiny, &Hint::default()),
        Err(Fallback::OverCap)
    );
}

/// M1 (departure 7): a hint that would reach the host is `OverCap` too: the crate never
/// returns a whole-host booking, whoever asked for it. A hint just under is passed.
#[test]
fn a_hint_never_reaches_the_host() {
    let mut s = Summaries::default();
    let k = linux(1);
    let b = on_host(8 * GIB);
    for _ in 0..5 {
        s.observe(&k, &success(GIB), DAY, &b);
    }
    let ask = |memory_bytes| {
        s.estimate(
            &k,
            DAY,
            &b,
            &Hint {
                memory_bytes: Some(memory_bytes),
                cpu_millis: None,
            },
        )
    };
    assert_eq!(ask(8 * GIB), Err(Fallback::OverCap));
    assert_eq!(ask(12 * GIB), Err(Fallback::OverCap));
    assert_eq!(ask(8 * GIB - 1).unwrap().memory_bytes, 8 * GIB - 1);
}

// ---- 2.4 hints --------------------------------------------------------------------------

/// Rule 2.4: the client's hint is never lowered, for memory and CPU.
#[test]
fn a_hint_is_never_lowered() {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &success(500 * MIB), 5);
    let ask = |memory_bytes, cpu_millis| {
        s.estimate(
            &k,
            DAY,
            &Bounds::default(),
            &Hint {
                memory_bytes,
                cpu_millis,
            },
        )
        .unwrap()
    };
    let est = ask(Some(12 * GIB), Some(4000));
    assert_eq!((est.memory_bytes, est.cpu_millis), (12 * GIB, 4000));
    let est = ask(Some(512 * MIB), Some(500));
    // The key has no CPU sample, so it is counted at the cold cost; the hint of 500 m
    // is below it.
    assert_eq!((est.memory_bytes, est.cpu_millis), (GIB, COLD_CPU_MILLIS));
    assert_eq!(ask(None, None).memory_bytes, GIB);
}

// ---- 3.3 CPU -----------------------------------------------------------------------------

fn cpu_of(runs: &[(u64, Option<u16>, usize)], bounds: &Bounds) -> u32 {
    let mut s = Summaries::default();
    let k = linux(1);
    for (par, pressure, times) in runs {
        for _ in 0..*times {
            s.observe(&k, &busy(*par, *pressure), DAY, bounds);
        }
    }
    s.estimate(&k, DAY, bounds, &Hint::default())
        .unwrap()
        .cpu_millis
}

/// Rule: a sample taken at or above the pressure threshold, or with no reading, below the
/// key's estimate is dropped, and the threshold is exclusive: one permille under it the
/// sample is always kept. A key with samples compares against its own estimate.
#[test]
fn a_pressured_sample_below_the_estimate_is_dropped() {
    let b = Bounds::default();
    // The key learned 4 cores on quiet nodes; 1-core samples under pressure do not lower it.
    for pressure in [Some(500), None, Some(CPU_PRESSURE_MAX_PERMILLE)] {
        assert_eq!(
            cpu_of(&[(4000, Some(0), 32), (1000, pressure, 32)], &b),
            4000,
            "{pressure:?}"
        );
    }
    // The same samples on a quiet node do lower it.
    assert_eq!(
        cpu_of(
            &[
                (4000, Some(0), 32),
                (1000, Some(CPU_PRESSURE_MAX_PERMILLE - 1), 32)
            ],
            &b
        ),
        1000
    );
    let mut s = Summaries::default();
    let k = linux(1);
    feed_on(&mut s, &k, &busy(4000, Some(0)), 3, DAY);
    let seen = s.observe(&k, &busy(1000, Some(500)), DAY, &b);
    assert_eq!((seen.cpu, seen.memory), (CpuSample::Dropped, Ok(())));
    // Equal to the estimate is not above it.
    let seen = s.observe(&k, &busy(4000, None), DAY, &b);
    assert_eq!(seen.cpu, CpuSample::Dropped);
    assert!(!seen.cpu.entered());
}

/// Rule: a pressured sample above the estimate is kept and raises it.
#[test]
fn a_pressured_sample_above_the_estimate_raises_it() {
    let b = Bounds::default();
    assert_eq!(
        cpu_of(&[(1000, Some(0), 32), (3000, Some(500), 32)], &b),
        3000
    );
    let mut s = Summaries::default();
    let k = linux(1);
    feed_on(&mut s, &k, &busy(1000, Some(0)), 3, DAY);
    let seen = s.observe(&k, &busy(1250, Some(900)), DAY, &b);
    assert_eq!(seen.cpu, CpuSample::Raised);
    assert!(seen.cpu.entered());
    // A quiet sample is Kept, not Raised, even when it is larger.
    let seen = s.observe(&k, &busy(2000, Some(0)), DAY, &b);
    assert_eq!(seen.cpu, CpuSample::Kept);
}

/// Rule: an unknown pressure follows the rule of a high one, in both directions.
#[test]
fn an_unknown_pressure_sample_is_kept_only_above_the_estimate() {
    let b = Bounds::default();
    assert_eq!(
        cpu_of(&[(2000, Some(0), 32), (3000, None, 32)], &b),
        3000,
        "above the estimate: kept"
    );
    assert_eq!(
        cpu_of(&[(2000, Some(0), 32), (1000, None, 32)], &b),
        2000,
        "below the estimate: dropped"
    );
    let mut s = Summaries::default();
    let k = linux(1);
    feed_on(&mut s, &k, &busy(2000, Some(0)), 3, DAY);
    assert_eq!(
        s.observe(&k, &busy(3000, None), DAY, &b).cpu,
        CpuSample::Raised
    );
    assert_eq!(
        s.observe(&k, &busy(500, None), DAY, &b).cpu,
        CpuSample::Dropped
    );
}

/// Rule: a key with no CPU sample compares a pressured sample with the cold cost (two
/// cores): above it is learned, at or below it is not, and the key stays at the cold cost.
#[test]
fn a_key_with_no_estimate_compares_with_the_cold_cost() {
    let b = Bounds::default();
    assert_eq!(COLD_CPU_MILLIS, 2000);
    let k = linux(1);
    let mut s = Summaries::default();
    let at = s.observe(&k, &busy(COLD_CPU_MILLIS.into(), Some(900)), DAY, &b);
    assert_eq!(at.cpu, CpuSample::Dropped);
    let below = s.observe(&k, &busy(300, None), DAY, &b);
    assert_eq!(below.cpu, CpuSample::Dropped);
    assert!(!s.get(&k).unwrap().has_cpu_sample());
    assert_eq!(estimate(&s, &k).unwrap().cpu_millis, COLD_CPU_MILLIS);
    let above = s.observe(&k, &busy(2001, Some(900)), DAY, &b);
    assert_eq!(above.cpu, CpuSample::Raised);
    assert!(s.get(&k).unwrap().has_cpu_sample());
    // The cold cost is within the node: on a node of 1 core, 1500 m exceeds it.
    let small = Bounds {
        node_cpu_millis: 1000,
        ..b
    };
    let k2 = linux(2);
    assert_eq!(
        s.observe(&k2, &busy(1001, None), DAY, &small).cpu,
        CpuSample::Raised
    );
    assert_eq!(
        s.observe(&linux(3), &busy(1000, None), DAY, &small).cpu,
        CpuSample::Dropped
    );
}

/// Rule: the counters count each case once per run, and a key counts as unsampled
/// until a CPU sample enters it.
#[test]
fn cpu_counters_count_each_case() {
    let b = Bounds::default();
    let mut s = Summaries::default();
    assert_eq!(s.cpu_counts(), CpuCounts::default());
    let k = linux(1);
    s.observe(&k, &busy(300, Some(900)), DAY, &b); // dropped (cold 2000)
    assert_eq!(s.unsampled_keys(), 2, "the key and its L2");
    s.observe(&k, &busy(1000, Some(0)), DAY, &b); // kept
    s.observe(&k, &busy(1000, Some(0)), DAY, &b); // kept
    s.observe(&k, &busy(3000, None), DAY, &b); // kept under pressure
    s.observe(&k, &busy(100, None), DAY, &b); // dropped
    s.observe(&k, &success(GIB), DAY, &b); // no CPU time: not counted
    assert_eq!(
        s.cpu_counts(),
        CpuCounts {
            kept: 2,
            kept_under_pressure: 1,
            dropped: 2
        }
    );
    assert_eq!(s.unsampled_keys(), 0);
    s.observe(&linux(2), &success(GIB), DAY, &b);
    assert_eq!(
        s.unsampled_keys(),
        1,
        "the new key; its L2 is already sampled"
    );
    let failed = Run {
        outcome: Outcome::Failed,
        ..busy(4000, Some(0))
    };
    assert_eq!(s.observe(&k, &failed, DAY, &b).cpu, CpuSample::Skipped);
    let short = Run {
        wall_us: 10,
        ..busy(4000, Some(0))
    };
    assert_eq!(s.observe(&k, &short, DAY, &b).cpu, CpuSample::Skipped);
    assert_eq!(s.cpu_counts().kept, 2);
}

/// Rule 3.3: the estimate is the nearest-rank median (P50) of the last 32 samples, each
/// the mean CPU of one run: the ceil(n / 2)-th smallest.
#[test]
fn cpu_is_the_p50_of_the_recent_samples() {
    let b = Bounds::default();
    // 32 samples: the median is the 16th smallest.
    assert_eq!(
        cpu_of(&[(1500, Some(0), 16), (4000, Some(0), 16)], &b),
        1500
    );
    assert_eq!(
        cpu_of(&[(1500, Some(0), 15), (4000, Some(0), 17)], &b),
        4000
    );
    // A burst of heavy runs in a light key does not move the booking.
    assert_eq!(cpu_of(&[(1500, Some(0), 29), (4000, Some(0), 3)], &b), 1500);
    // Few samples: 2 are ranked 1st (the lower), 3 are ranked 2nd (the middle).
    assert_eq!(cpu_of(&[(1000, Some(0), 1), (4000, Some(0), 1)], &b), 1000);
    assert_eq!(
        cpu_of(
            &[(1000, Some(0), 1), (4000, Some(0), 1), (2000, Some(0), 1)],
            &b
        ),
        2000
    );
    // Only the last 32 count: 32 heavy samples are replaced by 32 light ones.
    assert_eq!(
        cpu_of(&[(4000, Some(0), 32), (1000, Some(0), 32)], &b),
        1000
    );
}

/// Rule: a 0.3-core action costs 500 m (rounded up to the next 250 m), not a whole core.
#[test]
fn a_light_key_costs_half_a_core() {
    let b = Bounds::default();
    assert_eq!(cpu_of(&[(300, Some(0), 10)], &b), 500);
    assert_eq!(cpu_of(&[(250, Some(0), 10)], &b), 250);
    assert_eq!(cpu_of(&[(251, Some(0), 10)], &b), 500);
}

/// Rule 3.3: rounded up to 250 m, never below 250 m, never above the node.
#[test]
fn cpu_is_rounded_up_and_clamped() {
    let b = Bounds::default();
    assert_eq!(cpu_of(&[(1501, Some(0), 10)], &b), 1750);
    assert_eq!(cpu_of(&[(1750, Some(0), 10)], &b), 1750);
    assert_eq!(cpu_of(&[(200, Some(0), 10)], &b), CPU_MIN_MILLIS);
    assert_eq!(CPU_MIN_MILLIS, 250);
    assert_eq!(cpu_of(&[(1, Some(0), 10)], &b), 250);
    let small = Bounds {
        node_cpu_millis: 16_000,
        ..b
    };
    assert_eq!(cpu_of(&[(64_000, Some(0), 10)], &small), 16_000);
    let tiny = Bounds {
        node_cpu_millis: 500,
        ..b
    };
    assert_eq!(cpu_of(&[(64_000, Some(0), 10)], &tiny), 500);
}

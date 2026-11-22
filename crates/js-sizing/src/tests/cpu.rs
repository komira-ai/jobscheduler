//! CPU samples from runs held at a ceiling, mixed keys' memory, and the memory a key's
//! largest samples ask for.

use super::*;

/// A run whose CPU was held at a ceiling: `par` millicores is the mean it reached.
fn held(par: u64, pressure: Option<u16>) -> Run {
    Run {
        cpu_held: true,
        ..busy(par, pressure)
    }
}

fn cpu_estimate(s: &Summaries, k: &Key) -> u32 {
    estimate(s, k).unwrap().cpu_millis
}

/// Rule: a held sample at or below the key's estimate is dropped, so held runs, however
/// many, never lower the estimate the quiet runs set. Equal to the estimate is dropped.
#[test]
fn a_held_sample_at_or_below_the_estimate_is_dropped_and_cannot_lower_it() {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &busy(4000, Some(0)), 32);
    for par in [1000, 4000] {
        let seen = s.observe(&k, &held(par, Some(0)), DAY, &Bounds::default());
        assert_eq!(seen.cpu, CpuSample::Dropped, "{par}");
        assert!(!seen.cpu.entered());
    }
    feed(&mut s, &k, &held(1000, Some(0)), 64);
    assert_eq!(cpu_estimate(&s, &k), 4000);
}

/// Rule: a held sample above the key's estimate is kept and raises it, because the
/// run would have used at least that much.
#[test]
fn a_held_sample_above_the_estimate_raises_it() {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &busy(1000, Some(0)), 32);
    let seen = s.observe(&k, &held(3000, Some(0)), DAY, &Bounds::default());
    assert_eq!(seen.cpu, CpuSample::Raised);
    assert!(seen.cpu.entered());
    feed(&mut s, &k, &held(3000, Some(0)), 31);
    assert_eq!(cpu_estimate(&s, &k), 3000);
}

/// Rule: a held sample on a quiet node (pressure `Some(0)`) is still only a lower
/// bound: it is dropped on a cold key at the cold cost, where the same run unheld is
/// kept.
#[test]
fn a_held_quiet_sample_is_still_only_a_lower_bound() {
    let mut s = Summaries::default();
    let k = linux(1);
    let b = Bounds::default();
    let seen = s.observe(&k, &held(u64::from(COLD_CPU_MILLIS), Some(0)), DAY, &b);
    assert_eq!(seen.cpu, CpuSample::Dropped);
    assert!(!s.get(&k).unwrap().has_cpu_sample());
    let seen = s.observe(&k, &busy(u64::from(COLD_CPU_MILLIS), Some(0)), DAY, &b);
    assert_eq!(seen.cpu, CpuSample::Kept, "the same run unheld is kept");
}

/// Rule: a ring full of samples at a low cap is not held there. From a ring of 32
/// samples of 250 m, the P90 (the 29th smallest) is the real use of 1000 m once four
/// unheld quiet runs of 1000 m have come: after three it is still 250 m, after four
/// 1000 m, and held runs in between change nothing.
#[test]
fn a_ring_of_250m_samples_takes_the_real_use_within_four_unheld_runs() {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &busy(250, Some(0)), 32);
    assert_eq!(cpu_estimate(&s, &k), 250);
    for _ in 0..3 {
        feed(&mut s, &k, &held(250, Some(0)), 1);
        feed(&mut s, &k, &busy(1000, Some(0)), 1);
    }
    assert_eq!(cpu_estimate(&s, &k), 250, "three runs are not yet the P90");
    feed(&mut s, &k, &busy(1000, Some(0)), 1);
    assert_eq!(cpu_estimate(&s, &k), 1000, "the fourth run is");
}

// ---- mixed keys' memory ---------------------------------------------------------------

/// Rule: a mixed key whose largest recent peak with the margin is above the default is
/// booked at that (3.5 GiB x 1.2 + 128 MiB rounded up to a step); one below the
/// default stays at the default.
#[test]
fn a_mixed_key_is_booked_at_its_own_largest_peak_when_that_is_above_the_default() {
    let k = linux(1);
    let mut s = Summaries::default();
    feed(&mut s, &k, &success(500 * MIB), 10);
    feed(&mut s, &k, &success(3 * GIB + GIB / 2), 1);
    assert!(s.get(&k).unwrap().mixed());
    assert_eq!(memory(&s, &k), 4 * GIB + GIB / 2);

    let mut s = Summaries::default();
    feed(&mut s, &k, &success(500 * MIB), 10);
    feed(&mut s, &k, &success(5 * GIB / 2), 1);
    assert!(s.get(&k).unwrap().mixed());
    assert_eq!(
        memory(&s, &k),
        4 * GIB,
        "its sized 3.25 GiB is under the default"
    );
}

// ---- peak_memory ----------------------------------------------------------------------

fn peak(s: &Summaries, k: &Key) -> Option<u64> {
    s.peak_memory(k, DAY, &Bounds::default())
}

/// Rule: with no sample there is no peak, and one sample answers with its maximum plus
/// the margin, rounded up to a step and at least 1 GiB, whatever `N_MIN`.
#[test]
fn peak_memory_is_none_without_a_sample_and_one_sample_answers() {
    let mut s = Summaries::default();
    let k = linux(1);
    assert_eq!(peak(&s, &k), None);
    feed(&mut s, &k, &success(3 * GIB), 1);
    // 3 GiB x 1.2 + 128 MiB = 3.725 GiB, rounded up to 3.75 GiB.
    assert_eq!(peak(&s, &k), Some(3 * GIB + 3 * GIB / 4));
    assert_eq!(
        estimate(&s, &k).map(|e| e.memory_bytes),
        Err(Fallback::Cold),
        "one sample is no estimate"
    );
    let mut small = Summaries::default();
    feed(&mut small, &k, &success(100 * MIB), 1);
    assert_eq!(peak(&small, &k), Some(MIN_MEMORY));
}

/// Rule: a mixed key is answered at its sized peak, not at the default the estimate
/// gives it.
#[test]
fn peak_memory_ignores_the_mixed_rule_and_the_default() {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &success(500 * MIB), 10);
    feed(&mut s, &k, &success(5 * GIB / 2), 1);
    assert!(s.get(&k).unwrap().mixed());
    assert_eq!(memory(&s, &k), 4 * GIB);
    assert_eq!(peak(&s, &k), Some(3 * GIB + GIB / 4));
}

/// Rule: a sample that hit its limit (held) and a kill floor count in the answer.
#[test]
fn peak_memory_includes_held_samples_and_the_floor() {
    let mut s = Summaries::default();
    let k = linux(1);
    let at_limit = Run {
        limit_hits: 1,
        booked_memory_bytes: 3 * GIB,
        ..success(3 * GIB)
    };
    feed(&mut s, &k, &at_limit, 2);
    assert_eq!(peak(&s, &k), Some(3 * GIB), "the held peak, no margin");
    s.observe(&k, &heavy_kill(), DAY, &Bounds::default());
    assert_eq!(
        peak(&s, &k),
        Some(16 * GIB),
        "the floor is above the sized peak"
    );
}

/// Rule: the answer is the larger of the levels: L2 holds every config's runs, so a
/// config with small runs sees its sibling's peak.
#[test]
fn peak_memory_takes_the_larger_of_the_levels() {
    let mut s = Summaries::default();
    feed(&mut s, &linux(1), &success(500 * MIB), 3);
    feed(&mut s, &linux(2), &success(3 * GIB), 1);
    // L1 of config 1 alone would be 1 GiB; L2 (3.75 GiB) is larger.
    assert_eq!(peak(&s, &linux(1)), Some(3 * GIB + 3 * GIB / 4));
    assert_eq!(peak(&s, &linux(2)), Some(3 * GIB + 3 * GIB / 4));
}

/// Rule: a key not seen for longer than `STALE_DAYS` has no fresh summary: `None`.
#[test]
fn peak_memory_is_none_for_a_stale_key() {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &success(GIB), 3);
    assert!(peak(&s, &k).is_some());
    let later = Day(DAY.0 + STALE_DAYS + 1);
    assert_eq!(s.peak_memory(&k, later, &Bounds::default()), None);
}

/// Rule: `Summaries::cpu` answers from the first fresh level with a CPU sample, even with a
/// single run (too few for an estimate), and is `None` with no CPU sample or once stale.
#[test]
fn cpu_answers_from_one_sample_and_not_without_one_or_when_stale() {
    let mut s = Summaries::default();
    let k = linux(1);
    let bounds = Bounds::default();
    assert_eq!(s.cpu(&k, DAY, &bounds), None);
    feed(&mut s, &k, &success(GIB), 1);
    assert_eq!(s.cpu(&k, DAY, &bounds), None, "a run with no CPU time");
    feed(&mut s, &k, &busy(1300, Some(0)), 1);
    assert_eq!(estimate(&s, &k).map(|e| e.cpu_millis), Ok(1500));
    let other = linux(2);
    feed(&mut s, &other, &busy(700, Some(0)), 1);
    assert_eq!(
        s.cpu(&other, DAY, &bounds),
        Some(750),
        "its own level first"
    );
    let unseen = linux(3);
    assert_eq!(
        s.cpu(&unseen, DAY, &bounds),
        Some(1500),
        "then L2, which holds both"
    );
    let lone = key(Basis::Cgroup, None);
    let mut t = Summaries::default();
    feed(&mut t, &lone, &busy(700, Some(0)), 1);
    assert_eq!(t.cpu(&lone, DAY, &bounds), Some(750));
    assert!(matches!(
        t.estimate(&lone, DAY, &bounds, &Hint::default()),
        Err(Fallback::Cold)
    ));
    let late = Day(DAY.0 + STALE_DAYS + 1);
    assert_eq!(t.cpu(&lone, late, &bounds), None);
}

/// Rule: a run with a measured peak is sampled at its peak, not its mean: quiet runs that
/// averaged 5 cores and peaked at 20 make the estimate 20 cores; a held run's peak is still
/// only a lower bound; a run that reports a peak of 0 and no CPU time is no CPU sample.
#[test]
fn a_run_with_a_peak_is_sampled_at_its_peak() {
    let mut s = Summaries::default();
    let k = linux(1);
    let peaked = Run {
        cpu_peak_millis: Some(20_000),
        ..busy(5000, Some(0))
    };
    let seen = s.observe(&k, &peaked, DAY, &Bounds::default());
    assert_eq!(seen.cpu, CpuSample::Kept);
    assert_eq!(s.cpu(&k, DAY, &Bounds::default()), Some(20_000));
    feed(&mut s, &k, &peaked, 1);
    assert_eq!(cpu_estimate(&s, &k), 20_000);
    let held_low = Run {
        cpu_peak_millis: Some(8000),
        ..held(5000, Some(0))
    };
    assert_eq!(
        s.observe(&k, &held_low, DAY, &Bounds::default()).cpu,
        CpuSample::Dropped
    );
    assert_eq!(cpu_estimate(&s, &k), 20_000);
    let nothing = Run {
        cpu_peak_millis: Some(0),
        ..busy(0, Some(0))
    };
    assert_eq!(
        s.observe(&k, &nothing, DAY, &Bounds::default()).cpu,
        CpuSample::Skipped
    );
}

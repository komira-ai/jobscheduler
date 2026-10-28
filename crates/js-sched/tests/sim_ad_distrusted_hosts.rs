//! Sim ad: distrusted hosts. A fallback host (no usable measurement) and a host whose
//! measured covered CPU exceeds the predicted sum of its covered leases (`M > P_c`) look
//! fuller than a trusted host with the same leases, so best fit alone would pack them
//! hardest. Key 3 puts them last.
//!
//! Logical time only; no assertion reads a clock.

#[path = "load_rig/mod.rs"]
mod load_rig;
#[path = "support_score_z_ad/mod.rs"]
mod support_score_z_ad;

use js_sched::LoadConfig;
use load_rig::{GIB, Rig, w, warm};
use support_score_z_ad::{hold, host, place};

/// The lease on both hosts: four of 4 cores and 8 GiB, so 16 cores and 32 GiB predicted.
const HELD: usize = 4;
const WORKING_SET: u64 = 32 * GIB;

/// Two hosts named `first` and `second`, each holding the same four leases.
fn pair(first: &str, second: &str) -> Rig {
    // These show the score's key 3, which reads how full a host looks. They keep the floor
    // `f` of 750 they were written for: at 1000 a trusted host measured below its
    // predictions is charged those predictions, as a fallback host is, and no host looks
    // fuller than the other for being unmeasured.
    let mut r = Rig::new(LoadConfig {
        cpu_floor_permille: 750,
        ..LoadConfig::default()
    });
    r.up(first, host(352));
    r.up(second, host(352));
    hold(&mut r, first, &[second], HELD, warm(4_000, 8 * GIB));
    hold(&mut r, second, &[first], HELD, warm(4_000, 8 * GIB));
    r
}

/// Two windows of readings for `name`: `busy` millicores over its covered leases.
fn measure(r: &mut Rig, names: &[(&str, u64)]) {
    let held: Vec<_> = names.iter().map(|(n, _)| r.held(n)).collect();
    for _ in 0..2 {
        for ((name, busy), held) in names.iter().zip(&held) {
            r.window(name, *busy, WORKING_SET, held);
        }
    }
}

/// Key 3, fallback. `fallback` has no reading and is charged its full 16 cores; `fresh`
/// has a measurement of 14 cores and is charged that. The predicted load is the same, the
/// fallback host looks fuller, and best fit alone would take it. The lease goes to the
/// fresh host, whichever host is named first.
#[test]
fn a_fallback_host_loses_to_a_fresh_host_of_equal_predicted_load() {
    for (fallback, fresh) in [("A", "B"), ("B", "A")] {
        let mut r = pair(fallback, fresh);
        measure(&mut r, &[(fresh, 14_000)]);
        let (f, g) = (
            r.s.load_view(&w(fallback)).unwrap(),
            r.s.load_view(&w(fresh)).unwrap(),
        );
        assert!(f.fallback && !g.fallback);
        assert_eq!(f.predicted_cpu_millis, g.predicted_cpu_millis);
        assert_eq!(f.predicted_memory_bytes, g.predicted_memory_bytes);
        assert!(
            f.left_cpu_millis > g.left_cpu_millis,
            "the fallback host looks fuller: {} against {}",
            f.left_cpu_millis,
            g.left_cpu_millis
        );
        assert_eq!(place(&mut r, warm(4_000, 8 * GIB)), fresh);
        assert_eq!(r.s.load_counters().chose_distrusted, 0);
    }
}

/// Key 3, `M > P_c`. Two fresh hosts with 16 cores predicted each. On `above` the
/// measured covered CPU is 18 cores, above the predicted sum; on the other it is 16 (equal
/// is not above) or 14 (below). `above` is charged the more and fits better, and the lease
/// goes to the other host.
#[test]
fn a_host_measured_above_its_predictions_loses_to_one_at_or_below() {
    for (above, other) in [("A", "B"), ("B", "A")] {
        for measured in [16_000, 14_000] {
            let mut r = pair(above, other);
            measure(&mut r, &[(above, 18_000), (other, measured)]);
            let (x, y) = (
                r.s.load_view(&w(above)).unwrap(),
                r.s.load_view(&w(other)).unwrap(),
            );
            assert!(!x.fallback && !y.fallback);
            assert_eq!(x.predicted_cpu_millis, y.predicted_cpu_millis);
            assert!(x.measured_cpu_millis.unwrap() > 16_000);
            assert!(y.measured_cpu_millis.unwrap() <= 16_000);
            assert!(
                x.left_cpu_millis > y.left_cpu_millis,
                "the measured-above host looks fuller"
            );
            assert_eq!(
                place(&mut r, warm(4_000, 8 * GIB)),
                other,
                "other measured {measured}"
            );
        }
    }
}

/// The premise of the two tests above: with both hosts trusted the fuller one takes the
/// lease, so it is key 3 and not the measurement that moves the lease. Measured 16 and 14
/// cores: the host at 16 takes it.
#[test]
fn between_trusted_hosts_the_fuller_still_takes_the_lease() {
    for (full, light) in [("A", "B"), ("B", "A")] {
        let mut r = pair(full, light);
        measure(&mut r, &[(full, 16_000), (light, 14_000)]);
        assert_eq!(place(&mut r, warm(4_000, 8 * GIB)), full);
    }
}

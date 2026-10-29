//! Conversions into the types admission reads (`js_types::sched::usage`).

use js_types::sched::LeaseId as SchedLeaseId;
use js_types::sched::usage::{Curve, CurveTooLong, LeaseClock, LeaseMinute, NodeLoad};

use crate::fit::Fit;
use crate::input::{ClockIn, LoadIn, MinuteIn};

/// The scheduler's form of a lease id. The two types carry the same pair and stay apart
/// until the scheduler moves into this repository.
fn sched_lease(id: js_types::LeaseId) -> SchedLeaseId {
    SchedLeaseId::new(id.term(), id.sequence())
}

impl From<MinuteIn> for LeaseMinute {
    fn from(m: MinuteIn) -> Self {
        Self {
            lease: sched_lease(m.lease),
            minute: u32::from(m.minute),
            cpu_mean_millis: m.mean_cores_milli,
            cpu_peak_window_millis: m.peak_window_milli,
            ws_max_bytes: m.working_set_max_bytes,
        }
    }
}

impl From<ClockIn> for LeaseClock {
    fn from(c: ClockIn) -> Self {
        Self {
            lease: sched_lease(c.lease),
            started_mono_usec: c.started_mono_usec,
        }
    }
}

impl From<LoadIn> for NodeLoad {
    /// The reading admission reads. What a [`LoadIn`] does not carry is 0: the active
    /// file pages of the working set (so none is taken off it), and the throttled time
    /// and CPU pressure, which are telemetry only.
    fn from(l: LoadIn) -> Self {
        Self {
            seq: l.seq,
            mono_usec: l.mono_usec,
            cpu_usage_usec: l.cpu_usage_usec,
            memory_working_set_bytes: l.working_set_bytes,
            memory_active_file_bytes: 0,
            covered: l.covered.into_iter().map(sched_lease).collect(),
            actions_cpu_limited: l.actions_cpu_limited,
            cpu_throttled_usec: 0,
            cpu_pressure_some_total_usec: 0,
            lease_clocks: l.clocks.into_iter().map(LeaseClock::from).collect(),
            minutes: l.minutes.into_iter().map(LeaseMinute::from).collect(),
        }
    }
}

impl TryFrom<&Fit> for Curve {
    type Error = CurveTooLong;

    /// The curve of a fit: its points, from its runs; one whose cores were not learned
    /// is a memory-only curve. A fit holds at most 64 points by construction; the limit
    /// is checked anyway and is never cut to fit.
    fn try_from(fit: &Fit) -> Result<Self, CurveTooLong> {
        Curve::new(fit.points.clone(), fit.runs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use js_types::LeaseId;
    use js_types::sched::usage::MinutePoint;

    /// Catches: points reordered or cut, the run count lost, and a fit over the limit
    /// silently truncated to 64 instead of refused.
    #[test]
    fn a_fit_becomes_the_curve_with_its_points_and_runs() {
        let p = |c, w| MinutePoint {
            cores_milli: c,
            working_set_bytes: w,
        };
        let fit = Fit {
            points: vec![p(1, 2), p(3, 4)],
            cores_learned: true,
            runs: 7,
            wall_minutes_p90: 2,
        };
        let curve = Curve::try_from(&fit).unwrap();
        assert_eq!(curve.points(), &[p(1, 2), p(3, 4)]);
        assert_eq!(curve.runs(), 7);
        assert!(curve.cores_known());

        let memory = Fit {
            cores_learned: false,
            ..fit.clone()
        };
        let curve = Curve::try_from(&memory).unwrap();
        assert!(!curve.cores_known(), "a fit without cores says so");
        assert_eq!(curve.points(), &[p(1, 2), p(3, 4)]);

        let long = Fit {
            points: vec![p(0, 0); 65],
            cores_learned: true,
            runs: 3,
            wall_minutes_p90: 1,
        };
        assert_eq!(Curve::try_from(&long), Err(CurveTooLong { len: 65 }));
    }

    fn sched(term: u64, seq: u64) -> SchedLeaseId {
        SchedLeaseId::new(term, seq)
    }

    /// Catches: a field of the reading dropped, swapped for another of the same type
    /// (the three u64 counters and the two minute figures are all alike), or the lease
    /// records not converted; and the documented zeros not zero.
    #[test]
    fn a_reading_becomes_the_node_load_field_for_field() {
        let a = LeaseId::new(1, 2);
        let b = LeaseId::new(3, 4);
        let load = LoadIn {
            seq: 11,
            mono_usec: 22,
            cpu_usage_usec: 33,
            working_set_bytes: 44,
            actions_cpu_limited: true,
            covered: vec![a, b],
            minutes: vec![MinuteIn {
                lease: b,
                minute: 7,
                mean_cores_milli: 100,
                peak_window_milli: 200,
                working_set_max_bytes: 300,
            }],
            clocks: vec![ClockIn {
                lease: a,
                started_mono_usec: 55,
            }],
        };
        let n = NodeLoad::from(load);
        assert_eq!(
            (
                n.seq,
                n.mono_usec,
                n.cpu_usage_usec,
                n.memory_working_set_bytes
            ),
            (11, 22, 33, 44)
        );
        assert!(n.actions_cpu_limited);
        assert_eq!(n.covered, vec![sched(1, 2), sched(3, 4)]);
        assert_eq!(
            n.minutes,
            vec![LeaseMinute {
                lease: sched(3, 4),
                minute: 7,
                cpu_mean_millis: 100,
                cpu_peak_window_millis: 200,
                ws_max_bytes: 300,
            }]
        );
        assert_eq!(
            n.lease_clocks,
            vec![LeaseClock {
                lease: sched(1, 2),
                started_mono_usec: 55
            }]
        );
        assert_eq!(
            (
                n.memory_active_file_bytes,
                n.cpu_throttled_usec,
                n.cpu_pressure_some_total_usec
            ),
            (0, 0, 0)
        );
    }
}

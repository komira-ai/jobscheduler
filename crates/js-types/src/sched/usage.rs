//! Measured and predicted load: the types a node reports about itself, the per-minute
//! record of one lease, and the learned cost curve of an action key.
//!
//! These are plain values with pure arithmetic. Collection fills them (an agent samples
//! its cgroups and sends them in its heartbeat), and admission reads them; neither lives
//! here.

use thiserror::Error;

use super::{LeaseId, WorkKey};

/// A signal older than this many windows is stale.
pub const STALE_WINDOWS: u64 = 3;

/// The most points a [`Curve`] holds: minutes 0 to 63 since the lease's start.
pub const MAX_CURVE_POINTS: usize = 64;

/// One reading of the node's action cgroup, taken by the agent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeLoad {
    /// Counts readings within one agent process, one more for each. A reading whose `seq`
    /// is not above the last accepted one is dropped; a new session starts again.
    pub seq: u64,
    /// The agent's monotonic clock, in microseconds, when the reading was taken. The only
    /// time base used for windows.
    pub mono_usec: u64,
    /// Cumulative CPU time of the actions cgroup, in microseconds. Descendants removed
    /// since an earlier reading are still included.
    pub cpu_usage_usec: u64,
    /// Memory in use by the actions cgroup that cannot simply be dropped: `memory.current`
    /// minus the inactive file pages.
    pub working_set_bytes: u64,
    /// The actions cgroup's active file pages (`active_file` of `memory.stat`), in bytes.
    /// Hot page cache that `working_set_bytes` still counts; admission caps this part.
    pub active_file_bytes: u64,
    /// The leases that ran for the whole window since the previous reading, by the
    /// agent's own start.
    pub covered: Vec<LeaseId>,
    /// Whether the actions cgroup has a CPU limit.
    pub actions_cpu_limited: bool,
    /// Cumulative time the actions cgroup was throttled, in microseconds. Telemetry only.
    pub throttled_usec: u64,
    /// Cumulative `some` total of the CPU pressure, in microseconds. Telemetry only.
    pub pressure_some_total_usec: u64,
}

impl NodeLoad {
    /// The cores busy between `earlier` and this reading, in millicores, rounded down and
    /// saturating at `u32::MAX`. `None` when `seq` does not increase, a counter or the
    /// clock went backwards, or the window is empty.
    #[must_use]
    pub fn busy_millicores_since(&self, earlier: &Self) -> Option<u32> {
        if self.seq <= earlier.seq {
            return None;
        }
        let window = self.mono_usec.checked_sub(earlier.mono_usec)?;
        let used = self.cpu_usage_usec.checked_sub(earlier.cpu_usage_usec)?;
        if window == 0 {
            return None;
        }
        let milli = u128::from(used) * 1000 / u128::from(window);
        Some(u32::try_from(milli).unwrap_or(u32::MAX))
    }

    /// The working set admission counts: `working_set_bytes` less the active file pages
    /// beyond `file_share_permille` thousandths of `mem_capacity_bytes`, which are page
    /// cache and not demand. Saturates at 0.
    #[must_use]
    pub fn admission_working_set(&self, mem_capacity_bytes: u64, file_share_permille: u32) -> u64 {
        let share = u128::from(mem_capacity_bytes) * u128::from(file_share_permille) / 1000;
        let excess = u128::from(self.active_file_bytes).saturating_sub(share);
        let excess = u64::try_from(excess).unwrap_or(u64::MAX);
        self.working_set_bytes.saturating_sub(excess)
    }

    /// Whether this reading is older than [`STALE_WINDOWS`] windows of `window_usec` at
    /// `now_mono_usec` on the same clock. A reading from the future is not stale.
    #[must_use]
    pub fn is_stale(&self, window_usec: u64, now_mono_usec: u64) -> bool {
        let limit = window_usec.saturating_mul(STALE_WINDOWS);
        now_mono_usec.saturating_sub(self.mono_usec) > limit
    }
}

/// What one lease used during one minute of its own life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeaseMinute {
    /// The lease.
    pub lease: LeaseId,
    /// The minute since the lease's own start, from 0.
    pub minute: u16,
    /// Mean cores over the minute, in millicores.
    pub mean_cores_milli: u32,
    /// The busiest single sampling window of the minute, in millicores.
    pub peak_window_milli: u32,
    /// The largest working set seen in the minute, in bytes.
    pub working_set_max_bytes: u64,
}

/// When a running lease started, on the agent's monotonic clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeaseClock {
    /// The lease.
    pub lease: LeaseId,
    /// The agent's monotonic clock, in microseconds, at the lease's actual start.
    pub started_mono_usec: u64,
}

/// One minute of a [`Curve`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MinutePoint {
    /// Cores, in millicores.
    pub cores_milli: u32,
    /// Working set, in bytes.
    pub working_set_bytes: u64,
}

/// A curve is longer than [`MAX_CURVE_POINTS`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[error("a curve holds at most {MAX_CURVE_POINTS} points, got {len}")]
pub struct CurveTooLong {
    /// The number of points given.
    pub len: usize,
}

/// The learned load of an action key by minute since its start.
///
/// Point `i` is minute `i`; the last point stands for every later minute, so a run that
/// outlives the curve is charged the last point. At most [`MAX_CURVE_POINTS`] points.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Curve {
    points: Vec<MinutePoint>,
    runs: u16,
}

impl Curve {
    /// A curve of `points`, fitted from `runs` runs.
    ///
    /// # Errors
    /// [`CurveTooLong`] when there are more than [`MAX_CURVE_POINTS`] points.
    pub fn new(points: Vec<MinutePoint>, runs: u16) -> Result<Self, CurveTooLong> {
        if points.len() > MAX_CURVE_POINTS {
            return Err(CurveTooLong { len: points.len() });
        }
        Ok(Self { points, runs })
    }

    /// The points, minute 0 first.
    #[must_use]
    pub fn points(&self) -> &[MinutePoint] {
        &self.points
    }

    /// How many runs the curve was fitted from.
    #[must_use]
    pub const fn runs(&self) -> u16 {
        self.runs
    }

    /// The point for `minute`, the last point for a minute past the end; `None` for a
    /// curve with no points.
    #[must_use]
    pub fn at(&self, minute: u32) -> Option<MinutePoint> {
        self.remaining(minute).first().copied()
    }

    /// The points from `minute` on; for a minute past the end, just the last point,
    /// which stands for all later minutes.
    fn remaining(&self, minute: u32) -> &[MinutePoint] {
        let last = self.points.len().saturating_sub(1);
        let from = usize::try_from(minute).map_or(last, |m| m.min(last));
        &self.points[from..]
    }

    /// The most cores, in millicores, the curve asks for from `minute` on; 0 for a curve
    /// with no points.
    #[must_use]
    pub fn remaining_max_cores(&self, minute: u32) -> u32 {
        self.remaining(minute)
            .iter()
            .map(|p| p.cores_milli)
            .max()
            .unwrap_or(0)
    }

    /// The largest working set, in bytes, the curve asks for from `minute` on; 0 for a
    /// curve with no points.
    #[must_use]
    pub fn remaining_max_ws(&self, minute: u32) -> u64 {
        self.remaining(minute)
            .iter()
            .map(|p| p.working_set_bytes)
            .max()
            .unwrap_or(0)
    }
}

/// Where admission finds the learned curve of an action key.
pub trait CurveSource {
    /// The curve for `key`, or `None` when the key has none (cold, too few runs, or
    /// demoted): the caller charges it flat.
    fn curve(&self, key: &WorkKey) -> Option<Curve>;
}

/// A [`CurveSource`] that knows no curve: every key is charged flat.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NoCurves;

impl CurveSource for NoCurves {
    fn curve(&self, _key: &WorkKey) -> Option<Curve> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(cpu: u64, mono: u64) -> NodeLoad {
        NodeLoad {
            seq: 1,
            cpu_usage_usec: cpu,
            mono_usec: mono,
            ..NodeLoad::default()
        }
    }

    fn later(mut n: NodeLoad) -> NodeLoad {
        n.seq += 1;
        n
    }

    fn pt(cores_milli: u32, working_set_bytes: u64) -> MinutePoint {
        MinutePoint {
            cores_milli,
            working_set_bytes,
        }
    }

    fn curve(points: &[(u32, u64)]) -> Curve {
        let points = points.iter().map(|&(c, w)| pt(c, w)).collect();
        Curve::new(points, 5).unwrap()
    }

    #[test]
    fn a_normal_window_gives_millicores() {
        // 15 cpu-seconds in 10 seconds is 1.5 cores.
        let a = load(1_000_000, 5_000_000);
        let b = load(16_000_000, 15_000_000);
        assert_eq!(later(b).busy_millicores_since(&a), Some(1500));
        // Rounds down.
        assert_eq!(
            later(load(1, 3)).busy_millicores_since(&load(0, 0)),
            Some(333)
        );
    }

    #[test]
    fn a_counter_going_backwards_or_an_empty_window_is_none() {
        let a = load(10_000, 10_000);
        assert_eq!(later(load(9_999, 20_000)).busy_millicores_since(&a), None);
        assert_eq!(later(load(20_000, 9_999)).busy_millicores_since(&a), None);
        assert_eq!(later(load(20_000, 10_000)).busy_millicores_since(&a), None);
        assert_eq!(later(a.clone()).busy_millicores_since(&a), None);
    }

    #[test]
    fn no_cpu_in_a_window_is_zero_cores() {
        assert_eq!(
            later(load(7, 20)).busy_millicores_since(&load(7, 10)),
            Some(0)
        );
    }

    #[test]
    fn u64_edges_do_not_overflow_and_saturate() {
        let zero = load(0, 0);
        assert_eq!(
            later(load(u64::MAX, u64::MAX)).busy_millicores_since(&zero),
            Some(1000)
        );
        assert_eq!(
            later(load(u64::MAX, 1)).busy_millicores_since(&zero),
            Some(u32::MAX)
        );
        assert_eq!(
            later(load(u64::MAX, u64::MAX))
                .busy_millicores_since(&load(u64::MAX - 1, u64::MAX - 1)),
            Some(1000)
        );
    }

    #[test]
    fn a_reading_whose_seq_does_not_increase_is_none() {
        let a = load(0, 0);
        let b = load(1_000, 1_000);
        assert_eq!(b.busy_millicores_since(&a), None);
        let mut older = b.clone();
        older.seq = 0;
        assert_eq!(older.busy_millicores_since(&a), None);
        let mut next = b;
        next.seq = 2;
        assert_eq!(next.busy_millicores_since(&a), Some(1000));
    }

    fn with_file(working_set_bytes: u64, active_file_bytes: u64) -> NodeLoad {
        NodeLoad {
            working_set_bytes,
            active_file_bytes,
            ..NodeLoad::default()
        }
    }

    #[test]
    fn active_file_up_to_its_share_is_counted_in_full() {
        // Share is 25% of 1000 = 250.
        assert_eq!(with_file(800, 0).admission_working_set(1000, 250), 800);
        assert_eq!(with_file(800, 249).admission_working_set(1000, 250), 800);
        assert_eq!(with_file(800, 250).admission_working_set(1000, 250), 800);
    }

    #[test]
    fn active_file_beyond_its_share_is_taken_off() {
        assert_eq!(with_file(800, 251).admission_working_set(1000, 250), 799);
        assert_eq!(with_file(800, 600).admission_working_set(1000, 250), 450);
        // A zero share removes all of it; a zero capacity too.
        assert_eq!(with_file(800, 600).admission_working_set(1000, 0), 200);
        assert_eq!(with_file(800, 600).admission_working_set(0, 250), 200);
    }

    #[test]
    fn the_admission_working_set_saturates() {
        // The excess is larger than the working set.
        assert_eq!(with_file(100, 900).admission_working_set(1000, 250), 0);
        // Extremes do not overflow.
        assert_eq!(
            with_file(u64::MAX, u64::MAX).admission_working_set(u64::MAX, 1000),
            u64::MAX
        );
        assert_eq!(with_file(u64::MAX, u64::MAX).admission_working_set(0, 0), 0);
        assert_eq!(
            with_file(u64::MAX, u64::MAX).admission_working_set(u64::MAX, u32::MAX),
            u64::MAX
        );
    }

    #[test]
    fn stale_after_three_windows_not_at_three() {
        let r = load(0, 1_000);
        assert!(!r.is_stale(100, 1_000));
        assert!(!r.is_stale(100, 1_300));
        assert!(r.is_stale(100, 1_301));
        assert!(!r.is_stale(100, 500));
        assert!(!r.is_stale(u64::MAX, u64::MAX));
        assert!(!r.is_stale(0, 1_000));
        assert!(r.is_stale(0, 1_001));
    }

    #[test]
    fn a_curve_longer_than_the_bound_is_refused() {
        let full = vec![MinutePoint::default(); MAX_CURVE_POINTS];
        assert_eq!(
            Curve::new(full, 1).unwrap().points().len(),
            MAX_CURVE_POINTS
        );
        let over = vec![MinutePoint::default(); MAX_CURVE_POINTS + 1];
        let err = Curve::new(over, 1).unwrap_err();
        assert_eq!(err, CurveTooLong { len: 65 });
        assert_eq!(err.to_string(), "a curve holds at most 64 points, got 65");
    }

    #[test]
    fn at_clamps_past_the_end() {
        let c = curve(&[(100, 1), (900, 2), (300, 3)]);
        assert_eq!(c.runs(), 5);
        assert_eq!(c.at(0), Some(pt(100, 1)));
        assert_eq!(c.at(1), Some(pt(900, 2)));
        assert_eq!(c.at(2), Some(pt(300, 3)));
        assert_eq!(c.at(3), Some(pt(300, 3)));
        assert_eq!(c.at(u32::MAX), Some(pt(300, 3)));
    }

    #[test]
    fn remaining_max_is_taken_from_the_minute_on() {
        let c = curve(&[(100, 50), (900, 20), (300, 40), (200, 10)]);
        assert_eq!(c.remaining_max_cores(0), 900);
        assert_eq!(c.remaining_max_ws(0), 50);
        assert_eq!(c.remaining_max_cores(1), 900);
        assert_eq!(c.remaining_max_ws(1), 40);
        assert_eq!(c.remaining_max_cores(2), 300);
        assert_eq!(c.remaining_max_ws(2), 40);
        assert_eq!(c.remaining_max_cores(3), 200);
        assert_eq!(c.remaining_max_cores(1_000), 200);
        assert_eq!(c.remaining_max_ws(u32::MAX), 10);
    }

    #[test]
    fn an_empty_curve_has_no_point_and_no_demand() {
        let c = Curve::new(Vec::new(), 0).unwrap();
        assert_eq!(c.at(0), None);
        assert_eq!(c.at(u32::MAX), None);
        assert_eq!(c.remaining_max_cores(0), 0);
        assert_eq!(c.remaining_max_ws(7), 0);
    }

    #[test]
    fn no_curves_knows_no_key() {
        let key = WorkKey {
            class: "c".to_owned(),
            dedup: "d".to_owned(),
        };
        assert_eq!(NoCurves.curve(&key), None);
    }

    #[test]
    fn a_source_can_hand_out_a_curve() {
        struct One(Curve);
        impl CurveSource for One {
            fn curve(&self, key: &WorkKey) -> Option<Curve> {
                (key.class == "hit").then(|| self.0.clone())
            }
        }
        let source = One(curve(&[(1, 1)]));
        let key = |class: &str| WorkKey {
            class: class.to_owned(),
            dedup: String::new(),
        };
        assert!(source.curve(&key("hit")).is_some());
        assert!(source.curve(&key("miss")).is_none());
    }

    #[test]
    fn records_compare_by_value() {
        let lease = LeaseId::new(1, 2);
        let m = LeaseMinute {
            lease,
            minute: 3,
            mean_cores_milli: 4,
            peak_window_milli: 5,
            working_set_max_bytes: 6,
        };
        let other = LeaseMinute { minute: 4, ..m };
        assert_eq!(m, LeaseMinute { ..m });
        assert_ne!(m, other);
        let c = LeaseClock {
            lease,
            started_mono_usec: 9,
        };
        assert_ne!(
            c,
            LeaseClock {
                started_mono_usec: 10,
                ..c
            }
        );
    }
}

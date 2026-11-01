//! What the tail rule is measured by: how many leases started on a host that a whole-node
//! request had marked, how many of those were not the request's own (backfill), and how
//! far a lease's end was from the end the feed estimated for it.
//!
//! The repository has no histogram type, so the error is kept as a count, a signed sum in
//! milliseconds and signed buckets with fixed upper edges ([`ERROR_EDGES_MS`]), which is
//! what a histogram is made of. The counters live in memory and start at zero.
//!
//! Two limits of what is counted. A host keeps the bound of the last lease started on it
//! (`Record::bound`) and nothing for an earlier one, so the stop of an earlier lease on a
//! host that has started another counts nothing, neither estimated nor unestimated; with
//! one lease per host that cannot happen, and when sharing classes put many leases on a
//! host, each lease needs its own start and bound for the counters to count every one.
//! And the end of a lease is the time `stop` is called, on the registry's clock, not
//! when the work exited; a lease put back after a restart has no bound and no start, so
//! its stop counts nothing.

use js_types::RequestId;

/// The upper edges, in milliseconds, of the estimate-error buckets: bucket `i` counts the
/// errors up to and including `ERROR_EDGES_MS[i]` (and above the edge before it); the
/// last bucket counts the errors above the last edge. An error is the actual end minus the
/// estimated end, so a lease that outlives its estimate is positive.
pub const ERROR_EDGES_MS: [i64; 7] = [-300_000, -60_000, -10_000, 0, 10_000, 60_000, 300_000];

/// The counters of the tail rule ([`crate::Registry::tail_counters`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TailCounters {
    /// Leases started on a host that carried a mark, the marked request's own included.
    pub started_on_marked: u64,
    /// Of those, the leases that were not the marked request's own: work backfilled
    /// before the whole-node lease began.
    pub backfilled: u64,
    /// Leases that ended with an estimate: the number of errors counted below.
    pub ended_estimated: u64,
    /// Leases that ended with no estimate.
    pub ended_unestimated: u64,
    /// The sum of the errors, actual end minus estimated end, in milliseconds.
    pub error_sum_ms: i64,
    /// The errors by bucket ([`ERROR_EDGES_MS`]): one more bucket than there are edges.
    pub error_buckets: [u64; 8],
}

impl TailCounters {
    /// Counts a lease started by `starter` on a host whose mark is held by `holder`
    /// (`None` for a host with no mark, which counts nothing).
    pub(crate) fn started(&mut self, holder: Option<&RequestId>, starter: Option<&RequestId>) {
        let Some(holder) = holder else {
            return;
        };
        self.started_on_marked += 1;
        self.backfilled += u64::from(starter != Some(holder));
    }

    /// Counts a lease that ended at `ended_at` (milliseconds on the registry's clock)
    /// with the estimated end `estimate`, if the feed had one.
    pub(crate) fn ended(&mut self, ended_at: u64, estimate: Option<u64>) {
        let Some(estimate) = estimate else {
            self.ended_unestimated += 1;
            return;
        };
        self.ended_estimated += 1;
        let error = i128::from(ended_at) - i128::from(estimate);
        let error = error.clamp(i128::from(i64::MIN), i128::from(i64::MAX));
        // The clamp keeps it in range.
        let error = i64::try_from(error).unwrap_or_default();
        self.error_sum_ms = self.error_sum_ms.saturating_add(error);
        let bucket = ERROR_EDGES_MS.partition_point(|edge| *edge < error);
        self.error_buckets[bucket] += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(name: &str) -> RequestId {
        RequestId::new(name).unwrap()
    }

    /// Catches: a start on an unmarked host counted, the marked request's own start
    /// counted as backfill, or a start by another request (or by none) not counted as
    /// backfill.
    #[test]
    fn a_start_on_a_marked_host_is_backfill_unless_it_is_the_marked_requests_own() {
        let mut c = TailCounters::default();
        c.started(None, Some(&id("q")));
        assert_eq!((c.started_on_marked, c.backfilled), (0, 0));
        c.started(Some(&id("q")), Some(&id("q")));
        assert_eq!((c.started_on_marked, c.backfilled), (1, 0));
        c.started(Some(&id("q")), Some(&id("other")));
        assert_eq!((c.started_on_marked, c.backfilled), (2, 1));
        c.started(Some(&id("q")), None);
        assert_eq!((c.started_on_marked, c.backfilled), (3, 2));
    }

    /// Catches: an error with the wrong sign, an edge counted in the bucket above it, a
    /// lease with no estimate in the error buckets, and a sum that wraps.
    #[test]
    fn the_error_is_actual_minus_estimate_in_signed_buckets() {
        let mut c = TailCounters::default();
        // Ended 30 s late; 10 s early; exactly on time; 400 s early; 1000 s late.
        for (end, est) in [
            (130_000, 100_000),
            (90_000, 100_000),
            (100_000, 100_000),
            (100_000, 500_000),
            (1_100_000, 100_000),
        ] {
            c.ended(end, Some(est));
        }
        c.ended(5, None);
        assert_eq!((c.ended_estimated, c.ended_unestimated), (5, 1));
        assert_eq!(c.error_sum_ms, 30_000 - 10_000 - 400_000 + 1_000_000);
        // 400 s early is below the first edge; 10 s early sits on -10_000; on time on 0;
        // 30 s late in (10_000, 60_000]; 1000 s late above the last edge.
        assert_eq!(c.error_buckets, [1, 0, 1, 1, 0, 1, 0, 1]);
    }

    /// Catches: an error beyond the range of a signed count wrapping the sum.
    #[test]
    fn the_sum_saturates() {
        let mut c = TailCounters::default();
        c.ended(u64::MAX, Some(0));
        c.ended(u64::MAX, Some(0));
        assert_eq!(c.error_sum_ms, i64::MAX);
        let mut d = TailCounters::default();
        d.ended(0, Some(u64::MAX));
        d.ended(0, Some(u64::MAX));
        assert_eq!(d.error_sum_ms, i64::MIN);
    }
}

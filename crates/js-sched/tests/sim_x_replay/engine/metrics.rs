//! What a replay measures, in integers, and the guards that say a measurement means
//! something.
//!
//! Every value is an integer: shares are parts per million of the host's capacity,
//! times are milliseconds, percentiles are the nearest rank (`ceil(p n)`-th smallest).
//! A float appears only in the printed ratio columns of [`Metrics::render`], never in a
//! value that is hashed, compared or asserted.
//!
//! The metric window is the whole logical minutes from the warm-up to the minute of the
//! last arrival; a window, a host-minute or a lease belongs to it by the minute it falls in
//! (a lease by the minute it arrived). What happens before or after is simulated and not
//! sampled.

use std::collections::BTreeMap;
use std::fmt::Write;

use super::fixture::Row;
use super::model::{Capacity, EVALUATIONS_PER_WORKER, MINUTE_TICKS, WINDOW_TICKS};

/// The fewest samples a window percentile is reported from.
pub const MIN_PERCENTILE_SAMPLES: u64 = 20_000;

/// A round stops after this many refusals.
pub const SCAN_REFUSALS: u64 = 32;

/// FNV-1a over a stream of integers, to hash what a run consumed or decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fnv(pub u64);

impl Fnv {
    /// The empty hash.
    pub const fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }

    /// Folds in `v`, byte by byte, little endian.
    pub fn u64(&mut self, v: u64) {
        for b in v.to_le_bytes() {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

impl Default for Fnv {
    fn default() -> Self {
        Self::new()
    }
}

/// The `p`-th percentile of `sorted` (ascending) by nearest rank, `p = num / den`: the
/// `ceil(p n)`-th smallest. `None` for no samples.
pub fn nearest_rank(sorted: &[u64], num: u64, den: u64) -> Option<u64> {
    let n = u128::try_from(sorted.len()).ok()?;
    if n == 0 {
        return None;
    }
    let rank = (u128::from(num) * n).div_ceil(u128::from(den)).max(1);
    let i = usize::try_from(rank - 1).map_or(sorted.len() - 1, |i| i.min(sorted.len() - 1));
    Some(sorted[i])
}

/// `core_ms` burnt in `ticks` seconds, in parts per million of a host of `c_cpu_millis`.
pub fn cpu_ppm(core_ms: u64, c_cpu_millis: u64, ticks: u64) -> u64 {
    let capacity = u128::from(c_cpu_millis) * u128::from(ticks);
    if capacity == 0 {
        return 0;
    }
    u64::try_from(u128::from(core_ms) * 1_000_000 / capacity).unwrap_or(u64::MAX)
}

fn share_ppm(part: u64, whole: u64) -> u64 {
    if whole == 0 {
        0
    } else {
        u64::try_from(u128::from(part) * 1_000_000 / u128::from(whole)).unwrap_or(u64::MAX)
    }
}

/// How a placement was decided; filled by a scorer, `Unscored` for the rankers that do
/// not score.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Decided {
    /// Not scored (fewer than two candidates, or a ranker with no keys).
    Unscored,
    /// The only host that passed.
    Only,
    /// Key 1 to 5 of the score.
    Key(u8),
    /// Tie-break 1 to 4.
    TieBreak(u8),
}

/// Counts of what the engine did; the bounds of [`Counters::check_a9`] are over these.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// Rows handed to the replay.
    pub submitted: u64,
    /// Leases placed.
    pub placed: u64,
    /// Leases that ran to their end.
    pub completed: u64,
    /// Leases killed for memory.
    pub killed: u64,
    /// Leases still queued when the run stopped.
    pub queued_at_end: u64,
    /// Leases still running when the run stopped (0 for a run that drained).
    pub running_at_end: u64,
    /// Placement rounds.
    pub rounds: u64,
    /// Times a lease was looked at: one per lease per round that reached it.
    pub attempts: u64,
    /// Sum over rounds of the queue length at the start of the round.
    pub lease_rounds: u64,
    /// Attempts where no host admitted the lease.
    pub refusals: u64,
    /// Host refusals, by cause (every host looked at, not only for a lease left waiting).
    pub refused_lease_cap: u64,
    /// As above: CPU, any slot.
    pub refused_cpu: u64,
    /// As above: memory, any slot.
    pub refused_memory: u64,
    /// As above: the cold cap only.
    pub refused_cold_cap: u64,
    /// Placements a scorer decided from two candidates or more.
    pub scored_placements: u64,
    /// Candidates scored, summed over those placements.
    pub scored_candidates: u64,
    /// Evaluations the scorer reports.
    pub evaluations: u64,
    /// Candidates a scorer claimed to have scored beyond the ones it was handed (an
    /// excluded or refused host scored).
    pub scored_beyond_candidates: u64,
    /// The largest `attempts - placed` of any round.
    pub worst_round_excess: u64,
    /// Sum over rounds of the leases the round never looked at (the scan stopped first).
    pub unexamined_sum: u64,
    /// The most any one round left unexamined.
    pub unexamined_max: u64,
    /// Rounds that left at least one lease unexamined.
    pub rounds_with_unexamined: u64,
}

/// Which bound of the engine's loop a run broke ("A9": the counted bounds on what the
/// loop and a scorer do, checked by [`Counters::check_a9`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum A9 {
    /// Evaluations are not `64 x` the candidates scored.
    Evaluations {
        /// Reported.
        evaluations: u64,
        /// `64 x scored candidates`.
        expected: u64,
    },
    /// A scorer scored more candidates than it was handed.
    ScoredBeyondCandidates {
        /// How many more.
        extra: u64,
    },
    /// More scored candidates than hosts times attempts.
    ScoredOverHosts {
        /// Scored candidates.
        scored: u64,
        /// Hosts times attempts.
        bound: u64,
    },
    /// A round looked at more than its placements plus the refusals it allows.
    RoundAttempts {
        /// The worst round's `attempts - placed`.
        excess: u64,
    },
    /// More attempts than lease-rounds waited.
    Attempts {
        /// Attempts.
        attempts: u64,
        /// Lease-rounds.
        bound: u64,
    },
}

impl Counters {
    /// The engine's bounds, in counts and never in time: evaluations are `64 x` the
    /// candidates scored (`exact`; a ranker that scores more, as the flat-fit one may,
    /// passes `false` and is held to at least that), no more scored than hosts times
    /// attempts, no round looks at more than its placements plus 32, and no lease is
    /// looked at twice in a round. The first one broken is returned.
    pub fn check_a9(&self, hosts: usize, exact: bool) -> Result<(), A9> {
        let expected = EVALUATIONS_PER_WORKER * self.scored_candidates;
        let evaluations_ok = if exact {
            self.evaluations == expected
        } else {
            self.evaluations >= expected
        };
        if !evaluations_ok {
            return Err(A9::Evaluations {
                evaluations: self.evaluations,
                expected,
            });
        }
        if self.scored_beyond_candidates > 0 {
            return Err(A9::ScoredBeyondCandidates {
                extra: self.scored_beyond_candidates,
            });
        }
        let bound = self.attempts * hosts as u64;
        if self.scored_candidates > bound {
            return Err(A9::ScoredOverHosts {
                scored: self.scored_candidates,
                bound,
            });
        }
        if self.worst_round_excess > SCAN_REFUSALS {
            return Err(A9::RoundAttempts {
                excess: self.worst_round_excess,
            });
        }
        if self.attempts > self.lease_rounds {
            return Err(A9::Attempts {
                attempts: self.attempts,
                bound: self.lease_rounds,
            });
        }
        Ok(())
    }
}

/// The books of a run: every lease and every core-millisecond accounted for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Conservation {
    /// Completed leases whose truth core-ms differ from the row's `cpu_ms`.
    pub completed_mismatch: u64,
    /// Core-ms of completed leases.
    pub completed_core_ms: u64,
    /// Core-ms a killed lease burnt before it was killed.
    pub killed_core_ms: u64,
    /// Core-ms of leases still running at the end.
    pub running_core_ms: u64,
    /// Core-ms summed over every host and tick.
    pub host_core_ms: u64,
}

/// Which identity of the books failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unbalanced {
    /// `submitted != completed + killed + queued + running`.
    Leases {
        /// Rows submitted.
        submitted: u64,
        /// The other side.
        accounted: u64,
    },
    /// A completed lease did not burn its `cpu_ms`.
    Completed {
        /// How many.
        mismatched: u64,
    },
    /// The hosts burnt something other than the leases did.
    CoreMs {
        /// Summed over the hosts.
        host: u64,
        /// Summed over the leases.
        leases: u64,
    },
}

/// What the replay measured.
#[derive(Clone, Debug, Default)]
pub struct Metrics {
    /// The engine's counts.
    pub counters: Counters,
    /// The books.
    pub books: Conservation,
    /// Truth CPU of every host-window in the metric window, ppm of `C_cpu`, ascending.
    pub window_ppm: Vec<u64>,
    /// Truth CPU of every host-minute, ppm of `C_cpu`, ascending.
    pub minute_ppm: Vec<u64>,
    /// The largest working set of any host-window, ppm of `C_mem`.
    pub mem_max_ppm: u64,
    /// Whole minutes in the metric window.
    pub metric_minutes: u64,
    /// Host-minutes in it.
    pub host_minutes: u64,
    /// Host-minutes in which the predicate refused a queued lease on that host.
    pub refusal_host_minutes: u64,
    /// Idle truth capacity (ppm of `C_cpu`, summed over host-minutes) on those.
    pub stranded_a_ppm_sum: u64,
    /// The same on host-minutes with no refusal.
    pub headroom_b_ppm_sum: u64,
    /// Host-minutes with the queue non-empty after a round and no refusal on the host: the
    /// retired definition of stranding. A FIFO scan leaves it empty when every host is
    /// asked about every waiting lease; a marked host is never asked, records no refusal,
    /// and so counts here (and as headroom) whenever the queue is non-empty.
    pub retired_host_minutes: u64,
    /// Minutes in which consolidating onto one host fewer was possible.
    pub consolidation_minutes: u64,
    /// Host-minutes with no resident at all.
    pub empty_host_minutes: u64,
    /// Of those, in a minute where consolidation was possible.
    pub kept_empty_host_minutes: u64,
    /// Queue wait of the leases that arrived in the metric window, ms, ascending (a lease
    /// still queued at the end counts the wait so far).
    pub wait_ms: Vec<u64>,
    /// The same less the wait to the next round, ascending.
    pub wait_excess_ms: Vec<u64>,
    /// Leases that arrived in the metric window.
    pub leases_in_window: u64,
    /// Of those, cold.
    pub cold_in_window: u64,
    /// Completions in the metric window.
    pub completed_in_window: u64,
    /// Placed and cold leases by hour of arrival.
    pub by_hour: BTreeMap<u64, HourCount>,
    /// Placements by how they were decided.
    pub decided: BTreeMap<Decided, u64>,
    /// Rows whose mean cores or peak memory are over 0.9 of a host.
    pub oversize_rows: u64,
}

/// Leases placed and cold in an hour.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HourCount {
    /// Placed.
    pub placed: u64,
    /// Of those, cold.
    pub cold: u64,
}

impl Metrics {
    /// A window percentile, `None` below [`MIN_PERCENTILE_SAMPLES`] samples.
    pub fn window_pct(&self, num: u64, den: u64) -> Option<u64> {
        if (self.window_ppm.len() as u64) < MIN_PERCENTILE_SAMPLES {
            return None;
        }
        nearest_rank(&self.window_ppm, num, den)
    }

    /// The median of the host-windows.
    pub fn window_p50(&self) -> Option<u64> {
        self.window_pct(50, 100)
    }

    /// The 99th percentile of the host-windows.
    pub fn window_p99(&self) -> Option<u64> {
        self.window_pct(99, 100)
    }

    /// The 99.9th percentile of the host-windows.
    pub fn window_p999(&self) -> Option<u64> {
        self.window_pct(999, 1000)
    }

    /// Host-windows over `ppm` of capacity.
    pub fn windows_above(&self, ppm: u64) -> u64 {
        (self.window_ppm.len() - self.window_ppm.partition_point(|&v| v <= ppm)) as u64
    }

    /// A queue wait percentile, ms.
    pub fn wait_pct(&self, num: u64, den: u64) -> Option<u64> {
        nearest_rank(&self.wait_ms, num, den)
    }

    /// Idle capacity behind a refusal, per host-minute, ppm of `C_cpu`.
    pub fn stranded_a_rate_ppm(&self) -> u64 {
        self.stranded_a_ppm_sum
            .checked_div(self.host_minutes)
            .unwrap_or(0)
    }

    /// Idle capacity with no refusal, per host-minute, ppm of `C_cpu`.
    pub fn headroom_b_rate_ppm(&self) -> u64 {
        self.headroom_b_ppm_sum
            .checked_div(self.host_minutes)
            .unwrap_or(0)
    }

    /// Cold leases of the leases that arrived in the metric window, ppm.
    pub fn cold_share_ppm(&self) -> u64 {
        share_ppm(self.cold_in_window, self.leases_in_window)
    }

    /// Completions per 1000 minutes of the metric window.
    pub fn completed_per_1000_min(&self) -> u64 {
        (self.completed_in_window * 1000)
            .checked_div(self.metric_minutes)
            .unwrap_or(0)
    }

    /// Checks the books: every row is in exactly one state, a completed lease burnt what
    /// its row says, and the hosts burnt what the leases did.
    pub fn check_books(&self) -> Result<(), Unbalanced> {
        let c = &self.counters;
        let accounted = c.completed + c.killed + c.queued_at_end + c.running_at_end;
        if c.submitted != accounted {
            return Err(Unbalanced::Leases {
                submitted: c.submitted,
                accounted,
            });
        }
        if self.books.completed_mismatch > 0 {
            return Err(Unbalanced::Completed {
                mismatched: self.books.completed_mismatch,
            });
        }
        let leases =
            self.books.completed_core_ms + self.books.killed_core_ms + self.books.running_core_ms;
        if self.books.host_core_ms != leases {
            return Err(Unbalanced::CoreMs {
                host: self.books.host_core_ms,
                leases,
            });
        }
        Ok(())
    }

    /// The table a test prints: integer columns, and a ratio beside the ones that are
    /// shares. Printed for a reader; nothing compares it.
    pub fn render(&self) -> String {
        let mut s = String::new();
        let c = &self.counters;
        let ratio = |ppm: u64| ppm as f64 / 1e6;
        let pct = |p: Option<u64>| p.map_or_else(|| "-".to_string(), |v| v.to_string());
        let _ = writeln!(
            s,
            "windows {} (p50 {} p99 {} p99.9 {} ppm; over 1.1: {}, over 1.5: {})",
            self.window_ppm.len(),
            pct(self.window_p50()),
            pct(self.window_p99()),
            pct(self.window_p999()),
            self.windows_above(1_100_000),
            self.windows_above(1_500_000),
        );
        let _ = writeln!(
            s,
            "node-minutes {} (p50 {} p99 {} ppm)",
            self.minute_ppm.len(),
            pct(nearest_rank(&self.minute_ppm, 50, 100)),
            pct(nearest_rank(&self.minute_ppm, 99, 100)),
        );
        let _ = writeln!(
            s,
            "wait ms ({} leases) p50 {} p95 {} p99 {}; excess p95 {}",
            self.wait_ms.len(),
            pct(self.wait_pct(50, 100)),
            pct(self.wait_pct(95, 100)),
            pct(self.wait_pct(99, 100)),
            pct(nearest_rank(&self.wait_excess_ms, 95, 100)),
        );
        let _ = writeln!(
            s,
            "submitted {} placed {} completed {} ({} per 1000 min) killed {} queued_at_end {} running_at_end {}",
            c.submitted,
            c.placed,
            c.completed,
            self.completed_per_1000_min(),
            c.killed,
            c.queued_at_end,
            c.running_at_end,
        );
        let _ = writeln!(
            s,
            "refusals {} (host: lease cap {} cpu {} memory {} cold cap {})",
            c.refusals, c.refused_lease_cap, c.refused_cpu, c.refused_memory, c.refused_cold_cap,
        );
        let _ = writeln!(
            s,
            "stranded(a) {} ppm ({:.4}), headroom(b) {} ppm ({:.4}), refusal host-minutes {} of {}",
            self.stranded_a_rate_ppm(),
            ratio(self.stranded_a_rate_ppm()),
            self.headroom_b_rate_ppm(),
            ratio(self.headroom_b_rate_ppm()),
            self.refusal_host_minutes,
            self.host_minutes,
        );
        let _ = writeln!(
            s,
            "empty host-minutes {} (kept empty {} in {} consolidation minutes of {})",
            self.empty_host_minutes,
            self.kept_empty_host_minutes,
            self.consolidation_minutes,
            self.metric_minutes,
        );
        let _ = writeln!(
            s,
            "rounds {}; leases never examined in a round: {} in total, at most {}, in {} rounds",
            c.rounds, c.unexamined_sum, c.unexamined_max, c.rounds_with_unexamined,
        );
        let _ = writeln!(
            s,
            "attempts {} scored placements {} scored candidates {} evaluations {}",
            c.attempts, c.scored_placements, c.scored_candidates, c.evaluations,
        );
        let _ = writeln!(
            s,
            "cold share {} ppm ({:.4}); oversize rows {}",
            self.cold_share_ppm(),
            ratio(self.cold_share_ppm()),
            self.oversize_rows,
        );
        s
    }
}

/// The registered limits of the guards every assertion of the sims carries ("G0"): a run
/// that fails one of them measured too little, or a pool that was not serving, and says
/// nothing either way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct G0 {
    /// Host-windows in the metric window.
    pub min_window_samples: u64,
    /// Host-minutes in it.
    pub min_minute_samples: u64,
    /// Minutes of it.
    pub min_span_minutes: u64,
    /// Rows over 0.9 of a host, of all rows, ppm.
    pub max_oversize_ppm: u64,
    /// Cold share of the leases that arrived in it, ppm: the least and the most.
    pub cold_share_ppm: (u64, u64),
    /// Leases queued at the end, of those submitted, ppm.
    pub max_queued_at_end_ppm: u64,
    /// The 99th percentile wait, ms.
    pub max_wait_p99_ms: u64,
}

impl Default for G0 {
    fn default() -> Self {
        Self {
            min_window_samples: 20_000,
            min_minute_samples: 1_000,
            min_span_minutes: 240,
            max_oversize_ppm: 5_000,
            cold_share_ppm: (50_000, 500_000),
            max_queued_at_end_ppm: 10_000,
            max_wait_p99_ms: 30 * 60_000,
        }
    }
}

/// A guard that failed: the run says nothing, and the test fails rather than passes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Uninformative {
    /// Too few host-windows.
    WindowSamples {
        /// Had.
        have: u64,
        /// Needed.
        need: u64,
    },
    /// Too few host-minutes.
    MinuteSamples {
        /// Had.
        have: u64,
        /// Needed.
        need: u64,
    },
    /// The metric window is too short.
    Span {
        /// Had, minutes.
        have: u64,
        /// Needed.
        need: u64,
    },
    /// Too many rows clamped to the host.
    Oversize {
        /// Rows.
        rows: u64,
        /// Share, ppm.
        ppm: u64,
    },
    /// The cold share is out of range.
    ColdShare {
        /// Share, ppm.
        ppm: u64,
    },
    /// The queue did not drain.
    QueuedAtEnd {
        /// Queued at the end.
        queued: u64,
        /// Of submitted, ppm.
        ppm: u64,
    },
    /// The 99th percentile wait is too long: the pool is overloaded.
    WaitP99 {
        /// Its value, ms.
        ms: u64,
    },
}

impl std::fmt::Display for Uninformative {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "UNINFORMATIVE: ")?;
        match self {
            Self::WindowSamples { have, need } => write!(f, "{have} window samples, need {need}"),
            Self::MinuteSamples { have, need } => write!(f, "{have} node-minutes, need {need}"),
            Self::Span { have, need } => write!(f, "metric span {have} min, need {need}"),
            Self::Oversize { rows, ppm } => write!(f, "{rows} oversize rows ({ppm} ppm)"),
            Self::ColdShare { ppm } => write!(f, "cold share {ppm} ppm out of range"),
            Self::QueuedAtEnd { queued, ppm } => {
                write!(f, "{queued} leases queued at the end ({ppm} ppm)")
            }
            Self::WaitP99 { ms } => write!(f, "wait p99 {ms} ms"),
        }
    }
}

impl G0 {
    /// Every guard that `m` fails (none: it may be read).
    pub fn check(&self, m: &Metrics) -> Result<(), Vec<Uninformative>> {
        let mut bad = Vec::new();
        let windows = m.window_ppm.len() as u64;
        if windows < self.min_window_samples {
            bad.push(Uninformative::WindowSamples {
                have: windows,
                need: self.min_window_samples,
            });
        }
        let minutes = m.minute_ppm.len() as u64;
        if minutes < self.min_minute_samples {
            bad.push(Uninformative::MinuteSamples {
                have: minutes,
                need: self.min_minute_samples,
            });
        }
        if m.metric_minutes < self.min_span_minutes {
            bad.push(Uninformative::Span {
                have: m.metric_minutes,
                need: self.min_span_minutes,
            });
        }
        let oversize = share_ppm(m.oversize_rows, m.counters.submitted);
        if oversize > self.max_oversize_ppm {
            bad.push(Uninformative::Oversize {
                rows: m.oversize_rows,
                ppm: oversize,
            });
        }
        let cold = m.cold_share_ppm();
        if cold < self.cold_share_ppm.0 || cold > self.cold_share_ppm.1 {
            bad.push(Uninformative::ColdShare { ppm: cold });
        }
        let queued = share_ppm(m.counters.queued_at_end, m.counters.submitted);
        if queued >= self.max_queued_at_end_ppm {
            bad.push(Uninformative::QueuedAtEnd {
                queued: m.counters.queued_at_end,
                ppm: queued,
            });
        }
        if let Some(ms) = m.wait_pct(99, 100)
            && ms >= self.max_wait_p99_ms
        {
            bad.push(Uninformative::WaitP99 { ms });
        }
        if bad.is_empty() { Ok(()) } else { Err(bad) }
    }
}

/// Rows whose mean cores or peak memory exceed 0.9 of a host of `cap`.
pub fn oversize_rows(rows: &[Row], cap: &Capacity) -> u64 {
    rows.iter()
        .filter(|r| {
            // cores = cpu_ms / wall_ms, against 0.9 of cap.cpu_millis / 1000 cores.
            let cpu_over = u128::from(r.cpu_ms) * 10_000
                > u128::from(r.wall_ms) * 9 * u128::from(cap.cpu_millis);
            let mem_over = u128::from(r.peak_mem_mib) * 10 > 9 * u128::from(cap.mem_mib);
            cpu_over || mem_over
        })
        .count() as u64
}

/// The share of machine-time (wall) in runs over 5 minutes, ppm.
pub fn long_run_share_ppm(rows: &[Row]) -> u64 {
    let total: u64 = rows.iter().map(|r| r.wall_ms).sum();
    let long: u64 = rows
        .iter()
        .filter(|r| r.wall_ms > 300_000)
        .map(|r| r.wall_ms)
        .sum();
    share_ppm(long, total)
}

/// Collects a run's measurements as the replay goes.
#[derive(Debug)]
pub struct Recorder {
    hosts: usize,
    cap: Capacity,
    first_minute: u64,
    end_minute: u64,
    m: Metrics,
    minute_core_ms: Vec<u64>,
    minute_refused: Vec<bool>,
    minute_peak: Vec<usize>,
    minute_queued: bool,
}

impl Recorder {
    /// A recorder for `hosts` hosts of `cap` that samples the minutes from `warm_up_min`
    /// up to (not including) the minute of `last_arrival_ms`.
    pub fn new(hosts: usize, cap: Capacity, warm_up_min: u64, last_arrival_ms: u64) -> Self {
        let end_minute = last_arrival_ms / 60_000;
        Self {
            hosts,
            cap,
            first_minute: warm_up_min,
            end_minute: end_minute.max(warm_up_min),
            m: Metrics::default(),
            minute_core_ms: vec![0; hosts],
            minute_refused: vec![false; hosts],
            minute_peak: vec![0; hosts],
            minute_queued: false,
        }
    }

    /// The counters, for the replay to bump.
    pub fn counters(&mut self) -> &mut Counters {
        &mut self.m.counters
    }

    /// The books, for the replay to fill.
    pub fn books(&mut self) -> &mut Conservation {
        &mut self.m.books
    }

    /// Notes the rows that are over 0.9 of a host.
    pub fn set_oversize(&mut self, rows: u64) {
        self.m.oversize_rows = rows;
    }

    fn sampled(&self, minute: u64) -> bool {
        (self.first_minute..self.end_minute).contains(&minute)
    }

    /// A lease arrived (`start_ms`).
    pub fn arrival(&mut self, start_ms: u64) {
        if self.sampled(start_ms / 60_000) {
            self.m.leases_in_window += 1;
        }
    }

    /// A lease that arrived at `start_ms` was placed at `tick`; `cold` says whether it was
    /// charged cold.
    pub fn placed(&mut self, start_ms: u64, tick: u64, cold: bool, decided: Decided) {
        let hour = start_ms / 3_600_000;
        let h = self.m.by_hour.entry(hour).or_default();
        h.placed += 1;
        h.cold += u64::from(cold);
        *self.m.decided.entry(decided).or_default() += 1;
        if self.sampled(start_ms / 60_000) {
            self.m.cold_in_window += u64::from(cold);
            let wait = (tick * 1_000).saturating_sub(start_ms);
            let align = start_ms.div_ceil(WINDOW_TICKS * 1_000) * WINDOW_TICKS * 1_000 - start_ms;
            self.m.wait_ms.push(wait);
            self.m.wait_excess_ms.push(wait.saturating_sub(align));
        }
    }

    /// A lease completed at `tick`.
    pub fn completed(&mut self, tick: u64) {
        if self.sampled(tick / MINUTE_TICKS) {
            self.m.completed_in_window += 1;
        }
    }

    /// The predicate refused a lease on `host` in this minute.
    pub fn refused(&mut self, host: usize) {
        self.minute_refused[host] = true;
    }

    /// After a round: whether the queue is non-empty.
    pub fn queue_after_round(&mut self, nonempty: bool) {
        self.minute_queued |= nonempty;
    }

    /// Residents on `host` at the end of a tick.
    pub fn residents(&mut self, host: usize, n: usize) {
        self.minute_peak[host] = self.minute_peak[host].max(n);
    }

    /// A window ended at `tick` on `host`: `core_ms` burnt in it and `working_set_mib` held.
    pub fn window(&mut self, host: usize, tick: u64, core_ms: u64, working_set_mib: u64) {
        self.minute_core_ms[host] += core_ms;
        if self.sampled(tick / MINUTE_TICKS) {
            self.m
                .window_ppm
                .push(cpu_ppm(core_ms, self.cap.cpu_millis, WINDOW_TICKS));
            let mem = share_ppm(working_set_mib, self.cap.mem_mib);
            self.m.mem_max_ppm = self.m.mem_max_ppm.max(mem);
        }
    }

    /// The minute `minute` ended: settles the host-minutes.
    pub fn minute_end(&mut self, minute: u64) {
        if self.sampled(minute) {
            let m = &mut self.m;
            m.metric_minutes += 1;
            let mut pool = 0u64;
            let mut empty = 0u64;
            for h in 0..self.hosts {
                let core_ms = self.minute_core_ms[h];
                pool += core_ms;
                let mean = cpu_ppm(core_ms, self.cap.cpu_millis, MINUTE_TICKS);
                m.minute_ppm.push(mean);
                let idle = 1_000_000u64.saturating_sub(mean);
                m.host_minutes += 1;
                if self.minute_refused[h] {
                    m.refusal_host_minutes += 1;
                    m.stranded_a_ppm_sum += idle;
                } else {
                    m.headroom_b_ppm_sum += idle;
                    if self.minute_queued {
                        m.retired_host_minutes += 1;
                    }
                }
                if self.minute_peak[h] == 0 {
                    empty += 1;
                }
            }
            m.empty_host_minutes += empty;
            // Consolidation is possible when the pool's truth CPU would fit on one host
            // fewer at 0.6 of capacity: pool <= 0.6 (n - 1) C.
            let fits = u128::from(pool) * 5
                <= 3 * (self.hosts as u128).saturating_sub(1)
                    * u128::from(self.cap.cpu_millis)
                    * u128::from(MINUTE_TICKS);
            if fits {
                m.consolidation_minutes += 1;
                m.kept_empty_host_minutes += empty;
            }
        }
        self.minute_core_ms.fill(0);
        self.minute_refused.fill(false);
        self.minute_peak.fill(0);
        self.minute_queued = false;
    }

    /// Ends the run at `end_tick` with `queued` (their arrival ms) still waiting.
    pub fn finish(mut self, end_tick: u64, queued: &[u64]) -> Metrics {
        for &start_ms in queued {
            if self.sampled(start_ms / 60_000) {
                let wait = (end_tick * 1_000).saturating_sub(start_ms);
                let align =
                    start_ms.div_ceil(WINDOW_TICKS * 1_000) * WINDOW_TICKS * 1_000 - start_ms;
                self.m.wait_ms.push(wait);
                self.m.wait_excess_ms.push(wait.saturating_sub(align));
            }
        }
        self.m.window_ppm.sort_unstable();
        self.m.minute_ppm.sort_unstable();
        self.m.wait_ms.sort_unstable();
        self.m.wait_excess_ms.sort_unstable();
        self.m
    }
}

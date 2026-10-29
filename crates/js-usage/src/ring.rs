//! The ring of a key's last runs: each run is a fixed number of per-minute buckets.

use std::collections::{BTreeSet, VecDeque};

/// How many runs a key keeps; a new run past this evicts the oldest.
pub const RING_RUNS: usize = 32;

/// How many minutes a run keeps apart: minutes 0 to 62 are one bucket each, and bucket 63
/// holds every later minute together (the maximum of them, which is conservative).
pub const BUCKETS: usize = 64;

/// One minute of a run, as the lease reported it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunMinute {
    /// The minute since the lease's own start, from 0.
    pub minute: u32,
    /// Mean cores over the minute, in millicores.
    pub cores_milli: u32,
    /// The largest working set seen in the minute, in bytes.
    pub ws_bytes: u64,
}

/// What a run that ended on its own used, minute by minute.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunSeries {
    /// The minutes the run has records for, in any order. A minute given twice counts once,
    /// at the larger of its values.
    pub minutes: Vec<RunMinute>,
    /// Reserved bits, stored with the run: [`RunSeries::CENSORED`] and
    /// [`RunSeries::NODE_PRESSURE`]. The fit reads only the second.
    pub flags: u8,
}

impl RunSeries {
    /// Bit 0: the run's memory is censored (it was killed at its limit). The fit does not
    /// read it today.
    pub const CENSORED: u8 = 1;
    /// Bit 1: the node was under CPU pressure during the run, so it shows less CPU than it
    /// needed. The fit leaves such a run out of the CPU curve.
    pub const NODE_PRESSURE: u8 = 1 << 1;
}

/// The use of one minute, or of all minutes from 63 on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bucket {
    /// Mean cores, in millicores.
    pub cores_milli: u32,
    /// Working set, in bytes.
    pub ws_bytes: u64,
}

impl Bucket {
    fn join(self, other: Self) -> Self {
        Self {
            cores_milli: self.cores_milli.max(other.cores_milli),
            ws_bytes: self.ws_bytes.max(other.ws_bytes),
        }
    }
}

/// One run in the ring.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    /// How many wall minutes the run lasted, rounded up: the last minute it has a record
    /// for, plus one.
    pub wall_minutes: u32,
    /// The run's reserved flag bits ([`RunSeries::flags`]).
    pub flags: u8,
    /// Bucket `i` is minute `i` (the last one, from 63 on), up to the last minute the run
    /// reached. `None` is a minute with no record, which the fit skips.
    pub buckets: Vec<Option<Bucket>>,
}

/// Whether a run entered the ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admitted {
    /// The run entered the ring.
    Yes,
    /// More than a fifth of the minutes up to the run's last are missing: the run would
    /// bias the curve, and does not enter.
    MissingMinutes,
    /// The run has no minute at all.
    Empty,
}

impl Run {
    /// Whether the node was under CPU pressure during the run ([`RunSeries::NODE_PRESSURE`]).
    #[must_use]
    pub fn under_node_pressure(&self) -> bool {
        self.flags & RunSeries::CENSORED != 0
    }

    /// The run `series` makes, or why it does not enter.
    ///
    /// # Errors
    /// [`Admitted::Empty`] or [`Admitted::MissingMinutes`], never [`Admitted::Yes`].
    pub fn from_series(series: &RunSeries) -> Result<Self, Admitted> {
        let last = series
            .minutes
            .iter()
            .map(|m| m.minute)
            .max()
            .ok_or(Admitted::Empty)?;
        let seen = series
            .minutes
            .iter()
            .map(|m| m.minute)
            .collect::<BTreeSet<_>>()
            .len() as u64;
        let expected = u64::from(last) + 1;
        if (expected - seen) * 5 > expected {
            return Err(Admitted::MissingMinutes);
        }
        let top = usize::try_from(last).map_or(BUCKETS - 1, |l| l.min(BUCKETS - 1));
        let mut buckets: Vec<Option<Bucket>> = vec![None; top + 1];
        for m in &series.minutes {
            // A minute is at most `last`, so its bucket is at most `top`.
            let at = usize::try_from(m.minute).map_or(top, |i| i.min(BUCKETS - 1));
            let minute = Bucket {
                cores_milli: m.cores_milli,
                ws_bytes: m.ws_bytes,
            };
            buckets[at] = Some(buckets[at].map_or(minute, |held| held.join(minute)));
        }
        Ok(Self {
            wall_minutes: last.saturating_add(1),
            flags: series.flags,
            buckets,
        })
    }
}

/// The last [`RING_RUNS`] runs of one key, oldest first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ring {
    runs: VecDeque<Run>,
}

impl Ring {
    /// An empty ring.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds the run `series` makes, evicting the oldest run when the ring is full.
    pub fn push(&mut self, series: &RunSeries) -> Admitted {
        match Run::from_series(series) {
            Ok(run) => {
                self.push_run(run);
                Admitted::Yes
            }
            Err(refused) => refused,
        }
    }

    /// Adds a run already built, evicting the oldest run when the ring is full.
    pub fn push_run(&mut self, run: Run) {
        if self.runs.len() == RING_RUNS {
            self.runs.pop_front();
        }
        self.runs.push_back(run);
    }

    /// How many runs the ring holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.runs.len()
    }

    /// Whether the ring holds no run.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }

    /// The runs, oldest first.
    pub fn runs(&self) -> impl Iterator<Item = &Run> {
        self.runs.iter()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A run of `cores` and `ws` per minute, minute 0 first.
    pub(crate) fn series(minutes: &[(u32, u64)]) -> RunSeries {
        RunSeries {
            minutes: minutes
                .iter()
                .enumerate()
                .map(|(i, &(cores_milli, ws_bytes))| RunMinute {
                    minute: i as u32,
                    cores_milli,
                    ws_bytes,
                })
                .collect(),
            flags: 0,
        }
    }

    fn bucket(cores_milli: u32, ws_bytes: u64) -> Option<Bucket> {
        Some(Bucket {
            cores_milli,
            ws_bytes,
        })
    }

    /// Catches: a ring that keeps the newest run out, or keeps 33.
    #[test]
    fn the_33rd_run_evicts_the_oldest() {
        let mut ring = Ring::new();
        assert!(ring.is_empty());
        for i in 0..33u32 {
            assert_eq!(ring.push(&series(&[(i, 0)])), Admitted::Yes);
        }
        assert_eq!(ring.len(), RING_RUNS);
        let firsts: Vec<_> = ring
            .runs()
            .map(|r| r.buckets[0].unwrap().cores_milli)
            .collect();
        assert_eq!(firsts.first(), Some(&1), "run 0 left");
        assert_eq!(firsts.last(), Some(&32), "run 32 is the newest");
    }

    /// Catches: minutes past 63 being dropped or averaged, not held at their maximum.
    #[test]
    fn bucket_63_holds_the_maximum_of_every_later_minute() {
        let mut minutes: Vec<(u32, u64)> = (0..90).map(|_| (100, 1)).collect();
        minutes[63] = (300, 5);
        minutes[80] = (700, 2);
        minutes[89] = (200, 9);
        let run = Run::from_series(&series(&minutes)).unwrap();
        assert_eq!(
            run.buckets.len(),
            BUCKETS,
            "a run of 90 minutes fills 64 buckets"
        );
        assert_eq!(run.buckets[62], bucket(100, 1));
        assert_eq!(
            run.buckets[63],
            bucket(700, 9),
            "max of cores and of ws, apart"
        );
        assert_eq!(run.wall_minutes, 90);
    }

    /// Catches: a run under a minute being refused or given more than one bucket.
    #[test]
    fn a_run_under_a_minute_is_one_bucket() {
        let run = Run::from_series(&series(&[(250, 1000)])).unwrap();
        assert_eq!(run.buckets, vec![bucket(250, 1000)]);
        assert_eq!(run.wall_minutes, 1);
    }

    /// Catches: the missing-minutes rule off by one (exactly a fifth missing enters; one
    /// more refuses) and counting a resent minute as present twice.
    #[test]
    fn more_than_a_fifth_missing_is_refused() {
        let sparse = |present: &[u32]| RunSeries {
            minutes: present
                .iter()
                .map(|&minute| RunMinute {
                    minute,
                    cores_milli: 1,
                    ws_bytes: 1,
                })
                .collect(),
            flags: 0,
        };
        // Minutes 0..=9 are expected: 8 present is a fifth missing, which enters.
        let fifth = sparse(&[0, 1, 2, 3, 4, 5, 6, 9]);
        let run = Run::from_series(&fifth).unwrap();
        assert_eq!(
            run.buckets[7], None,
            "a missing minute is a hole, not a zero"
        );
        assert_eq!(
            Run::from_series(&sparse(&[0, 1, 2, 3, 4, 5, 9])),
            Err(Admitted::MissingMinutes)
        );
        // Duplicates do not make up for missing minutes.
        assert_eq!(
            Run::from_series(&sparse(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 9])),
            Err(Admitted::MissingMinutes)
        );
        let mut ring = Ring::new();
        assert_eq!(ring.push(&sparse(&[0, 9])), Admitted::MissingMinutes);
        assert!(ring.is_empty(), "a refused run is not stored");
    }

    #[test]
    fn a_run_with_no_minute_is_empty() {
        let mut ring = Ring::new();
        assert_eq!(ring.push(&RunSeries::default()), Admitted::Empty);
        assert!(ring.is_empty());
    }

    /// Catches: a minute given twice taking the first value, not the larger.
    #[test]
    fn a_minute_given_twice_keeps_the_larger_values() {
        let mut s = series(&[(100, 9)]);
        s.minutes.push(RunMinute {
            minute: 0,
            cores_milli: 300,
            ws_bytes: 4,
        });
        s.flags = RunSeries::CENSORED | RunSeries::NODE_PRESSURE;
        let run = Run::from_series(&s).unwrap();
        assert_eq!(run.buckets, vec![bucket(300, 9)]);
        assert_eq!(run.flags, 3);
        assert!(run.under_node_pressure());
        assert!(
            !Run::from_series(&series(&[(1, 1)]))
                .unwrap()
                .under_node_pressure()
        );
    }

    /// Catches: a minute index near `u32::MAX` overflowing the expected count.
    #[test]
    fn a_huge_minute_index_is_refused_not_overflowed() {
        let s = RunSeries {
            minutes: vec![RunMinute {
                minute: u32::MAX,
                cores_milli: 1,
                ws_bytes: 1,
            }],
            flags: 0,
        };
        assert_eq!(Run::from_series(&s), Err(Admitted::MissingMinutes));
    }
}

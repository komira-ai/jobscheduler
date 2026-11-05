//! A job's result, how long an ended job is kept, and which jobs are past that (design
//! sections 3.2, 6.7 and 8, rows 3, 30 and 36).
//!
//! An ended job's record keeps its [`JobResult`] (exit code, reason, log tails, timing,
//! resource usage) and its retention end, both written by [`crate::JobStore::end_job`] in
//! the transaction that ends the job. The retention end is a stored instant: the store's
//! clock at the end plus the [`Retention`] window the store was opened with, so a window
//! changed later applies to the jobs that end afterwards and not to those already ended.
//! [`sweep`] says which jobs are past their retention end at a given time; it reads no clock.
//! At boot every stored retention end is raised to at least the boot plus [`BOOT_GRACE`]
//! ([`crate::JobStore::raise_deadlines_at_boot`]), so an outage longer than the window
//! deletes no result before a client could read it.
//!
//! Only job rows are swept. Allocation rows are history: they are kept as long as the
//! placement estimates need them (design question 14), are not part of the job retention,
//! and nothing here reads, writes or deletes them.

use std::num::NonZeroUsize;
use std::time::Duration;

use js_agent_wire::limits::MAX_LOG_STREAM_LEN;
use js_state::StoreTime;

use crate::ids::JobId;
use crate::record::{JobRecord, JobState, StartFailure};

/// How far past the boot every stored retention end is raised (design section 6.7, step 7).
/// It is a constant, not a setting.
pub const BOOT_GRACE: Duration = Duration::from_secs(120);

/// The most job rows one pass of [`crate::JobStore::tick`] reads, and so the most it deletes,
/// in one transaction.
pub const SWEEP_BATCH: NonZeroUsize = NonZeroUsize::MIN.saturating_add(499);

/// Why a job that has a result ended (design section 3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultReason {
    /// Its process exited by itself.
    Exit,
    /// It was stopped at its wall limit.
    WallLimit,
    /// It was stopped for memory.
    Memory,
    /// The client cancelled it.
    Cancelled,
}

/// When a job was recorded, placed, started and ended, by the store's clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobTiming {
    /// When it was recorded as queued.
    pub queued_at: StoreTime,
    /// When it was last placed; `None` when it never was.
    pub placed_at: Option<StoreTime>,
    /// When it began running; `None` when it never did.
    pub started_at: Option<StoreTime>,
    /// When it ended.
    pub ended_at: StoreTime,
}

/// What a job used, as the agent reported it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceUsage {
    /// The CPU time it used.
    pub cpu_time: Duration,
    /// The most memory it held at once, in bytes.
    pub peak_memory_bytes: u64,
}

/// What a client reads of a job that ended with a result (design section 5.3, "Get, list,
/// result, logs"). A job that was lost or failed to start has none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobResult {
    /// The exit code, when the process gave one.
    pub exit_code: Option<i32>,
    /// Why it ended.
    pub reason: ResultReason,
    /// The end of what it wrote to standard output: at most
    /// [`MAX_LOG_STREAM_LEN`] bytes of UTF-8.
    pub stdout_tail: String,
    /// The end of what it wrote to standard error, as `stdout_tail`.
    pub stderr_tail: String,
    /// Whether it wrote more than the two tails hold.
    pub truncated: bool,
    /// When it was queued, placed, started and ended.
    pub timing: JobTiming,
    /// What it used; `None` when the agent reported nothing.
    pub usage: Option<ResourceUsage>,
}

/// What the host reported of a job that stopped: the tails of its output and its usage.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    /// The end of its standard output. A tail longer than [`MAX_LOG_STREAM_LEN`] bytes is
    /// cut to its last bytes on a character boundary when stored.
    pub stdout: String,
    /// The end of its standard error, as `stdout`.
    pub stderr: String,
    /// Whether the host already cut either tail.
    pub truncated: bool,
    /// What it used, when the agent reported it.
    pub usage: Option<ResourceUsage>,
}

/// How a job ended: what [`crate::JobStore::end_job`] records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobEnd {
    /// Its process exited by itself: succeeded with code 0, failed with any other.
    Exited {
        /// The exit code.
        exit_code: i32,
        /// What the host reported.
        output: Output,
    },
    /// It was stopped at its wall limit: failed.
    WallLimit {
        /// The exit code, when the stopped process gave one.
        exit_code: Option<i32>,
        /// What the host reported.
        output: Output,
    },
    /// It was stopped for memory: failed.
    Memory {
        /// The exit code, when the stopped process gave one.
        exit_code: Option<i32>,
        /// What the host reported.
        output: Output,
    },
    /// The client cancelled it. A job cancelled before it ran has no exit code and no output.
    Cancelled {
        /// The exit code, when the stopped process gave one.
        exit_code: Option<i32>,
        /// What the host reported.
        output: Output,
    },
    /// Its container is gone and no result exists.
    Lost,
    /// It did not get to run.
    FailedToStart(StartFailure),
}

impl JobEnd {
    /// The state the job ends in.
    #[must_use]
    pub fn state(&self) -> JobState {
        match self {
            JobEnd::Exited { exit_code: 0, .. } => JobState::Succeeded,
            JobEnd::Exited { .. } | JobEnd::WallLimit { .. } | JobEnd::Memory { .. } => {
                JobState::Failed
            }
            JobEnd::Cancelled { .. } => JobState::Cancelled,
            JobEnd::Lost => JobState::Lost,
            JobEnd::FailedToStart(failure) => JobState::FailedToStart(*failure),
        }
    }

    /// The reason, the exit code and the output, for an end that has a result.
    fn report(self) -> Option<(ResultReason, Option<i32>, Output)> {
        match self {
            JobEnd::Exited { exit_code, output } => {
                Some((ResultReason::Exit, Some(exit_code), output))
            }
            JobEnd::WallLimit { exit_code, output } => {
                Some((ResultReason::WallLimit, exit_code, output))
            }
            JobEnd::Memory { exit_code, output } => Some((ResultReason::Memory, exit_code, output)),
            JobEnd::Cancelled { exit_code, output } => {
                Some((ResultReason::Cancelled, exit_code, output))
            }
            JobEnd::Lost | JobEnd::FailedToStart(_) => None,
        }
    }
}

/// How long an ended job's record and result are kept: at least [`Retention::MIN`], and
/// [`Retention::DEFAULT`] unless the operator sets another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention(Duration);

/// A retention window shorter than [`Retention::MIN`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a retention window of {window:?} is shorter than the minimum of one minute")]
pub struct RetentionError {
    /// The window asked for.
    pub window: Duration,
}

impl Retention {
    /// The shortest window: one minute.
    pub const MIN: Duration = Duration::from_secs(60);

    /// The window unless the operator sets another: seven days (design question 14).
    pub const DEFAULT: Retention = Retention(Duration::from_secs(7 * 24 * 60 * 60));

    /// The window `window`.
    ///
    /// # Errors
    /// [`RetentionError`] when `window` is shorter than [`Retention::MIN`] (zero included).
    pub fn new(window: Duration) -> Result<Retention, RetentionError> {
        if window < Self::MIN {
            Err(RetentionError { window })
        } else {
            Ok(Retention(window))
        }
    }

    /// The window's length.
    #[must_use]
    pub fn window(self) -> Duration {
        self.0
    }
}

impl Default for Retention {
    fn default() -> Retention {
        Retention::DEFAULT
    }
}

/// `text` cut to its last [`MAX_LOG_STREAM_LEN`] bytes, the cut moved forward to the next
/// character boundary so that no character is split, and whether it was cut.
fn tail_of(mut text: String) -> (String, bool) {
    let Some(mut start) = text.len().checked_sub(MAX_LOG_STREAM_LEN) else {
        return (text, false);
    };
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let cut = start > 0;
    (text.split_off(start), cut)
}

/// `job` ended as `end` says at `now`: its state, its end time, its result (none for a job
/// that was lost or failed to start, with the tails cut to the cap) and its retention end,
/// `now` plus `retention`. This is the one place a job record is made ended.
pub(crate) fn ended(
    job: JobRecord,
    end: JobEnd,
    now: StoreTime,
    retention: Retention,
) -> JobRecord {
    let state = end.state();
    let timing = JobTiming {
        queued_at: job.submitted_at,
        placed_at: job.placed_at,
        started_at: job.started_at,
        ended_at: now,
    };
    let result = end.report().map(|(reason, exit_code, output)| {
        let (stdout_tail, stdout_cut) = tail_of(output.stdout);
        let (stderr_tail, stderr_cut) = tail_of(output.stderr);
        JobResult {
            exit_code,
            reason,
            stdout_tail,
            stderr_tail,
            truncated: output.truncated | stdout_cut | stderr_cut,
            timing,
            usage: output.usage,
        }
    });
    JobRecord {
        state,
        ended_at: Some(now),
        result,
        retention_ends: Some(now.saturating_add(retention.window())),
        ..job
    }
}

/// Whether `job` is past its retention end at `now`: it has one, and `now` is at or after it.
pub(crate) fn past_retention(job: &JobRecord, now: StoreTime) -> bool {
    job.retention_ends.is_some_and(|end| now >= end)
}

/// The ids of the jobs among `jobs` that are past their retention end at `now`, in the order
/// given. A job with no retention end (one that has not ended) is never listed. Pure: it
/// reads no clock and does no I/O.
pub fn sweep<'a>(jobs: impl IntoIterator<Item = &'a JobRecord>, now: StoreTime) -> Vec<JobId> {
    jobs.into_iter()
        .filter(|job| past_retention(job, now))
        .map(|job| job.id.clone())
        .collect()
}

/// `job` with its deadline floor raised to `boot` plus [`BOOT_GRACE`], when it waits to be
/// placed or to start (queued, placed or starting) and its floor is earlier or unset; `None`
/// for a job in any other state, and for one whose floor is at least that late. The raise is
/// a maximum, never the stored floor plus the grace, so boots in a loop do not stretch a
/// wait. Nothing else changes.
pub(crate) fn floored_at_boot(job: &JobRecord, boot: StoreTime) -> Option<JobRecord> {
    let floor = boot.saturating_add(BOOT_GRACE);
    let waits = matches!(
        job.state,
        JobState::Queued | JobState::Placed | JobState::Starting
    );
    let lower = job.deadline_floor.is_none_or(|stored| stored < floor);
    (waits & lower).then(|| JobRecord {
        deadline_floor: Some(floor),
        ..job.clone()
    })
}

/// `job` with its retention end raised to `boot` plus [`BOOT_GRACE`], when it has one that
/// is earlier; `None` when it has none or one at least that late. Nothing else changes.
pub(crate) fn raised_at_boot(job: &JobRecord, boot: StoreTime) -> Option<JobRecord> {
    let floor = boot.saturating_add(BOOT_GRACE);
    let stored = job.retention_ends?;
    (stored < floor).then(|| JobRecord {
        retention_ends: Some(floor),
        ..job.clone()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suite::full_spec;

    fn at(secs: u64) -> StoreTime {
        StoreTime::from_duration(Duration::from_secs(secs))
    }

    fn job(text: &str, state: JobState, retention_ends: Option<StoreTime>) -> JobRecord {
        JobRecord {
            id: JobId::parse(text).expect("an id"),
            spec: full_spec(),
            priority: 0,
            state,
            submitted_at: at(1),
            placed_at: Some(at(2)),
            started_at: Some(at(3)),
            ended_at: None,
            host: None,
            cancel_requested: false,
            cancel_requested_at: None,
            result: None,
            retention_ends,
            deadline_floor: None,
            carried: crate::codec::Carried::default(),
        }
    }

    fn output(stdout: &str) -> Output {
        Output {
            stdout: stdout.to_owned(),
            stderr: "err".to_owned(),
            truncated: false,
            usage: Some(ResourceUsage {
                cpu_time: Duration::from_millis(1500),
                peak_memory_bytes: 4096,
            }),
        }
    }

    // Catches an end mapped to the wrong state or reason (a clean exit read as failed, a wall
    // limit read as an exit, a cancel without its reason), an exit code dropped or invented,
    // and a result made for a job that was lost or failed to start.
    #[test]
    fn every_end_has_its_state_and_its_result() {
        let cases = [
            (
                JobEnd::Exited {
                    exit_code: 0,
                    output: output("ok"),
                },
                JobState::Succeeded,
                Some((ResultReason::Exit, Some(0))),
            ),
            (
                JobEnd::Exited {
                    exit_code: 3,
                    output: output("ok"),
                },
                JobState::Failed,
                Some((ResultReason::Exit, Some(3))),
            ),
            (
                JobEnd::WallLimit {
                    exit_code: Some(137),
                    output: output("ok"),
                },
                JobState::Failed,
                Some((ResultReason::WallLimit, Some(137))),
            ),
            (
                JobEnd::Memory {
                    exit_code: None,
                    output: output("ok"),
                },
                JobState::Failed,
                Some((ResultReason::Memory, None)),
            ),
            (
                JobEnd::Cancelled {
                    exit_code: Some(143),
                    output: output("ok"),
                },
                JobState::Cancelled,
                Some((ResultReason::Cancelled, Some(143))),
            ),
            (JobEnd::Lost, JobState::Lost, None),
            (
                JobEnd::FailedToStart(StartFailure::StartTimeout),
                JobState::FailedToStart(StartFailure::StartTimeout),
                None,
            ),
        ];
        let retention = Retention::new(Duration::from_secs(60)).expect("a window");
        for (end, state, said) in cases {
            assert_eq!(end.state(), state, "{end:?}");
            let record = ended(
                job("e", JobState::Running, None),
                end.clone(),
                at(9),
                retention,
            );
            assert_eq!(record.state, state, "{end:?}");
            assert_eq!(record.ended_at, Some(at(9)), "{end:?}");
            assert_eq!(record.retention_ends, Some(at(69)), "{end:?}");
            let got = record
                .result
                .as_ref()
                .map(|result| (result.reason, result.exit_code));
            assert_eq!(got, said, "{end:?}");
            if let Some(result) = record.result {
                assert_eq!(
                    result,
                    JobResult {
                        exit_code: result.exit_code,
                        reason: result.reason,
                        stdout_tail: "ok".to_owned(),
                        stderr_tail: "err".to_owned(),
                        truncated: false,
                        timing: JobTiming {
                            queued_at: at(1),
                            placed_at: Some(at(2)),
                            started_at: Some(at(3)),
                            ended_at: at(9),
                        },
                        usage: Some(ResourceUsage {
                            cpu_time: Duration::from_millis(1500),
                            peak_memory_bytes: 4096,
                        }),
                    },
                    "{end:?}"
                );
            }
        }
    }

    // Catches a cap that is not enforced, one that keeps the head instead of the tail, a cut
    // that splits a character, a tail at the cap read as cut, and a flag the host set that
    // the store drops.
    #[test]
    fn a_tail_is_cut_to_its_last_bytes_on_a_character_boundary() {
        let at_cap = "a".repeat(MAX_LOG_STREAM_LEN);
        assert_eq!(tail_of(at_cap.clone()), (at_cap.clone(), false));
        assert_eq!(tail_of(String::new()), (String::new(), false));

        // One byte over, with the first character two bytes long: the cut would fall inside
        // it, so it moves to the character after, and the last character (three bytes) is
        // kept whole.
        let body = "a".repeat(MAX_LOG_STREAM_LEN - 4);
        let over = format!("é{body}€");
        assert_eq!(over.len(), MAX_LOG_STREAM_LEN + 1);
        let (kept, cut) = tail_of(over);
        assert_eq!(kept, format!("{body}€"));
        assert!(cut);
        assert!(kept.len() <= MAX_LOG_STREAM_LEN);

        // One byte over, all one byte long: exactly the cap is kept.
        let (kept, cut) = tail_of(format!("x{at_cap}"));
        assert_eq!((kept, cut), (at_cap, true));

        // The host's own flag is kept when nothing is cut here.
        let flagged = Output {
            truncated: true,
            ..output("short")
        };
        let end = JobEnd::Exited {
            exit_code: 0,
            output: flagged,
        };
        let record = ended(
            job("t", JobState::Running, None),
            end,
            at(9),
            Retention::DEFAULT,
        );
        assert_eq!(record.result.map(|result| result.truncated), Some(true));
    }

    // Catches a window below the minimum (zero included) accepted, the minimum itself refused,
    // and a default other than seven days.
    #[test]
    fn a_window_is_at_least_a_minute_and_seven_days_by_default() {
        assert_eq!(
            Retention::new(Duration::ZERO),
            Err(RetentionError {
                window: Duration::ZERO
            })
        );
        let just_under = Duration::from_secs(60) - Duration::from_nanos(1);
        assert_eq!(
            Retention::new(just_under),
            Err(RetentionError { window: just_under })
        );
        assert_eq!(
            Retention::new(Duration::from_secs(60)).map(Retention::window),
            Ok(Duration::from_secs(60))
        );
        assert_eq!(Retention::default().window(), Duration::from_secs(604_800));
        assert_eq!(
            RetentionError {
                window: Duration::ZERO
            }
            .to_string(),
            "a retention window of 0ns is shorter than the minimum of one minute"
        );
    }

    // Catches a sweep that lists a job one instant early, misses one at its end, lists one
    // with no retention end, or reorders what it is given.
    #[test]
    fn the_sweep_lists_exactly_the_jobs_at_or_past_their_end() {
        let jobs = [
            job("late", JobState::Succeeded, Some(at(10))),
            job("live", JobState::Running, None),
            job("early", JobState::Failed, Some(at(5))),
            job("later", JobState::Cancelled, Some(at(11))),
        ];
        let ids = |texts: &[&str]| -> Vec<JobId> {
            texts
                .iter()
                .map(|text| JobId::parse(text).expect("an id"))
                .collect()
        };
        assert_eq!(sweep(&jobs, at(4)), ids(&[]));
        assert_eq!(sweep(&jobs, at(5)), ids(&["early"]));
        assert_eq!(sweep(&jobs, at(10)), ids(&["late", "early"]));
        assert_eq!(sweep(&jobs, at(u64::MAX)), ids(&["late", "early", "later"]));
    }

    // Catches a raise from the stored end (stored plus grace) instead of from the boot, a
    // raise that lowers a later end, and one that gives a live job a retention end.
    #[test]
    fn a_raise_at_boot_lifts_an_earlier_end_to_the_boot_plus_the_grace() {
        let boot = at(1000);
        let early = job("early", JobState::Succeeded, Some(at(10)));
        assert_eq!(
            raised_at_boot(&early, boot),
            Some(JobRecord {
                retention_ends: Some(at(1120)),
                ..early.clone()
            })
        );
        let at_floor = job("floor", JobState::Succeeded, Some(at(1120)));
        assert_eq!(raised_at_boot(&at_floor, boot), None);
        let later = job("later", JobState::Succeeded, Some(at(5000)));
        assert_eq!(raised_at_boot(&later, boot), None);
        let live = job("live", JobState::Queued, None);
        assert_eq!(raised_at_boot(&live, boot), None);
    }

    // Catches a floor added to the stored one (stretching a wait on every boot), a floor
    // that lowers a later one, and a floor on a job that waits for nothing: running, ended.
    #[test]
    fn a_floor_at_boot_is_the_boot_plus_the_grace_for_a_waiting_job_only() {
        let boot = at(1000);
        let floored = |state, floor| JobRecord {
            deadline_floor: floor,
            ..job("j", state, None)
        };
        for state in [JobState::Queued, JobState::Placed, JobState::Starting] {
            assert_eq!(
                floored_at_boot(&floored(state, None), boot),
                Some(floored(state, Some(at(1120)))),
                "{state:?}"
            );
            assert_eq!(
                floored_at_boot(&floored(state, Some(at(1119))), boot),
                Some(floored(state, Some(at(1120)))),
                "{state:?}: an earlier floor is raised to the boot plus the grace, not past it"
            );
            assert_eq!(floored_at_boot(&floored(state, Some(at(1120))), boot), None);
            assert_eq!(floored_at_boot(&floored(state, Some(at(9000))), boot), None);
        }
        for state in [
            JobState::Running,
            JobState::Succeeded,
            JobState::Lost,
            JobState::FailedToStart(StartFailure::NoCapacity),
        ] {
            assert_eq!(
                floored_at_boot(&floored(state, None), boot),
                None,
                "{state:?}"
            );
        }
    }
}

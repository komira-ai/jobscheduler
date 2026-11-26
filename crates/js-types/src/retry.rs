//! A job's retry policy and the decision it gives at the end of each attempt
//! (`docs/design/job-retry-and-completion-callback.md`, sections 3.1 to 3.4).
//!
//! An attempt is one placement of a job. When an attempt ends, [`decide`] is asked, with the
//! job's [`RetryPolicy`], how the attempt ended ([`AttemptEnd`], one variant per row of the
//! design's table in section 3.3), the attempt's number and the job's id, whether another
//! attempt follows ([`Decision::Retry`], with the wait before it) or the job ends
//! ([`Decision::End`]). The decision is pure: it reads no clock and draws no random number,
//! so a simulation replays it.
//!
//! Only a start failure for a cause of the host and a lost host retry: a lost host, a start
//! timeout, an input the host could not read, and an agent's refusal with a code that is not
//! the job's own fault. A refusal code this crate does not know is the host's. Every other end
//! (the job's own exit, limit or input failure, the pool's bound on waiting, the whole-host
//! bound, a cancel) ends the job.
//!
//! The wait before attempt `n + 1` is `min(max, initial × 2^min(n - 1, 12) × (1 + j))`, with
//! `initial` and `max` the policy's effective waits and the jitter `j` in `[0, 0.1)` taken
//! from SHA-256 of the job's id and `n`, so one job's attempt always waits the same.
//!
//! The server refuses a job that carries a policy at submit (`retry-not-enabled`) until it can
//! carry a retry out, so no job is retried yet.

use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::JobId;

/// A job's retry policy: how many attempts it may have, and how long it waits between two.
/// Each attempt runs the work again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// The most attempts, the first included. 0 or 1: no retry.
    pub max_attempts: u32,
    /// The wait before the second attempt. Absent: 10 seconds. Cut to an hour.
    pub initial_backoff: Option<Duration>,
    /// The longest wait, jitter included. Absent: the larger of 5 minutes and the initial
    /// wait. Cut to an hour.
    pub max_backoff: Option<Duration>,
}

/// How one attempt of a job ended: one variant per row of the table in section 3.3 of the
/// design, the two rows of an agent's refusal (a code of the host and a code of the job)
/// sharing [`AttemptEnd::AgentRefused`], whose code tells them apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptEnd {
    /// The job exited with code 0.
    Succeeded,
    /// The job exited with a code other than 0.
    Exited,
    /// The job was stopped at its wall limit.
    WallLimit,
    /// The job was stopped because it passed its memory.
    Memory,
    /// A host path or host port the job named failed while it ran.
    InputFailed,
    /// The attempt's host was lost: the provider read it absent, or it did not list the job
    /// after a hello. The job was placed, starting or running.
    HostLost,
    /// The start did not complete within the pool's start timeout.
    StartTimeout,
    /// An input the job named could not be read on its host.
    InputUnavailable,
    /// The host's agent refused the start with `code`, its own code for why.
    AgentRefused {
        /// The agent's code, as its refusal gave it.
        code: String,
    },
    /// No capacity came within the pool's maximum queue wait.
    NoCapacity,
    /// The whole-host prepare failed as many times as the pool allows.
    ExclusiveNotReady,
    /// The client cancelled the job.
    Cancelled,
}

/// What follows an attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Another attempt, placeable once `wait` has passed from the end of this one.
    Retry {
        /// The wait before the next attempt may be placed.
        wait: Duration,
    },
    /// The job ends with this attempt.
    End,
}

/// The initial wait when a policy gives none.
const DEFAULT_INITIAL_BACKOFF: Duration = Duration::from_secs(10);

/// The least longest wait when a policy gives none: the default is the larger of this and the
/// effective initial wait.
const DEFAULT_MAX_BACKOFF_FLOOR: Duration = Duration::from_secs(300);

/// Each of the policy's waits is cut to this, and so is every wait [`decide`] gives.
const BACKOFF_CEILING: Duration = Duration::from_secs(3600);

/// The most times the initial wait is doubled, so that the shift cannot overflow.
const MAX_DOUBLINGS: u32 = 12;

/// The agent refusal codes that name the job's own fault: a start refused with one of them is
/// refused on every host, so it never retries. They are the codes of
/// `crates/js-agent-wire/src/refusal.rs` that a bad job gets (its spec, image, bundle, program,
/// environment, mounts, files, platform or labels), and those of a host's image and bundle
/// trust policy, which refuse the job's own choice of image or bundle. Every other code, one
/// this crate does not know included, is the host's. Kept sorted, so a test can find a code
/// listed twice.
const JOB_FAULT_REFUSALS: [&str; 18] = [
    "bundle-denied",
    "bundle-digest-required",
    "bundle-index",
    "bundle-not-allowed",
    "bundle-registry-not-allowed",
    "image-digest-required",
    "image-not-allowed",
    "invalid-bundle",
    "invalid-env",
    "invalid-file",
    "invalid-image",
    "invalid-job",
    "invalid-mount",
    "invalid-program",
    "platform-mismatch",
    "provider-refused",
    "reserved-label",
    "wall-time-not-positive",
];

/// Whether an attempt that ended as `end` may be followed by another: the "Retries?" column of
/// the table in section 3.3 of the design. The match names every variant, so a new one does
/// not compile until it is classed here.
fn retries(end: &AttemptEnd) -> bool {
    match end {
        AttemptEnd::HostLost | AttemptEnd::StartTimeout | AttemptEnd::InputUnavailable => true,
        AttemptEnd::AgentRefused { code } => !JOB_FAULT_REFUSALS.contains(&code.as_str()),
        AttemptEnd::Succeeded
        | AttemptEnd::Exited
        | AttemptEnd::WallLimit
        | AttemptEnd::Memory
        | AttemptEnd::InputFailed
        | AttemptEnd::NoCapacity
        | AttemptEnd::ExclusiveNotReady
        | AttemptEnd::Cancelled => false,
    }
}

/// What follows attempt `attempt` (counted from 1; 0 is read as 1, as an older record's
/// attempt is) of the job `job`, which ended as `end`, under `policy`.
///
/// [`Decision::Retry`] when `end` is one that retries (see the module docs) and `attempt` is
/// below `policy.max_attempts`; [`Decision::End`] otherwise, so a `max_attempts` of 0 or 1
/// allows one attempt. The wait is that of the module docs: the same for the same job and
/// attempt, and never above an hour.
#[must_use]
pub fn decide(policy: &RetryPolicy, end: &AttemptEnd, attempt: u32, job: &JobId) -> Decision {
    let attempt = attempt.max(1);
    if !retries(end) || attempt >= policy.max_attempts {
        return Decision::End;
    }
    Decision::Retry {
        wait: backoff(policy, attempt, job),
    }
}

/// The wait after attempt `attempt` (at least 1) of `job`, under `policy`'s effective waits.
/// Computed in whole nanoseconds, so it is the same on every platform.
fn backoff(policy: &RetryPolicy, attempt: u32, job: &JobId) -> Duration {
    let initial = policy
        .initial_backoff
        .unwrap_or(DEFAULT_INITIAL_BACKOFF)
        .min(BACKOFF_CEILING);
    let max = policy
        .max_backoff
        .unwrap_or_else(|| initial.max(DEFAULT_MAX_BACKOFF_FLOOR))
        .min(BACKOFF_CEILING);
    // At most 3.6e12 ns shifted by 12: about 1.5e16, so neither the shift nor the product
    // with a 64-bit fraction (below 2^64) overflows 128 bits.
    let base = initial.as_nanos() << (attempt - 1).min(MAX_DOUBLINGS);
    let jitter = base * u128::from(jitter_fraction(job, attempt)) / (10_u128 << 64);
    Duration::from_nanos_u128((base + jitter).min(max.as_nanos()))
}

/// The jitter's fraction of 2^64: the first 8 bytes, big-endian, of SHA-256 of the job id's
/// bytes followed by the attempt as 4 big-endian bytes. The fixed width keeps `job-1` with
/// attempt 12 apart from `job-11` with attempt 2.
fn jitter_fraction(job: &JobId, attempt: u32) -> u64 {
    let digest = Sha256::new()
        .chain_update(job.as_str().as_bytes())
        .chain_update(attempt.to_be_bytes())
        .finalize();
    let mut word = [0_u8; 8];
    word.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(word)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: &str) -> JobId {
        JobId::parse(id).expect("a job id")
    }

    fn refused(code: &str) -> AttemptEnd {
        AttemptEnd::AgentRefused {
            code: code.to_owned(),
        }
    }

    /// A policy with attempts left at every attempt number below `u32::MAX`, and its waits at
    /// their defaults.
    fn many() -> RetryPolicy {
        RetryPolicy {
            max_attempts: u32::MAX,
            initial_backoff: None,
            max_backoff: None,
        }
    }

    fn policy(max_attempts: u32, initial: Option<Duration>, max: Option<Duration>) -> RetryPolicy {
        RetryPolicy {
            max_attempts,
            initial_backoff: initial,
            max_backoff: max,
        }
    }

    /// What follows attempt `attempt` of the job `id` that lost its host, under `policy`.
    fn after(policy: &RetryPolicy, attempt: u32, id: &str) -> Decision {
        decide(policy, &AttemptEnd::HostLost, attempt, &job(id))
    }

    fn retry(wait: Duration) -> Decision {
        Decision::Retry { wait }
    }

    /// Whether `decision` is a retry whose wait is at least `low` and below `high`.
    fn waits_within(decision: Decision, low: Duration, high: Duration) -> bool {
        matches!(decision, Decision::Retry { wait } if (low..high).contains(&wait))
    }

    /// Asserts that `end` ends the job at each attempt number given, under a policy that
    /// retries a lost host at the same attempt: the end alone decides it, not the policy.
    fn never_retries(end: &AttemptEnd) {
        let id = job("job-1");
        for attempt in [1, 2, 13, u32::MAX - 1] {
            assert!(
                matches!(
                    decide(&many(), &AttemptEnd::HostLost, attempt, &id),
                    Decision::Retry { .. }
                ),
                "the policy has attempts left at attempt {attempt}"
            );
            assert_eq!(
                decide(&many(), end, attempt, &id),
                Decision::End,
                "{end:?} at attempt {attempt}"
            );
        }
    }

    /// Asserts each row of the table in section 3.3 of the design, one case per row, under a
    /// policy with attempts left: the five rows marked "retries" give a retry, and every other
    /// row ends the job. Catches a row classed the other way, such as a non-zero exit that
    /// retries or a start timeout that ends the job.
    #[test]
    fn each_outcome_is_classed_as_the_table_says() {
        let rows = [
            ("exit code 0", AttemptEnd::Succeeded, false),
            ("a non-zero exit", AttemptEnd::Exited, false),
            ("stopped at the wall limit", AttemptEnd::WallLimit, false),
            ("stopped for memory", AttemptEnd::Memory, false),
            (
                "a host path or port failed while it ran",
                AttemptEnd::InputFailed,
                false,
            ),
            ("lost", AttemptEnd::HostLost, true),
            ("the start timed out", AttemptEnd::StartTimeout, true),
            (
                "an input could not be read on the host",
                AttemptEnd::InputUnavailable,
                true,
            ),
            (
                "refused with a code of the host",
                refused("host-tainted"),
                true,
            ),
            (
                "refused with a code of the job",
                refused("invalid-job"),
                false,
            ),
            ("the queue wait ran out", AttemptEnd::NoCapacity, false),
            (
                "max_replace prepares failed",
                AttemptEnd::ExclusiveNotReady,
                false,
            ),
            ("the client cancelled it", AttemptEnd::Cancelled, false),
        ];
        let id = job("job-1");
        for (row, end, retries) in &rows {
            let decision = decide(&many(), end, 1, &id);
            assert_eq!(
                matches!(decision, Decision::Retry { .. }),
                *retries,
                "{row}: {decision:?}"
            );
        }
    }

    /// Asserts that a non-zero exit, the wall limit, a memory stop and an input failure while
    /// the job ran end the job at the first attempt, later ones and the last but one, under a
    /// policy that retries a lost host at each of them. Catches the service retrying a job's
    /// own failure, which is the client's to judge.
    #[test]
    fn a_jobs_own_failure_never_retries() {
        for end in [
            AttemptEnd::Exited,
            AttemptEnd::WallLimit,
            AttemptEnd::Memory,
            AttemptEnd::InputFailed,
            AttemptEnd::Succeeded,
        ] {
            never_retries(&end);
        }
    }

    /// Asserts that a refusal code this crate does not know is the host's and retries: a code
    /// a later agent may add, the empty code, and near misses of a job-fault code (another
    /// case, a trailing space), which an agent never sends for a job's fault. Also asserts that
    /// every host code of a start the agents send today retries. Catches an unknown code read
    /// as the job's fault, which would end a job a host's new fault refused.
    #[test]
    fn an_unknown_refusal_code_is_the_hosts() {
        let id = job("job-1");
        let codes = [
            "a-code-from-a-later-agent",
            "",
            "Invalid-Job",
            "invalid-job ",
            "host-tainted",
            "quota-unavailable",
            "input-not-allowed",
            "input-unavailable",
            "network-above-ceiling",
            "port-above-ceiling",
            "ports-not-supported",
            "native-unsupported",
            "vm-unavailable",
            "image-unsupported-here",
            "run-unsupported",
            "rootfs-unsupported-here",
            "no-full-disk-access",
            "bundle-unsupported",
            "bundle-not-held",
            "bundle-no-space",
            "bundle-fetch-failed",
            "bundle-os-too-old",
            "bundle-xcode-mismatch",
            "xcode-not-held",
            "fuse-jobs-not-allowed",
            "host-jobs-not-allowed",
        ];
        for code in codes {
            assert!(
                !JOB_FAULT_REFUSALS.contains(&code),
                "{code:?} is listed as a fault of the job"
            );
            let decision = decide(&many(), &refused(code), 1, &id);
            assert!(
                matches!(decision, Decision::Retry { .. }),
                "{code:?}: {decision:?}"
            );
        }
    }

    /// Asserts that a refusal with each code naming the job's own fault ends the job, under a
    /// policy with attempts left, and that the list holds the codes the design names, the
    /// host's two other bundle trust-policy codes, and no code twice. Catches a bad job
    /// retried on host after host until its attempts run out.
    #[test]
    fn a_job_fault_refusal_never_retries() {
        for named in [
            "invalid-job",
            "invalid-file",
            "invalid-image",
            "image-not-allowed",
            "bundle-not-allowed",
            "bundle-denied",
            "bundle-registry-not-allowed",
        ] {
            assert!(JOB_FAULT_REFUSALS.contains(&named), "{named} is the job's");
        }
        assert!(
            JOB_FAULT_REFUSALS.windows(2).all(|pair| pair[0] < pair[1]),
            "the list is sorted with no code twice"
        );
        for code in JOB_FAULT_REFUSALS {
            never_retries(&refused(code));
        }
    }

    /// Asserts that `NO_CAPACITY`, `EXCLUSIVE_NOT_READY` and a cancel end the job, under a
    /// policy that retries a lost host at the same attempt. Catches a retry that outlives the
    /// pool's bound on waiting or the whole-host bound, and a cancel that does not win.
    #[test]
    fn an_exhausted_pool_bound_or_a_cancel_never_retries() {
        for end in [
            AttemptEnd::NoCapacity,
            AttemptEnd::ExclusiveNotReady,
            AttemptEnd::Cancelled,
        ] {
            never_retries(&end);
        }
    }

    /// Asserts that a `max_attempts` of 0 or 1 ends the job after its first attempt (named 1,
    /// or 0 as an older record names it) for every end that retries, and that 2 retries it.
    /// Catches 0 read as "no limit", or 1 read as one retry.
    #[test]
    fn zero_and_one_mean_one_attempt() {
        let id = job("job-1");
        let retrying = [
            AttemptEnd::HostLost,
            AttemptEnd::StartTimeout,
            AttemptEnd::InputUnavailable,
            refused("host-tainted"),
        ];
        for end in &retrying {
            for max_attempts in [0, 1] {
                for attempt in [0, 1] {
                    assert_eq!(
                        decide(&policy(max_attempts, None, None), end, attempt, &id),
                        Decision::End,
                        "max_attempts {max_attempts}, attempt {attempt}, {end:?}"
                    );
                }
            }
            for attempt in [0, 1] {
                assert!(
                    matches!(
                        decide(&policy(2, None, None), end, attempt, &id),
                        Decision::Retry { .. }
                    ),
                    "max_attempts 2, attempt {attempt}, {end:?}"
                );
            }
        }
    }

    /// Asserts that under a `max_attempts` of 3 attempts 1 and 2 retry and attempt 3 ends the
    /// job, as does an attempt past it. Catches an off-by-one that gives one attempt too many
    /// or too few.
    #[test]
    fn the_last_attempt_ends_the_job() {
        let id = job("job-1");
        let three = policy(3, None, None);
        for attempt in [1, 2] {
            assert!(
                matches!(
                    decide(&three, &AttemptEnd::StartTimeout, attempt, &id),
                    Decision::Retry { .. }
                ),
                "attempt {attempt}"
            );
        }
        for attempt in [3, 4, u32::MAX] {
            assert_eq!(
                decide(&three, &AttemptEnd::StartTimeout, attempt, &id),
                Decision::End,
                "attempt {attempt}"
            );
        }
    }

    /// Asserts that `max_attempts` of `u32::MAX` is taken as given: the last but one attempt
    /// retries, with a wait within the ceiling, and attempt `u32::MAX` ends the job. Catches a
    /// ceiling the server does not set, and an overflow at the largest attempt number.
    #[test]
    fn the_largest_max_attempts_is_accepted() {
        let id = job("job-1");
        assert_eq!(
            decide(&many(), &AttemptEnd::HostLost, u32::MAX - 1, &id),
            retry(Duration::from_secs(300)),
            "the default longest wait"
        );
        assert_eq!(
            decide(&many(), &AttemptEnd::HostLost, u32::MAX, &id),
            Decision::End
        );
    }

    /// Asserts the backoff of section 3.4: the wait doubles with each attempt from the initial
    /// wait, with a jitter of at least 0 and under a tenth; the doubling stops after 12; the
    /// defaults are 10 s and the larger of 300 s and the initial wait; both waits are cut to an
    /// hour; the longest wait caps the jitter too; an initial wait of 0 waits 0; and one job's
    /// attempt waits the same every time, pinned to SHA-256 of the id and the attempt's 4
    /// big-endian bytes. Catches a wait that grows otherwise, an exponent that overflows, a
    /// default or ceiling that is not the design's, and a jitter that is drawn at random or
    /// scaled past a tenth.
    #[test]
    fn backoff_doubles_to_its_cap_with_bounded_jitter() {
        let ms = Duration::from_millis;
        let secs = Duration::from_secs;

        // Doubling, with the jitter in [0, 0.1) of each base.
        let roomy = policy(u32::MAX, Some(ms(1)), Some(secs(3600)));
        for attempt in 1..=12 {
            let base = ms(1) * (1 << (attempt - 1));
            let got = after(&roomy, attempt, "job-1");
            assert!(
                waits_within(got, base, base + base / 10),
                "attempt {attempt}: {got:?}"
            );
        }
        // The exponent stops at 12: attempts 13 and later wait about 4.096 s, not more.
        for attempt in [13, 14, 1000, u32::MAX - 1] {
            let got = after(&roomy, attempt, "job-1");
            assert!(
                waits_within(got, ms(4096), ms(4096) + ms(4096) / 10),
                "attempt {attempt}: {got:?}"
            );
        }

        // Defaults: 10 s, then a longest wait of 300 s. The first three are pinned.
        let defaults = many();
        let nanos = Duration::from_nanos;
        assert_eq!(after(&defaults, 1, "job-1"), retry(nanos(10_879_015_696)));
        assert_eq!(after(&defaults, 2, "job-1"), retry(nanos(20_491_825_561)));
        assert_eq!(after(&defaults, 1, "job-2"), retry(nanos(10_353_226_127)));
        assert_eq!(
            after(&defaults, 6, "job-1"),
            retry(secs(300)),
            "320 s and more is cut"
        );
        // The default longest wait follows an initial wait above 300 s, and caps its jitter.
        let long = policy(9, Some(secs(400)), None);
        assert_eq!(after(&long, 1, "job-1"), retry(secs(400)));
        // Both waits are cut to an hour.
        let over = policy(9, Some(secs(7200)), None);
        assert_eq!(after(&over, 1, "job-1"), retry(secs(3600)));
        let high_cap = policy(9, Some(secs(1000)), Some(secs(10_000)));
        assert_eq!(after(&high_cap, 3, "job-1"), retry(secs(3600)));
        let both_over = policy(9, Some(secs(7200)), Some(secs(7200)));
        assert_eq!(after(&both_over, 1, "job-1"), retry(secs(3600)));
        // The largest waits the API can carry are cut before any arithmetic, at every attempt.
        let largest = Some(secs(u64::from(u32::MAX)));
        for attempt in [1, 13, u32::MAX - 1] {
            let huge = policy(u32::MAX, largest, None);
            assert_eq!(after(&huge, attempt, "job-1"), retry(secs(3600)));
            let huge_both = policy(u32::MAX, largest, largest);
            assert_eq!(after(&huge_both, attempt, "job-1"), retry(secs(3600)));
        }
        // A longest wait below the initial one (refused at submit) still bounds the wait.
        let low_cap = policy(9, None, Some(secs(5)));
        assert_eq!(after(&low_cap, 1, "job-1"), retry(secs(5)));
        // An initial wait of 0 waits 0.
        let zero = policy(9, Some(Duration::ZERO), None);
        assert_eq!(after(&zero, 5, "job-1"), retry(Duration::ZERO));

        // The same job and attempt wait the same; another job or attempt has its own jitter.
        assert_eq!(after(&defaults, 3, "job-7"), after(&defaults, 3, "job-7"));
        assert_ne!(after(&defaults, 1, "job-1"), after(&defaults, 1, "job-2"));
        // The attempt is 4 fixed bytes, so these two inputs differ.
        assert_ne!(
            jitter_fraction(&job("job-1"), 12),
            jitter_fraction(&job("job-11"), 2)
        );

        // Over many jobs, the jitter spans its range and never reaches a tenth.
        let waits: Vec<Decision> = (0..1000)
            .map(|n| after(&defaults, 1, &format!("job-{n}")))
            .collect();
        assert!(
            waits
                .iter()
                .all(|got| waits_within(*got, secs(10), secs(11)))
        );
        assert!(
            waits
                .iter()
                .any(|got| waits_within(*got, secs(10), ms(10_100))),
            "some jitter is small"
        );
        assert!(
            waits
                .iter()
                .any(|got| waits_within(*got, ms(10_900), secs(11))),
            "some jitter is near a tenth"
        );
    }

    /// Asserts that the two agent refusals are told apart by their code alone, and that a
    /// retry names its wait. Catches a variant that drops the agent's code, which the
    /// classing needs to tell a fault of the host from one of the job.
    #[test]
    fn an_agent_refusal_keeps_its_code_and_a_retry_its_wait() {
        let host = refused("host-tainted");
        let job = refused("invalid-job");
        assert_ne!(host, job);
        assert_eq!(host.clone(), host);
        let retry = Decision::Retry {
            wait: Duration::from_secs(10),
        };
        assert_ne!(retry, Decision::End);
        assert_eq!(
            format!("{retry:?}"),
            "Retry { wait: 10s }",
            "the wait is the decision's"
        );
    }
}

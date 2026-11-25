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
//! This release decides [`Decision::End`] for every attempt: no outcome is classed as one that
//! retries yet, and the server refuses a job that carries a policy, so every job ends after
//! its one attempt, as before the policy existed.

use std::time::Duration;

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

/// What follows attempt `attempt` (counted from 1) of the job `job`, which ended as `end`,
/// under `policy`.
///
/// This release answers [`Decision::End`] for every attempt (see the module docs).
#[must_use]
pub fn decide(policy: &RetryPolicy, end: &AttemptEnd, attempt: u32, job: &JobId) -> Decision {
    let _ = (policy, end, attempt, job);
    Decision::End
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every row of the table in section 3.3, as the variant that stands for it.
    fn every_end() -> [AttemptEnd; 13] {
        [
            AttemptEnd::Succeeded,
            AttemptEnd::Exited,
            AttemptEnd::WallLimit,
            AttemptEnd::Memory,
            AttemptEnd::InputFailed,
            AttemptEnd::HostLost,
            AttemptEnd::StartTimeout,
            AttemptEnd::InputUnavailable,
            AttemptEnd::AgentRefused {
                code: "host-tainted".to_owned(),
            },
            AttemptEnd::AgentRefused {
                code: "invalid-job".to_owned(),
            },
            AttemptEnd::NoCapacity,
            AttemptEnd::ExclusiveNotReady,
            AttemptEnd::Cancelled,
        ]
    }

    /// Asserts that every row of the table ends the job at every attempt, under a policy with
    /// attempts left (the largest `max_attempts`, waits given), one with none, and one at its
    /// defaults: this release makes no retry. Catches a retry made before the server can
    /// carry one out, which would leave a job queued that nothing places again.
    #[test]
    fn no_outcome_retries_in_this_release() {
        let job = JobId::parse("job-1").expect("a job id");
        let policies = [
            RetryPolicy {
                max_attempts: u32::MAX,
                initial_backoff: Some(Duration::from_secs(1)),
                max_backoff: Some(Duration::from_secs(60)),
            },
            RetryPolicy {
                max_attempts: 1,
                initial_backoff: None,
                max_backoff: None,
            },
            RetryPolicy {
                max_attempts: 0,
                initial_backoff: None,
                max_backoff: None,
            },
        ];
        for policy in &policies {
            for end in &every_end() {
                for attempt in [1, 2, u32::MAX] {
                    assert_eq!(
                        decide(policy, end, attempt, &job),
                        Decision::End,
                        "{policy:?} {end:?} {attempt}"
                    );
                }
            }
        }
    }

    /// Asserts that the two agent refusals are told apart by their code alone, and that a
    /// retry names its wait. Catches a variant that drops the agent's code, which the
    /// classing needs to tell a fault of the host from one of the job.
    #[test]
    fn an_agent_refusal_keeps_its_code_and_a_retry_its_wait() {
        let host = AttemptEnd::AgentRefused {
            code: "host-tainted".to_owned(),
        };
        let job = AttemptEnd::AgentRefused {
            code: "invalid-job".to_owned(),
        };
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

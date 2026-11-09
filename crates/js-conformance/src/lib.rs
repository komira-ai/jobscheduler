//! The conformance suites of the job scheduler: what a host agent must do, what the job
//! service must answer, and what the reference core must keep.
//!
//! The host-agent suite, which is wire-neutral and takes a harness per transport, is the
//! [`agent`] module.
//!
//! The job-service suite, which holds a server of `jobscheduler.v1.JobService` (the real one
//! or a fake that a client's tests talk to) to the answers of the real service, is the
//! [`jobs`] module. It takes a harness that gives a tonic client and a few hooks (the
//! scheduler's and the agent's moves, the clock, the retention sweep) and ships an in-memory
//! reference service ([`jobs::ReferenceJobs`]) that each of its scenarios is shown able to
//! fail against.

pub mod agent;
pub mod core;
pub mod jobs;

/// What one check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The subject did what the contract says.
    Passed,
    /// The subject differed from the contract; the text says how.
    Failed(String),
    /// The subject cannot take part in this check.
    NotApplicable,
}

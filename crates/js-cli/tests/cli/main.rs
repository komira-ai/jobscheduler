//! Behaviour of `js`: the checks before any request, the commands that were removed, the
//! program itself run as a child process, and the job commands against an in-memory job
//! server.

mod binary;
mod fake_jobs;
mod flags;
mod help;
mod job_conformance;
mod job_loops;
mod jobs;
mod queue_status;
mod support;

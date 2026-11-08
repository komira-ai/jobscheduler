//! Behaviour of `js` against a real lease server started in the test process on
//! 127.0.0.1: every subcommand and exit status, the pending poll driven through the
//! injected waiter, TLS, the refusals before any request, the commands that were removed,
//! lost replies, and the program itself run as a child process; and the job commands
//! against an in-memory job server.

mod acquire;
mod binary;
mod failures;
mod fake_jobs;
mod flags;
mod help;
mod job_conformance;
mod job_loops;
mod jobs;
mod join_token;
mod lost_reply;
mod native;
mod ops;
mod queue_status;
mod servers;
mod support;
mod tls;

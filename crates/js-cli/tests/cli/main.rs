//! Behaviour of `js` against a real server started in the test process on 127.0.0.1: the
//! join-token commands and their exit statuses, TLS, the refusals before any request, the
//! commands that were removed, and the program itself run as a child process; and the job
//! commands against an in-memory job server.

mod binary;
mod failures;
mod fake_jobs;
mod flags;
mod help;
mod job_conformance;
mod job_loops;
mod jobs;
mod join_token;
mod queue_status;
mod support;
mod tls;

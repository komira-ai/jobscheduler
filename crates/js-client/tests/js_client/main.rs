//! Behaviour of the lease client: the server address rules, the owner-only token file,
//! and requests against small servers started in the test process on 127.0.0.1, over
//! plain http and over TLS with a certificate made for the test; with the `jobs` feature,
//! the job client against an in-memory job server.

mod client;
// The job client is behind the `jobs` feature, which a workspace test build turns on (js-cli
// needs it) and a test build of js-client alone does not.
#[cfg(feature = "jobs")]
mod fake_jobs;
#[cfg(feature = "jobs")]
mod job_conformance;
#[cfg(feature = "jobs")]
mod jobs;
#[cfg(feature = "jobs")]
mod loops;
mod run;
mod server_url;
mod support;
mod token;

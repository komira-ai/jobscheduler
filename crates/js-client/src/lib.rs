//! A client of the server's token-bearing HTTP API, shared by the programs that call it:
//! a server address checked for the rule that a token crosses a network only inside TLS,
//! the bearer token read from an owner-only file, and an HTTP client that trusts only
//! the PEM file it is given or, without one, the public roots bundled in `webpki-roots`
//! (never the platform's store), and sends the token to the one host named. It keeps one
//! connection to the server between requests, idle for at most 8 s, so a program that calls
//! the server again and again (the agent's heartbeats) pays one TCP and TLS handshake a
//! connection, not one a request.
//!
//! A client can authenticate by the host's identity instead: a client certificate and key in
//! two files (`HostIdentity`, given by `--host-cert` and `--host-key`), presented in the TLS
//! handshake with no `Authorization` header and read again before each request, so a
//! renewed pair is used without a restart. The key must pass `host_key_problem`: owned by
//! root or by the user running the program, no access for others, not writable by its group.
//!
//! Failures name the flags `--server`, `--token-file`, `--ca-cert` and (for the identity
//! files) `--host-cert` and `--host-key`, so a program that uses this crate takes its
//! settings from flags of those names.
//!
//! With the `jobs` feature (off by default; js-cli turns it on), `jobs` is the client of
//! the job service (`jobscheduler.v1`, gRPC), under the same rules: `jobs::JobClient`, whose
//! methods are generated from the service definition, and `jobs::JobError`, which says
//! whether a failed call may have taken effect. It sends the one token it is given; the
//! server knows it as one of its clients' (`--token-file` or a `--client-token` entry there)
//! and records that client's name on each job it submits.

mod client;
mod error;
mod identity;
#[cfg(feature = "jobs")]
pub mod jobs;
mod server_url;
mod token;

pub use client::{Client, REQUEST_LIMIT, Reply};
pub use error::ClientError;
pub use identity::{HostIdentity, host_key_problem};
pub use server_url::ServerUrl;
pub use token::{bearer, bearer_of};

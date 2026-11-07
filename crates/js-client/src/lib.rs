//! A client of the server's token-bearing HTTP API, shared by the programs that call it:
//! a server address checked for the rule that a token crosses a network only inside TLS,
//! the bearer token read from an owner-only file, and an HTTP client that trusts only
//! the PEM file it is given or, without one, the public roots bundled in `webpki-roots`
//! (never the platform's store), and sends the token to the one host named.
//!
//! Failures name the flags `--server`, `--token-file` and `--ca-cert`, so a program that
//! uses this crate takes its settings from flags of those names.
//!
//! With the `jobs` feature (off by default; js-cli turns it on), `jobs` is the client of
//! the job service (`jobscheduler.v1`, gRPC), under the same rules: `jobs::JobClient`, whose
//! methods are generated from the service definition, and `jobs::JobError`, which says
//! whether a failed call may have taken effect. It sends the one token it is given; the
//! server knows it as one of its clients' (`--token-file` or a `--client-token` entry there)
//! and records that client's name on each job it submits.

mod client;
mod error;
#[cfg(feature = "jobs")]
pub mod jobs;
mod server_url;
mod token;

pub use client::{Client, REQUEST_LIMIT, Reply};
pub use error::ClientError;
pub use server_url::ServerUrl;
pub use token::{bearer, bearer_of};

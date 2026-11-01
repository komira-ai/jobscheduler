//! The client side of the lease server's HTTP API, shared by the programs that call it:
//! a server address checked for the rule that a token crosses a network only inside TLS,
//! the bearer token read from an owner-only file, and an HTTP client that trusts only
//! the PEM file it is given or, without one, the public roots bundled in `webpki-roots`
//! (never the platform's store), and sends the token to the one host named.
//!
//! Failures name the flags `--server`, `--token-file` and `--ca-cert`, so a program that
//! uses this crate takes its settings from flags of those names.
//!
//! [`create_new`] is how the programs that put a secret in a file do it: a new file,
//! written whole beside its name and linked to it, never over a file that is there.

mod client;
mod error;
mod new_file;
mod run;
mod server_url;
mod token;

pub use client::{Client, REQUEST_LIMIT, Reply};
pub use error::ClientError;
pub use new_file::create_new;
pub use run::{LogTail, Outcome, RunRead, RunReadError};
pub use server_url::ServerUrl;
pub use token::{bearer, bearer_of};

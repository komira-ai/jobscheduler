//! The client side of the lease server's HTTP API, shared by the programs that call it:
//! a server address checked for the rule that a token crosses a network only inside TLS,
//! the bearer token read from an owner-only file, and an HTTP client that trusts only
//! the roots it is given and sends the token to the one host named.
//!
//! Failures name the flags `--server`, `--token-file` and `--ca-cert`, so a program that
//! uses this crate takes its settings from flags of those names.

mod client;
mod error;
mod server_url;
mod token;

pub use client::{Client, REQUEST_LIMIT, Reply};
pub use error::ClientError;
pub use server_url::ServerUrl;
pub use token::bearer;

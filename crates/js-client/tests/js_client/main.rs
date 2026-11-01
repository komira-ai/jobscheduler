//! Behaviour of the lease client: the server address rules, the owner-only token file,
//! and requests against small servers started in the test process on 127.0.0.1, over
//! plain http and over TLS with a certificate made for the test.

mod client;
mod run;
mod server_url;
mod support;
mod token;

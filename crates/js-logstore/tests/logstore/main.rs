//! The tests of js-logstore, in one binary so that every line and branch is covered by the
//! tests of one profile: the layout, the signer against the published vectors, the client
//! against the fake store and against scripted answers, and `UreqTransport` against a server on
//! 127.0.0.1. No test sleeps or reads the time: the fake store and the clients share a
//! `ManualClock`. Only the transport tests open sockets, all on 127.0.0.1 with port 0.
//!
//! The one exception is `s3_compat` (`tests/s3_compat/`): the client and the STS minter against
//! a real S3-compatible store with STS, at the system clock. Its tests are `#[ignore]`d, and
//! CI's `test` job, which runs the store, runs them with `--ignored`.

mod answers;
mod endpoint;
mod fake_presigned;
mod fake_requests;
mod fake_sts;
mod layout;
mod presigned;
#[path = "../s3_compat/mod.rs"]
mod s3_compat;
mod secrets;
mod server;
mod signer;
mod store;
mod support;
mod transport;
mod vectors;

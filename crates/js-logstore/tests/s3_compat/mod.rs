//! The store client and the STS minter against a real S3-compatible store with STS: the one
//! `js_testkit::s3` names (CI's `test` job runs it in a service container, pinned by digest).
//! The fake store the rest of the binary runs against checks requests with this crate's own
//! signer, so a mistake made the same way on both sides passes there; here the store's own
//! checks decide.
//!
//! Every test is `#[ignore]`d, with the reason, so the default `cargo test` never reaches for
//! the store; the `test` job runs them with `--ignored`. They sign at the system clock (the
//! store refuses a signing time more than 15 minutes from its own) and wait for nothing but the
//! store's answers. Each puts its objects under a log prefix of its own (`js_testkit::s3::unique`),
//! so no two tests, and no two runs, share a key.

mod objects;
mod refusals;
mod sts;
mod support;

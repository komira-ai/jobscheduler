//! The tests of js-logmint, in one binary so that every line and branch is covered by the tests
//! of one profile: the names and the session policy, the STS minter against the fake store's
//! STS and against scripted answers, and the fake minter. No test sleeps, reads the time or
//! opens a socket: the fake store and the minters share a `ManualClock`, and a held mint is
//! waited for through the fake minter's own count.

mod fake;
mod names;
mod policy;
mod presigned;
mod scope;
mod sts;
mod sts_answers;
mod support;

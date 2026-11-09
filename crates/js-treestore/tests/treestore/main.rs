//! The tree store's tests, in one binary so the generic store is compiled once.
//!
//! Nothing here starts a child process; those tests are in `tests/children`. A child holds
//! a copy of every file the test process has open from its spawn until its exec closes
//! them, among them the lock of a store another test has just dropped, and that test's
//! next open of the directory is then refused as `Locked`.

mod common;
mod crash;
mod ensure;
mod evict;
mod flight;
mod mac_checks;
mod mac_seal;
mod restart;
mod review;

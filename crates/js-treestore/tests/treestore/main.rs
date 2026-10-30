//! The tree store's tests, in one binary so the generic store is compiled once.

mod common;
mod crash;
mod ensure;
mod evict;
mod flight;
mod harness_checks;
mod mac_checks;
#[cfg(target_os = "macos")]
mod mac_host;
mod mac_seal;
#[path = "../support/priv_harness.rs"]
mod priv_harness;
mod restart;
mod review;

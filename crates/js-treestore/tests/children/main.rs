//! The tests that start child processes, kept apart from the store's tests in
//! `tests/treestore`: a child spawned while another test drops a store and opens its
//! directory again holds the store's lock until its exec, and the open is refused.

mod harness_checks;
#[cfg(target_os = "macos")]
mod mac_host;
#[path = "../support/priv_harness.rs"]
mod priv_harness;

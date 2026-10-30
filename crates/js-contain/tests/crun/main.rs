//! Tests against a real `crun`: the image unpack run by the daemon's helper modes in a
//! helper container, and the root it publishes, seen from the host and from a job.
//!
//! Every test is `#[ignore]`: they need `crun`, `newuidmap`/`newgidmap`, a subordinate id
//! range for the user, cgroup v2 with a delegated unit, and (for the image a job runs)
//! the network. A missing prerequisite fails a test, never skips it. They are run by
//! `cargo xtask crun-tests`, which starts each test target in a `Delegate=yes` unit as an
//! unprivileged user; the normal `cargo test` lists them as ignored.
//!
//! The helper containers run the `js-helper` binary of this package (the default
//! helper modes), copied under the scratch directory by the runner. No test waits for a
//! clock or asserts on how long anything took.

#[allow(dead_code)]
#[path = "../support/mod.rs"]
mod support;

mod cell;
mod image;
mod job;
mod source;

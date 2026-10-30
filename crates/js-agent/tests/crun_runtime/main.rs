//! The runtime conformance suite (`tests/runtime_suite`) over `CrunRuntime` and a real
//! `crun`, as the agent runs it on a node: a root process that starts every container as a
//! dedicated non-root job user.
//!
//! Every test is `#[ignore]`: they need `crun`, `newuidmap`/`newgidmap`, a job user with a
//! subordinate id range (`JS_JOB_USER`), cgroup v2 with a delegated unit, root, and the
//! network (the image is busybox, pinned by digest in `JS_TEST_IMAGE`). A missing
//! prerequisite fails a test, never skips it. They are run by `cargo xtask crun-tests
//! --job-user <name>`, which starts the binary as root in a `Delegate=yes` unit.
//!
//! No test waits for a clock or asserts on how long anything took: a job that must be up
//! before it is signalled says so by opening a named pipe the test reads, and a job that
//! must have ended is waited for by the runtime.
#![cfg(target_os = "linux")]

#[path = "../runtime_suite/mod.rs"]
mod runtime_suite;
mod subject;
mod sweep;

use runtime_suite::conformance;

conformance!(subject::crun_subject, all());

//! Tests against a real `crun`, driven the way the agent drives it: by a ROOT daemon that
//! runs each job's container as a dedicated non-root job user (privilege option B of
//! `docs/design/agent-privilege.md`), from a `Delegate=yes` system unit. Each test starts a
//! container and reads, from INSIDE it, what the job sees; none asserts what the bundle
//! says it should see.
//!
//! - `inside`: the ids the job runs as, its capabilities, the seccomp filter, the root's
//!   read-only mount, `/tmp`, the network namespace, the cgroup's OOM group and the
//!   exec-root overlay's options.
//! - `limits`: an out-of-memory kill is the lease's own, a kill leaves no process or
//!   cgroup, and one job that has used up its process limit does not stop another from
//!   forking (the limit is the lease cgroup's `pids.max` or the job's `RLIMIT_NPROC`, and
//!   both jobs run as the same ids).
//! - `isolation`: a lease cannot see a sibling lease's directory.
//! - `pull`: an image pulled through the real unpack helper is published owned by the
//!   helper's id, in a store other accounts cannot list or write.
//!
//! Every test is `#[ignore]`: they need root, `crun` at `/usr/bin/crun`, `uidmap`, a static
//! `busybox`, a job user with a subordinate id range (named by `JS_JOB_USER`) and a
//! delegated cgroup v2 unit. A missing prerequisite fails a test, never skips it. They are
//! run by `cargo xtask crun-tests --job-user <name>`, which starts this target in a unit as
//! root; the normal `cargo test` lists them as ignored.
//!
//! No test waits for a clock or asserts how long anything took. Two jobs that must overlap
//! are ordered by named pipes they open (an open blocks until the other end is opened), and
//! an out-of-memory run ends when its process is killed.
//!
//! The image is a root filesystem made here from the static busybox (no network, no
//! unpacking), seeded into the image store as an unpack leaves it. The hosted runner's
//! kernel and AppArmor policy differ from a node's, so a node's own canary stays the gate.

#[allow(dead_code)]
#[path = "../support/mod.rs"]
mod support;

mod cell;
mod inside;
mod isolation;
mod limits;
mod planted;
mod pull;

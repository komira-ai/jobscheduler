//! The privilege model of js-agent when it launches jobs through the rootless crun runner
//! instead of podman, settled by what the kernel does. Two shapes are compared, each duty
//! of the agent against each (see `docs/design/agent-privilege.md`):
//!
//! - **A**: an agent that is not root and holds an explicit set of capabilities (ambient);
//! - **B**: an agent that is root and runs its jobs as a non-root job user.
//!
//! The tests are `#[ignore]`d, so `cargo test` is unchanged. They need root: the `agent-
//! privilege` job of `.github/workflows/ci.yml` runs them under `sudo`. A test that plays
//! an agent of shape A starts this very test binary again (the `harness::child` test) as an
//! ordinary user (uid 2000) with exactly the capabilities the test names, runs one probe in
//! it and reads what the probe printed; the harness also reads the child's own
//! `CapEff`/`CapAmb`, so a child that did not get the set the test asked for fails the test.
//! A test that plays shape B runs in the test process itself, which is root.
//!
//! The XFS tests need a prjquota file system: the workflow mounts a loop image and names it
//! in `JS_PRIV_XFS_ROOT`. A missing variable is a failure, never a skip.
#![cfg(target_os = "linux")]

mod egress;
mod harness;
mod init_ns;
mod layout;
mod quota;
mod state_dir;
mod watch;

use harness::Kv;

/// The probe `name`, run in the child.
fn probe(name: &str) -> Vec<Kv> {
    init_ns::probe(name)
        .or_else(|| egress::probe(name))
        .or_else(|| watch::probe(name))
        .or_else(|| quota::probe(name))
        .or_else(|| layout::probe(name))
        .or_else(|| state_dir::probe(name))
        .unwrap_or_else(|| panic!("no probe named {name}"))
}

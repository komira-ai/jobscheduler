//! The egress spike (plan item P5): which way of giving a rootless `crun` job a network
//! that reaches a few endpoints, while the build command it runs has none, works on a
//! hosted `ubuntu-24.04` runner, and what each way needs.
//!
//! Every test is `#[ignore]`d: they need `crun`, `pasta`, `nft`, `newuidmap`, `nsenter`,
//! passwordless `sudo` and a subordinate id range, which the `egress-spike` workflow
//! installs and sets up. Each runs a real `crun` container as the unprivileged runner user.
//! The container's program is this test binary itself (`probe_entry` below), bound in
//! read-only, so the probe needs no image. Results are printed as `RESULT` lines (and
//! appended to the file named by `JS_EGRESS_SPIKE_REPORT`) for the table in the pull request.
//!
//! - `exp_a`: the inner `unshare(NEWUSER|NEWNET)` of the build command, with and without
//!   `kernel.apparmor_restrict_unprivileged_userns`, and with a job AppArmor profile.
//! - `exp_b`: the container joining a pre-made network namespace by path.
//! - `exp_c`: an nft allow-list in the container's own namespace that the job cannot edit.
//! - `exp_d`: rootless `pasta` as the container's network, with the allow-list.
//! - `exp_e`: a root helper that plumbs a veth, and a namespace pair the container joins.

mod bundle;
mod exp_a;
mod exp_b;
mod exp_c;
mod exp_d;
mod exp_e;
mod exp_f;
mod exp_g;
mod host;
mod probe;
mod report;
mod scenario;
mod seccomp;

/// The program the container runs: does what `JS_EGRESS_PROBE` says (see `probe`) and
/// prints `PROBE key=value` lines. Without the variable it does nothing.
#[test]
#[ignore = "the container's program; run by the egress tests"]
fn probe_entry() {
    probe::entry();
}

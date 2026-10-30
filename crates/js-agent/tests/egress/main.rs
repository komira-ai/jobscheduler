//! Egress of jobs in `crun` containers under privilege option B (a root agent; the job's
//! `crun` runs as a dedicated non-root user), against the real kernel: the agent's
//! `egress` module, its firewall and the veth it wires, and real containers made by
//! js-contain's `Oci`. See `docs/design/agent-egress.md`.
//!
//! Every test is `#[ignore]`d. They need root, `ip`, `nft`, `unshare`, `nsenter`, the
//! distribution's `/usr/bin/crun`, `uidmap`, a static `busybox` and a job user with a
//! subordinate id range; the `agent-egress` job of `.github/workflows/ci.yml` prepares all of
//! it, runs this target under `sudo` with `--test-threads=1`, and sets:
//!
//! - `JS_B_CRUN`, `JS_B_BUSYBOX`: the programs;
//! - `JS_B_UID`, `JS_B_GID`: the job user.
//!
//! A missing variable or tool is a failure, never a skip. The tests change host-wide state
//! (the `js_egress` tables, `net.ipv4.ip_forward`, a far network namespace `js-far` standing
//! for another host, a veth pair `vfar0`), so they take one lock and run one at a time.
//!
//! Two kinds of job stand in each other's place. A *holder* is a process in a network
//! namespace of its own (`unshare --net sleep`): what the agent wires is a namespace, and a
//! holder shows that part with no container in the way, so most behaviours are measured
//! there. A *container* is the real thing: `Oci` starts it as the job user with a user
//! namespace of its own, and the agent wires it between the start and the job's first
//! instruction. The end-to-end tests use containers.
#![cfg(target_os = "linux")]

mod allow;
mod container;
mod dns;
mod ipv6;
mod lifecycle;
mod modes;
mod premise;
mod sys;
mod tamper;

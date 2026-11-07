//! The v1 job path end to end over real processes and real sockets: the `js-lease-server`
//! binary with `--job-listen` and `--placement js-sched` on loopback ports the kernel picks, an
//! agent registered on its lease port that runs each job through the `crun` runtime over a
//! stand-in `crun`, and jobs submitted through the `js` command line (over gRPC) and through
//! the HTTP/JSON gateway (`POST /v1/jobs`). Each case runs against a server whose `--store` is
//! a SQLite file (`--allow-sqlite-store`) and against one whose store is a Postgres database
//! of its own. Linux only: only Linux runs the crun runtime. With either store the jobs and
//! their usage rows are in the job store the server keeps beside its lease file; `--store`
//! holds the hosts, the join tokens and the scaler records. So the Postgres cases run the
//! whole path with the server's state in Postgres, and the restart and the usage row (which
//! `GetUsage` lists beside the usage store's) are read from that job store in both.
//!
//! The agent is `js_agent::run_with` on a thread of this binary: the real binary needs root
//! and the machine's own `/etc` and `/sys/fs/cgroup` for `--runtime crun`, which a test may not
//! have. Its transport, its job worker and its crun runtime are the binary's; the probes'
//! programs and files, the post-job checks' view of the host and the quota are replaced
//! (`v1_acceptance/agent.rs`). Its waits are a gate the case opens, so every wait for progress
//! is a bounded number of the agent's heartbeats, never a clock. The command line is
//! `js_cli::run_with` with the services `js`'s `main` passes.
//!
//! This binary has no libtest harness: each container's supervisor is this binary run in a
//! helper mode, which `main` answers before anything else, as `js-agent`'s `main` does. It
//! takes libtest's common arguments: names to filter by (a case whose name contains one runs),
//! `--exact`, `--skip NAME`, `--list`, and `--ignored` (no case is ignored, so it runs
//! none); the others are accepted and change nothing. The Postgres cases need the server
//! `KBF_TEST_POSTGRES_URL` names (CI's `test` and `coverage` jobs have one) and fail, not
//! skip, without it, as the server's other Postgres tests do.

#[cfg(target_os = "linux")]
#[path = "v1_acceptance/agent.rs"]
mod agent;
#[cfg(target_os = "linux")]
#[path = "v1_acceptance/cases.rs"]
mod cases;
#[cfg(target_os = "linux")]
#[path = "v1_acceptance/cli.rs"]
mod cli;
#[cfg(target_os = "linux")]
#[path = "v1_acceptance/http.rs"]
mod http;
#[cfg(target_os = "linux")]
#[path = "v1_acceptance/node.rs"]
mod node;
#[cfg(target_os = "linux")]
#[path = "v1_acceptance/rig.rs"]
mod rig;
#[cfg(target_os = "linux")]
#[path = "v1_acceptance/runner.rs"]
mod runner;
#[cfg(target_os = "linux")]
#[path = "v1_acceptance/server.rs"]
mod server;

use std::process::ExitCode;

#[cfg(target_os = "linux")]
fn main() -> ExitCode {
    let modes = js_contain::oci::helper::HelperModes::default();
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    node::before_supervise(&modes.supervise, &args);
    if let Some(code) = js_contain::modes::dispatch_args(&modes, &args) {
        return ExitCode::from(code);
    }
    runner::run(&cases::all())
}

#[cfg(not(target_os = "linux"))]
fn main() -> ExitCode {
    println!("v1_acceptance: no case runs here: the agent's crun runtime is Linux only");
    ExitCode::SUCCESS
}

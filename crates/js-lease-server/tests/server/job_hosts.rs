//! What the tests that place jobs on registered hosts share: a server whose scheduler
//! places jobs (`--placement js-sched`) over a pool of CPU hosts that name their arch, and
//! the hello of such a host.

use std::ffi::OsString;

use serde_json::{Value, json};

use crate::agents_support::POOL;
use crate::jobs_support::{job_args, per_job_hello};
use crate::support::{Harness, Kept, Scratch, replace};

/// The shared pool without its GPU: the scheduler starts no job on a host with GPUs.
pub fn cpu_pool() -> String {
    POOL.replace("gpus = 1\ngpu_class = \"nvidia-a100\"\n", "")
}

/// The flags of a server with agent registration on, the job service on a loopback port,
/// the scheduler placing jobs on the CPU pool, and the bindings kept as `kept` says.
pub fn placing_args(scratch: &Scratch, kept: Kept) -> Vec<OsString> {
    let args = job_args(scratch, "127.0.0.1:0");
    scratch.write("agent-pool.toml", &cpu_pool(), 0o644);
    kept.args(scratch, args)
}

/// A server over a fresh scratch directory that places jobs on CPU hosts, keeping its
/// bindings as `kept` says.
pub async fn placing_server(name: &str, kept: Kept) -> Harness {
    let scratch = Scratch::new(&kept.named(name));
    let args = placing_args(&scratch, kept);
    Harness::with(scratch, args, None).await
}

/// Stops `first` and starts a server over its scratch directory with `--boot resume`.
pub async fn restarted_placing(first: Harness) -> Harness {
    let kept = Kept::of(&first);
    let Harness {
        running, scratch, ..
    } = first;
    running.shutdown().await.unwrap();
    let args = replace(placing_args(&scratch, kept), "--boot", "resume");
    Harness::with(scratch, args, None).await
}

/// The hello of a CPU host `host` of the pool that speaks the per-job messages: 8 cores and
/// 16 GiB, no GPU.
pub fn cpu_hello(host: &str) -> Value {
    let mut hello = per_job_hello(host);
    hello["shape"] = json!({ "cpu_millis": 8000, "memory_bytes": 17179869184u64, "gpus": 0 });
    hello
}

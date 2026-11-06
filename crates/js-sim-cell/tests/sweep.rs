//! The sweep: the smoke family (`cell/burst.rs`, `cell/lossy_wire.rs`,
//! `cell/partition.rs`) over the seeds its flags name, one shard of a sweep at a time
//! (`js_sim_cell::sweep`, docs/simulation.md). The nightly `sim-sweep` workflow runs it:
//!
//! ```text
//! cargo test --release --locked -p js-sim-cell --test sweep -- --start 0 --count 4000 --shards 8 --shard 3
//! ```
//!
//! It has no harness (see `Cargo.toml`): run with none of its flags, as a plain `cargo test`
//! runs it, it says so on stderr and sweeps nothing.

#[path = "cell/burst.rs"]
mod burst;
#[path = "cell/lossy_wire.rs"]
mod lossy_wire;
#[path = "cell/partition.rs"]
mod partition;

use std::process::ExitCode;

fn main() -> ExitCode {
    let family = [
        burst::a_hundred_hosts_settle_four_hundred_runs(),
        lossy_wire::a_lossy_agent_wire_settles_every_run(),
        partition::a_partition_of_ten_agents_that_heals_settles_every_run(),
    ];
    let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
    js_sim_cell::sweep::run(&family, std::env::args_os(), &mut out, &mut err)
}

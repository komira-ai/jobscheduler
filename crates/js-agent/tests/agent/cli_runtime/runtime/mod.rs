//! `CliRuntime` over a runner that answers like the command line: every call, every
//! answer, and every failure. One file per topic: the `create` call and its refusals,
//! the cleanup of a failed create, the mount read-back, `poll`, `stop` and `remove`, the
//! listing, and the engine's environment, scratch and clock.

mod cleanup;
mod create;
mod engine;
mod list;
mod mounts;
mod poll;

use js_agent::cli_runtime::CliRuntime;
use js_agent::lifecycle::{ContainerId, ContainerRuntime};

use super::scripted::Scripted;
use super::spec;

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

/// Starts `run_id` in `world` and returns its container id.
fn start(rt: &mut CliRuntime, world: &Scripted, run_id: &str) -> ContainerId {
    let id = rt.create_and_start(&spec(run_id)).unwrap();
    assert_eq!(Some(id.0.clone()), world.id_of(run_id));
    id
}

/// The first subcommand of every call, in order.
fn subs(world: &Scripted) -> Vec<String> {
    world
        .calls()
        .into_iter()
        .map(|call| call[0].clone())
        .collect()
}

//! `js-update-hook-fake`: `js-agent update-hook` over the fake ports, for the conformance
//! harness (tests only; not shipped).
//!
//! `js-update-hook-fake [--world <dir>] [--control <dir>] update-hook <step> ...`. The two
//! directories are the fakes' world and control files (tests/update_hook_fakes); without a
//! flag the variables `UPDATE_HOOK_FAKE_WORLD` and `UPDATE_HOOK_FAKE_CONTROL` name them. Only
//! this test binary reads the environment; the agent reads none.
//!
//! The steps are still stubs: it ends `not_implemented`, exit 1.

#![allow(dead_code)]

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use js_agent::update_hook::{Gate, Host, run};

#[path = "../update_hook_fakes/mod.rs"]
mod fakes;

/// The kill points of the conformance harness.
// TODO(slice 9): map this to `update_hook::conformance::gate::point()` once the kbf
// `update-hook` crate is a dev-dependency; until then a point is not counted.
struct HarnessGate;

impl Gate for HarnessGate {
    fn point(&self) {}
}

/// The directory a flag or its variable names.
fn dir_of(flag: &str, var: &str, args: &mut Vec<OsString>) -> Option<PathBuf> {
    match args.iter().position(|arg| arg == flag) {
        Some(at) if at + 1 < args.len() => {
            let value = args.remove(at + 1);
            args.remove(at);
            Some(PathBuf::from(value))
        }
        _ => std::env::var_os(var).map(PathBuf::from),
    }
}

fn main() -> ExitCode {
    let mut args: Vec<OsString> = std::env::args_os().collect();
    let world = dir_of("--world", "UPDATE_HOOK_FAKE_WORLD", &mut args);
    let control = dir_of("--control", "UPDATE_HOOK_FAKE_CONTROL", &mut args);
    let (Some(world), Some(control)) = (world, control) else {
        eprintln!("js-update-hook-fake: --world and --control (or their variables) are required");
        return ExitCode::from(2);
    };
    let _world = fakes::World::new(&world, &control, Box::new(HarnessGate));
    // TODO(slice 5): build `Ports` over `_world` and run the steps with it.
    let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
    run(&args, &mut out, &mut err, &Host::system())
}

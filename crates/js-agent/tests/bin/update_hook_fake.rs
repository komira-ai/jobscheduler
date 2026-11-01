//! `js-update-hook-fake`: `js-agent update-hook` over the fake ports, for the conformance
//! harness (tests only; not shipped).
//!
//! `js-update-hook-fake [--world <dir>] [--control <dir>] update-hook <step> ...`. The two
//! directories are the fakes' world and control files (tests/update_hook_fakes); without a
//! flag the variables `UPDATE_HOOK_FAKE_WORLD` and `UPDATE_HOOK_FAKE_CONTROL` name them. Only
//! this test binary reads the environment; the agent reads none.
//!
//! The steps are still stubs here: it ends `not_implemented`, exit 1.

#![allow(dead_code)]

#[cfg(target_os = "linux")]
use std::ffi::OsString;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::process::ExitCode;

#[cfg(target_os = "linux")]
use js_agent::update_hook::{Done, Gate, Host, Invocation, Outcome, Params, Reason, Report, run};

#[cfg(target_os = "linux")]
#[path = "../update_hook_fakes/mod.rs"]
mod fakes;

/// A step with no body: exit 1, reason `not_implemented`. The machine's host runs the steps for
/// real; this binary stands in for it until the harness drives the steps over the fakes.
#[cfg(target_os = "linux")]
fn not_implemented(invocation: &Invocation, _params: &Params) -> Done {
    Done {
        outcome: Outcome::Failure,
        report: Report::failure(
            Reason::NotImplemented,
            format!("{} is not implemented", invocation.step),
        ),
    }
}

/// The kill points of the conformance harness.
// TODO(slice 9): map this to `update_hook::conformance::gate::point()` once the kbf
// `update-hook` crate is a dev-dependency; until then a point is not counted.
#[cfg(target_os = "linux")]
struct HarnessGate;

#[cfg(target_os = "linux")]
impl Gate for HarnessGate {
    fn point(&self) {}
}

/// The directory a flag or its variable names.
#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
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
    run(
        &args,
        &mut out,
        &mut err,
        &Host {
            step: &not_implemented,
        },
    )
}

/// The hook is Linux code (`js_agent::update_hook`); on any other target this test binary
/// only says so.
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("js-update-hook-fake: linux only");
    std::process::exit(2);
}

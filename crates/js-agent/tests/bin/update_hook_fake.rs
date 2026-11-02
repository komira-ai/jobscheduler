//! `js-update-hook-fake`: `js-agent update-hook` over the fake ports, the program the
//! conformance harness of the hook contract runs (tests only; not shipped).
//!
//! `js-update-hook-fake [--world <dir>] [--control <dir>] [--state-dir <dir>] update-hook <step>
//! ...`. The three directories are the fakes' world, the fakes' own files and the hook's state
//! directory (tests/update_hook_fakes); without a flag the variables of
//! `update_hook_fakes::harness` name them. Only this test binary reads the environment; the
//! agent reads none.
//!
//! The steps are the hook's own (`update_hook::step`), over the declaration and params of
//! `update_hook_fakes::harness`, the world's ports seen through the fault the test arranged for
//! the step, kill points in the harness's protocol, and the hang guard on the clock.

#![allow(dead_code)]

#[cfg(target_os = "linux")]
use std::ffi::OsString;
#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::process::ExitCode;
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
use js_agent::update_hook::step::{self, Env};
#[cfg(target_os = "linux")]
use js_agent::update_hook::{
    Clock, DirStat, Done, Host, Invocation, NoGate, Outcome, Params, Ports, Reason, Report, Step,
    run,
};

#[cfg(target_os = "linux")]
#[path = "../update_hook_fakes/mod.rs"]
mod fakes;

#[cfg(target_os = "linux")]
use fakes::World;
#[cfg(target_os = "linux")]
use fakes::harness::{
    CONTROL_VAR, DEFECT_VAR, Defect, Fault, Faulted, FaultedState, Frozen, Guard, HarnessGate,
    STATE_DIR_VAR, StagedAgent, WORLD_VAR, arranged, declaration,
};

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

/// The state directory as the hook requires it, whoever runs the test: the harness's sandbox
/// is the test user's, not `root:root`.
#[cfg(target_os = "linux")]
fn as_root(_: &Path, _: std::os::fd::BorrowedFd<'_>) -> std::io::Result<DirStat> {
    Ok(DirStat {
        is_symlink: false,
        is_dir: true,
        uid: 0,
        gid: 0,
        mode: 0o700,
    })
}

/// What the binary was started with.
#[cfg(target_os = "linux")]
struct Setup {
    world: PathBuf,
    control: PathBuf,
    state_dir: PathBuf,
    defect: Option<Defect>,
}

/// The hang guard fired: the step asked to wait past its bound. The updater would kill it;
/// so does this, with a signal, so the harness sees a step that died of one.
#[cfg(target_os = "linux")]
fn hung(asked: Duration) {
    eprintln!(
        "js-update-hook-fake: the step asked to wait {} s, past its declared timeout: stopped as the updater would stop it",
        asked.as_secs()
    );
    // SIGKILL, as the updater sends: no core file, nothing more runs.
    let _ = rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::KILL);
    std::process::exit(1);
}

/// Runs the step over the fakes.
#[cfg(target_os = "linux")]
fn fake_step(setup: &Setup, invocation: &Invocation, params: &Params) -> Done {
    let declaration = match declaration(&setup.state_dir) {
        Ok(declaration) => declaration,
        Err(why) => {
            return Done {
                outcome: Outcome::Failure,
                report: Report::failure(Reason::StateInvalid, why),
            };
        }
    };
    let world = World::new(
        &setup.world,
        &setup.control,
        Box::new(HarnessGate::from_env()),
    );
    world.set_releases(params);
    let quiet = World::new(&setup.world, &setup.control, Box::new(NoGate));
    let step_now = invocation.step;
    let fault = arranged(&setup.control, step_now);
    let hang = setup.defect == Some(Defect::Hang);
    let ports = Faulted {
        world: &world,
        quiet: &quiet,
        step: step_now,
        fault,
        silent_start: hang,
    };
    let root = world.state_root();
    let state = FaultedState {
        root: &root,
        tainted: fault == Some(Fault::Halt),
    };
    let staged = StagedAgent {
        version: if step_now == Step::Prepare && fault == Some(Fault::Fail) {
            format!("{}-other", params.target_version)
        } else {
            params.target_version.clone()
        },
    };
    let frozen = Frozen(&world);
    let clock: &dyn Clock = if hang { &frozen } else { &world };
    let guard = Guard::new(clock, Guard::budget_of(step_now), &hung);
    let env = Env {
        ports: Ports {
            units: &ports,
            link: &ports,
            lease: &ports,
            state: &state,
            clock: &guard,
            gate: &HarnessGate::from_env(),
        },
        runner: &staged,
        declaration: &declaration,
        stat: &as_root,
    };
    let done = match step_now {
        Step::Prepare => step::prepare(&env, params),
        Step::Cutover => step::cutover(&env, params),
        Step::Verify => step::verify(&env, params),
        Step::Rollback => step::rollback(&env, params),
        Step::Cleanup => step::cleanup(&env, params),
    };
    if step_now == Step::Cutover
        && setup.defect == Some(Defect::CutoverRestarts)
        && done.outcome == Outcome::Success
    {
        // Starts the agent again although it runs: a new session on every run.
        let _ = world.hello(&params.target_version);
    }
    done
}

#[cfg(target_os = "linux")]
fn main() -> ExitCode {
    let mut args: Vec<OsString> = std::env::args_os().collect();
    let world = dir_of("--world", WORLD_VAR, &mut args);
    let control = dir_of("--control", CONTROL_VAR, &mut args);
    let state_dir = dir_of("--state-dir", STATE_DIR_VAR, &mut args);
    let (Some(world), Some(control), Some(state_dir)) = (world, control, state_dir) else {
        eprintln!(
            "js-update-hook-fake: --world, --control and --state-dir (or their variables) are required"
        );
        return ExitCode::from(2);
    };
    let setup = Setup {
        world,
        control,
        state_dir,
        defect: Defect::from_word(std::env::var_os(DEFECT_VAR).as_ref()),
    };
    let body = |invocation: &Invocation, params: &Params| fake_step(&setup, invocation, params);
    let mut sink = std::io::sink();
    let mut stdout = std::io::stdout();
    let out: &mut dyn std::io::Write = if setup.defect == Some(Defect::NoLastLine) {
        &mut sink
    } else {
        &mut stdout
    };
    run(&args, out, &mut std::io::stderr(), &Host { step: &body })
}

/// The hook is Linux code here (the fakes are); on any other target this test binary only says
/// so.
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("js-update-hook-fake: linux only");
    std::process::exit(2);
}

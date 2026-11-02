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
//! the step, kill points in the harness's protocol, the hang guard on the clock, and a watchdog
//! thread that ends a step which stops making progress (`update_hook_fakes::watchdog`).

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
    Clock, DirStat, Done, Host, Invocation, Lease, NoGate, Outcome, Params, Ports, Reason, Report,
    Step, run,
};

#[cfg(target_os = "linux")]
#[path = "../update_hook_fakes/mod.rs"]
mod fakes;

#[cfg(target_os = "linux")]
use fakes::World;
#[cfg(target_os = "linux")]
use fakes::harness::{
    CONTROL_VAR, DEFECT_VAR, Defect, Fault, Faulted, FaultedState, Frozen, Guard, HarnessGate,
    MirroringGate, STATE_DIR_VAR, StagedAgent, WORLD_VAR, arranged, declaration, mirror_ownership,
};
#[cfg(target_os = "linux")]
use fakes::watchdog::{self, DEFAULT_WINDOW_MS, WINDOW_VAR};

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

/// The state directory cannot be looked at: the first-install `cleanup` fault (`Fault::Fail`).
#[cfg(target_os = "linux")]
fn unreadable(path: &Path, _: std::os::fd::BorrowedFd<'_>) -> std::io::Result<DirStat> {
    Err(std::io::Error::other(format!(
        "{}: the disk does not answer",
        path.display()
    )))
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
    watchdog::kill_step(&format!(
        "the step asked to wait {} s, past its declared timeout, as the updater would stop it",
        asked.as_secs()
    ));
}

/// The kill points, keeping the world's view of the record as the record says.
#[cfg(target_os = "linux")]
fn gate(setup: &Setup) -> MirroringGate {
    let mut inner = HarnessGate::from_env();
    inner.break_after_first = setup.defect == Some(Defect::BrokenCount);
    MirroringGate {
        inner,
        world: setup.world.clone(),
        state_dir: setup.state_dir.clone(),
    }
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
    let world = World::new(&setup.world, &setup.control, Box::new(gate(setup)));
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
        stall: setup.defect == Some(Defect::Stall),
        point_loop: setup.defect == Some(Defect::PointLoop),
        keeps_cordon: step_now == Step::Cleanup && setup.defect == Some(Defect::ForgetsCordon),
        control: &setup.control,
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
            gate: &gate(setup),
        },
        runner: &staged,
        declaration: &declaration,
        stat: if step_now == Step::Cleanup
            && fault == Some(Fault::Fail)
            && params.previous_version.is_none()
        {
            &unreadable
        } else {
            &as_root
        },
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
    if step_now == Step::Rollback && setup.defect == Some(Defect::LiftsAnyCordon) {
        let _ = world.uncordon();
    }
    // The step's last change to the record is after its last kill point.
    mirror_ownership(&setup.world, &setup.state_dir);
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
    // A hello a killed step left pending has reached the server by now.
    if let Err(why) = World::new(&setup.world, &setup.control, Box::new(NoGate)).deliver_hello() {
        eprintln!("js-update-hook-fake: the pending hello could not be delivered: {why}");
        return ExitCode::from(2);
    }
    let window = std::env::var_os(WINDOW_VAR);
    let on_stall = window.as_deref() == Some(std::ffi::OsStr::new(watchdog::ON_STALL));
    let span = window.and_then(|ms| ms.to_str()?.parse().ok()).map_or(
        Duration::from_millis(DEFAULT_WINDOW_MS),
        Duration::from_millis,
    );
    std::thread::spawn(move || {
        let mut wait = || {
            if on_stall {
                watchdog::wait_announced()
            } else {
                watchdog::wait_for(span)
            }
        };
        watchdog::watch(&watchdog::events, &mut wait, &stalled);
    });
    let body = |invocation: &Invocation, params: &Params| fake_step(&setup, invocation, params);
    let mut sink = std::io::sink();
    let mut stdout = std::io::stdout();
    let out: &mut dyn std::io::Write = if setup.defect == Some(Defect::NoLastLine) {
        &mut sink
    } else {
        &mut stdout
    };
    let code = run(&args, out, &mut std::io::stderr(), &Host { step: &body });
    watchdog::finish();
    code
}

/// The watchdog fired: no progress event in a whole window.
#[cfg(target_os = "linux")]
fn stalled() {
    watchdog::kill_step("the step made no progress in a watch window");
}

/// The hook is Linux code here (the fakes are); on any other target this test binary only says
/// so.
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("js-update-hook-fake: linux only");
    std::process::exit(2);
}

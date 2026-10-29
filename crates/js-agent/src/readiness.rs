//! The readiness hook (whole-node design, section 5, item 5): a command the operator
//! allowed, which the agent runs during `preparing` after its own checks passed, and whose
//! answer decides whether the agent reports `exclusive_ready`.
//!
//! The consumer owns what the hook measures (timer noise, CPU frequency, the traces of
//! other tenants); the agent knows only the hook's exit status and the first line it
//! printed.
//!
//! # The command
//!
//! `--readiness-hook <PATH>` names an absolute path to an executable file and
//! `--readiness-hook-arg <ARG>` (any number of times, in order) its arguments: an argv
//! list, never a shell line. The program runs with an environment of its own, a fixed
//! `PATH` and nothing the agent inherited, and with no standard input. No argument or
//! variable carries anything of the scheduler's (no lease id, no token, no session
//! secret). It is bounded by the `prepare_timeout` the agent counts from the grant
//! ([`hook_runner`]): at most that long for its output to end and at most that long again
//! for its exit, so twice `prepare_timeout` in the worst case. It leads a process group of
//! its own; when it overruns, the whole group is killed (SIGKILL) and the hook collected,
//! so a helper it started and left holding its output does not leak. A helper that makes
//! a session or process group of its own is out of reach.
//!
//! # The answer
//!
//! | outcome | reported |
//! |---|---|
//! | exit 0 | ready |
//! | exit [`EXIT_NOISE_OVER_BOUND`] (75) | `noise-over-bound` |
//! | exit [`EXIT_NOT_QUIET`] (76) | `not-quiet` |
//! | exit [`EXIT_SETUP_FAILED`] (77) | `setup-failed` |
//! | exit [`EXIT_UNSUPPORTED`] (78) | `unsupported` |
//! | any other exit status, or a signal | `setup-failed` |
//! | more output than [`PROBE_OUTPUT_CAP`] | `setup-failed` |
//! | not finished within the limit | `not-quiet` |
//! | could not be started | `unsupported` |
//!
//! The detail of a refusal is the first line of the hook's standard output, cut to
//! [`limits::MAX_REFUSAL_DETAIL`] bytes at a character boundary; a hook that printed no
//! line gets a fixed text. The agent reads nothing else from the output.
//!
//! That line goes to the scheduler verbatim (cut to the byte bound), so a hook must not
//! print a secret, a token or anything else that must not leave the host. The hook runs
//! only for a host the agent's own checks found quiet and clean ([`Hooked`]), once per
//! `exclusive` grant, and not for a lease taken over after a restart (the consumer is
//! already using that host).
//!
//! The job worker runs the hook on its own thread, so while it runs (up to twice
//! `prepare_timeout`) the worker applies no reply: starts, stops and acknowledgements wait
//! in the queue, a start may go stale there, and replies beyond the queue are dropped and
//! sent again. Heartbeats are not delayed.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use js_agent_wire::{RefusalReason, limits};

use crate::cli::Cli;
use crate::exclusive::Quiesce;
use crate::facts::{PROBE_OUTPUT_CAP, Ran, SystemCommands};
use crate::lifecycle::{Quiet, Restored};

/// The exit status of a hook that found the host's noise over the consumer's bound.
pub const EXIT_NOISE_OVER_BOUND: i32 = 75;
/// The exit status of a hook that found something still running on the host.
pub const EXIT_NOT_QUIET: i32 = 76;
/// The exit status of a hook whose own setup (a cpuset, a probe) failed.
pub const EXIT_SETUP_FAILED: i32 = 77;
/// The exit status of a hook that found the host cannot run the consumer's step at all.
pub const EXIT_UNSUPPORTED: i32 = 78;

/// The `PATH` of the hook's environment, the whole of it.
const HOOK_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// Starts the hook's program. [`SystemCommands`] does; a test answers from a script.
pub trait HookRunner {
    /// Runs `program` with `args`; its environment is exactly `env`.
    fn execute(&self, program: &str, args: &[&str], env: &[(&str, &str)]) -> Ran;
}

impl HookRunner for SystemCommands {
    fn execute(&self, program: &str, args: &[&str], env: &[(&str, &str)]) -> Ran {
        self.ran(program, args, Some(env))
    }
}

/// The runner a hook runs under: it waits at most `prepare_timeout` for the output and as
/// long again for the exit, and accepts [`PROBE_OUTPUT_CAP`] bytes of output. The hook
/// leads a process group of its own, and an overrun kills the group, so a helper it left
/// running with the output open does not outlive the call.
pub const fn hook_runner(prepare_timeout: Duration) -> SystemCommands {
    SystemCommands::grouped(prepare_timeout, PROBE_OUTPUT_CAP)
}

/// What the hook said about the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The hook found the host ready.
    Ready,
    /// The hook refused, or could not be run.
    Refused {
        /// The typed reason.
        code: RefusalReason,
        /// The first line it printed, or a fixed text; at most
        /// [`limits::MAX_REFUSAL_DETAIL`] bytes.
        detail: String,
    },
}

/// The checked hook command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadinessHook {
    program: String,
    args: Vec<String>,
}

impl ReadinessHook {
    /// Checks `program` (an absolute path to an executable file) and keeps `args`.
    ///
    /// # Errors
    ///
    /// The text names the flag at fault.
    pub fn new(program: &str, args: &[String]) -> Result<Self, String> {
        let executable = fs::metadata(program)
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0);
        match (Path::new(program).is_absolute(), executable) {
            (true, true) => Ok(Self {
                program: program.to_owned(),
                args: args.to_vec(),
            }),
            (false, _) => Err("--readiness-hook: not an absolute path".to_owned()),
            (true, false) => Err("--readiness-hook: not an executable file".to_owned()),
        }
    }

    /// The hook the flags name, none when `--readiness-hook` is not given.
    ///
    /// # Errors
    ///
    /// The text names the flag at fault; an argument without a program is one.
    pub fn from_cli(cli: &Cli) -> Result<Option<Self>, String> {
        match &cli.readiness_hook {
            Some(program) => Self::new(program, &cli.readiness_hook_args).map(Some),
            None if cli.readiness_hook_args.is_empty() => Ok(None),
            None => Err("--readiness-hook-arg: needs --readiness-hook".to_owned()),
        }
    }

    /// Runs the hook on `runner` and says what it found.
    pub fn run(&self, runner: &dyn HookRunner) -> Verdict {
        let args: Vec<&str> = self.args.iter().map(String::as_str).collect();
        match runner.execute(&self.program, &args, &[("PATH", HOOK_PATH)]) {
            Ran::Exited { code: Some(0), .. } => Verdict::Ready,
            Ran::Exited { code, text, stdout } => {
                let code = code.map_or(RefusalReason::SetupFailed, reason_of);
                let first = stdout.lines().next().unwrap_or("");
                refused(code, first, || format!("the readiness hook ended: {text}"))
            }
            Ran::TimedOut => refused(RefusalReason::NotQuiet, "", || {
                "the readiness hook did not finish before prepare_timeout".to_owned()
            }),
            Ran::Overflow => refused(RefusalReason::SetupFailed, "", || {
                format!("the readiness hook printed more than {PROBE_OUTPUT_CAP} bytes")
            }),
            Ran::Spawn(why) => refused(RefusalReason::Unsupported, "", || {
                format!("the readiness hook could not be started: {why}")
            }),
        }
    }
}

/// The refusal an exit status other than 0 maps to.
fn reason_of(code: i32) -> RefusalReason {
    match code {
        EXIT_NOISE_OVER_BOUND => RefusalReason::NoiseOverBound,
        EXIT_NOT_QUIET => RefusalReason::NotQuiet,
        EXIT_UNSUPPORTED => RefusalReason::Unsupported,
        _ => RefusalReason::SetupFailed,
    }
}

/// A refusal whose detail is `line` cut to the wire's bound, or `otherwise()` when the
/// line is empty.
fn refused(code: RefusalReason, line: &str, otherwise: impl FnOnce() -> String) -> Verdict {
    let text = if line.is_empty() {
        otherwise()
    } else {
        line.to_owned()
    };
    Verdict::Refused {
        code,
        detail: cut(&text).to_owned(),
    }
}

/// `text` cut to [`limits::MAX_REFUSAL_DETAIL`] bytes at a character boundary.
fn cut(text: &str) -> &str {
    let end = (0..=limits::MAX_REFUSAL_DETAIL.min(text.len()))
        .rev()
        .find(|&at| text.is_char_boundary(at))
        .unwrap_or(0);
    &text[..end]
}

/// A host that is quiet when its own answer is and the hook agrees: the lifecycle (or any
/// [`Quiesce`]) is asked first, and the hook runs only when it found the host quiet and
/// clean. It blocks as long as the hook runs, so, like the lifecycle's checks, it is asked
/// through the worker and not from the heartbeat loop.
pub struct Hooked<'a> {
    host: &'a mut dyn Quiesce,
    hook: &'a ReadinessHook,
    runner: &'a dyn HookRunner,
}

impl<'a> Hooked<'a> {
    /// `host` with `hook`, started by `runner`.
    pub fn new(
        host: &'a mut dyn Quiesce,
        hook: &'a ReadinessHook,
        runner: &'a dyn HookRunner,
    ) -> Self {
        Self { host, hook, runner }
    }
}

impl Quiesce for Hooked<'_> {
    fn prepare(&mut self, lease: &str) -> Quiet {
        match self.host.prepare(lease) {
            Quiet::Ready => vetted(self.hook.run(self.runner)),
            other => other,
        }
    }

    fn restore(&mut self, lease: &str) -> Restored {
        self.host.restore(lease)
    }
}

/// What the session is told of the hook's verdict.
fn vetted(verdict: Verdict) -> Quiet {
    match verdict {
        Verdict::Ready => Quiet::Ready,
        Verdict::Refused { code, detail } => Quiet::Refused { code, detail },
    }
}

/// The hook as the job worker owns it: the checked command, the runner that starts it and
/// the longest a run of it can take, which the worker's watchdog is told of so a hook that
/// takes its full time is not taken for a stuck worker. The worker runs it, on its own
/// thread, once for each lease its lifecycle found quiet.
pub struct WorkerHook {
    hook: ReadinessHook,
    runner: Box<dyn HookRunner + Send>,
    bound: Duration,
}

impl WorkerHook {
    /// `hook` started by `runner`, which takes at most `bound` ([`hook_runner`] takes twice
    /// its `prepare_timeout`).
    pub fn new(hook: ReadinessHook, runner: Box<dyn HookRunner + Send>, bound: Duration) -> Self {
        Self {
            hook,
            runner,
            bound,
        }
    }

    /// The longest a run of the hook can take.
    pub fn bound(&self) -> Duration {
        self.bound
    }

    /// `found`, the lifecycle's answer: when it is [`Quiet::Ready`] the hook runs and its
    /// verdict is the answer; any other answer is returned as it is and the hook does not
    /// run. Blocks as long as the hook runs, up to [`WorkerHook::bound`]; `before_run` is
    /// told that bound just before the hook starts (the worker extends its watchdog by it).
    pub fn vet(&self, found: Quiet, before_run: &dyn Fn(Duration)) -> Quiet {
        match found {
            Quiet::Ready => {
                before_run(self.bound);
                vetted(self.hook.run(self.runner.as_ref()))
            }
            other => other,
        }
    }
}

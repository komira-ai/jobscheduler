//! Starting a program: the one way the helper and the agent do it, and the one place that
//! decides, on macOS, which process a program they start answers to for the privacy controls.
//!
//! macOS's privacy controls (TCC: Full Disk Access, the camera, another app's files)
//! judge a process by its *responsible* process: the process whose grants count for it. A
//! process started the usual way (`fork` and `exec`, or `posix_spawn` with no more) answers
//! for the process its starter answers for, so it holds whatever that process was granted.
//! It answers for itself only when its starter disclaims the responsibility as it starts it
//! (`responsibility_spawnattrs_setdisclaim` on the `posix_spawn` attributes), or when
//! launchd starts it as a job of its own; it is then judged by its own code signature. The
//! helper and the agent are launchd daemons and answer for themselves. The agent holds Full
//! Disk Access (`packaging/js-agent-macos/mdm`).
//!
//! **The default: disclaimed.** A [`Program`] starts its program with the responsibility
//! disclaimed, so the program holds none of its starter's grants: the host probes, the
//! firewall and `launchctl` tools, the VM launcher (whether it should keep the
//! responsibility is not decided) and every other program the agent or the helper starts.
//! A start that does not choose is this one: forgetting to choose can cost a
//! program a read it needed, never give it a grant it should not have.
//!
//! **The one exception: a job and its supervisor.** [`Program::keep_responsibility`] leaves
//! the disclaim out for one start, and its argument, [`Kept`], says which of the two starts
//! allowed to do so this is: the agent's start of the supervisor of a native job
//! ([`Kept::Supervisor`]), or the supervisor's start of the job's program ([`Kept::Job`]).
//! With both kept, every process between the agent and the job is started without the
//! disclaim, so the job answers for the agent and holds what the agent holds, Full Disk
//! Access included. Its callers are the two starts of `js-agent`'s native job supervisor
//! (`mac_supervisor`), which the agent's native runtime starts for every native job; the
//! helper's job start (`crate::spawn`) disclaims, and the helper holds no grant to pass on. A
//! job started disclaimed is judged by its own signature and is refused every read of the
//! network file system mount its build trees are served from, and so are Apple's own tools
//! it runs. What keeps a job from using the rest of the grant is the user it runs as (Unix
//! permissions) and the Seatbelt profile its supervisor applies before the program runs. The
//! job's own children answer for the agent too, as every process's children follow their
//! parent. `cargo test -p xtask --test spawn_sites` fails when production code calls
//! [`Program::keep_responsibility`] or names [`Kept`] outside the files listed in
//! `crates/xtask/src/spawn_sites.rs` (`KEEPERS`). Off macOS there is no responsibility, and
//! the call changes nothing.
//!
//! [`Program`] is the standard library's [`Command`], with its program and arguments fixed
//! when it is made. Off macOS there is nothing to disclaim, and it is the standard
//! library's [`Command`] unchanged. On macOS, when it is started, it makes in the parent
//! everything its program is executed with (`plan`):
//!
//! - the argument vector: the program, then each argument;
//! - the environment: this process's, or none after [`Program::env_clear`], with the
//!   variables set or removed since, applied in order;
//! - the files to try: the program itself when it holds a `/`; otherwise the program in
//!   each directory of the environment's `PATH` (`/usr/bin:/bin` when it has none), in
//!   order, an empty directory being the working directory;
//! - the `posix_spawn` attributes: `POSIX_SPAWN_SETEXEC` and the disclaim.
//!
//! A program whose responsibility is kept skips all of this: the child does the standard
//! library's set-up and the steps, and the standard library executes the program, as it
//! does off macOS.
//!
//! The child that the standard library forks does its usual set-up (standard streams,
//! group and user ids, working directory, process group), then the steps given to
//! [`Program::with_steps`]. Then, where the standard library would call `execvp`, it tries
//! the files with `posix_spawn` and those attributes, which executes a file in the same
//! process with the disclaim set. It decides as `execvp` does, in the directory it is now
//! in: a file that is missing or may not be executed passes to the next, a file the kernel
//! does not recognise is run by `/bin/sh`, and the answer when none runs is `EACCES` if
//! one was refused so, `ENOENT` otherwise. The program is then its own responsible process:
//! TCC judges it by its own code signature. Its children answer for it, as any process's
//! do. [`exec`] does the same for a process that executes a program in its own place
//! (`js-agent`'s run of itself again over its own process), in the environment that process
//! has.
//!
//! Everything the child uses is made before the fork, because between `fork` and `exec`
//! the child may not allocate. That is why the program and its arguments are fixed when
//! the [`Program`] is made, and why a [`Program`] is used once: a second
//! [`Program::spawn`] or [`Program::output`] is refused, on every platform, whether or
//! not the first one started the program.
//!
//! This module is the only code of `js-agent` and `js-mac-session` that starts a program.
//! `cargo test -p xtask --test spawn_sites` fails when their production code, on any
//! platform, does it another way: a `Command::new`, a `pre_exec` or `exec` call, or a C
//! library call that executes a program (`crates/xtask/src/spawn_sites.rs`).
//!
//! `responsibility_spawnattrs_setdisclaim` and `responsibility_get_pid_responsible_for_pid`
//! are not in Apple's public headers; libSystem exports both, and the binaries link
//! against them directly, so a macOS without them would refuse to load the binary rather
//! than start a program it was asked to disclaim without the disclaim. The macOS CI job
//! links and calls both. On macOS, [`responsible_pid`] reads a process's responsible
//! process back, and the agent's tests use it to check the processes they start.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};

use std::os::unix::process::CommandExt as _;

/// Steps the child runs before the program executes.
type Steps = Box<dyn FnMut() -> io::Result<()> + Send + Sync>;

/// Which of the two starts that keep the responsibility a start is (see the module
/// documentation): every other start disclaims it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kept {
    /// The start of a native job's program, by its supervisor.
    Job,
    /// The agent's start of the supervisor of a native job, which starts the job.
    Supervisor,
}

/// A program to start, with its arguments fixed (see the module documentation).
pub struct Program {
    command: Command,
    /// The steps the child runs before the program, until the program is started.
    steps: Option<Steps>,
    /// Whether the program has been started (it is started once).
    started: bool,
    /// Why the program keeps its starter's responsibility, when it does
    /// ([`Self::keep_responsibility`]); it is disclaimed otherwise.
    kept: Option<Kept>,
    /// The program, for the file and the vector `posix_spawn` is given.
    #[cfg(target_os = "macos")]
    program: OsString,
    /// Its arguments.
    #[cfg(target_os = "macos")]
    args: Vec<OsString>,
    /// Whether [`Self::env_clear`] was called.
    #[cfg(target_os = "macos")]
    cleared: bool,
}

impl fmt::Debug for Program {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Program")
            .field("command", &self.command)
            .field("started", &self.started)
            .field("kept", &self.kept)
            .finish_non_exhaustive()
    }
}

impl Program {
    /// `program` with `args`.
    pub fn new<I, S>(program: impl AsRef<OsStr>, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<OsString> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        Self::make(program.as_ref(), args, None)
    }

    /// [`Self::new`], with `steps` run in the child after the standard library's own
    /// set-up and before the program executes. A step that fails ends the start with its
    /// error, and the program is not executed.
    ///
    /// # Safety
    /// `steps` runs in the child between `fork` and `exec`, in a copy of a process that
    /// may have other threads: it must call only async-signal-safe functions and must not
    /// allocate.
    pub unsafe fn with_steps<I, S>(
        program: impl AsRef<OsStr>,
        args: I,
        steps: impl FnMut() -> io::Result<()> + Send + Sync + 'static,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<OsString> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        Self::make(program.as_ref(), args, Some(Box::new(steps)))
    }

    /// The command, and what [`Self::arm`] needs to finish it.
    fn make(program: &OsStr, args: Vec<OsString>, steps: Option<Steps>) -> Self {
        let mut command = Command::new(program);
        command.args(&args);
        #[cfg(target_os = "macos")]
        {
            Self {
                command,
                steps,
                started: false,
                kept: None,
                program: program.to_owned(),
                args,
                cleared: false,
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            Self {
                command,
                steps,
                started: false,
                kept: None,
            }
        }
    }

    /// Gives the command its one closure, once: the steps and, on macOS unless the
    /// responsibility is kept, the disclaimed execution after them, with everything it uses
    /// made here, before the fork.
    ///
    /// # Errors
    /// The program was started already (an attempt that failed counts); on macOS also: the
    /// program, an argument or the environment holds NUL, or the attributes could not be
    /// made.
    fn arm(&mut self) -> io::Result<()> {
        if self.started {
            return Err(io::Error::other("a Program starts its program once"));
        }
        self.started = true;
        let steps = self.steps.take();
        #[cfg(target_os = "macos")]
        if self.kept.is_none() {
            let env = darwin::environment(self.cleared, &self.command);
            let plan = plan::Plan::new(&self.program, &self.args, &env)?;
            let attributes = darwin::Attributes::new()?;
            darwin::disclaim(&mut self.command, plan, attributes, steps);
            return Ok(());
        }
        if let Some(steps) = steps {
            // SAFETY: `with_steps`'s caller promises that `steps` is async-signal-safe and
            // allocates nothing; it is the only closure the command has.
            unsafe {
                self.command.pre_exec(steps);
            }
        }
        Ok(())
    }

    /// Starts the program without disclaiming its responsibility: on macOS it then answers to
    /// the privacy controls for the process its starter answers for, and holds what that
    /// process was granted. `why` says which of the two starts allowed to do this it is: a
    /// native job's supervisor, or the job's program (see the module documentation, which
    /// also names the check that holds every call to the files listed for it). Off macOS
    /// nothing changes.
    pub fn keep_responsibility(&mut self, why: Kept) -> &mut Self {
        self.kept = Some(why);
        self
    }

    /// Clears the environment the program gets ([`Command::env_clear`]).
    pub fn env_clear(&mut self) -> &mut Self {
        self.command.env_clear();
        #[cfg(target_os = "macos")]
        {
            self.cleared = true;
        }
        self
    }

    /// Sets `key` to `value` in the program's environment ([`Command::env`]).
    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.command.env(key, value);
        self
    }

    /// Sets each pair of `vars` in the program's environment ([`Command::envs`]).
    pub fn envs<I, K, V>(&mut self, vars: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.command.envs(vars);
        self
    }

    /// Sets the program's working directory ([`Command::current_dir`]).
    pub fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.command.current_dir(dir);
        self
    }

    /// Sets the user id the program runs as (`CommandExt::uid`). When the caller is root,
    /// std then calls `setgroups(0)`, which macOS turns into group 0 (`wheel`): nothing that
    /// starts a program as a lease user uses it; they drop with the steps of `crate::spawn`.
    pub fn uid(&mut self, id: u32) -> &mut Self {
        std::os::unix::process::CommandExt::uid(&mut self.command, id);
        self
    }

    /// Sets the group id the program runs as (`CommandExt::gid`).
    pub fn gid(&mut self, id: u32) -> &mut Self {
        std::os::unix::process::CommandExt::gid(&mut self.command, id);
        self
    }

    /// Puts the program in the process group `group`; 0 makes it the leader of a new one
    /// (`CommandExt::process_group`).
    pub fn process_group(&mut self, group: i32) -> &mut Self {
        std::os::unix::process::CommandExt::process_group(&mut self.command, group);
        self
    }

    /// Sets the program's standard input ([`Command::stdin`]).
    pub fn stdin(&mut self, stream: impl Into<Stdio>) -> &mut Self {
        self.command.stdin(stream);
        self
    }

    /// Sets the program's standard output ([`Command::stdout`]).
    pub fn stdout(&mut self, stream: impl Into<Stdio>) -> &mut Self {
        self.command.stdout(stream);
        self
    }

    /// Sets the program's standard error ([`Command::stderr`]).
    pub fn stderr(&mut self, stream: impl Into<Stdio>) -> &mut Self {
        self.command.stderr(stream);
        self
    }

    /// Starts the program ([`Command::spawn`]).
    ///
    /// # Errors
    /// It could not be started: the program is not found or may not be executed, a step or
    /// the set-up failed, the program, an argument or the environment holds NUL, or it was
    /// started already (a failed attempt counts: a [`Program`] is used once); on macOS also
    /// the attributes that disclaim the responsibility could not be made (a program whose
    /// responsibility is kept makes none).
    pub fn spawn(&mut self) -> io::Result<Child> {
        self.arm()?;
        self.command.spawn()
    }

    /// Starts the program, waits for it and collects what it printed
    /// ([`Command::output`]).
    ///
    /// # Errors
    /// As [`Self::spawn`], or it could not be waited for.
    pub fn output(&mut self) -> io::Result<Output> {
        self.arm()?;
        self.command.output()
    }
}

/// Executes `program` with `args` in this process's place, in its environment, as
/// `execvp` does, with its responsibility disclaimed; returns only when it could not.
#[cfg(target_os = "macos")]
#[must_use]
pub fn exec(program: &OsStr, args: &[OsString]) -> io::Error {
    let env: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    match plan::Plan::new(program, args, &env).and_then(|plan| {
        let attributes = darwin::Attributes::new()?;
        Ok((plan, attributes))
    }) {
        Ok((plan, attributes)) => darwin::exec(&plan, &attributes),
        Err(why) => why,
    }
}

/// Executes `program` with `args` in this process's place, in its environment, as `execvp`
/// does: off macOS there is no responsibility to disclaim. Returns only when it could not.
#[cfg(not(target_os = "macos"))]
#[must_use]
pub fn exec(program: &OsStr, args: &[OsString]) -> io::Error {
    Command::new(program).args(args).exec()
}

/// The pid of the process that `pid` answers to for the privacy controls: its own pid
/// when its starter disclaimed the responsibility, or launchd started it.
///
/// # Errors
/// The system could not say (there is no such process).
#[cfg(target_os = "macos")]
pub fn responsible_pid(pid: u32) -> io::Result<u32> {
    darwin::responsible_pid(pid)
}

#[cfg(target_os = "macos")]
mod darwin;
#[cfg(any(target_os = "macos", test))]
mod plan;
#[cfg(test)]
mod tests;

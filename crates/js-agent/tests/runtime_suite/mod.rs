//! The behaviour every container runtime shows to the lifecycle, written once and run
//! against each runtime: the in-memory `FakeRuntime` (the `agent` test binary, in every
//! test run) and `CrunRuntime` over a real `crun` (the `crun_runtime` test binary, in the
//! real-`crun` jobs).
//!
//! A case is a function over a [`Subject`]: the runtime under test, plus what the case
//! cannot do through the [`Runtime`] trait alone (a program for the container to run, a
//! wait for it to be up or to have ended, the files a lifecycle needs). The fake
//! interprets a [`Program`] by scripting its container; the real runtime runs it as
//! busybox `sh` words. Each case states what it proves and which defect it would catch.
//!
//! Nothing here waits for a clock. A case that needs a job to be up before it signals it
//! waits on a named pipe the job opens (an `open` of a pipe blocks until the other end is
//! opened); a case that needs a job to have ended waits for the runtime to say so
//! ([`Subject::settled`]). "SIGTERM, then the kill when the grace is spent" is shown by the
//! program's own exit code: a program that handles SIGTERM exits with its code when
//! stopped with a grace no test will wait out, and a program that ignores it is stopped
//! with a grace of zero. The lifecycle's wall-time limit is read from a clock a case moves.
//!
//! The cases are plain functions over a trait object, so each is one instantiation, and
//! they hold no branches of their own.

mod cases;
mod life;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use js_agent::lifecycle::{Config, Exit, Found, RunHandle, RunSpec, Runtime, Status};

pub use cases::*;
pub use life::*;

/// A runtime that a case and the lifecycle under test both hold.
pub type Shared = Rc<RefCell<dyn Runtime>>;

/// A grace no case waits out: a stop with it ends a program that handles SIGTERM at once.
pub const LONG: Duration = Duration::from_secs(3600);

/// What a container runs. The real runtime runs the words of [`Program::args`]; the fake is
/// told what the program does ([`Subject::after`]).
#[derive(Clone, Copy, Debug)]
pub enum Program {
    /// Exits at once with the code.
    Exit(i32),
    /// Runs until it is stopped; SIGTERM ends it (the signal's default action).
    Hold,
    /// Runs until it is stopped; on SIGTERM it exits with the code.
    OnTerm(i32),
    /// Runs until it is killed; it ignores SIGTERM.
    IgnoreTerm,
    /// Uses memory until the kernel kills it. `own_limit` says whose limit it reaches:
    /// the lease's own booking, or one above the lease (a node short of memory).
    Hog {
        /// Whether the kill is at the booking of the run.
        own_limit: bool,
    },
    /// Writes [`OUT_LINE`] to standard output and [`ERR_LINE`] to standard error, then
    /// exits 0 (`hold` false), or, with `hold`, is up and runs until it is killed (it
    /// ignores SIGTERM).
    Say {
        /// Whether the program stays up after it wrote.
        hold: bool,
    },
    /// Writes far more to standard output than any tail holds, the last line being
    /// [`FLOOD_END`], then exits 0.
    Flood,
}

/// What [`Program::Say`] writes to standard output.
pub const OUT_LINE: &str = "to-stdout\n";
/// What [`Program::Say`] writes to standard error.
pub const ERR_LINE: &str = "to-stderr\n";
/// The last line of [`Program::Flood`].
pub const FLOOD_END: &str = "flood-end\n";

impl Program {
    /// The words after the image. `ready` is the path of the pipe a program opens when it
    /// is up (one that does not need to be up never opens it).
    pub fn args(self, ready: &str) -> Vec<String> {
        let shell = |script: String| vec!["/bin/sh".to_owned(), "-c".to_owned(), script];
        match self {
            Self::Exit(code) => shell(format!("exit {code}")),
            Self::Hold => shell(format!("echo up > {ready}; exec sleep 100000")),
            Self::OnTerm(code) => shell(format!(
                "trap 'exit {code}' TERM; echo up > {ready}; sleep 100000 & wait"
            )),
            Self::IgnoreTerm => shell(format!(
                "trap '' TERM; echo up > {ready}; exec sleep 100000"
            )),
            Self::Say { hold: false } => {
                shell("echo to-stdout; echo to-stderr >&2; exit 0".to_owned())
            }
            Self::Say { hold: true } => shell(format!(
                "trap '' TERM; echo to-stdout; echo to-stderr >&2; echo up > {ready}; \
                 exec sleep 100000"
            )),
            Self::Flood => shell("yes 0123456789 | head -n 20000; echo flood-end".to_owned()),
            // One line that never ends, held in memory by `tail`.
            Self::Hog { .. } => vec!["/bin/tail".to_owned(), "/dev/zero".to_owned()],
        }
    }

    /// Whether the program opens its pipe when it is up (read by the real runtime's
    /// subject; the fake needs no wait).
    #[allow(dead_code)]
    pub fn opens_ready(self) -> bool {
        !matches!(
            self,
            Self::Exit(_) | Self::Hog { .. } | Self::Say { hold: false } | Self::Flood
        )
    }

    /// The memory the run books: a hog's booking is small, so that it reaches it, and a
    /// hog of a busy node books more than the node leaves it.
    pub fn memory_bytes(self) -> u64 {
        match self {
            Self::Hog { own_limit: true } => 192 << 20,
            Self::Hog { own_limit: false } => 1 << 30,
            _ => 256 << 20,
        }
    }
}

/// The runtime under test and what its cases need beyond the trait.
pub trait Subject {
    /// The runtime. Every call returns a handle to the same one.
    fn runtime(&self) -> Shared;

    /// The pinned image every run names.
    fn image(&self) -> String;

    /// Gets the image onto the host: returns once [`Runtime::prepare`] says `Ready`.
    fn prepared(&mut self);

    /// A new directory for the run `run_id`'s scratch, as the runtime wants it.
    fn scratch(&mut self, run_id: &str) -> PathBuf;

    /// The directory the run `run_id` opens its ready pipe in, made (with the pipe) before
    /// the run starts. It is mounted at `/mnt/sync/<run_id>`.
    fn sync(&mut self, run_id: &str) -> PathBuf;

    /// Called before a run of `program` is created: arranges what the program needs from
    /// the host.
    fn before(&mut self, program: Program);

    /// Called once the run `run_id` of `program` has been created and started (by the
    /// runtime or by a lifecycle): waits until the program is up if it says it is, and
    /// scripts the fake's container.
    fn after(&mut self, run_id: &str, program: Program);

    /// Waits until the container `id` has exited.
    fn settled(&mut self, id: &RunHandle);

    /// Creates the container of `spec` and does not start it.
    fn unstarted(&mut self, spec: &RunSpec) -> RunHandle;

    /// Makes `spec` one the runtime cannot start.
    fn spoil(&mut self, spec: &mut RunSpec);

    /// The lifecycle's configuration over this subject's directories: a scratch root in
    /// the layout the runtime wants, a mount root `sync` and a state root.
    fn config(&mut self) -> Config;
}

/// A case's helper: the status of `id`.
pub fn poll(s: &dyn Subject, id: &RunHandle) -> Status {
    let status = s.runtime().borrow_mut().poll(id);
    status.expect("poll")
}

/// A case's helper: `status`, which must be an exit.
pub fn exited(status: Status) -> Exit {
    let Status::Exited(exit) = status else {
        panic!("expected an exited container, found {status:?}");
    };
    exit
}

/// A case's helper: every container the runtime lists.
pub fn listed(s: &dyn Subject) -> Vec<Found> {
    let found = s.runtime().borrow_mut().list_labelled();
    found.expect("list")
}

/// A case's helper: stops `id` with `grace`.
pub fn stop(s: &dyn Subject, id: &RunHandle, grace: Duration) {
    let stopped = s.runtime().borrow_mut().stop(id, grace);
    stopped.expect("stop");
}

/// A case's helper: removes `id`.
pub fn remove(s: &dyn Subject, id: &RunHandle) {
    let removed = s.runtime().borrow_mut().remove(id);
    removed.expect("remove");
}

/// Stops and removes every container the runtime lists: the end of every case, so that
/// nothing a case left runs on.
pub fn clean_up(s: &dyn Subject) {
    for found in listed(s) {
        stop(s, &found.id, Duration::ZERO);
        remove(s, &found.id);
    }
    assert!(listed(s).is_empty(), "the runtime lists what was removed");
}

/// The run `run_id` of `program`, as a start asks the runtime for it.
pub fn spec_of(s: &mut dyn Subject, run_id: &str, program: Program) -> RunSpec {
    let sync = s.sync(run_id);
    let ready = format!("/mnt/sync/{run_id}/ready");
    RunSpec {
        run_id: run_id.to_owned(),
        body: js_agent::lifecycle::Body::Container {
            image: s.image(),
            args: program.args(&ready),
        },
        cpu_millis: 1000,
        memory_bytes: program.memory_bytes(),
        pids: Some(256),
        gpu_indices: Vec::new(),
        mounts: vec![js_agent::lifecycle::ResolvedMount {
            host: sync,
            target: format!("/mnt/sync/{run_id}"),
            ro: false,
        }],
        scratch: s.scratch(run_id),
        files: None,
        slot: 0,
        isolation: js_agent_wire::Isolation::Contained,
        labels: std::collections::BTreeMap::from([(
            js_agent::lifecycle::label::RUN.to_owned(),
            run_id.to_owned(),
        )]),
    }
}

/// Creates and starts the run `run_id` of `program` and returns once it is up.
pub fn begin(s: &mut dyn Subject, run_id: &str, program: Program) -> RunHandle {
    s.prepared();
    s.before(program);
    let spec = spec_of(s, run_id, program);
    let started = s.runtime().borrow_mut().create_and_start(&spec);
    let id = started.expect("create and start");
    s.after(run_id, program);
    id
}

/// Defines, in the module that calls it, one `#[test]` for each case of the suite, over
/// the subject `new(name)` builds. `ignored` is `all()` to mark them `#[ignore]` (the tests
/// of a real `crun`, run by `cargo xtask crun-tests`) or `any()` to run them everywhere.
macro_rules! conformance {
    ($new:expr, $ignored:meta) => {
        conformance! { @each $new, $ignored;
            a_container_goes_through_create_start_poll_stop_remove,
            an_exit_code_is_reported,
            a_kill_at_the_runs_own_memory_limit_is_flagged,
            a_kill_by_a_node_short_of_memory_is_not_flagged,
            sigterm_reaches_a_stopped_job_before_any_kill,
            a_job_that_ignores_sigterm_is_killed_when_the_grace_is_spent,
            a_container_never_started_is_listed_as_such,
            a_start_that_fails_leaves_no_container,
            a_run_id_in_use_is_refused,
            a_running_container_names_its_cgroup,
            the_tail_of_a_run_that_ends_normally_reaches_the_result,
            the_tail_of_a_run_that_is_killed_reaches_the_result,
            a_run_never_started_has_an_empty_tail,
            a_tail_is_bounded_and_says_it_dropped_bytes,
            a_runtime_without_gpus_says_so_and_refuses_one,
            the_wall_time_ends_a_job_with_sigterm,
            the_wall_time_kills_a_job_that_ignores_sigterm,
            egress_is_checked_before_every_create,
            a_gpu_request_is_refused_and_nothing_is_created,
        }
    };
    (@each $new:expr, $ignored:meta; $($case:ident,)*) => {
        $(
            #[test]
            #[cfg_attr($ignored, ignore = "real crun: run through cargo xtask crun-tests")]
            fn $case() {
                let mut subject = $new(stringify!($case));
                $crate::runtime_suite::$case(&mut subject);
            }
        )*
    };
}

pub(crate) use conformance;

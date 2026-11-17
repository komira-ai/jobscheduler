//! A job's declared outputs (`docs/design/job-outputs.md`): the files a job start names in
//! `outputs`, put in the object store under the job's prefix when the job has ended, with the
//! manifest last.
//!
//! A runtime that uploads live logs keeps a [`Plan`] for a run whose start declared outputs
//! and carried a sink ([`Plan::asked`], at staging; the sink is the live logs', and so is the
//! host's credential the upload signs with). When the run is removed, after its process has
//! ended and before what it wrote is removed, the runtime calls [`Plan::start`] with the
//! job's working directory on the host: the outputs are gathered there and then
//! ([`gather`]: each opened, following no link), and a thread of the job's own puts them
//! ([`upload`]), reading the open files, so the removal goes on at once and does not cut the
//! upload short. Nothing waits for the thread; it ends with the manifest put, or when the store
//! refuses the manifest, or after [`OUTPUTS_BUDGET`].
//!
//! An agent that restarts while it uploads does not take the upload up again: the outputs of
//! that job then have no manifest.

mod gather;
mod upload;

#[cfg(test)]
mod gather_tests;
#[cfg(test)]
mod upload_tests;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::Arc;
use std::thread::JoinHandle;

pub use upload::{Limits, OUTPUTS_BUDGET};

use self::gather::Caps;
use self::upload::{Next, Upload};
use crate::lifecycle::RunSpec;
use crate::logs::{LiveLogs, Sink};

/// The most bytes all of a job's output files may hold together (1 GiB).
pub const MAX_OUTPUT_BYTES: u64 = 1 << 30;

/// The most output files of a job: each is held open from the gathering until it is put, so
/// this bounds the descriptors a job's upload holds, and with the paths' length the manifest's.
pub const MAX_OUTPUT_FILES: usize = 256;

/// The most entries (files, directories and anything else) a gathering looks at, all
/// declared directories together; a directory with more names than are left is refused whole.
pub const MAX_OUTPUT_ENTRIES: usize = 4096;

/// The most failures the manifest lists; past them one more, `over-limit`, says the list was
/// cut. With [`MAX_OUTPUT_FILES`] and the declaration's own bound, the manifest stays far under
/// the 1 MiB a reader takes.
pub const MAX_OUTPUT_FAILURES: usize = 256;

/// The most levels of directories below a declared directory.
pub const MAX_OUTPUT_DEPTH: usize = 16;

/// What a run whose start declared outputs keeps until it is removed: the paths, where they
/// go, and how.
#[derive(Debug, Clone)]
pub struct Plan {
    declared: Vec<String>,
    sink: Sink,
    logs: LiveLogs,
    limits: Limits,
}

impl Plan {
    /// The plan of a run whose start declared `declared` and carried `sink`, on a runtime with
    /// `logs`: `None` when it declared none, and, with a warning, when the runtime uploads no
    /// live logs or the start carried no sink (the server sends outputs only with both).
    #[must_use]
    pub fn asked(
        logs: Option<&LiveLogs>,
        sink: Option<&Sink>,
        declared: &[String],
    ) -> Option<Self> {
        if declared.is_empty() {
            return None;
        }
        let Some((logs, sink)) = logs.zip(sink) else {
            tracing::warn!(
                outputs = declared.len(),
                "job outputs: the start carried no sink, or this agent uploads no live logs: \
                 the outputs are not uploaded"
            );
            return None;
        };
        Some(Self {
            declared: declared.to_vec(),
            sink: sink.clone(),
            logs: logs.clone(),
            limits: Limits::default(),
        })
    }

    /// [`Plan::asked`] of the run `spec` (its sink and its outputs), on a runtime with `logs`.
    #[must_use]
    pub fn of(logs: Option<&LiveLogs>, spec: &RunSpec) -> Option<Self> {
        Self::asked(logs, spec.log_sink.as_ref(), &spec.outputs)
    }

    /// The plan with `limits` (a test's small ones).
    #[must_use]
    pub fn with_limits(self, limits: Limits) -> Self {
        Self { limits, ..self }
    }

    /// Gathers the outputs below `root`, the job's working directory on the host, now, and
    /// starts the thread that puts them; returns at once.
    pub fn start(self, root: &Path) -> Uploading {
        let caps = Caps {
            file_bytes: self.limits.max_put,
            total_bytes: MAX_OUTPUT_BYTES,
            files: MAX_OUTPUT_FILES,
            entries: MAX_OUTPUT_ENTRIES,
            failures: MAX_OUTPUT_FAILURES,
            depth: MAX_OUTPUT_DEPTH,
        };
        let gathered = gather::gather(root, &self.declared, caps);
        let logs = &self.logs;
        let mut upload = Upload::new(
            (self.sink.prefix().clone(), self.sink.endpoint().clone()),
            gathered,
            (
                Arc::clone(&logs.transport),
                Arc::clone(&logs.clock),
                Arc::clone(&logs.grant),
            ),
            self.limits,
        );
        let wake = (logs.wakes)();
        let prefix = self.sink.prefix().as_str().to_owned();
        // The thread logs where its starter does.
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        let thread = std::thread::spawn(move || {
            tracing::dispatcher::with_default(&dispatch, || {
                let ran = catch_unwind(AssertUnwindSafe(|| {
                    while let Next::Sleep(until) = upload.step() {
                        wake.sleep_until(until);
                    }
                }));
                if ran.is_err() {
                    tracing::warn!(prefix, "job outputs: the upload stopped on a panic");
                }
                wake.ended();
            });
        });
        Uploading {
            thread: Some(thread),
        }
    }
}

/// The thread of one job's outputs upload. Dropping it leaves the thread to end by itself.
#[derive(Debug)]
pub struct Uploading {
    thread: Option<JoinHandle<()>>,
}

impl Uploading {
    /// Waits for the thread to end, for a caller that must know it has (a test). Whether it
    /// ended without a panic that escaped (the thread catches its own).
    pub fn join(mut self) -> bool {
        self.thread
            .take()
            .is_some_and(|thread| thread.join().is_ok())
    }
}

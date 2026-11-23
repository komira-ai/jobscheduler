//! A run the runtime holds: what its start made and what became of it.

use std::collections::BTreeMap;
use std::path::PathBuf;

use js_agent_wire::OutputSpec;

use super::outputs;
use crate::bundle::Pinned;
use crate::lifecycle::{Exit, Status};
use crate::logs::RunLogs;
use crate::mac_accounts::Account;
use crate::mac_supervisor::{Ended, Ready, Supervised};

/// A run: its scratch directory (where its log files and its supervisor's records are), its
/// declared outputs, and, for a run this process started, its account and supervisor.
pub(super) struct Run {
    /// The run's scratch directory, which only the agent can enter.
    pub(super) scratch: PathBuf,
    /// The outputs the start declared.
    pub(super) outputs: Vec<OutputSpec>,
    /// What the removal copied of them, once it has: each file's path and bytes, and the
    /// paths that could not be copied (taken when the removal reports them).
    pub(super) collected: Option<outputs::Collected>,
    /// Set once the run has ended, so it is answered the same way again.
    pub(super) ended: Option<Status>,
    /// The run's labels, as its start had them.
    pub(super) labels: BTreeMap<String, String>,
    /// When the run started, in milliseconds since the Unix epoch.
    pub(super) started_ms: u64,
    /// The trees of the run's bundle, pinned until the run is removed.
    pub(super) _pinned: Option<Pinned>,
    /// Its live logs: the uploader, then the report.
    pub(super) logs: RunLogs,
    /// The steps of its start, each with when it was done, until
    /// [`Runtime::start_steps`](crate::lifecycle::Runtime::start_steps) takes them.
    pub(super) marks: Vec<(&'static str, u64)>,
    /// The account and the supervisor of a run this process started; none for a run an
    /// earlier process of the agent left, whose account the pool's open retired.
    pub(super) job: Option<Job>,
}

/// The live part of a run this process started.
pub(super) struct Job {
    /// The account the run is lent, until its removal retires it.
    pub(super) account: Account,
    /// The account's home, whose `exec/` holds the run's outputs.
    pub(super) home: PathBuf,
    /// The supervisor that waits on the job as the account.
    pub(super) supervised: Box<dyn Supervised>,
    /// What the supervisor said once the job started: its pid (and process group) and its
    /// audit session.
    pub(super) ready: Ready,
    /// When a stop's grace ends, in milliseconds since the Unix epoch: set by
    /// [`Runtime::stop`](crate::lifecycle::Runtime::stop).
    pub(super) kill_after: Option<u64>,
}

impl Run {
    /// Copies the run's outputs into its scratch directory, unless that was done, and logs
    /// what was copied and what was not, by path and byte count. A run with no account (an
    /// earlier process's) has nothing to copy: its home went with the account.
    pub(super) fn collect(&mut self, lease: &str, max_file_bytes: u64) {
        if self.collected.is_some() {
            return;
        }
        let Some(job) = &self.job else {
            self.collected = Some(outputs::Collected::default());
            return;
        };
        let into = outputs::dir_in(&self.scratch);
        let caps = outputs::Caps {
            file_bytes: max_file_bytes,
            run_bytes: outputs::MAX_COLLECTED_BYTES,
            entries: outputs::MAX_ENTRIES,
        };
        let done = outputs::collect(&job.home, &self.outputs, &into, caps);
        for (path, bytes) in &done.copied {
            tracing::info!("native {lease}: output {path:?} copied, {bytes} bytes");
        }
        for why in &done.failed {
            tracing::warn!("native {lease}: output {why:?}");
        }
        self.collected = Some(done);
    }
}

/// How a job ended, as a container's exit is told: a job that a signal ended has the status a
/// shell would give it (128 and the signal). `None` for a setup that failed, which ends no job.
pub(super) fn exit_of(ended: &Ended) -> Option<Exit> {
    let code = match ended {
        Ended::Exited(code) => *code,
        Ended::Signalled(signal) => 128 + signal,
        Ended::SetupFailed { .. } => return None,
    };
    Some(Exit {
        code,
        oom_killed: false,
    })
}

//! The fake's post-job checks ([`ScriptedChecks`]): a child of `fake`, so they read the
//! fake runtime's state as the runtime does.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use js_agent::lifecycle::{CheckFinding, CheckHook, Finished};

use super::{FakeRuntime, Log};

/// Post-job checks that pass until a test makes one find something, and record when
/// they ran and what the host looked like then.
pub struct ScriptedChecks {
    log: Log,
    runtime: FakeRuntime,
    scratch_root: PathBuf,
    findings: BTreeMap<String, CheckFinding>,
}

impl ScriptedChecks {
    pub fn new(log: &Log, runtime: &FakeRuntime, scratch_root: &Path) -> Self {
        Self {
            log: Rc::clone(log),
            runtime: runtime.clone(),
            scratch_root: scratch_root.to_path_buf(),
            findings: BTreeMap::new(),
        }
    }

    pub fn find(mut self, run_id: &str, finding: CheckFinding) -> Self {
        self.findings.insert(run_id.to_owned(), finding);
        self
    }
}

impl CheckHook for ScriptedChecks {
    fn idle(&mut self) {
        self.runtime.state.borrow_mut().idles += 1;
    }

    fn run(&mut self, job: &Finished<'_>) -> Result<(), CheckFinding> {
        let run_id = job.run_id;
        let container = match self.runtime.gone(run_id) {
            true => "container gone",
            false => "container present",
        };
        let scratch = match self.scratch_root.join(run_id).exists() {
            true => "scratch present",
            false => "scratch gone",
        };
        self.log.borrow_mut().push(format!(
            "check {run_id} ({container}, {scratch}){}",
            self.runtime.marker_note()
        ));
        if !job.live.is_empty() {
            let live: Vec<&str> = job.live.iter().map(|id| id.0.as_str()).collect();
            self.log
                .borrow_mut()
                .push(format!("live beside {run_id}: {}", live.join(",")));
        }
        if !job.held_mounts.is_empty() {
            let held: Vec<String> = job
                .held_mounts
                .iter()
                .map(|dir| dir.display().to_string())
                .collect();
            self.log
                .borrow_mut()
                .push(format!("mounts held beside {run_id}: {}", held.join(",")));
        }
        self.findings
            .get(run_id)
            .map_or(Ok(()), |finding| Err(finding.clone()))
    }
}

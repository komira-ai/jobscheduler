//! What the fake commits of a run's outputs at its removal (`Runtime::outputs_report`),
//! and a removal that fails as its outputs not copied.

use js_agent::lifecycle::{OutputsReport, RunHandle, RuntimeError};

use super::FakeRuntime;

impl FakeRuntime {
    /// The removal of the container of `run_id` commits `report`: once it is done,
    /// `outputs_report` answers it for that container, once.
    pub fn commit_outputs(&self, run_id: &str, report: OutputsReport) {
        let mut state = self.state.borrow_mut();
        state.committed.insert(run_id.to_owned(), report);
    }

    /// Removes the container `id`: what its run commits is answered for it from now on. A
    /// removal made to fail as `outputs` removes it and commits nothing.
    pub(super) fn removed(&self, id: &RunHandle) -> Result<(), RuntimeError> {
        let outputs_fail = self.fails("outputs");
        let mut state = self.state.borrow_mut();
        let run = state.containers.remove(&id.0).map(|gone| gone.run_id);
        if outputs_fail {
            return Err(RuntimeError::outputs(
                "outputs not copied: out: a symbolic link: not followed",
            ));
        }
        if let Some(report) = run.and_then(|run| state.committed.remove(&run)) {
            state.answers.insert(id.0.clone(), report);
        }
        Ok(())
    }
}

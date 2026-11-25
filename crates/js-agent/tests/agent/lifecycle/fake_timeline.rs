//! The fake runtime's script of what it gives a start's timeline: its own steps
//! (`start_steps`) and the stamps of the lookup the start waited on (`lookup_stamps`), each
//! given once for the run it was scripted for. A child of `fake`, so it sets the fake
//! runtime's state as the runtime reads it.

use js_agent::lifecycle::{LookupStamps, RunHandle};

use super::FakeRuntime;

impl FakeRuntime {
    /// `start_steps` answers `steps` for `run_id`, once.
    pub fn steps_of(&self, run_id: &str, steps: &[(&'static str, u64)]) {
        let mut state = self.state.borrow_mut();
        state.steps.insert(run_id.to_owned(), steps.to_vec());
    }

    /// `lookup_stamps` answers `stamps` for `run_id`, once.
    pub fn lookup_of(&self, run_id: &str, stamps: LookupStamps) {
        let mut state = self.state.borrow_mut();
        state.lookups.insert(run_id.to_owned(), stamps);
    }

    /// The run id of the container `id`, when the fake made it.
    fn run_of(&self, id: &RunHandle) -> Option<String> {
        let state = self.state.borrow();
        state.containers.get(&id.0).map(|c| c.run_id.clone())
    }

    /// What `start_steps` answers for the container `id`: its run's steps, once.
    pub(super) fn take_steps(&self, id: &RunHandle) -> Vec<(&'static str, u64)> {
        let run_id = self.run_of(id);
        let steps = run_id.and_then(|run_id| self.state.borrow_mut().steps.remove(&run_id));
        steps.unwrap_or_default()
    }

    /// What `lookup_stamps` answers for the container `id`: its run's stamps, once.
    pub(super) fn take_lookup(&self, id: &RunHandle) -> Option<LookupStamps> {
        let run_id = self.run_of(id)?;
        self.state.borrow_mut().lookups.remove(&run_id)
    }
}

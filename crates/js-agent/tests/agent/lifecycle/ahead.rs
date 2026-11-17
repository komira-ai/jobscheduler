//! What the fake runtime says of containers made ahead of their jobs ([`Runtime::tend`],
//! [`Runtime::idle_handles`], [`Runtime::started_warm`]): the test sets it, and the fake
//! answers from it. Clones share one record.

use std::cell::RefCell;
use std::rc::Rc;

use js_agent::lifecycle::RunHandle;

/// The script and the record of the hooks for containers made ahead of their jobs.
#[derive(Clone, Default)]
pub struct Ahead {
    tends: Rc<RefCell<Vec<u64>>>,
    idle: Rc<RefCell<Vec<RunHandle>>>,
    warm: Rc<RefCell<Vec<String>>>,
}

impl Ahead {
    /// From now on the runtime holds the idle containers `handles`.
    pub fn hold_idle(&self, handles: &[&str]) {
        *self.idle.borrow_mut() = handles
            .iter()
            .map(|id| RunHandle((*id).to_owned()))
            .collect();
    }

    /// From now on the runtime says the run in the container `handle` started warm.
    pub fn started_in(&self, handle: &str) {
        self.warm.borrow_mut().push(handle.to_owned());
    }

    /// The clock reading of each call of `tend`, in order.
    pub fn tended(&self) -> Vec<u64> {
        self.tends.borrow().clone()
    }

    pub(super) fn tend(&self, now_ms: u64) {
        self.tends.borrow_mut().push(now_ms);
    }

    pub(super) fn idle(&self) -> Vec<RunHandle> {
        self.idle.borrow().clone()
    }

    pub(super) fn is_warm(&self, id: &RunHandle) -> bool {
        self.warm.borrow().contains(&id.0)
    }
}

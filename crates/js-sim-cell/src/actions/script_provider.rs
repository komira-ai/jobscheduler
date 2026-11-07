//! Queues steps for a call of the elastic pool's cloud.

use js_elastic_fake::{Call, Step};

use super::Action;
use crate::cell::Cell;

/// Queues steps for the cloud's calls of one kind, after those already queued
/// ([`Cell::script_provider`]): each call of that kind takes the oldest. Only `acquire`,
/// `list`, `release` and `is_gone` are scripted; nothing for a cell without an elastic pool.
#[derive(Clone, Debug)]
pub struct ScriptProvider {
    call: Call,
    steps: Vec<Step>,
}

/// Queues `steps` for `call` now.
#[must_use]
pub fn script_provider(call: Call, steps: &[Step]) -> ScriptProvider {
    ScriptProvider {
        call,
        steps: steps.to_vec(),
    }
}

impl Action for ScriptProvider {
    fn run(&self, cell: &mut Cell) {
        cell.script_provider(self.call, &self.steps);
    }
}

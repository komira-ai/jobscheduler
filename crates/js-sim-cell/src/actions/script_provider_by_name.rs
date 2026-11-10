//! Queues steps for a call of one named cloud of the pool.

use js_elastic_fake::{Call, Step};

use super::Action;
use crate::cell::Cell;

/// Queues steps for the calls of one kind of the cloud named `provider`, after those already
/// queued ([`Cell::script_provider_by_name`]): each call of that kind takes the oldest. Only
/// `acquire`, `list`, `release` and `is_gone` are scripted; nothing for a cell with no cloud
/// of that name.
#[derive(Clone, Debug)]
pub struct ScriptProviderByName {
    provider: String,
    call: Call,
    steps: Vec<Step>,
}

/// Queues `steps` for `call` of the cloud named `provider` now.
#[must_use]
pub fn script_provider_by_name(provider: &str, call: Call, steps: &[Step]) -> ScriptProviderByName {
    ScriptProviderByName {
        provider: provider.to_owned(),
        call,
        steps: steps.to_vec(),
    }
}

impl Action for ScriptProviderByName {
    fn run(&self, cell: &mut Cell) {
        cell.script_provider_by_name(&self.provider, self.call, &self.steps);
    }
}

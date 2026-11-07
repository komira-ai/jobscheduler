//! Sets the step a call of the elastic pool's cloud repeats.

use js_elastic_fake::{Call, Step};

use super::Action;
use crate::cell::Cell;

/// Makes every call of one kind with no step queued take one step, for good or until it is
/// set again ([`Cell::script_provider_forever`]): `Step::Pass` ends a storm. Nothing for a
/// cell without an elastic pool.
#[derive(Clone, Debug)]
pub struct ScriptProviderForever {
    call: Call,
    step: Step,
}

/// Makes `call` take `step` from now on.
#[must_use]
pub fn script_provider_forever(call: Call, step: Step) -> ScriptProviderForever {
    ScriptProviderForever { call, step }
}

impl Action for ScriptProviderForever {
    fn run(&self, cell: &mut Cell) {
        cell.script_provider_forever(self.call, self.step);
    }
}

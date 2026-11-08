//! Loses the answer to the client's next submit.

use super::Action;
use crate::cell::Cell;
use crate::jobs::LostAnswer;

/// Throws a [`LostAnswer`] switch: the job front that shares it
/// ([`crate::JobFront::losing_answers`]) reads the answer to its next SubmitJob as none and
/// submits again under the same job id. The client's own fault, not the bus's: a client
/// call is never dropped on the wire.
#[derive(Clone, Debug)]
pub struct LoseSubmitAnswer(LostAnswer);

/// Throws `switch` now.
#[must_use]
pub fn lose_submit_answer(switch: &LostAnswer) -> LoseSubmitAnswer {
    LoseSubmitAnswer(switch.clone())
}

impl Action for LoseSubmitAnswer {
    fn run(&self, _cell: &mut Cell) {
        self.0.throw();
    }
}

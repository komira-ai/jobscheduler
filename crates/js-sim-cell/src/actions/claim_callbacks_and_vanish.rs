//! A second instance claims the due callbacks and dies.

use super::Action;
use crate::cell::Cell;

/// Has a second instance claim every callback that is due now and never answer
/// ([`Cell::claim_callbacks_and_vanish`]): the claims lapse after 30 s, and the cell's
/// server delivers the callbacks then.
#[derive(Clone, Debug)]
pub struct ClaimCallbacksAndVanish;

/// Has a second instance claim the due callbacks now and vanish.
#[must_use]
pub fn claim_callbacks_and_vanish() -> ClaimCallbacksAndVanish {
    ClaimCallbacksAndVanish
}

impl Action for ClaimCallbacksAndVanish {
    fn run(&self, cell: &mut Cell) {
        cell.claim_callbacks_and_vanish();
    }
}

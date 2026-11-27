//! Switches the callback receiver down for a while.

use std::time::Duration;

use super::Action;
use crate::cell::Cell;

/// Switches the cell's callback receiver down now for `lasting`
/// ([`Cell::set_callback_receiver_down`]): it answers a failed try to every call made
/// until then, and answers 2xx again after.
#[derive(Clone, Debug)]
pub struct CallbackReceiverDown(Duration);

/// Switches the receiver down now for `lasting`.
#[must_use]
pub fn callback_receiver_down(lasting: Duration) -> CallbackReceiverDown {
    CallbackReceiverDown(lasting)
}

impl Action for CallbackReceiverDown {
    fn run(&self, cell: &mut Cell) {
        cell.set_callback_receiver_down(self.0);
    }
}

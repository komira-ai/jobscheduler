//! Cancels items.

use super::Action;
use crate::cell::Cell;
use crate::observe::ItemId;

/// Asks the client to cancel each item now ([`Cell::cancel_item`]); the client's front
/// says what a cancel does at each stage of an item.
#[derive(Clone, Debug)]
pub struct CancelWork(Vec<ItemId>);

/// Cancels `items` now.
#[must_use]
pub fn cancel_work(items: Vec<ItemId>) -> CancelWork {
    CancelWork(items)
}

impl Action for CancelWork {
    fn run(&self, cell: &mut Cell) {
        for item in &self.0 {
            cell.cancel_item(*item);
        }
    }
}

//! Submits units of work.

use super::Action;
use crate::cell::Cell;
use crate::work::Work;

/// Submits a number of items of one [`Work`] now ([`Cell::submit`]), numbered after the
/// items submitted before.
#[derive(Clone, Debug)]
pub struct SubmitWork {
    count: u32,
    work: Work,
}

/// Submits `count` items of `work` now.
#[must_use]
pub fn submit_work(count: u32, work: Work) -> SubmitWork {
    SubmitWork { count, work }
}

impl Action for SubmitWork {
    fn run(&self, cell: &mut Cell) {
        for _ in 0..self.count {
            cell.submit(&self.work);
        }
    }
}

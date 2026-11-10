//! Cancels a whole-host job once the server has placed it and its host prepares.

use std::time::Duration;

use js_jobs::JobState;
use js_types::sched::FarmTime;

use super::Action;
use crate::cell::{Cell, LISTING_EVERY};
use crate::jobs::JOB_PREFIX;
use crate::observe::ItemId;

/// Looks at the server's job table as the cell last read it, now and then once every
/// [`LISTING_EVERY`] until `within` has passed from the action's first run, and asks the
/// client to cancel `item` ([`Cell::cancel_item`]) at the first look that finds its job
/// placed (`placed`, not yet starting): a whole-host job is placed from the commit of its
/// host until the agent's ready report, so this is a cancel while the host prepares. It
/// cancels nothing when no look finds the job placed.
#[derive(Clone, Debug)]
pub struct CancelWhenPreparing {
    item: ItemId,
    until: Option<FarmTime>,
    within: Duration,
}

/// Cancels `item` at the first look, within `within`, that finds its job placed.
#[must_use]
pub fn cancel_when_preparing(item: ItemId, within: Duration) -> CancelWhenPreparing {
    CancelWhenPreparing {
        item,
        until: None,
        within,
    }
}

impl Action for CancelWhenPreparing {
    fn run(&self, cell: &mut Cell) {
        let now = cell.now();
        let until = self.until.unwrap_or(now.saturating_add(self.within));
        let job = format!("{JOB_PREFIX}{}", self.item.0);
        let mut reads = cell.observations().job_reads.iter().rev();
        let last = reads.find(|read| read.job.as_str() == job);
        let placed = last.is_some_and(|read| read.state == JobState::Placed);
        let next = now.saturating_add(LISTING_EVERY);
        if placed {
            cell.cancel_item(self.item);
        } else if next <= until {
            let again = Self {
                until: Some(until),
                ..self.clone()
            };
            cell.at(next, move |cell| again.run(cell));
        }
    }
}

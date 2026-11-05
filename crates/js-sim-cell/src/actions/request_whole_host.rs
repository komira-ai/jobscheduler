//! Requests whole hosts.

use std::time::Duration;

use super::Action;
use crate::cell::Cell;
use crate::work::Work;

/// Submits a number of whole-host requests now ([`Cell::submit`]): each is a whole-node
/// lease asked for under a request id of its own, with a wall limit and a ttl of
/// `max_wall`, answered 202 and pending until the agent of the host it is committed on
/// reports ready; once held it is given a run that ends at once, and released. It is
/// [`Work::whole_host`] of no time, given up after `max_wall`.
#[derive(Clone, Debug)]
pub struct RequestWholeHost {
    count: u32,
    max_wall: Duration,
}

/// Requests `count` whole hosts of wall limit `max_wall`, now.
#[must_use]
pub fn request_whole_host(count: u32, max_wall: Duration) -> RequestWholeHost {
    RequestWholeHost { count, max_wall }
}

impl Action for RequestWholeHost {
    fn run(&self, cell: &mut Cell) {
        let work = Work::whole_host(Duration::ZERO).give_up_after(self.max_wall);
        for _ in 0..self.count {
            cell.submit(&work);
        }
    }
}

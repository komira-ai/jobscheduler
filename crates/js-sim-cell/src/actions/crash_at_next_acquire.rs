//! Crashes the server at the next acquire of its elastic pool.

use super::Action;
use crate::cell::Cell;
use crate::elastic::Cloud;

/// Makes the next acquire of the elastic pool's scaler crash the server once the call has
/// reached the cloud and before the server reads its answer
/// ([`crate::Cloud::crash_at_next_acquire`]); nothing for a cell without an elastic pool.
#[derive(Clone, Debug)]
pub struct CrashAtNextAcquire;

/// Arms the crash now.
#[must_use]
pub fn crash_at_next_acquire() -> CrashAtNextAcquire {
    CrashAtNextAcquire
}

impl Action for CrashAtNextAcquire {
    fn run(&self, cell: &mut Cell) {
        cell.cloud()
            .into_iter()
            .for_each(Cloud::crash_at_next_acquire);
    }
}

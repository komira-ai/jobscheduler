//! Resets the brake of an elastic pool, as an operator does.

use super::Action;
use crate::cell::Cell;

/// Asks the server to reset a pool's brake through its operator route
/// (`POST /v1/pools/{pool}/brake/reset`, [`Cell::reset_brake_by_route`]); the scaler applies
/// it at its next tick, and an accepted reset is recorded with the cloud's calls.
#[derive(Clone, Debug)]
pub struct ResetBrake(String);

/// Resets the brake of `pool` now.
#[must_use]
pub fn reset_brake(pool: &str) -> ResetBrake {
    ResetBrake(pool.to_owned())
}

impl Action for ResetBrake {
    fn run(&self, cell: &mut Cell) {
        cell.reset_brake_by_route(&self.0);
    }
}

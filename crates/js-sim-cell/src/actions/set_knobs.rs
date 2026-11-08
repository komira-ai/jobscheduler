//! Replaces the limits and delays of the elastic pool's cloud.

use js_elastic_fake::Knobs;

use super::Action;
use crate::cell::Cell;

/// Replaces the cloud's knobs as a whole ([`Cell::set_cloud_knobs`]): its quota, its room,
/// its list and removal lags and how long a lost acquire stays unresolved. A machine's boot
/// time stays the pool's. Nothing for a cell without an elastic pool.
#[derive(Clone, Debug)]
pub struct SetKnobs(Knobs);

/// Sets `knobs` now.
#[must_use]
pub fn set_knobs(knobs: Knobs) -> SetKnobs {
    SetKnobs(knobs)
}

impl Action for SetKnobs {
    fn run(&self, cell: &mut Cell) {
        cell.set_cloud_knobs(&self.0);
    }
}

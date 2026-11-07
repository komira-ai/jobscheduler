//! Makes the machines of an elastic pool that boot from now on never say hello, or say it
//! again.

use super::Action;
use crate::cell::Cell;

/// Switches whether the machines of a pool that boot from now on say hello
/// ([`Cell::machines_never_join`]): with `never`, the cell boots no agent for them (the
/// pool's `joins(false)`, at a time), so the scaler's boot timeout gives up on each. A
/// machine that has booted keeps what it did. Nothing for a pool the cell does not have.
#[derive(Clone, Debug)]
pub struct MachinesNeverJoin {
    pool: String,
    never: bool,
}

/// Makes the machines of `pool` that boot from now on never say hello (`never`), or say it
/// again (`!never`).
#[must_use]
pub fn machines_never_join(pool: &str, never: bool) -> MachinesNeverJoin {
    MachinesNeverJoin {
        pool: pool.to_owned(),
        never,
    }
}

impl Action for MachinesNeverJoin {
    fn run(&self, cell: &mut Cell) {
        cell.machines_never_join(&self.pool, self.never);
    }
}

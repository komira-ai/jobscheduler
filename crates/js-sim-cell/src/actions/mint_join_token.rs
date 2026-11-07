//! Mints one-time join tokens through the operator route.

use super::Action;
use crate::cell::Cell;

/// Mints `count` one-time join tokens through the operator route, each bound to an
/// instance of its own ([`Cell::mint_join_token`]); [`super::join_agents`] gives each a
/// host.
#[derive(Clone, Debug)]
pub struct MintJoinToken(usize);

/// Mints `count` one-time join tokens now.
#[must_use]
pub fn mint_join_token(count: usize) -> MintJoinToken {
    MintJoinToken(count)
}

impl Action for MintJoinToken {
    fn run(&self, cell: &mut Cell) {
        for _ in 0..self.0 {
            cell.mint_join_token();
        }
    }
}

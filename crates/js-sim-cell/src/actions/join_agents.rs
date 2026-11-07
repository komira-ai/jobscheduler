//! Starts hosts that say hello with tokens of their own.

use std::ops::Range;

use super::Action;
use crate::cell::Cell;

/// Adds a host for each minted token of the range, in order, whose agent says hello with
/// that token and names its instance ([`Cell::join`]): a group of hosts with tokens of
/// their own. A token named twice gives two hosts that present it.
#[derive(Clone, Debug)]
pub struct JoinAgents(Range<usize>);

/// Adds a host now for each minted token of `tokens` (by the order they were minted, from
/// 0).
#[must_use]
pub fn join_agents(tokens: Range<usize>) -> JoinAgents {
    JoinAgents(tokens)
}

impl Action for JoinAgents {
    fn run(&self, cell: &mut Cell) {
        for token in self.0.clone() {
            cell.join(token);
        }
    }
}

//! Makes one named cloud of the pool refuse every acquire for capacity.

use super::Action;
use crate::cell::Cell;

/// Makes every acquire of the cloud named `provider` refuse for `capacity`, every offer and
/// zone of it, from now on ([`Cell::exhaust_provider`]); nothing for a cell with no cloud of
/// that name.
#[derive(Clone, Debug)]
pub struct ExhaustProvider(String);

/// Exhausts the cloud named `provider` now.
#[must_use]
pub fn exhaust_provider(provider: &str) -> ExhaustProvider {
    ExhaustProvider(provider.to_owned())
}

impl Action for ExhaustProvider {
    fn run(&self, cell: &mut Cell) {
        cell.exhaust_provider(&self.0);
    }
}

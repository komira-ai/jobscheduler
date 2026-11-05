//! Cancels a whole-host item once the server sets a host aside for it.

use std::time::Duration;

use js_types::sched::FarmTime;

use super::Action;
use crate::cell::{Cell, LISTING_EVERY};
use crate::front::REQUEST_PREFIX;
use crate::observe::ItemId;

/// Looks at the server's listing as the cell last read it, now and then once every
/// [`LISTING_EVERY`] until `within` has passed from the action's first run, and asks the
/// client to cancel `item` ([`Cell::cancel_item`]) at the first look that finds a host
/// set aside for the item's request (`reserved_for` is `sim-item-<n>`): a cancel of a
/// marked whole-host request. It cancels nothing when no look finds one.
#[derive(Clone, Debug)]
pub struct CancelWhenSetAside {
    item: ItemId,
    until: Option<FarmTime>,
    within: Duration,
}

/// Cancels `item` at the first look, within `within`, that finds a host set aside for it.
#[must_use]
pub fn cancel_when_set_aside(item: ItemId, within: Duration) -> CancelWhenSetAside {
    CancelWhenSetAside {
        item,
        until: None,
        within,
    }
}

impl Action for CancelWhenSetAside {
    fn run(&self, cell: &mut Cell) {
        let now = cell.now();
        let until = self.until.unwrap_or(now.saturating_add(self.within));
        let request = format!("{REQUEST_PREFIX}{}", self.item.0);
        let mut listed = cell.observations().server.hosts.values();
        let aside = listed.any(|host| host.reserved_for.as_deref() == Some(request.as_str()));
        let next = now.saturating_add(LISTING_EVERY);
        if aside {
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

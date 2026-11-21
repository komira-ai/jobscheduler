//! Which user the helper lent a run's lease, and so which home is the run's.

use js_mac_session::client::Client;
use js_mac_session::lease::{Slot, pool_user_name, user_name};
use js_types::LeaseId;

use super::fail;
use crate::lifecycle::RuntimeError;

/// The name of the user `helper` lent `lease` (whose id is `id`), whose home is the run's:
/// the pool user of the slot `pool-status` names for the lease (`kbf-lease-pool-<slot>`);
/// the uid alone does not name it, and `user-create` answers with the uid only. A helper
/// from before the pool, which does not know `pool-status`, made a user of the lease's own
/// (`kbf-lease-<term>-<seq>`).
pub(super) fn user_of(helper: &Client, id: LeaseId, lease: &str) -> Result<String, RuntimeError> {
    let state = match helper.pool_status() {
        Ok(state) => state,
        Err(why) if why.unknown_to_helper() => return Ok(user_name(id)),
        Err(why) => return Err(fail(&"pool-status", &why)),
    };
    state
        .leased
        .iter()
        .find(|(_, held)| held == lease)
        .map(|(slot, _)| pool_user_name(Slot::new(*slot)))
        .ok_or_else(|| {
            RuntimeError::new(format!(
                "pool-status: the helper names no pool user for lease {lease}"
            ))
        })
}

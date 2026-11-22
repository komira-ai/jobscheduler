//! The user the helper made a run's lease, and so which home is the run's.

use js_mac_session::client::Client;
use js_mac_session::lease::user_name;
use js_types::LeaseId;

use crate::lifecycle::RuntimeError;

/// The name of the user `helper` made `lease` (whose id is `id`), whose home is the run's:
/// `kbf-lease-<term>-<seq>`, the account the helper makes for each lease.
pub(super) fn user_of(_helper: &Client, id: LeaseId, _lease: &str) -> Result<String, RuntimeError> {
    Ok(user_name(id))
}

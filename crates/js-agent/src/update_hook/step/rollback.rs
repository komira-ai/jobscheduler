//! `rollback` (host-updates design, 4.2): not written yet.

use crate::update_hook::{Done, Params, Reason};

use super::{Env, failure};

/// Runs `rollback`.
pub fn rollback(_env: &Env<'_>, _params: &Params) -> Done {
    failure(Reason::NotImplemented, "rollback is not implemented")
}

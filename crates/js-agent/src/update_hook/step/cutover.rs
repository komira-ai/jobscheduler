//! `cutover` (host-updates design, 4.2): red stub.

use crate::update_hook::{Done, Params, Reason};

use super::{Env, failure};

/// Runs `cutover`.
pub fn cutover(_env: &Env<'_>, _params: &Params) -> Done {
    failure(Reason::NotImplemented, "cutover is not implemented")
}

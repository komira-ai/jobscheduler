//! `verify` (host-updates design, 4.2): look until the new agent is proved or a bound passes.
//! It reads and never changes the machine; the one write is the record's `verify_ok`, after
//! the observation it names.

use crate::update_hook::{Done, Params, Reason};

use super::{Env, failure};

/// Runs `verify`.
pub fn verify(_env: &Env<'_>, _params: &Params) -> Done {
    failure(Reason::NotImplemented, "verify is not implemented")
}

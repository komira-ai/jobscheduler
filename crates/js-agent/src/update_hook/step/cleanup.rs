//! `cleanup`.

use super::super::{Done, Outcome, Params, Reason, Report};
use super::Env;

/// Runs `cleanup`.
pub fn cleanup(_env: &Env<'_>, _params: &Params) -> Done {
    Done {
        outcome: Outcome::Failure,
        report: Report::failure(Reason::NotImplemented, "cleanup is not implemented"),
    }
}

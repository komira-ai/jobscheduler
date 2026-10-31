//! `prepare`.

use super::super::{Done, Outcome, Params, Reason, Report};
use super::Env;

/// Runs `prepare`.
pub fn prepare(_env: &Env<'_>, _params: &Params) -> Done {
    Done {
        outcome: Outcome::Failure,
        report: Report::failure(Reason::NotImplemented, "prepare is not implemented"),
    }
}

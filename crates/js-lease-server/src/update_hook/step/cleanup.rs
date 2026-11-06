//! `cleanup`: run only after `verify` succeeded. It deletes the hook record; the server's own
//! release directories are the updater's to prune. A `cleanup` that finds no record has
//! finished before. A record without `verify_ok` is kept, and the step fails `not_verified`:
//! it is what a `rollback` needs.

use crate::update_hook::{Done, Params, Reason};

use super::{Env, failure, open_state, state_failure, success, success_with};

/// Runs `cleanup`.
pub fn cleanup(env: &Env<'_>, params: &Params) -> Done {
    run(env, params).unwrap_or_else(|done| done)
}

fn run(env: &Env<'_>, params: &Params) -> Result<Done, Done> {
    let (held, record) = open_state(env, params)?;
    let Some(record) = record else {
        return Ok(success_with("there is no record: cleanup has finished"));
    };
    if !record.verify_ok {
        return Err(failure(
            Reason::NotVerified,
            "the record does not say that verify succeeded",
        ));
    }
    env.ports.point();
    held.delete().map_err(state_failure)?;
    Ok(success())
}

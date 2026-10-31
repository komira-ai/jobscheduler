//! `cleanup` (host-updates design, 4.2): run only after `verify` succeeded. It lifts the
//! cordon if this update placed it, and deletes the hook record.
//!
//! The record is deleted last, so a `cleanup` that is killed or whose uncordon fails leaves a
//! record that still says there is a cordon to lift, and the next run lifts it again (an
//! uncordon of nothing succeeds). A `cleanup` that finds no record has finished before.

use crate::update_hook::{Done, Params, Reason, Report};

use super::{Env, failure, lease_failure, open_state, state_failure, success};

/// Runs `cleanup`.
pub fn cleanup(env: &Env<'_>, params: &Params) -> Done {
    run(env, params).unwrap_or_else(|done| done)
}

fn run(env: &Env<'_>, params: &Params) -> Result<Done, Done> {
    let (held, record) = open_state(env, params)?;
    let Some(record) = record else {
        return Ok(Done {
            report: Report {
                detail: Some("there is no record: cleanup has finished".to_owned()),
                ..Report::default()
            },
            ..success()
        });
    };
    if !record.verify_ok {
        return Err(failure(
            Reason::NotVerified,
            "the record does not say that verify succeeded",
        ));
    }
    if record.cordon.placed {
        env.ports.point();
        env.ports.lease.uncordon().map_err(lease_failure)?;
    }
    env.ports.point();
    held.delete().map_err(state_failure)?;
    Ok(success())
}

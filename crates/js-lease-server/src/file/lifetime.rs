//! The host-lifetime keys of an elastic pool (`idle`, `max_age`, `min_host_lifetime`) and
//! the rules between them and `server.max_lease_wall`.

use std::time::Duration;

use super::Section;
use crate::error::StartupError;

/// The most `max_age` and `server.max_lease_wall` may be: 30 days. A longer figure is a typo
/// or an intent to run unbounded, and the cloud lifetime built from them would be useless.
pub(super) const LONGEST: Duration = Duration::from_secs(30 * 24 * 3600);

/// Why a pool with accelerators must say how long a host may live.
const ACCELERATORS_NEED: &str = "required of a pool with accelerators: an idle host costs money by the hour, so the file names how long it may live";

/// The lifetime keys as the pool's table gave them.
pub(super) struct Given {
    pub(super) idle: Option<Duration>,
    pub(super) max_age: Option<Duration>,
    pub(super) min_host_lifetime: Option<Duration>,
}

/// The `max_age` and `min_host_lifetime` of an elastic pool, and the refusals that keep a
/// pool with accelerators from running up a bill unbounded: it must set `idle`, `max_age`
/// and `min_host_lifetime`, none with a default, and the server `max_lease_wall`, since
/// `max_age` plus that ceiling is what a draining host takes to empty and what the cloud is
/// told to enforce. `max_age` must exceed `boot_timeout` (a machine would be retired before
/// it could say hello) and not pass [`LONGEST`]; `min_host_lifetime` may not exceed
/// `max_age`.
///
/// A pool has accelerators when its kind's floor says one GPU or more. The floor is the
/// least of the sizes the kind may launch, so this relies on every size of the kind having
/// at least one accelerator, as Lambda's do; a provider whose catalog mixes sizes with and
/// without accelerators must revisit the test, since its floor would read as none and the
/// pool would escape these refusals.
pub(super) fn check(
    s: &Section,
    given: &Given,
    (accelerators, boot_timeout, ceiling): (bool, Duration, Option<Duration>),
) -> Result<(Option<Duration>, Duration), StartupError> {
    if accelerators {
        if given.idle.is_none() {
            return Err(s.fail("idle", ACCELERATORS_NEED));
        }
        if given.max_age.is_none() {
            return Err(s.fail("max_age", ACCELERATORS_NEED));
        }
        if given.min_host_lifetime.is_none() {
            return Err(s.fail("min_host_lifetime", ACCELERATORS_NEED));
        }
        if ceiling.is_none() {
            let reason = "required with a pool with accelerators: it bounds how long a lease, and so a draining host, can last";
            return Err(StartupError::at("server.max_lease_wall", reason));
        }
    }
    let age = given.max_age.unwrap_or(Duration::MAX);
    if age <= boot_timeout {
        return Err(s.fail("max_age", "must be longer than boot_timeout"));
    }
    if given.max_age.is_some_and(|age| age > LONGEST) {
        return Err(s.fail("max_age", "must not be longer than 30 days"));
    }
    let minimum = given.min_host_lifetime.unwrap_or(Duration::ZERO);
    if minimum > age {
        return Err(s.fail("min_host_lifetime", "must not be longer than max_age"));
    }
    Ok((given.max_age, minimum))
}

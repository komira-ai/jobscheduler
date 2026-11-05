//! The silence bound (`--silence-stop`): how long the agent goes on running its jobs when
//! it hears nothing from the server, and the [`Contact`] the session stamps and the
//! lifecycle reads (see `Lifecycle::stop_on_silence`).
//!
//! # Why 300 seconds
//!
//! The bound must not trip on a routine loss of the server, and must end a job the server
//! gave up on long before the job's own wall limit. A server restart silences a well agent
//! for at most: the heartbeat interval before the last reply it heard (5 s by default), the
//! server's downtime, then the wait before the agent's next hello, which a `Retry-After`
//! raises to at most [`RETRY_AFTER_CAP`] (120 s; the agent's own backoff is at most 60 s),
//! and a refusal of that hello while the old session is still within its grace (15 s by
//! default) before the hello is taken. That is 140 s and the downtime, so the default of
//! 300 s ([`DEFAULT_SILENCE_STOP`]) lets a server restart take up to 160 s without stopping a
//! job, and a lost reply or two (5 s each) is nothing beside it. Once the bound is reached
//! each job gets the stop grace (10 s by default) before it is killed, so a partitioned
//! host runs a job the server gave up on for at most 310 s by default, against wall limits
//! of up to days. The default is [`SILENCE_STOP_GRACE_RATIO`] (20) times the server's
//! default grace, which a const assertion holds it to.
//!
//! A bound at or below the server's grace plus the stop grace could stop the jobs of a host
//! the server still counts as present ([`fits_grace`]). The agent refuses such a bound at
//! start against the default grace (status 2, so a `--stop-grace` of 285 s or more needs a
//! longer `--silence-stop`), and again when a welcome names the server's own grace: the
//! agent then ends, naming the flag. The most the flag takes is [`MAX_SILENCE_STOP_SECS`]
//! (two hours, so that the longest `--stop-grace`, an hour, has a bound above it). There is
//! no value that turns the stop off.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use js_agent_wire::{DEFAULT_GRACE_MS, DEFAULT_HEARTBEAT_INTERVAL_MS, RETRY_AFTER_CAP};

use super::DEFAULT_STOP_GRACE;

/// How long the agent goes on when it hears nothing from the server, when `--silence-stop`
/// is not given: 300 seconds (see the module for the arithmetic).
pub const DEFAULT_SILENCE_STOP: Duration = Duration::from_secs(300);

/// The most seconds `--silence-stop` takes: two hours.
pub const MAX_SILENCE_STOP_SECS: u64 = 7200;

/// How many times the server's default grace ([`DEFAULT_GRACE_MS`]) the default bound is,
/// at least.
pub const SILENCE_STOP_GRACE_RATIO: u64 = 20;

const _: () = assert!(
    DEFAULT_SILENCE_STOP.as_millis()
        >= (SILENCE_STOP_GRACE_RATIO as u128) * (DEFAULT_GRACE_MS as u128),
    "the default silence bound must be at least twenty default graces"
);
const _: () = assert!(
    DEFAULT_SILENCE_STOP.as_millis()
        > (DEFAULT_HEARTBEAT_INTERVAL_MS as u128)
            + RETRY_AFTER_CAP.as_millis()
            + (DEFAULT_GRACE_MS as u128)
            + DEFAULT_STOP_GRACE.as_millis(),
    "the default silence bound must outlast what a server restart costs a well agent"
);
const _: () = assert!(DEFAULT_SILENCE_STOP.as_secs() <= MAX_SILENCE_STOP_SECS);
const _: () = assert!(
    (MAX_SILENCE_STOP_SECS as u128) * 1000
        > (super::MAX_GRACE_SECS as u128) * 1000 + (DEFAULT_GRACE_MS as u128),
    "the longest stop grace must have a silence bound above it"
);

/// `--silence-stop`, checked: whole seconds from 1 to [`MAX_SILENCE_STOP_SECS`], or
/// [`DEFAULT_SILENCE_STOP`] when the flag is not given.
///
/// # Errors
///
/// The text names the flag: the value is 0 or above the most.
pub fn silence_stop(given: Option<u64>) -> Result<Duration, String> {
    match given {
        None => Ok(DEFAULT_SILENCE_STOP),
        Some(0) => Err(
            "--silence-stop: at least 1 second: the agent always stops its jobs after a silence"
                .to_owned(),
        ),
        Some(secs) if secs > MAX_SILENCE_STOP_SECS => Err(format!(
            "--silence-stop: at most {MAX_SILENCE_STOP_SECS} seconds"
        )),
        Some(secs) => Ok(Duration::from_secs(secs)),
    }
}

/// Whether the silence bound `bound` is above the grace a welcome names (`grace_ms`) plus
/// the stop grace: a bound at or below it could stop the jobs of a host the server still
/// counts as present.
///
/// # Errors
///
/// The text names the flag and the three numbers.
pub fn fits_grace(bound: Duration, grace_ms: u64, stop_grace: Duration) -> Result<(), String> {
    let floor = Duration::from_millis(grace_ms).saturating_add(stop_grace);
    match bound > floor {
        true => Ok(()),
        false => Err(format!(
            "--silence-stop: {} ms is at or below the server's grace of {grace_ms} ms plus the \
             stop grace of {} ms, so a host the server still counts as present could stop its \
             jobs; give a longer bound",
            bound.as_millis(),
            stop_grace.as_millis()
        )),
    }
}

/// When the agent last heard from the server, on the lifecycle's clock
/// ([`Clock`](super::Clock)): the session stamps it at every welcome and at every heartbeat
/// reply it accepts (one the worker had no room for included), and the lifecycle reads it
/// at every step. Clones share one stamp, across threads.
#[derive(Debug, Clone, Default)]
pub struct Contact(Arc<AtomicU64>);

impl Contact {
    /// Records that the server was heard at `at_ms`. A stamp earlier than the one held
    /// changes nothing.
    pub fn heard(&self, at_ms: u64) {
        // Kept one above the reading, so that 0 means never.
        self.0.fetch_max(at_ms.saturating_add(1), Ordering::SeqCst);
    }

    /// When the server was last heard; none before the first contact.
    pub fn last(&self) -> Option<u64> {
        self.0.load(Ordering::SeqCst).checked_sub(1)
    }
}

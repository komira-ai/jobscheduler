//! The rules of a cordon request that the host's own route (`POST /v1/hosts/{id}:cordon`)
//! and the operator's call (`CordonHost` of the job service) share: none asked for is
//! [`DEFAULT_CORDON`], zero is refused, and the length asked for is cut to [`MAX_CORDON`] on
//! the host's route ([`length`]) and to [`MAX_OPERATOR_CORDON`] on the operator's
//! ([`operator_length`]). The host's route is a lease its caller renews; the operator's call
//! takes a host out of placement for as long as it says with nothing renewing it, kept over
//! restarts of the agent and of the server, until it lapses or `UncordonHost` lifts it. The
//! cordon's rule ([`js_types::cordon`]) does the rest (a repeat renews and never shortens),
//! and the caller writes the cordon's record before it answers.

use std::time::Duration;

/// The length of a cordon asked for without one.
pub(crate) const DEFAULT_CORDON: Duration = Duration::from_secs(5 * 60);

/// The longest a cordon is set for by one request; a longer one asked for is cut to it. A
/// requester that needs a cordon for longer renews it.
pub(crate) const MAX_CORDON: Duration = Duration::from_secs(60 * 60);

/// The longest an operator's cordon (`CordonHost`) is set for by one call: seven days. A
/// longer one asked for is cut to it. It bounds a cordon an operator forgot, which lapses by
/// itself at the latest this long after the last call.
pub(crate) const MAX_OPERATOR_CORDON: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The refusal of a cordon of zero seconds (and, on the host's route, of a body that does
/// not read).
pub(crate) const INVALID_CORDON: &str = "invalid-cordon";

/// The answer when the cordon or uncordon holds on this server but the bindings that keep
/// it over a restart could not be written.
pub(crate) const CORDON_UNSAVED: &str = "cordon-unsaved";

/// The answer when the clear of a host's taint was not committed (the store failed, or holds
/// another taint than this server's), or holds on this server but the bindings that keep it
/// over a restart could not be written.
pub(crate) const TAINT_UNSAVED: &str = "taint-unsaved";

/// The answer when a retirement could not be read or written to the job store or the bindings.
pub(crate) const RETIRE_UNSAVED: &str = "retire-unsaved";

/// The answer to a retirement of a host heard within the grace.
pub(crate) const HOST_LIVE: &str = "host-live";

/// The answer to a retirement of a host on which a job's allocation is live.
pub(crate) const HOST_HOLDS_JOBS: &str = "host-holds-jobs";

/// The answer to a retirement of a tainted host.
pub(crate) const HOST_TAINTED: &str = "host-tainted";

/// The answer to a retirement of a cordoned host.
pub(crate) const HOST_CORDONED: &str = "host-cordoned";

/// The answer to a retirement of a host with a scale-down mark.
pub(crate) const HOST_MARKED: &str = "host-marked";

/// The answer for a host the registry does not hold.
pub(crate) const UNKNOWN_HOST: &str = "unknown-host";

/// The length of the cordon that `secs` asks for: [`DEFAULT_CORDON`] when it asks for none,
/// cut to [`MAX_CORDON`]; `None` for zero, which is refused.
pub(crate) fn length(secs: Option<u64>) -> Option<Duration> {
    cut(secs, MAX_CORDON)
}

/// The length of the operator's cordon that `secs` asks for: [`DEFAULT_CORDON`] when it asks
/// for none, cut to [`MAX_OPERATOR_CORDON`]; `None` for zero, which is refused.
pub(crate) fn operator_length(secs: Option<u64>) -> Option<Duration> {
    cut(secs, MAX_OPERATOR_CORDON)
}

/// The length `secs` asks for, [`DEFAULT_CORDON`] for none, cut to `max`; `None` for zero.
fn cut(secs: Option<u64>, max: Duration) -> Option<Duration> {
    match secs.unwrap_or(DEFAULT_CORDON.as_secs()) {
        0 => None,
        secs => Some(Duration::from_secs(secs).min(max)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts the length of each kind of request: none is the default, a length up to the
    /// cap is kept to the second, a longer one (also `u64::MAX`) is cut to the cap, and zero
    /// is refused. It catches a cap or a default that drifts, and a zero that becomes a
    /// cordon that has already lapsed.
    #[test]
    fn a_length_is_defaulted_cut_and_zero_refused() {
        assert_eq!(length(None), Some(Duration::from_secs(300)));
        assert_eq!(length(Some(1)), Some(Duration::from_secs(1)));
        assert_eq!(length(Some(3600)), Some(Duration::from_secs(3600)));
        assert_eq!(length(Some(3601)), Some(Duration::from_secs(3600)));
        assert_eq!(length(Some(u64::MAX)), Some(MAX_CORDON));
        assert_eq!(length(Some(0)), None);
    }

    /// Asserts the length of each kind of operator's request: none is the default, 48 hours
    /// is kept to the second, a length past seven days (also `u64::MAX`) is cut to seven days,
    /// and zero is refused. It catches the operator's cordon cut to the host route's hour, so
    /// that a host taken out for two days returns to placement after one hour.
    #[test]
    fn an_operators_length_reaches_seven_days() {
        assert_eq!(operator_length(None), Some(Duration::from_secs(300)));
        let two_days = Duration::from_secs(48 * 3600);
        assert_eq!(operator_length(Some(48 * 3600)), Some(two_days));
        let week = Duration::from_secs(7 * 24 * 3600);
        assert_eq!(operator_length(Some(7 * 24 * 3600 + 1)), Some(week));
        assert_eq!(operator_length(Some(u64::MAX)), Some(MAX_OPERATOR_CORDON));
        assert_eq!(operator_length(Some(0)), None);
    }
}

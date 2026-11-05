//! The rules of a cordon request that the host's own route (`POST /v1/hosts/{id}:cordon`)
//! and the operator's call (`CordonHost` of the job service) share: the length asked for
//! is cut to [`MAX_CORDON`], none asked for is [`DEFAULT_CORDON`], and zero is refused. The
//! registry does the rest (a repeat renews and never shortens), and the caller writes the
//! bindings before it answers.

use std::time::Duration;

/// The length of a cordon asked for without one.
pub(crate) const DEFAULT_CORDON: Duration = Duration::from_secs(5 * 60);

/// The longest a cordon is set for by one request; a longer one asked for is cut to it. A
/// requester that needs a cordon for longer renews it.
pub(crate) const MAX_CORDON: Duration = Duration::from_secs(60 * 60);

/// The refusal of a cordon of zero seconds (and, on the host's route, of a body that does
/// not read).
pub(crate) const INVALID_CORDON: &str = "invalid-cordon";

/// The answer when the cordon or uncordon holds on this server but the bindings that keep
/// it over a restart could not be written.
pub(crate) const CORDON_UNSAVED: &str = "cordon-unsaved";

/// The answer for a host the registry does not hold.
pub(crate) const UNKNOWN_HOST: &str = "unknown-host";

/// The length of the cordon that `secs` asks for: [`DEFAULT_CORDON`] when it asks for none,
/// cut to [`MAX_CORDON`]; `None` for zero, which is refused.
pub(crate) fn length(secs: Option<u64>) -> Option<Duration> {
    match secs.unwrap_or(DEFAULT_CORDON.as_secs()) {
        0 => None,
        secs => Some(Duration::from_secs(secs).min(MAX_CORDON)),
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
}

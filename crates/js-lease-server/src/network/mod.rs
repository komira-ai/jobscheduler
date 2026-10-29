//! The network layer: how a machine the service launches reaches the service.
//!
//! One layer for the whole service. A provider does not implement it and does not see
//! it: it receives the opaque [`js_elastic::BootPayload`] the renderer
//! ([`crate::bootstrap`]) makes from a plan, and never a mode, a step or a secret slot.
//! A [`NetworkMode`] contributes boot steps and a per-launch secret ([`prepare`] and
//! [`describe`]); [`check`] and [`forget`] are lifecycle hooks of the service and are not
//! part of any plan.
//!
//! - [`DefaultMode`] adds nothing: the host uses the network it already has.
//! - [`TailscaleMode`] joins the host to a tailnet with an auth key minted for that
//!   launch ([`TailscaleApi`] speaks to the Tailscale API with an [`OAuthClient`]).
//!
//! # Operator setup for the tailscale mode
//!
//! The OAuth client needs two scopes, both bound to the configured tag: `auth_keys`
//! (to mint a key for a launch) and `devices:core` (to list and delete the devices of
//! released hosts). The service deletes only a device whose hostname is the host's and
//! whose tags include that tag.
//!
//! [`prepare`]: NetworkMode::prepare
//! [`describe`]: NetworkMode::describe
//! [`check`]: NetworkMode::check
//! [`forget`]: NetworkMode::forget

mod default;
mod oauth;
mod tailscale;
mod tailscale_api;

use std::time::Duration;

use js_elastic::{BootStep, HostName, Secret, SlotName};

pub use default::DefaultMode;
pub use oauth::{OAUTH_FILE_LIMIT, OAuthClient};
pub use tailscale::{
    AUTH_KEY_PATH, AUTH_KEY_SLOT, MAX_KEY_VALID_FOR, TailscaleMode, TailscaleSettings,
};
pub use tailscale_api::{
    ANSWER_LIMIT, API_BASE, API_TIMEOUT, FORGET_ATTEMPTS, TOKEN_REFRESH_MARGIN, TailscaleApi,
};

/// What a mode is told about the launch it contributes to.
#[derive(Debug, Clone, Copy)]
pub struct LaunchCtx<'a> {
    /// The pool the machine is for.
    pub pool: &'a str,
    /// The machine's name.
    pub host: &'a HostName,
    /// How long a secret minted for this launch stays valid.
    pub valid_for: Duration,
}

/// What a mode adds to one launch's plan.
#[derive(Debug)]
pub struct Contribution {
    /// Boot steps, in order. They name secrets by slot, never hold them.
    pub steps: Vec<BootStep>,
    /// The secrets those steps name, by slot.
    pub secrets: Vec<(SlotName, Secret)>,
}

impl Contribution {
    /// A contribution of nothing.
    pub fn empty() -> Self {
        Self {
            steps: Vec::new(),
            secrets: Vec::new(),
        }
    }
}

/// How a call to a remote API failed. No variant holds the body of an answer, which can
/// echo a secret.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApiFailure {
    /// The request did not complete: it could not be made, timed out, or the answer was
    /// longer than [`ANSWER_LIMIT`].
    #[error("the request failed: {0}")]
    Transport(String),
    /// The server refused the request (status 400 to 499).
    #[error("refused with status {0}")]
    Rejected(u16),
    /// The server failed (status 500 to 599).
    #[error("server error, status {0}")]
    Unavailable(u16),
    /// The server answered a status that is neither a success nor an error class.
    #[error("unexpected status {0}")]
    Unexpected(u16),
    /// The answer was a success but did not say what it must.
    #[error("the answer is malformed")]
    Malformed,
}

impl ApiFailure {
    /// Whether the same call may succeed if made again.
    pub fn transient(&self) -> bool {
        matches!(self, Self::Transport(_) | Self::Unavailable(_))
    }
}

/// Why a network mode could not do what the service asked. The messages are fit for a
/// status page: none holds a secret.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NetworkError {
    /// The mode's settings are unusable.
    #[error("network: {0}")]
    Misconfigured(String),
    /// The OAuth client file is unusable.
    #[error("network: oauth client file: {0}")]
    ClientFile(String),
    /// The OAuth client could not be exchanged for an access token.
    #[error("network: credentials failed: {0}")]
    Credentials(ApiFailure),
    /// An auth key could not be minted.
    #[error("network: key mint failed: {0}")]
    KeyMint(ApiFailure),
    /// A device could not be deleted.
    #[error("network: device delete failed: {0}")]
    DeviceDelete(ApiFailure),
    /// The key lifetime asked for is zero or over the limit.
    #[error("network: a key may be valid for 1 to {max} seconds, not {asked:?}")]
    KeyLifetime {
        /// What was asked for.
        asked: Duration,
        /// The longest allowed, in seconds.
        max: u64,
    },
}

impl NetworkError {
    /// Whether making the same call again may succeed.
    pub fn transient(&self) -> bool {
        match self {
            Self::Credentials(failure) | Self::KeyMint(failure) | Self::DeviceDelete(failure) => {
                failure.transient()
            }
            Self::Misconfigured(_) | Self::ClientFile(_) | Self::KeyLifetime { .. } => false,
        }
    }
}

/// How machines launched for a pool reach the service.
pub trait NetworkMode: Send + Sync {
    /// The contribution to the plan of one launch, with its secret minted now. Never
    /// called in plan mode, where a dry run creates nothing.
    ///
    /// # Errors
    ///
    /// [`NetworkError`] when the secret cannot be had; the launch is then not made.
    fn prepare(&self, launch: &LaunchCtx<'_>) -> Result<Contribution, NetworkError>;

    /// The same steps as [`prepare`](Self::prepare) with their secret slots empty, for
    /// plan mode and the status page. Creates nothing.
    fn describe(&self, launch: &LaunchCtx<'_>) -> Vec<BootStep>;

    /// Whether the mode can issue what a launch needs. Creates nothing.
    ///
    /// # Errors
    ///
    /// [`NetworkError`] naming what is wrong.
    fn check(&self) -> Result<(), NetworkError>;

    /// Cleans up after `host` is released. Best effort with a bounded number of tries.
    ///
    /// # Errors
    ///
    /// [`NetworkError`] when the cleanup did not happen; the release goes on without it.
    fn forget(&self, host: &HostName) -> Result<(), NetworkError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that only a transport failure and a server error are worth repeating,
    /// through each error kind that wraps a failure, and that the messages name the
    /// step and the status.
    #[test]
    fn only_transport_and_server_failures_are_transient() {
        let transport = ApiFailure::Transport("timed out".to_owned());
        let cases = [
            (transport, true),
            (ApiFailure::Unavailable(503), true),
            (ApiFailure::Rejected(403), false),
            (ApiFailure::Unexpected(302), false),
            (ApiFailure::Malformed, false),
        ];
        for (failure, transient) in cases {
            assert_eq!(failure.transient(), transient, "{failure}");
            assert_eq!(
                NetworkError::Credentials(failure.clone()).transient(),
                transient
            );
            assert_eq!(
                NetworkError::KeyMint(failure.clone()).transient(),
                transient
            );
            assert_eq!(NetworkError::DeviceDelete(failure).transient(), transient);
        }
        for error in [
            NetworkError::Misconfigured("x".to_owned()),
            NetworkError::ClientFile("x".to_owned()),
            NetworkError::KeyLifetime {
                asked: Duration::ZERO,
                max: 1,
            },
        ] {
            assert!(!error.transient(), "{error}");
        }
        assert_eq!(
            NetworkError::KeyMint(ApiFailure::Rejected(403)).to_string(),
            "network: key mint failed: refused with status 403"
        );
        assert_eq!(
            NetworkError::Credentials(ApiFailure::Unexpected(302)).to_string(),
            "network: credentials failed: unexpected status 302"
        );
        assert_eq!(
            NetworkError::DeviceDelete(ApiFailure::Unavailable(503)).to_string(),
            "network: device delete failed: server error, status 503"
        );
        assert_eq!(
            NetworkError::DeviceDelete(ApiFailure::Malformed).to_string(),
            "network: device delete failed: the answer is malformed"
        );
        assert_eq!(
            NetworkError::Credentials(ApiFailure::Transport("x".to_owned())).to_string(),
            "network: credentials failed: the request failed: x"
        );
        assert_eq!(Contribution::empty().steps.len(), 0);
        assert_eq!(Contribution::empty().secrets.len(), 0);
    }
}

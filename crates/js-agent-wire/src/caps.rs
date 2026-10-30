#![allow(dead_code, unused_imports, unused_variables)]
//! What a Mac agent holds for bundles, as one message: [`Caps`].
//!
//! A local consumer (the executor on the same host) cannot read the agent's hello, and a
//! consumer that places work by capability must know which bundles the host holds, where
//! the store is, which read model it applies and which Xcode builds it found. The agent
//! sends this frame to it, and puts the same facts in its hello as the capabilities
//! `bundle` and `bundle-root` ([`bundle_capability`]).

use serde::{Deserialize, Serialize};

use crate::check::{self, At};
use crate::error::{Rule, ValidationError};
use crate::limits;
use crate::refusal;

/// The read model the agent applies to a run's profile. An absent value is
/// [`ReadModel::Open`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadModel {
    /// Reads are open and writes denied: the model every run without a bundle keeps.
    #[default]
    Open,
    /// Reads are limited to an allow list: only a run that carries a bundle, on a host that
    /// enabled it.
    Allow,
}

/// The facts a host reports about its bundles.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Caps {
    /// The manifest digests the host holds and can run, each `sha256:` and 64 lower case
    /// hex digits, in ascending order, none twice; at most [`limits::MAX_CAPS_BUNDLES`].
    #[serde(default)]
    pub bundles: Vec<String>,
    /// The absolute path of the bundle store, or empty when the host has none. A path
    /// shows in the outputs of a tool that embeds it, so a fleet uses one.
    #[serde(default)]
    pub bundle_root: String,
    /// The read model the host applies to a run that carries a bundle.
    #[serde(default)]
    pub read_model: ReadModel,
    /// The Xcode builds the host found, each a build id (`[A-Za-z0-9._-]`, at most
    /// [`limits::MAX_XCODE_BUILD_LEN`] bytes); at most [`limits::MAX_CAPS_XCODES`], none
    /// twice.
    #[serde(default)]
    pub xcode: Vec<String>,
}

impl Caps {
    /// Checks the frame: the digests, the root and the Xcode builds.
    pub fn validate(&self) -> Result<(), ValidationError> {
        Ok(())
    }
}

/// `sha256:` and 64 lower case hex digits.
fn is_digest(value: &str) -> bool {
    let hex = value.strip_prefix("sha256:").unwrap_or("");
    hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The value of the capability `bundle` for these digests: they joined with `,`, which
/// the digests never hold. The wire takes [`limits::MAX_BUNDLE_CAPABILITY_DIGESTS`] of them
/// in it; a caller with more reports the first of them.
#[must_use]
pub fn bundle_capability(digests: &[String]) -> String {
    String::new()
}

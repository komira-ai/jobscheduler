//! The settings and the view of the live curve mode, `--placement-curves on`
//! (`docs/design/placement-profiles-live.md`): which hosts book by curve
//! (`server.curve_hosts`), how long a job waits for its curve to fit before it is tested with
//! its flat number (`server.curve_max_wait_seconds`), and what `/metrics` reads of it.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js_types::HostId;

/// The wait bound a server sets unless its file names another: 30 s.
pub const DEFAULT_CURVE_MAX_WAIT: Duration = Duration::from_secs(30);

/// The longest wait bound a server accepts: 300 s.
pub const MAX_CURVE_MAX_WAIT_SECS: u64 = 300;

/// The hosts whose jobs are booked by curve under [`crate::sizes::Curves::On`]: those named,
/// or every host when none is named.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CurveHosts(BTreeSet<HostId>);

impl CurveHosts {
    /// The hosts `named` names; every host when it names none.
    #[must_use]
    pub fn new(named: BTreeSet<HostId>) -> Self {
        Self(named)
    }

    /// Whether `host` books by curve: it is named, or no host is.
    #[must_use]
    pub fn books(&self, host: &HostId) -> bool {
        self.0.is_empty() || self.0.contains(host)
    }
}

/// The settings of the live curve mode, read once at the server's start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurveSettings {
    /// The hosts that book by curve.
    pub hosts: CurveHosts,
    /// How long a job of a curve class waits with no host fitting its curve before it is
    /// tested with its flat number, from then on.
    pub max_wait: Duration,
}

impl Default for CurveSettings {
    /// Every host, and a wait bound of [`DEFAULT_CURVE_MAX_WAIT`].
    fn default() -> Self {
        Self {
            hosts: CurveHosts::default(),
            max_wait: DEFAULT_CURVE_MAX_WAIT,
        }
    }
}

/// What `/metrics` reads of the curve mode: how many classes book their curve and how many
/// book flat, and the fullest booked slot of each curve host at the last walk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CurveView {
    /// Classes passing all four checks, which book their curve.
    pub curve_classes: u64,
    /// Classes with a profile that fail a check, which book their flat number.
    pub flat_classes: u64,
    /// The fullest slot of each curve host's booked curve, in millicores, as the last walk
    /// found it.
    pub host_peaks: BTreeMap<HostId, u64>,
}

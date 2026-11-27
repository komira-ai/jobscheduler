//! The settings and the view of the live curve mode, `--placement-curves on`
//! (`docs/design/placement-profiles-live.md`): which hosts book by curve
//! (`server.curve_hosts`), how long a job waits for its curve to fit before it is tested with
//! its flat number (`server.curve_max_wait_seconds`), and what `/metrics` reads of it and of the
//! memory shadow, which runs under `shadow` and `on` and books nothing.

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

/// What `/metrics` reads of the curve mode: how many classes pass their checks and how many do
/// not, and the fullest booked slot of each curve host at the last walk. Under
/// [`crate::sizes::Curves::Shadow`] the classes are counted and no host has a peak, as nothing
/// is booked by curve; none is read under [`crate::sizes::Curves::Off`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CurveView {
    /// Classes passing all four checks, which book their curve under
    /// [`crate::sizes::Curves::On`].
    pub curve_classes: u64,
    /// Classes with a profile that fail a check, which book their flat number under
    /// [`crate::sizes::Curves::On`].
    pub flat_classes: u64,
    /// The fullest slot of each curve host's booked curve, in millicores, as the last walk
    /// found it; empty under [`crate::sizes::Curves::Shadow`].
    pub host_peaks: BTreeMap<HostId, u64>,
}

/// What `/metrics` reads of the memory shadow under `--placement-curves shadow` and `on`
/// (`docs/design/placement-profiles-live.md`, "Memory over time, in shadow"): how each placement
/// compared with what a booking that follows the classes' memory curves would have done, and
/// how each host's projected memory compared with what its readings then showed. Every count is
/// since the server started; nothing in it places a job.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryShadowView {
    /// Placements the memory curve would have made at the same walk: it fitted the host the flat
    /// booking chose, and fitted no host the flat memory booking refused at an earlier walk.
    pub same: u64,
    /// Placements the memory curve would have made sooner: at an earlier walk it fitted a host
    /// the flat memory booking refused, while the job was placed nowhere.
    pub sooner: u64,
    /// Placements the memory curve would not have made: its projection of the host the flat
    /// booking chose, with the job's curve added, was over the host's memory at some slot.
    pub later: u64,
    /// The sum, over the placements counted in [`MemoryShadowView::sooner`], of the time from
    /// the first walk the memory curve fitted a host to the placement, in milliseconds.
    pub sooner_ms: u64,
    /// Each host's projection and its comparisons with its readings.
    pub hosts: BTreeMap<HostId, HostMemoryShadow>,
}

/// One host's memory projection and how it compared with what the host then used.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostMemoryShadow {
    /// What the host's live jobs are projected to hold now, by their classes' memory curves
    /// (their flat memory booking where a class has none), in bytes, as the last walk found it.
    pub projected_bytes: u64,
    /// The memory the host's last reading showed in use, in bytes; `None` before its first.
    pub measured_bytes: Option<u64>,
    /// Readings compared with the projection the walk before them made of their moment.
    pub checks: u64,
    /// The comparisons in which the reading showed more than that projection.
    pub too_low: u64,
    /// The sum, over the comparisons counted in [`HostMemoryShadow::too_low`], of what the reading
    /// showed beyond the projection, in bytes.
    pub short_bytes: u64,
}

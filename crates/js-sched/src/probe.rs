//! A probe: a request pinned to one named worker, which the worker's cordon lets through
//! (`docs/design/placement.md`, 6.7).
//!
//! An operator who installs or updates a node's daemon proves it with one job on that
//! node while the node stays cordoned for ordinary work. The job names the node
//! ([`Request::pin`]). [`admit_probe`] is the rule that says whether it may run there, and
//! it is the only place the rule is written: the scheduler's round
//! ([`crate::Scheduler`]) asks it of every queued probe, over the workers it holds, and
//! the lease server's placer asks it of every probe lease, over the facts its registry
//! keeps of the host.
//!
//! The rule: the worker is known and live, serving, cordoned by an operator without a
//! drain, serves the probe's lease kind, satisfies its platform and is large enough for
//! it. Anything else is refused by name ([`ProbeRefusal`]), at once: none of these is a
//! wait for a cordon to end, which is what an ordinary job on a cordoned worker waits for.
//! Room is not part of the rule. A probe the rule admits is then placed as any request is,
//! on that one worker: it waits while the worker's room or its load predicate says no,
//! and is never placed on another worker meanwhile.

use std::fmt;

use js_caps::NodeCaps;
use js_types::sched::{LeaseKind, NodePhase, Resources, WorkerId};

use crate::cordon::Cordon;
use crate::input::Request;
use crate::servable::serves;

/// What the engine placing a probe knows of the worker the probe names.
#[derive(Clone, Copy, Debug)]
pub struct ProbeHost<'a> {
    /// The worker's cordon, if it has one.
    pub cordon: Option<&'a Cordon>,
    /// The worker's phase.
    pub phase: &'a NodePhase,
    /// What placement may book on the worker: its capacity after the
    /// memory_schedulable_percent cut.
    pub capacity: &'a Resources,
    /// What the worker offers.
    pub caps: &'a NodeCaps,
}

/// Why a probe may not run on the worker it names. Each is final for that request: the
/// probe is refused, not kept waiting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProbeRefusal {
    /// No live worker has the name.
    UnknownHost {
        /// The worker the probe names.
        host: WorkerId,
    },
    /// The worker is not serving: switching its session, quarantined, under memory
    /// pressure or draining by its own word.
    NotServing {
        /// The worker the probe names.
        host: WorkerId,
        /// Its phase.
        phase: NodePhase,
    },
    /// The worker is not cordoned: it takes ordinary work, and a probe is only for a
    /// worker kept from it.
    NotCordoned {
        /// The worker the probe names.
        host: WorkerId,
    },
    /// The worker is being drained (or was): it is being emptied, and a probe would be
    /// new work on it.
    Draining {
        /// The worker the probe names.
        host: WorkerId,
        /// Its cordon: [`Cordon::Draining`], [`Cordon::Drained`] or [`Cordon::Paused`].
        cordon: Cordon,
    },
    /// The worker reports no driver that serves the probe's lease kind.
    NoDriver {
        /// The worker the probe names.
        host: WorkerId,
        /// The lease kind asked for.
        kind: LeaseKind,
    },
    /// The worker does not satisfy the probe's platform.
    PlatformMismatch {
        /// The worker the probe names.
        host: WorkerId,
        /// The requirements it does not meet, as the platform match words them.
        lacks: Vec<String>,
    },
    /// The worker's whole capacity, after the memory_schedulable_percent cut, is smaller
    /// than the probe's request.
    TooLarge {
        /// The worker the probe names.
        host: WorkerId,
        /// What the probe asked for.
        asked: Resources,
    },
}

impl ProbeRefusal {
    /// The worker the refused probe names.
    #[must_use]
    pub const fn host(&self) -> &WorkerId {
        match self {
            Self::UnknownHost { host }
            | Self::NotServing { host, .. }
            | Self::NotCordoned { host }
            | Self::Draining { host, .. }
            | Self::NoDriver { host, .. }
            | Self::PlatformMismatch { host, .. }
            | Self::TooLarge { host, .. } => host,
        }
    }

    /// The refusal as a constant code, for an answer on the wire.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::UnknownHost { .. } => "probe-host-unknown",
            Self::NotServing { .. } => "probe-host-not-serving",
            Self::NotCordoned { .. } => "probe-host-not-cordoned",
            Self::Draining { .. } => "probe-host-draining",
            Self::NoDriver { .. } | Self::PlatformMismatch { .. } => "probe-host-mismatch",
            Self::TooLarge { .. } => "probe-host-too-small",
        }
    }
}

impl fmt::Display for ProbeRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let host = self.host();
        match self {
            Self::UnknownHost { .. } => write!(f, "probe host {host} is not a live worker"),
            Self::NotServing { phase, .. } => {
                write!(f, "probe host {host} is not serving ({})", phase.name())
            }
            Self::NotCordoned { .. } => write!(
                f,
                "probe host {host} is not cordoned: a probe runs only on a worker kept from \
                 ordinary work"
            ),
            Self::Draining { cordon, .. } => {
                let state = match cordon {
                    Cordon::Drained => "drained",
                    Cordon::Paused { .. } => "draining (paused)",
                    Cordon::Cordoned | Cordon::Draining { .. } => "draining",
                };
                write!(
                    f,
                    "probe host {host} is {state}: a probe runs only on a worker cordoned \
                     without a drain"
                )
            }
            Self::NoDriver { kind, .. } => write!(
                f,
                "probe host {host} serves no lease of kind {kind}: that needs a driver among {}",
                kind.drivers().join(", ")
            ),
            Self::PlatformMismatch { lacks, .. } => write!(
                f,
                "probe host {host} does not satisfy the probe's platform: it lacks {}",
                lacks.join(", ")
            ),
            Self::TooLarge { asked, .. } => write!(
                f,
                "probe host {host} is smaller than the probe's request ({} millicores, {} bytes \
                 of memory, {} GPU(s), {} VM slot(s), {} bytes of disk)",
                asked.cpu_millis, asked.memory_bytes, asked.gpus, asked.vms, asked.disk_bytes
            ),
        }
    }
}

/// Whether `request`, a probe naming `host`, may run there, given what the engine placing
/// it knows of the worker (`None`: no live worker of that name). The checks, in order:
/// known and live, serving, cordoned with a plain [`Cordon::Cordoned`], serving the lease
/// kind, satisfying the platform, and large enough ([`Request::demand`] within
/// `capacity`). The first that fails is the refusal.
///
/// It looks at no booking: a probe admitted here waits for room on the worker like any
/// request, and the load predicate and the room checks of placement apply to it
/// unchanged.
///
/// # Errors
/// The first check that fails, as a [`ProbeRefusal`].
pub fn admit_probe(
    host: &WorkerId,
    known: Option<ProbeHost<'_>>,
    request: &Request,
) -> Result<(), ProbeRefusal> {
    let host = host.clone();
    let Some(known) = known else {
        return Err(ProbeRefusal::UnknownHost { host });
    };
    if *known.phase != NodePhase::Serving {
        let phase = known.phase.clone();
        return Err(ProbeRefusal::NotServing { host, phase });
    }
    match known.cordon {
        None => return Err(ProbeRefusal::NotCordoned { host }),
        Some(Cordon::Cordoned) => {}
        Some(cordon) => {
            let cordon = cordon.clone();
            return Err(ProbeRefusal::Draining { host, cordon });
        }
    }
    if !serves(known.caps, request.kind) {
        let kind = request.kind;
        return Err(ProbeRefusal::NoDriver { host, kind });
    }
    if !request.needs.matches(known.caps) {
        let unmet = request.needs.unmet(known.caps);
        let lacks = unmet.iter().map(ToString::to_string).collect();
        return Err(ProbeRefusal::PlatformMismatch { host, lacks });
    }
    if !known.capacity.fits(&request.demand()) {
        let asked = request.resources;
        return Err(ProbeRefusal::TooLarge { host, asked });
    }
    Ok(())
}

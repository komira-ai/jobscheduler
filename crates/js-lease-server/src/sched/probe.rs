//! Probes on the lease server: what the registry knows of a probe's host, judged by the
//! probe rule of `js-sched` ([`js_sched::admit_probe`]), the one the scheduler's round
//! applies.
//!
//! The registry's cordon is the host's own (a lease that lapses), so a cordoned host here
//! is one whose cordon has not lapsed; the registry has no drain of that cordon, so a
//! probe is never refused here as draining. A host's taint, a scale-away and its phase
//! are the scheduler's phase: a tainted host is quarantined, a host being scaled away is
//! draining, and the agent's phases are the scheduler's of the same name.

use js_agent_wire::Phase;
use js_provider::{RefusalCode, StartSpec};
use js_registry::ProbeHost;
use js_sched::{Cordon, ProbeHost as SchedHost, ProbeRefusal, admit_probe};
use js_types::HostId;
use js_types::sched::{LeaseId as SchedLeaseId, NodePhase};

use tracing::info;

use super::map;

/// Whether the probe `spec` may be booked on `pin`, the host it names, by the probe rule
/// over `host`, what the registry knows of it (`None`: no live host of that name).
///
/// A probe whose request or host has no scheduler form (a capability that is not a
/// platform requirement, a shape that does not describe a node) cannot be judged, and is
/// refused [`RefusalCode::ProbeUnsupported`]: unlike an ordinary start, a probe never
/// falls back to the first host.
///
/// # Errors
/// The refusal's code. Each refusal is logged with its sentence.
pub(crate) fn admit(
    spec: &StartSpec,
    pin: &HostId,
    host: Option<&ProbeHost>,
) -> Result<(), RefusalCode> {
    let lease = spec.lease();
    let unsupported = |error: map::MapError| {
        info!(%lease, %error, "a probe has no scheduler form: refused");
        RefusalCode::ProbeUnsupported
    };
    let mut request = map::job(spec.request(), &lease.to_string()).map_err(unsupported)?;
    request.whole_worker = spec.class().admission().books_whole_host();
    let pin = map::worker(pin);
    let facts = host
        .map(|host| map::host(&host.shape).map(|node| (host, node)))
        .transpose()
        .map_err(unsupported)?;
    let cordoned = Cordon::Cordoned;
    let phase = host.map(phase_of);
    let known = facts
        .as_ref()
        .zip(phase.as_ref())
        .map(|((host, (capacity, caps)), phase)| SchedHost {
            cordon: host.cordoned.then_some(&cordoned),
            phase,
            capacity,
            caps,
        });
    admit_probe(&pin, known, &request).map_err(|refusal| {
        info!(%lease, %refusal, "probe refused");
        code_of(&refusal)
    })
}

/// The scheduler's phase of the host the registry describes as `host`: quarantined if it
/// is tainted, else draining if it is being scaled away, else its own phase. A preparing
/// or restoring host is named so with the null lease: the registry keeps which lease it
/// prepares for elsewhere, and the rule reads only that it is not serving.
fn phase_of(host: &ProbeHost) -> NodePhase {
    if let Some(reason) = &host.taint {
        return NodePhase::Quarantined(format!("tainted: {reason}"));
    }
    if host.leaving {
        return NodePhase::Draining;
    }
    match host.phase {
        Phase::Serving => NodePhase::Serving,
        Phase::Preparing => NodePhase::Preparing(SchedLeaseId::new(0, 0)),
        Phase::Restoring => NodePhase::Restoring(SchedLeaseId::new(0, 0)),
        Phase::MemoryPressure => NodePhase::MemoryPressure,
        Phase::Draining => NodePhase::Draining,
        Phase::Quarantined => NodePhase::Quarantined("quarantined".to_owned()),
        Phase::Tainted => NodePhase::Quarantined("tainted".to_owned()),
    }
}

/// The refusal code the lease API answers for `refusal`: the one its
/// [`ProbeRefusal::code`] names.
fn code_of(refusal: &ProbeRefusal) -> RefusalCode {
    match refusal {
        ProbeRefusal::UnknownHost { .. } => RefusalCode::ProbeHostUnknown,
        ProbeRefusal::NotServing { .. } => RefusalCode::ProbeHostNotServing,
        ProbeRefusal::NotCordoned { .. } => RefusalCode::ProbeHostNotCordoned,
        ProbeRefusal::Draining { .. } => RefusalCode::ProbeHostDraining,
        ProbeRefusal::NoDriver { .. } | ProbeRefusal::PlatformMismatch { .. } => {
            RefusalCode::ProbeHostMismatch
        }
        ProbeRefusal::TooLarge { .. } => RefusalCode::ProbeHostTooSmall,
    }
}

#[cfg(test)]
mod tests {
    use js_types::sched::{LeaseKind, Resources, WorkerId};

    use super::*;

    /// Catches: a refusal answered by a code other than the one the rule names for it,
    /// which would tell a client a different reason than the scheduler's.
    #[test]
    fn every_refusal_is_answered_the_code_the_rule_names() {
        let host = || WorkerId::new("a");
        let refusals = [
            ProbeRefusal::UnknownHost { host: host() },
            ProbeRefusal::NotServing {
                host: host(),
                phase: NodePhase::Draining,
            },
            ProbeRefusal::NotCordoned { host: host() },
            ProbeRefusal::Draining {
                host: host(),
                cordon: Cordon::Drained,
            },
            ProbeRefusal::NoDriver {
                host: host(),
                kind: LeaseKind::Vm,
            },
            ProbeRefusal::PlatformMismatch {
                host: host(),
                lacks: Vec::new(),
            },
            ProbeRefusal::TooLarge {
                host: host(),
                asked: Resources::default(),
            },
        ];
        for refusal in refusals {
            assert_eq!(code_of(&refusal).as_str(), refusal.code(), "{refusal:?}");
        }
    }

    /// Catches: a host's taint or scale-away read as serving, or an agent phase mapped to
    /// another, so that the probe rule judges a host that is not serving as one that is.
    #[test]
    fn a_hosts_taint_scale_away_and_phase_are_the_schedulers_phase() {
        let host = |phase| ProbeHost {
            shape: js_types::Shape::new(
                js_types::Resources::new(1000, 1, 0, None).unwrap(),
                Default::default(),
            ),
            cordoned: true,
            phase,
            taint: None,
            leaving: false,
        };
        let null = SchedLeaseId::new(0, 0);
        let cases = [
            (Phase::Serving, NodePhase::Serving),
            (Phase::Preparing, NodePhase::Preparing(null)),
            (Phase::Restoring, NodePhase::Restoring(null)),
            (Phase::MemoryPressure, NodePhase::MemoryPressure),
            (Phase::Draining, NodePhase::Draining),
            (
                Phase::Quarantined,
                NodePhase::Quarantined("quarantined".to_owned()),
            ),
            (Phase::Tainted, NodePhase::Quarantined("tainted".to_owned())),
        ];
        for (agent, sched) in cases {
            assert_eq!(phase_of(&host(agent)), sched, "{agent:?}");
        }
        let leaving = ProbeHost {
            leaving: true,
            ..host(Phase::Serving)
        };
        assert_eq!(phase_of(&leaving), NodePhase::Draining);
        let tainted = ProbeHost {
            taint: Some("residue".to_owned()),
            ..leaving
        };
        assert_eq!(
            phase_of(&tainted),
            NodePhase::Quarantined("tainted: residue".to_owned())
        );
    }
}

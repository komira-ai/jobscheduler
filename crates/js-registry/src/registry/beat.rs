//! What a heartbeat is checked against, and what taint it records, shared by
//! [`Registry::check_heartbeat`], which changes nothing, and [`Registry::heartbeat`],
//! which records the beat. Both run the same checks in the same order and compute the
//! taint from the same [`Plan`], so the two cannot answer differently for one state.

use std::collections::BTreeMap;

use js_agent_wire::refusal as refusal_code;
use js_agent_wire::{Heartbeat, Outcome, Taint};
use js_types::{HostId, LeaseId};

use crate::credential::Credential;
use crate::refusal::Refusal;
use crate::whole::ending::prepare_taint;
use crate::whole::exclusive::Exclusive;
use crate::whole::reasons::{Cause, HostEffect};

use super::{Record, Registry};

/// What [`Registry::check_heartbeat`] found: the heartbeat would be admitted, from
/// `host`, and would record `taint`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeartbeatCheck {
    /// The host the heartbeat is from, as its admitted session names it.
    pub host: HostId,
    /// The taint the beat would record on a host that has none: the taint the heartbeat
    /// reports; else, when a start it reports was refused as `host-tainted`, a taint
    /// naming that run; else, when it refuses the whole-node lease committed to the host
    /// for a reason that taints the host (`host-unsafe`), a `host-unsafe` taint. `None`
    /// when the beat records none: it carries none of these, or the host is tainted
    /// already, since the first taint is kept. After the beat,
    /// [`crate::HostView::taint`] and the binding ([`Registry::bindings`]) carry this
    /// taint.
    pub taint: Option<Taint>,
}

/// What a beat does to one record's taint and whole-node state, computed from the
/// record without changing it.
pub(super) struct Plan<'a> {
    /// The host's taint once the beat's own report, or a start it reports refused as
    /// `host-tainted`, is counted: the host's first taint is kept.
    pub(super) taint: Option<Taint>,
    /// The whole-node state after the beat and the ready report it carries.
    pub(super) exclusive: Exclusive,
    /// The heartbeat's typed refusal of the lease committed to the host, when it names
    /// that lease: the lease, the cause and the agent's detail.
    pub(super) refused: Option<(LeaseId, Cause, &'a str)>,
}

impl<'a> Plan<'a> {
    pub(super) fn of(record: &Record, heartbeat: &'a Heartbeat) -> Self {
        // An agent that refuses a start because its host is tainted may be unable to write
        // its own marker (a full disk), so that refusal taints the host here as a report
        // would. It is the host's fault: the result is answered like any other. A start
        // refused as `quota-unavailable` is the host's fault too, and taints nothing.
        let refused_start = heartbeat.results.iter().find(|result| {
            let by_host = result.detail.as_deref() == Some(refusal_code::HOST_TAINTED);
            by_host && result.outcome == Outcome::FailedToStart
        });
        let reported = heartbeat.taint.clone().or_else(|| {
            refused_start.map(|result| Taint {
                reason: refusal_code::HOST_TAINTED.to_string(),
                run_id: Some(result.run_id.clone()),
                check: None,
            })
        });
        let taint = record.taint.clone().or(reported);
        let clean = taint.is_none();
        let exclusive = record.exclusive.after_beat();
        let exclusive = match &heartbeat.exclusive_ready {
            Some(ready) => exclusive.report_ready(ready, clean),
            None => exclusive,
        };
        // A typed refusal of the lease committed to the host ends it at once; one for any
        // other lease says nothing about this host's preparation.
        let refused = heartbeat.exclusive_refused.as_ref().and_then(|refused| {
            let lease = exclusive.committed()?;
            let named = lease.to_string() == refused.lease_id;
            named.then_some((lease, Cause::Refused(refused.code), refused.detail.as_str()))
        });
        Self {
            taint,
            exclusive,
            refused,
        }
    }

    /// The taint the host holds once the beat is recorded: the refused prepare adds
    /// the `host-unsafe` taint (as the failed prepare's ending does) when its reason
    /// taints the host and nothing else did.
    fn taint_after(&self) -> Option<Taint> {
        let taints = self
            .refused
            .is_some_and(|(_, cause, _)| cause.row().host == HostEffect::Taint);
        self.taint.clone().or(taints.then(prepare_taint))
    }
}

/// The host id of `heartbeat`, once the message keeps the wire rules: the checks that
/// need no state.
pub(super) fn parse(heartbeat: &Heartbeat) -> Result<HostId, Refusal> {
    let host = HostId::new(&heartbeat.host_id).map_err(Refusal::BadHeartbeatHostId)?;
    heartbeat.validate().map_err(Refusal::Invalid)?;
    Ok(host)
}

/// The record of `host` when `heartbeat` is admitted for it, checked in this order: the
/// host has a session, `credential` is the one its id is bound to, the epoch is the
/// current one, and the heartbeat carries the session secret. Changes nothing; the
/// borrow is mutable so that [`Registry::heartbeat`] records the beat through it.
pub(super) fn admitted<'r>(
    records: &'r mut BTreeMap<HostId, Record>,
    host: &HostId,
    heartbeat: &Heartbeat,
    credential: &Credential,
) -> Result<&'r mut Record, Refusal> {
    let record = records
        .get_mut(host)
        .filter(|record| record.heard.is_some())
        .ok_or(Refusal::UnknownHost)?;
    if record
        .binding
        .as_ref()
        .is_some_and(|bound| bound.credential() != credential)
    {
        return Err(Refusal::HostBound);
    }
    if record.epoch != heartbeat.epoch {
        return Err(Refusal::StaleEpoch);
    }
    let issued = record
        .session
        .as_ref()
        .is_some_and(|digest| digest.matches(&heartbeat.session));
    if !issued {
        return Err(Refusal::SessionMismatch);
    }
    Ok(record)
}

impl Registry {
    /// Answers what [`Registry::heartbeat`] would do with `heartbeat` from `credential`,
    /// and records nothing: refused with the same [`Refusal`] for the same reason (the
    /// same checks, run by the same code in the same order), or admitted, with the
    /// taint the beat would record ([`HeartbeatCheck::taint`]).
    ///
    /// This does not mutate: the host's last-heard time, phase, epoch, session, held runs,
    /// load, taint and whole-node state, its booking and its binding stay as they were, and
    /// no run is touched. Like every call it first applies the registry's time-driven
    /// housekeeping (forgetting the hosts lost for longer than the retention with no
    /// booking, expiring waits, sweeping idle requests), which any call would apply at the
    /// same time, so it refuses a forgotten host as [`Registry::heartbeat`] does.
    ///
    /// It lets a caller make a taint durable before the registry records it: check, commit
    /// the taint, then call [`Registry::heartbeat`]. The answer holds for the registry as
    /// it is at the call; a call in between (a hello that replaces the session, a lease
    /// that ends) can change what the heartbeat then does, and the heartbeat answers for
    /// the registry as it is then.
    pub fn check_heartbeat(
        &self,
        heartbeat: &Heartbeat,
        credential: &Credential,
    ) -> Result<HeartbeatCheck, Refusal> {
        let host = parse(heartbeat)?;
        let now = self.shared.clock.now();
        let mut state = self.lock_at(now);
        let record = admitted(&mut state.records, &host, heartbeat, credential)?;
        let taint = match record.taint {
            Some(_) => None,
            None => Plan::of(record, heartbeat).taint_after(),
        };
        Ok(HeartbeatCheck { host, taint })
    }
}

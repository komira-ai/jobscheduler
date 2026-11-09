//! What a heartbeat is checked against, what taint it records and how it is answered,
//! shared by [`Registry::check_heartbeat`], which changes nothing, and
//! [`Registry::heartbeat`], which records the beat. Both run the same checks in the same
//! order and compute the taint by the same [`taint_after`], so the two cannot answer
//! differently for one state.

use std::ops::Deref;
use std::sync::PoisonError;

use js_agent_wire::refusal as refusal_code;
use js_agent_wire::{Ack, Command, Heartbeat, HeartbeatReply, Outcome, ResultAck, Taint, limits};
use js_provider::HostBook;
use js_types::HostId;
use js_types::Timestamp;

use crate::config::PoolConfig;
use crate::credential::Credential;
use crate::refusal::Refusal;

use super::{Record, Registry};

/// What [`Registry::check_heartbeat`] found: the heartbeat would be admitted, from
/// `host`, and would record `taint`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeartbeatCheck {
    /// The host the heartbeat is from, as its admitted session names it.
    pub host: HostId,
    /// The taint the beat would record on a host that has none: the taint the heartbeat
    /// reports; else, when a start it reports was refused as `host-tainted`, a taint
    /// naming that run. `None` when the beat records none: it carries none of these, or the
    /// host is tainted already, since the first taint is kept. After the beat,
    /// [`crate::HostView::taint`] and the binding ([`Registry::bindings`]) carry this
    /// taint.
    pub taint: Option<Taint>,
}

/// The taint the host holds once the beat is recorded: the host's own, else the one the
/// beat reports, else (the first taint is kept) one that names a start the agent refused
/// as `host-tainted`. An agent that refuses a start because its host is tainted may be
/// unable to write its own marker (a full disk), so that refusal taints the host here as a
/// report would. A start refused as `quota-unavailable` is the host's fault too, and
/// taints nothing.
pub(super) fn taint_after(record: &Record, heartbeat: &Heartbeat) -> Option<Taint> {
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
    record.taint.clone().or(reported)
}

/// The reply to the heartbeat of an agent that keys its work by run, which this registry no
/// longer queues: a cancel for each run the heartbeat lists as held, at most
/// [`limits::MAX_COMMANDS`] of them, and an `UnknownLease` acknowledgement for each result
/// it carries, so the agent kills what it holds and drops what it reports. The taint
/// acknowledgement is that of the heartbeat; the rest of the reply is left empty, and the
/// transport adds the job half.
pub(super) fn answer(heartbeat: &Heartbeat) -> HeartbeatReply {
    let cancels = heartbeat.held.iter().map(|run| Command::Cancel {
        run_id: run.run_id.clone(),
    });
    let acks = heartbeat.results.iter().map(|result| ResultAck {
        run_id: result.run_id.clone(),
        ack: Ack::UnknownLease,
    });
    HeartbeatReply {
        commands: cancels.take(limits::MAX_COMMANDS).collect(),
        result_acks: acks.collect(),
        taint_ack: heartbeat.taint.is_some(),
        exclusive: None,
        job_acks: Vec::new(),
        retired_acks: Vec::new(),
        log_grant: None,
    }
}

/// The host id of `heartbeat`, once the message keeps the wire rules: the checks that
/// need no state.
pub(super) fn parse(heartbeat: &Heartbeat) -> Result<HostId, Refusal> {
    let host = HostId::new(&heartbeat.host_id).map_err(Refusal::BadHeartbeatHostId)?;
    heartbeat.validate().map_err(Refusal::Invalid)?;
    Ok(host)
}

/// `record`, the host's record if the registry holds one, when `heartbeat` is admitted
/// for it, checked in this order: the host has a session, `credential` is the one its id
/// is bound to, the epoch is the current one, and the heartbeat carries the session
/// secret. Changes nothing; [`Registry::heartbeat`] passes a mutable borrow and records
/// the beat through the one returned, [`Registry::check_heartbeat`] a shared one.
pub(super) fn admitted<R: Deref<Target = Record>>(
    record: Option<R>,
    heartbeat: &Heartbeat,
    credential: &Credential,
) -> Result<R, Refusal> {
    let record = record
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

/// Whether the housekeeping every locking call runs first ([`super::State::forget_lost`])
/// would forget `host` at `now`: its record is out of retention, and the host book lists
/// it. Reads only, so a check can answer as if the housekeeping had run.
fn forgotten(
    book: &HostBook,
    (host, record): (&HostId, &Record),
    now: Timestamp,
    config: &PoolConfig,
) -> bool {
    let silent = record.out_of_retention(now, config.grace_ms(), config.lost_retention_ms());
    [silent, book.contains(host)].into_iter().all(|ok| ok)
}

impl Registry {
    /// Answers what [`Registry::heartbeat`] would do with `heartbeat` from `credential`,
    /// and records nothing: refused with the same [`Refusal`] for the same reason (the
    /// same checks, run by the same code in the same order), or admitted, with the
    /// taint the beat would record ([`HeartbeatCheck::taint`]).
    ///
    /// This does not mutate: the host's last-heard time, phase, epoch, session, held runs,
    /// load, taint and its binding stay as they were. Unlike every other call it does not
    /// run the registry's time-driven housekeeping (forgetting hosts lost for longer than the
    /// retention that their placer holds no work on) either: it answers as if that had run,
    /// so a host the housekeeping would forget is refused as unknown, as
    /// [`Registry::heartbeat`] refuses it after forgetting it.
    ///
    /// It lets a caller make a taint durable before the registry records it: check, commit
    /// the taint, then call [`Registry::heartbeat`]. The answer holds for the registry as
    /// it is at the call; a call in between (a hello that replaces the session) can change
    /// what the heartbeat then does, and the heartbeat answers for the registry as it is
    /// then.
    pub fn check_heartbeat(
        &self,
        heartbeat: &Heartbeat,
        credential: &Credential,
    ) -> Result<HeartbeatCheck, Refusal> {
        let host = parse(heartbeat)?;
        let now = self.shared.clock.now();
        let state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let config = &self.shared.config;
        // As the housekeeping, a host its placer holds work on is kept however long silent.
        let placer = self.placer();
        let held = |host: &HostId| placer.holds_work_on(host);
        let kept = state
            .records
            .get(&host)
            .filter(|record| !forgotten(&state.book, (&host, *record), now, config) || held(&host));
        let record = admitted(kept, heartbeat, credential)?;
        let taint = match record.taint {
            Some(_) => None,
            None => taint_after(record, heartbeat),
        };
        Ok(HeartbeatCheck { host, taint })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::Duration;

    use js_agent_wire::{DEFAULT_GRACE_MS, Hello, Phase, ProtocolRange, WireShape};
    use js_types::ManualClock;
    use js_types::{Admission, ClassId, ProviderId, ResourceClass, Resources, Shape};

    use super::*;
    use crate::placer::Plain;
    use crate::{MIN_LOST_RETENTION_MS, SequentialSessions};

    fn macos() -> BTreeMap<String, String> {
        BTreeMap::from([("os".to_string(), "macos".to_string())])
    }

    /// A pool of one class of 4 cores and 8 GiB, forgetting a lost host after the
    /// shortest retention.
    fn config() -> PoolConfig {
        let class = ResourceClass::new(ClassId::new("mac-host").unwrap(), Admission::BookAll, 0);
        let resources = Resources::new(4000, 8 << 30, 0, None).unwrap();
        let name = ProviderId::new("builders").unwrap();
        PoolConfig::new(name, class, Shape::new(resources, macos()), 2)
            .with_lost_retention(MIN_LOST_RETENTION_MS)
            .unwrap()
    }

    fn hello() -> Hello {
        Hello {
            protocol: ProtocolRange { min: 1, max: 1 },
            host_id: "a".into(),
            pool: "builders".into(),
            agent_version: "0.1.0".into(),
            instance: None,
            shape: WireShape {
                cpu_millis: 8000,
                memory_bytes: 16 << 30,
                gpus: 0,
                disk_bytes: 0,
                gpu_class: None,
                max_leases: None,
                accelerator: None,
            },
            capabilities: macos(),
            gpu_devices: Vec::new(),
            labels: BTreeMap::new(),
            phase: Phase::Serving,
            held: Vec::new(),
            accepts_host_credential: false,
            identity: None,
        }
    }

    fn heartbeat(session: String) -> Heartbeat {
        Heartbeat {
            host_id: "a".into(),
            epoch: 1,
            session,
            phase: Phase::Serving,
            held: Vec::new(),
            results: Vec::new(),
            taint: None,
            exclusive_ready: None,
            exclusive_refused: None,
            load: None,
            disk_gib: None,
            jobs: Vec::new(),
            retired_jobs: Vec::new(),
            log_grant_state: None,
        }
    }

    /// The raw state a check must leave as it is: the host records and whether the host book
    /// lists the host. Read without the housekeeping every other call runs first.
    fn raw(registry: &Registry, host: &HostId) -> (Vec<HostId>, bool) {
        let state = registry.shared.state.lock().unwrap();
        let records = state.records.keys().cloned().collect();
        (records, state.book.contains(host))
    }

    /// Asserts that a check with housekeeping due changes the raw state not at all: a host
    /// lost past the retention with no booking is refused as unknown and is still held,
    /// record and book entry, after the check; the heartbeat that follows refuses it the
    /// same and forgets it, as it did before. It catches a check that runs the housekeeping
    /// of the other calls on the live state.
    #[test]
    fn a_check_with_housekeeping_due_leaves_the_raw_state_as_it_was() {
        let clock = ManualClock::new(Timestamp::from_millis(1_000_000));
        let sessions = SequentialSessions::new();
        let registry = Registry::with_sessions(config(), clock.clone(), sessions, Arc::new(Plain));
        let credential = Credential::of_token("t");
        let welcome = registry.hello(&hello(), &credential).unwrap();
        let beat = heartbeat(welcome.session);
        let host = HostId::new("a").unwrap();
        let lost = Duration::from_millis(DEFAULT_GRACE_MS + MIN_LOST_RETENTION_MS + 1);
        clock.advance(lost);
        let before = raw(&registry, &host);
        assert_eq!(before, (vec![host.clone()], true));

        let checked = registry.check_heartbeat(&beat, &credential).unwrap_err();
        assert_eq!(checked, Refusal::UnknownHost);
        assert_eq!(raw(&registry, &host), before);

        let beaten = registry.heartbeat(&beat, &credential).unwrap_err();
        assert_eq!(beaten, Refusal::UnknownHost);
        assert_eq!(raw(&registry, &host), (Vec::new(), false));
    }
}

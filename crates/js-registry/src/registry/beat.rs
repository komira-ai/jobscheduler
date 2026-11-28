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
use crate::whole::requests::prepare_taint;

use super::{Record, Registry};

/// The reason the lease server gives the taint of a host job's placement (the constant
/// `HOST_JOB_TAINT` of the jobs crate). The server makes that taint itself, so the agent's
/// marker does not hold it and the agent's report that it holds none cannot end it.
const HOST_JOB: &str = "host-job";

/// What [`Registry::check_heartbeat`] found: the heartbeat would be admitted, from
/// `host`, and would record `taint`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeartbeatCheck {
    /// The host the heartbeat is from, as its admitted session names it.
    pub host: HostId,
    /// The taint the beat would record on a host that has none, or has none left after
    /// the beat clears it ([`HeartbeatCheck::cleared`]): the taint the heartbeat
    /// reports; else, when a start it reports was refused as `host-tainted`, a taint
    /// naming that run; else, when the beat says the agent is tainted (`tainted`) and
    /// reports no clear, the `host-tainted` mirror of the agent's own taint, which names no
    /// run. `None` when the beat records none: it carries none of these, or the
    /// host is tainted already, since the first taint is kept. After the beat,
    /// [`crate::HostView::taint`] and the binding ([`Registry::bindings`]) carry this
    /// taint.
    pub taint: Option<Taint>,
    /// The beat ends the host's taint: it reports the taint cleared (`taint_cleared` is the
    /// taint the host holds), or it says the agent holds none (`tainted` is false) while
    /// the host holds a taint the agent made, and the beat is not a late one ([`Heartbeat::seq`]
    /// no newer than the newest taken in the session). The host holds [`HeartbeatCheck::taint`]
    /// after it, which is none unless the same beat reports a new taint. A caller that makes the taint durable before the
    /// registry records it commits the binding when this is set, as when `taint` is.
    pub cleared: bool,
}

/// Whether the taint was made by the agent, whose marker is then the truth of it: every
/// taint but the two the server makes itself, a host job's placement (`host-job`) and a
/// refused whole-host prepare (`host-unsafe`). The agent's own report that it holds none
/// does not speak for those two.
pub(super) fn agent_made(taint: &Taint) -> bool {
    let server_made = [HOST_JOB.to_string(), prepare_taint().reason];
    !server_made.contains(&taint.reason)
}

/// Whether the registry already took a beat of this session whose sequence number is at least
/// as new as `heartbeat`'s, so `heartbeat` is a late arrival: the agent's word of its taint
/// (`tainted`) in it is older than the word the registry holds, and is not acted on. A beat
/// without a sequence number (an agent that predates it) is never overtaken, and one taken
/// when the session had no sequence number yet overtakes nothing.
fn overtaken(record: &Record, heartbeat: &Heartbeat) -> bool {
    matches!((heartbeat.seq, record.last_seq), (Some(seq), Some(last)) if seq <= last)
}

/// Whether `heartbeat` ends the taint `record` holds, in one of two ways. The agent says it
/// holds none (`tainted` is false) and the taint held is one the agent made, which its
/// marker is the truth of, so a taint the server kept from before the agent's restart or
/// the agent's own clear ends at the agent's first beat; a beat without the field (an agent
/// that predates it) says nothing. Or the beat reports the taint held as cleared: the taint it
/// names is the one held, field for field; or the one held is the taint a refused start made
/// (reason `host-tainted`), which only mirrors the agent's own taint and which the agent,
/// clean again, never reports, so its report of any cleared taint ends it. A host whose held
/// taint is a host job's or a refused whole-host prepare's stays tainted for the agent's
/// word; only the operator's [`Registry::clear_taint`] ends those. A beat that is overtaken
/// ([`overtaken`]) says nothing of the agent's taint by `tainted`: its report of a cleared taint
/// still counts, the agent sends it until acknowledged.
pub(super) fn clears(record: &Record, heartbeat: &Heartbeat) -> bool {
    let Some(held) = &record.taint else {
        return false;
    };
    let says_none = heartbeat.tainted == Some(false) && !overtaken(record, heartbeat);
    let holds_none = says_none && agent_made(held);
    let reported = heartbeat
        .taint_cleared
        .as_ref()
        .is_some_and(|cleared| held == cleared || held.reason == refusal_code::HOST_TAINTED);
    holds_none || reported
}

/// The taint the host holds once the beat is recorded: the host's own unless the beat clears
/// it, else the one the beat reports, else (the first taint is kept) one that names a start
/// the agent refused as `host-tainted`. An agent that refuses a start because its host is
/// tainted may be unable to write its own marker (a full disk), so that refusal taints the
/// host here as a report would. A start refused as `quota-unavailable` is the host's fault
/// too, and taints nothing. A beat that reports a taint cleared is the agent saying the
/// host is clean again, so the refusals it still carries (results not yet acknowledged,
/// from before the clear) taint nothing; a taint the agent makes again after the clear is
/// reported in `taint`. When none of these gives a taint and the beat says the agent holds
/// one (`tainted` is true) with no clear to report, the host is given the `host-tainted`
/// mirror of it, naming no run: the agent is tainted and the server holds none (the
/// operator cleared the server's taint while the marker stayed), and the agent's next clean
/// beat then ends it. The first taint is kept, so a host that holds a taint after the
/// clearing keeps it. A beat that is overtaken ([`overtaken`]) gives no mirror, as it ends no
/// taint by `tainted`.
pub(super) fn taint_after(record: &Record, heartbeat: &Heartbeat) -> Option<Taint> {
    let refused_start = heartbeat.results.iter().find(|result| {
        let by_host = result.detail.as_deref() == Some(refusal_code::HOST_TAINTED);
        by_host && result.outcome == Outcome::FailedToStart
    });
    let refused_start = refused_start.filter(|_| heartbeat.taint_cleared.is_none());
    let reported = heartbeat.taint.clone().or_else(|| {
        refused_start.map(|result| Taint {
            reason: refusal_code::HOST_TAINTED.to_string(),
            run_id: Some(result.run_id.clone()),
            check: None,
        })
    });
    let says_tainted = heartbeat.tainted == Some(true) && !overtaken(record, heartbeat);
    let mirror = (says_tainted && heartbeat.taint_cleared.is_none()).then(|| Taint {
        reason: refusal_code::HOST_TAINTED.to_string(),
        run_id: None,
        check: None,
    });
    let held = record.taint.clone().filter(|_| !clears(record, heartbeat));
    held.or(reported).or(mirror)
}

/// The reply to the heartbeat of an agent that keys its work by run, which this registry no
/// longer queues: a cancel for each run the heartbeat lists as held, at most
/// [`limits::MAX_COMMANDS`] of them, and an `UnknownLease` acknowledgement for each result
/// it carries, so the agent kills what it holds and drops what it reports. The taint
/// acknowledgement is that of the heartbeat; the rest of the reply is left empty, and the
/// transport adds the job half. `held` is the taint the host holds once the beat is
/// recorded, which the reply names as `host_taint`, none when the host holds none.
pub(super) fn answer(heartbeat: &Heartbeat, held: Option<Taint>) -> HeartbeatReply {
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
        taint_cleared_ack: heartbeat.taint_cleared.is_some(),
        host_taint: held,
        exclusive: None,
        job_acks: Vec::new(),
        retired_acks: Vec::new(),
        unknown_jobs: Vec::new(),
        log_grant: None,
        beat_now: false,
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
        // As the housekeeping, a host its placer holds work on is kept however long silent.
        let placer = self.placer();
        let held = |host: &HostId| placer.holds_work_on(host);
        let kept = state.records.get(&host).filter(|record| {
            let config = self.pool_of(record);
            !forgotten(&state.book, (&host, *record), now, config) || held(&host)
        });
        let record = admitted(kept, heartbeat, credential)?;
        let cleared = clears(record, heartbeat);
        let taint = match (record.taint.is_some(), cleared) {
            (true, false) => None,
            _ => taint_after(record, heartbeat),
        };
        Ok(HeartbeatCheck {
            host,
            taint,
            cleared,
        })
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
            agent_process: None,
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
            identity: None,
        }
    }

    fn heartbeat(session: String) -> Heartbeat {
        Heartbeat {
            seq: None,
            host_id: "a".into(),
            epoch: 1,
            session,
            phase: Phase::Serving,
            held: Vec::new(),
            results: Vec::new(),
            taint: None,
            taint_cleared: None,
            tainted: None,
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

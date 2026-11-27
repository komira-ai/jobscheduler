//! The hold of a heartbeat's reply ([`crate::held_beat`]): a reply with nothing to send to a
//! serving host that reported `held-beat` is held open until a placement on the host wakes it, or for
//! [`Agents::hold_bound`] at most, and says `beat_now`, so the agent sends its next heartbeat
//! at once. A beat that lists a job as `starting` is held for [`Agents::starting_hold`] at
//! most instead, so that the start's `running`, which rides on the next heartbeat, is not
//! kept a whole hold. A host that polls dispatch is held differently, since its starts come by
//! the poll and never in this reply ([`hold`]): a placement ends its hold, and a beat of it that
//! lists a job `starting` is paced rather than held, both answered without `beat_now`.
//!
//! A reply is held only when it carries nothing at all: no command, no acknowledgement, no
//! taint acknowledgement and no log credential, so a hold never delays what the beat asked
//! for. The taint the host holds ([`HeartbeatReply::host_taint`]) is information for the
//! agent's health route, not something it acts on, so a reply that names it and nothing else
//! is held, and a tainted host's replies are held as a clean host's are. A reply that is not held says `beat_now` too when it carries no command, so the
//! heartbeat after an acknowledgement is held at once rather than an interval later. Everything the beat reported was committed and counted before the hold: the listing,
//! the taint, the usage. When a wake comes, the stops and starts are built again from the
//! store over the listing the beat brought ([`per_job::commands_now`]); a reply that now has
//! a command, or names another whole-host allocation, is answered at once, and one that is
//! still empty (another server's change, a placement this one undid) is held on, except for a
//! host that polls dispatch, whose hold the wake ends. A read that
//! fails answers the reply as it was, and the agent's next heartbeat, sent at once, is served
//! in full.
//!
//! A server that drains ([`crate::drain`]) ends every hold at once and begins none: the reply
//! goes out as it is, saying `beat_now`, so the agent's next heartbeat opens a new connection
//! at once and reaches whichever server still accepts one.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::{HeartbeatReply, HeldJob, HeldState, NetworkCeiling};
use js_types::{Clock, HostId, Resources, Shape};
use tracing::warn;

use crate::admit::{
    reported_mount_providers, reported_xcodes, reports_dispatch_poll, reports_host_jobs,
    reports_layered_rootfs,
};
use crate::agents::Agents;
use crate::held_beat::Watch;
use crate::job_expiry::JobLimits;
use crate::job_store::ServerJobs;
use crate::logs::LogGrants;
use crate::metrics::MetricsSink;
use crate::per_job::{self, BeatContext};
use crate::util::blocking;

/// What the registry holds of a host that said hello, as the starts and stops of the host are
/// built from it: read by the heartbeat's per-job half and by the dispatch poll.
pub(super) struct HostFacts {
    pub(super) id: HostId,
    /// The shape the host announced at its hello, with the capabilities it reported.
    pub(super) claim: Shape,
    /// What the registry books for the host: what a start of the whole host books.
    pub(super) booked: Resources,
    /// Whether the registry holds the host tainted.
    pub(super) tainted: bool,
    /// The pool's limits, of which a refusal for capacity reads its requeue count and bar.
    pub(super) limits: JobLimits,
}

impl HostFacts {
    /// The facts of the host named `host` as the registry holds it, `None` for a text that is
    /// not a host id, a host the registry does not know, and one it holds no hello claim of
    /// (after a restart of the server, before the host says hello again).
    pub(super) fn of(agents: &Agents, host: &str) -> Option<HostFacts> {
        HostId::new(host)
            .ok()
            .and_then(|host| agents.registry.own_host(&host))
            .and_then(|own| {
                let tainted = own.host.taint.is_some();
                let booked = own.host.shape.resources().clone();
                own.host.claim.map(|claim| HostFacts {
                    id: own.host.id,
                    claim,
                    booked,
                    tainted,
                    limits: agents.job_limits,
                })
            })
    }
}

/// What the per-job half of a heartbeat reads besides the store: kept, so that a held reply
/// is built again from the same beat. The dispatch poll builds its starts from the same.
pub(super) struct Inputs {
    pub(super) jobs: Arc<ServerJobs>,
    pub(super) host: HostId,
    pub(super) ceiling: NetworkCeiling,
    pub(super) host_jobs: bool,
    pub(super) layered_rootfs: bool,
    pub(super) mount_providers: Vec<String>,
    pub(super) xcodes: Vec<String>,
    /// Whether the host's agent polls dispatch (and the server holds the job tables, as it
    /// does when it has these inputs): the reply leaves out what the queue carries.
    pub(super) polls_dispatch: bool,
    /// What the registry books for the host: what a start of the whole host books.
    pub(super) booked: Resources,
    pub(super) logs: Option<Arc<LogGrants>>,
    pub(super) listed: Vec<HeldJob>,
    pub(super) retired: Vec<String>,
    /// How many commands the reply has room for.
    pub(super) room: usize,
    /// The pool's limits ([`HostFacts::limits`]).
    pub(super) limits: JobLimits,
}

impl Inputs {
    /// The inputs for the host `facts` describes, over `jobs`, its starts carrying the sinks
    /// of `logs`, for a beat that lists `listed` and `retired` with room for `room` commands
    /// (the dispatch poll lists nothing and has the whole room of a reply).
    pub(super) fn new(
        jobs: &Arc<ServerJobs>,
        facts: &HostFacts,
        logs: Option<Arc<LogGrants>>,
        (listed, retired, room): (Vec<HeldJob>, Vec<String>, usize),
    ) -> Inputs {
        let capabilities = facts.claim.capabilities();
        Inputs {
            jobs: Arc::clone(jobs),
            host: facts.id.clone(),
            ceiling: NetworkCeiling::from_capabilities(capabilities).unwrap_or_default(),
            host_jobs: reports_host_jobs(capabilities),
            layered_rootfs: reports_layered_rootfs(capabilities),
            mount_providers: reported_mount_providers(capabilities),
            xcodes: reported_xcodes(capabilities),
            polls_dispatch: reports_dispatch_poll(capabilities),
            booked: facts.booked.clone(),
            logs,
            listed,
            retired,
            room,
            limits: facts.limits,
        }
    }

    /// The context the per-job half builds the starts in, counting into `metrics`.
    pub(super) fn context<'a>(&'a self, metrics: &'a dyn MetricsSink) -> BeatContext<'a> {
        BeatContext {
            host: &self.host,
            ceiling: &self.ceiling,
            host_jobs: self.host_jobs,
            layered_rootfs: self.layered_rootfs,
            mount_providers: &self.mount_providers,
            xcodes: &self.xcodes,
            shape: Some(&self.booked),
            logs: self.logs.as_deref(),
            polls_dispatch: self.polls_dispatch,
            metrics,
            limits: self.limits,
        }
    }
}

/// How long a reply to a beat is held at most when its host does not poll dispatch (or the
/// beat lists no young start): [`Agents::starting_hold`] when the beat's listing has a young
/// start (`young`, [`young_start`]), [`Agents::hold_bound`] otherwise. (The server sees jobs
/// only: the registry answers a beat that lists runs with no command, so such a reply is held
/// as any other quiet reply is.) A host that polls dispatch is paced instead when the listing
/// has a young start ([`hold`]).
pub(super) fn bound(agents: &Agents, young: bool) -> Duration {
    match young {
        true => agents.starting_hold,
        false => agents.hold_bound,
    }
}

/// Whether `listed`, the listing of a beat of `host`, has a job `starting` that a hold first saw
/// so less than [`crate::held_beat::STARTING_WINDOW_MS`] ago, now on the server's clock
/// ([`crate::held_beat::Starts::young`], which this records).
pub(super) fn young_start(agents: &Agents, host: &HostId, listed: &[HeldJob]) -> bool {
    let starting: Vec<&str> = listed
        .iter()
        .filter(|job| job.state == HeldState::Starting)
        .map(|job| job.job_id.as_str())
        .collect();
    agents
        .starts
        .young(host, &starting, agents.clock.now().millis())
}

/// What holding one heartbeat's reply needs.
pub(super) struct Hold {
    /// The watch of the host's heartbeat, taken before the store was read for the reply.
    pub(super) watch: Watch,
    pub(super) inputs: Arc<Inputs>,
    /// The whole-host allocation the registry's own reply named, which a reply built again
    /// keeps first, as the first build did.
    pub(super) registry_exclusive: Option<String>,
}

/// Holds `reply` while it carries nothing (beside the whole-host allocation it names and the
/// taint the host holds, `host_taint`, which the registry names in every reply to a tainted
/// host and which asks the agent for nothing): until a
/// wake builds a reply with something in it, which replaces its commands and allocation, or
/// until the bound ([`bound`]). A reply says `beat_now` when it carries no command:
/// one held to the bound, one woken with a new whole-host allocation alone, and one not held
/// that carries only acknowledgements, a taint acknowledgement or a log credential.
///
/// A host that polls dispatch takes its starts from the poll, so its heartbeat's hold carries
/// none, and holding it only keeps the agent from sending its next heartbeat: the one that
/// tells the server a start runs. Its reply is therefore held differently:
///
/// - A wake whose rebuilt reply is still empty (the poll carries the start) ends the hold, and
///   the reply does not say `beat_now`: the agent's next heartbeat leaves when its worker has
///   news (the start taken, running or ended) or after the interval.
/// - A beat that lists a job `starting`, however long it has been so, is not held to a bound
///   but paced ([`crate::held_beat::Starts::paced`]): answered at once when no such beat of the
///   host was answered in the last [`Agents::starting_hold`], held for the rest of it
///   otherwise. A paced reply answered at once or at the end of its spacing does not say
///   `beat_now`; one a wake or a drain ends says what that end says, as any hold's. So a host
///   with one start under way reports the start's `running` in the heartbeat that leaves when
///   the container runs, and a host with many is answered no more often than a held beat with
///   a start was before.
pub(super) async fn hold(agents: &Arc<Agents>, hold: Hold, reply: &mut HeartbeatReply) {
    let quiet = HeartbeatReply {
        exclusive: reply.exclusive.clone(),
        host_taint: reply.host_taint.clone(),
        ..HeartbeatReply::default()
    };
    if *reply != quiet {
        // Acknowledgements are answered once, so the next heartbeat can go at once and be
        // held; a command is sent again until the agent acts on it, so a reply with one waits
        // the interval, and an agent that is slow to act is not asked in a loop.
        reply.beat_now = reply.commands.is_empty();
        return;
    }
    reply.beat_now = true;
    if agents.draining.begun() {
        return;
    }
    let (host, listed) = (&hold.inputs.host, &hold.inputs.listed);
    let starting = listed.iter().any(|job| job.state == HeldState::Starting);
    let paced = hold.inputs.polls_dispatch && starting;
    let limit = match paced {
        true => {
            let now_ms = agents.clock.now().millis();
            agents.starts.paced(host, now_ms, agents.starting_hold)
        }
        false => bound(agents, young_start(agents, host, listed)),
    };
    if paced && limit.is_zero() {
        // The host's first beat with a start under way after a quiet spell: nothing is
        // queued behind it, so it is answered now, and the agent beats again on news.
        answered(agents, host);
        reply.beat_now = false;
        return;
    }
    agents.wakes.holding_for(limit);
    let at_bound = held(agents, &hold, reply, limit).await;
    if paced {
        answered(agents, host);
        // At the end of its spacing it says nothing new either: the agent beats again on news.
        reply.beat_now = reply.beat_now && !at_bound;
    }
}

/// Records that a paced beat of `host` was answered now ([`crate::held_beat::Starts::answered`]).
fn answered(agents: &Agents, host: &HostId) {
    let now_ms = agents.clock.now().millis();
    agents.starts.answered(host, now_ms, agents.starting_hold);
}

/// The hold of `reply` under way, for `limit` at most; for a host that polls dispatch a wake
/// ends it even when the reply is still empty (see [`hold`]). Says whether it ended at the
/// bound.
async fn held(
    agents: &Arc<Agents>,
    hold: &Hold,
    reply: &mut HeartbeatReply,
    limit: Duration,
) -> bool {
    let polls = hold.inputs.polls_dispatch;
    let bound = tokio::time::sleep(limit);
    tokio::pin!(bound);
    let draining = agents.draining.begins();
    tokio::pin!(draining);
    loop {
        agents.wakes.waiting();
        // A drain ends the hold before anything else, and a wake comes before the bound: a
        // start that is there is sent now rather than on the next heartbeat.
        tokio::select! {
            biased;
            () = &mut draining => return false,
            () = hold.watch.woken() => {}
            () = &mut bound => return true,
        }
        let inputs = Arc::clone(&hold.inputs);
        let metering = Arc::clone(agents);
        let rebuilt = blocking(move || {
            let context = inputs.context(metering.meter().sink());
            let beat = (inputs.listed.as_slice(), inputs.retired.as_slice());
            // The reply was empty, so the listing named no job the host holds no live
            // allocation for: none is stopped as unknown.
            per_job::commands_now(&inputs.jobs, &context, beat, &BTreeSet::new(), inputs.room)
        })
        .await;
        match rebuilt {
            Ok((commands, exclusive)) => {
                let exclusive = hold.registry_exclusive.clone().or(exclusive);
                if !commands.is_empty() || exclusive != reply.exclusive {
                    // A start is sent again until the agent lists it: the agent waits the
                    // interval after it, as after any reply with a command.
                    reply.beat_now = commands.is_empty();
                    reply.commands = commands;
                    reply.exclusive = exclusive;
                    return false;
                }
                if polls {
                    // The placement's start went out on the host's dispatch poll: the agent
                    // beats again when its worker has news of it, not before.
                    reply.beat_now = false;
                    return false;
                }
            }
            Err(error) => {
                let host = &hold.inputs.host;
                warn!(%host, %error, "a held heartbeat's reply could not be built again; it is answered as it was");
                return false;
            }
        }
    }
}

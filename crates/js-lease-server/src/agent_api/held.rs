//! The hold of a heartbeat's reply ([`crate::held_beat`]): a reply with nothing to send to a
//! serving host that reported `held-beat` is held open until a placement on the host wakes it, or for
//! [`Agents::hold_bound`] at most, and says `beat_now`, so the agent sends its next heartbeat
//! at once. A beat that lists a job as `starting` is held for [`Agents::starting_hold`] at
//! most instead, so that the start's `running`, which rides on the next heartbeat, is not
//! kept a whole hold.
//!
//! A reply is held only when it carries nothing at all: no command, no acknowledgement, no
//! taint acknowledgement and no log credential, so a hold never delays what the beat asked
//! for. A reply that is not held says `beat_now` too when it carries no command, so the
//! heartbeat after an acknowledgement is held at once rather than an interval later. Everything the beat reported was committed and counted before the hold: the listing,
//! the taint, the usage. When a wake comes, the stops and starts are built again from the
//! store over the listing the beat brought ([`per_job::commands_now`]); a reply that now has
//! a command, or names another whole-host allocation, is answered at once, and one that is
//! still empty (another server's change, a placement this one undid) is held on. A read that
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
    reported_mount_providers, reports_dispatch_poll, reports_host_jobs, reports_layered_rootfs,
};
use crate::agents::{Agents, Session};
use crate::held_beat::Watch;
use crate::job_store::ServerJobs;
use crate::logs::LogGrants;
use crate::metrics::MetricsSink;
use crate::per_job::{self, BeatContext};
use crate::util::blocking;

/// What the registry holds of a host that said hello, as the starts and stops of the host are
/// built from it: read by the heartbeat's per-job half and by the dispatch poll.
pub(super) struct HostFacts {
    pub(super) id: HostId,
    pub(super) session: Session,
    /// The shape the host announced at its hello, with the capabilities it reported.
    pub(super) claim: Shape,
    /// What the registry books for the host: what a start of the whole host books.
    pub(super) booked: Resources,
    /// Whether the registry holds the host tainted.
    pub(super) tainted: bool,
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
                let session = (own.host.epoch, own.session_started.map(|at| at.millis()));
                let tainted = own.host.taint.is_some();
                let booked = own.host.shape.resources().clone();
                own.host.claim.map(|claim| HostFacts {
                    id: own.host.id,
                    session,
                    claim,
                    booked,
                    tainted,
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
            polls_dispatch: reports_dispatch_poll(capabilities),
            booked: facts.booked.clone(),
            logs,
            listed,
            retired,
            room,
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
            shape: Some(&self.booked),
            logs: self.logs.as_deref(),
            polls_dispatch: self.polls_dispatch,
            metrics,
        }
    }
}

/// How long a reply to a beat of `host` that lists `listed` is held at most, now on the
/// server's clock: [`Agents::starting_hold`] when the listing has a job `starting` that a hold
/// first saw so less than [`crate::held_beat::STARTING_WINDOW_MS`] ago
/// ([`crate::held_beat::Starts::young`]), [`Agents::hold_bound`] otherwise. (The server sees
/// jobs only: the registry cancels every run a beat lists, so a reply to a beat that lists one
/// is never quiet and never held.)
pub(super) fn limit(agents: &Agents, host: &HostId, listed: &[HeldJob]) -> Duration {
    let starting: Vec<&str> = listed
        .iter()
        .filter(|job| job.state == HeldState::Starting)
        .map(|job| job.job_id.as_str())
        .collect();
    let now_ms = agents.clock.now().millis();
    match agents.starts.young(host, &starting, now_ms) {
        true => agents.starting_hold,
        false => agents.hold_bound,
    }
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

/// Holds `reply` while it carries nothing (beside the whole-host allocation it names): until a
/// wake builds a reply with something in it, which replaces its commands and allocation, or
/// until the bound ([`limit`]). A reply says `beat_now` when it carries no command:
/// one held to the bound, one woken with a new whole-host allocation alone, and one not held
/// that carries only acknowledgements, a taint acknowledgement or a log credential.
pub(super) async fn hold(agents: &Arc<Agents>, hold: Hold, reply: &mut HeartbeatReply) {
    let quiet = HeartbeatReply {
        exclusive: reply.exclusive.clone(),
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
    let limit = limit(agents, &hold.inputs.host, &hold.inputs.listed);
    agents.wakes.holding_for(limit);
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
            () = &mut draining => return,
            () = hold.watch.woken() => {}
            () = &mut bound => return,
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
                    return;
                }
            }
            Err(error) => {
                let host = &hold.inputs.host;
                warn!(%host, %error, "a held heartbeat's reply could not be built again; it is answered as it was");
                return;
            }
        }
    }
}

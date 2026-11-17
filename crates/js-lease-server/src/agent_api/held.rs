//! The hold of a heartbeat's reply ([`crate::held_beat`]): a reply with nothing to send to a
//! serving host that reported `held-beat` is held open until a placement on the host wakes it, or for
//! [`Agents::hold_bound`] at most, and says `beat_now`, so the agent sends its next heartbeat
//! at once.
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

use std::collections::BTreeSet;
use std::sync::Arc;

use js_agent_wire::{HeartbeatReply, HeldJob, NetworkCeiling};
use js_types::{HostId, Resources};
use tracing::warn;

use crate::agents::Agents;
use crate::held_beat::Watch;
use crate::job_store::ServerJobs;
use crate::logs::LogGrants;
use crate::metrics::MetricsSink;
use crate::per_job::{self, BeatContext};
use crate::util::blocking;

/// What the per-job half of a heartbeat reads besides the store: kept, so that a held reply
/// is built again from the same beat.
pub(super) struct Inputs {
    pub(super) jobs: Arc<ServerJobs>,
    pub(super) host: HostId,
    pub(super) ceiling: NetworkCeiling,
    pub(super) host_jobs: bool,
    pub(super) layered_rootfs: bool,
    pub(super) job_outputs: bool,
    pub(super) mount_providers: Vec<String>,
    /// What the registry books for the host: what a start of the whole host books.
    pub(super) booked: Resources,
    pub(super) logs: Option<Arc<LogGrants>>,
    pub(super) listed: Vec<HeldJob>,
    pub(super) retired: Vec<String>,
    /// How many commands the reply has room for.
    pub(super) room: usize,
}

impl Inputs {
    /// The context the per-job half builds the starts in, counting into `metrics`.
    pub(super) fn context<'a>(&'a self, metrics: &'a dyn MetricsSink) -> BeatContext<'a> {
        BeatContext {
            host: &self.host,
            ceiling: &self.ceiling,
            host_jobs: self.host_jobs,
            layered_rootfs: self.layered_rootfs,
            job_outputs: self.job_outputs,
            mount_providers: &self.mount_providers,
            shape: Some(&self.booked),
            logs: self.logs.as_deref(),
            metrics,
        }
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
/// until the bound. A reply says `beat_now` when it carries no command: one held to the bound,
/// one woken with a new whole-host allocation alone, and one not held that carries only
/// acknowledgements, a taint acknowledgement or a log credential.
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
    let bound = tokio::time::sleep(agents.hold_bound);
    tokio::pin!(bound);
    loop {
        agents.wakes.waiting();
        tokio::select! {
            () = &mut bound => return,
            () = hold.watch.woken() => {}
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

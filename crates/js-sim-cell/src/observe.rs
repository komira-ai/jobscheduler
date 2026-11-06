//! What the cell observes, at one virtual time.
//!
//! Four sources, each kept apart so an invariant says which it reads:
//!
//! - the client driver: for each unit of work (an item, numbered from zero), what it
//!   asked for ([`Ask`]) and every answer it heard, in order ([`ClientEvent`]);
//! - each agent process: what it runs now, what it completed and which results the
//!   server acknowledged ([`AgentTruth`]), its ground truth, not what it reported;
//! - the cell's launch ledger ([`Launch`]): one entry per run an agent process
//!   launched, with the runs that ended on their own ([`Completion`]) and the verdicts the
//!   agents read on their results ([`Acked`]). The ledger is the cell's, not the agent's,
//!   so an agent that dies and is replaced cannot reset it; the cell adds the agent
//!   processes it killed ([`Killed`]). Beside them the ledger keeps what the agents sent
//!   and were sent for the whole-host handshake: each ready report sent
//!   ([`ReadyReport`]), each start delivered ([`StartSeen`]) and each stop or cancel that
//!   reached a running run ([`StopSeen`]);
//! - the server's list of hosts ([`ServerView`]), as the cell last read it. The cell
//!   fills it from the registry's host views, never through the JSON route, and keeps
//!   beside it each booking the listing showed, from the read that first showed it to
//!   the read that no longer did ([`Booking`]).
//!
//! Some facts are carried for invariants that later parts add and no check here reads
//! yet: [`Ask::gives_up_at`], [`AgentTruth::completed`], [`AgentTruth::acknowledged`]
//! and [`ServerHost::set_aside`].
//!
//! Jobs have sources of their own beside these (the server's job and allocation tables, and
//! what the per-job agents sent and were sent), kept in [`crate::jobs`].
//!
//! [`Observations`] gathers them, with whether a fault is active and the items the client
//! asked to cancel ([`Observations::cancels`]). Its builder
//! ([`Observations::quiet`], [`Observations::with_agent`], [`Observations::with_event`],
//! [`Observations::with_ready`] and the rest) is public test support: the negative tests of every invariant family
//! build their histories with it.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js_jobs::{AllocationId, JobId};
use js_types::sched::FarmTime;
use js_types::{HostId, LeaseId, Resources};

use crate::elastic::CloudFacts;
use crate::jobs::{AllocationRead, JobBeat, JobRead, JobReply, ResultMade};
use crate::ledger::{Acked, Completion};

/// One unit of work the client driver submits, numbered from zero in the order it
/// submits them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct ItemId(pub u32);

/// A run's id, as the start that launches it names it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RunId(String);

impl RunId {
    /// The run named `id`.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// How an item ended, as the client heard it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum HowEnded {
    /// The run exited with status zero.
    Succeeded,
    /// The run failed (a nonzero exit, a limit, or no start).
    Failed,
    /// The run was cancelled at the client's request.
    Cancelled,
    /// The server answered that the run is lost: its host went away under it.
    Lost,
    /// The server answered that the run is gone (`410 run-forgotten`).
    Forgotten,
    /// The client gave up: its bound ([`Ask::gives_up_at`]) passed and it released the
    /// lease. The only terminal the client chooses rather than hears.
    GaveUp,
}

impl HowEnded {
    /// Every way an item ends.
    pub const ALL: [HowEnded; 6] = [
        HowEnded::Succeeded,
        HowEnded::Failed,
        HowEnded::Cancelled,
        HowEnded::Lost,
        HowEnded::Forgotten,
        HowEnded::GaveUp,
    ];

    /// A stable kebab-case name, as a violation writes it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            HowEnded::Succeeded => "succeeded",
            HowEnded::Failed => "failed",
            HowEnded::Cancelled => "cancelled",
            HowEnded::Lost => "lost",
            HowEnded::Forgotten => "forgotten",
            HowEnded::GaveUp => "gave-up",
        }
    }
}

/// What the client driver reports about one item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientEvent {
    /// The client submitted the item.
    Submitted,
    /// The client learned the ids the server gave the item: its lease and its run. An
    /// item is queued once: one item is one lease.
    Queued {
        /// The item's lease.
        lease: LeaseId,
        /// The item's run.
        run: RunId,
    },
    /// The server answered the acquire of a whole-host item pending (202) with this
    /// lease: one try of the item's request, which its host's agent prepares for once
    /// the server commits it. A try whose prepare fails ends without a grant, and the
    /// item tries again under a new lease. The lease of the try that is granted is the
    /// one [`ClientEvent::Queued`] names.
    Pending {
        /// The try's lease.
        lease: LeaseId,
    },
    /// The client learned the host the item was placed on.
    Placed {
        /// The host.
        host: HostId,
    },
    /// The client learned the run started.
    Started,
    /// The item reached a terminal answer.
    Terminal {
        /// How it ended.
        how: HowEnded,
    },
    /// The client sent the release of the item's lease. It is recorded before the
    /// answer, so a release whose answer a fault drops is still known to have been
    /// sent.
    ReleaseSent,
    /// The client's release of the item's lease was answered. The client releases every
    /// lease it learned, whatever the item's terminal (a [`HowEnded::GaveUp`] item
    /// too), so an item holds its lease from [`ClientEvent::Queued`] to here.
    Released,
}

/// What the client asked for an item, so no check takes the server's word for it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ask {
    /// Whether the item wants a whole host.
    pub whole_host: bool,
    /// The resources it asked for.
    pub resources: Resources,
    /// The virtual time at which the client gives up on it. No check reads it yet.
    pub gives_up_at: FarmTime,
}

/// One event the client heard, and when.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Heard {
    /// When the client heard it.
    pub at: FarmTime,
    /// What it heard.
    pub event: ClientEvent,
}

/// Everything the client driver holds about one item.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ItemRecord {
    /// What the client asked for.
    pub ask: Ask,
    /// What it heard, in the order it heard it.
    pub heard: Vec<Heard>,
}

/// A run an agent runs now, as the start that launched it named it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Running {
    /// The lease the run belongs to.
    pub lease: LeaseId,
    /// The resources the run holds on the host.
    pub resources: Resources,
}

/// A job a per-job agent runs now, as the start that launched it named it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunningJob {
    /// The allocation the start named.
    pub allocation: AllocationId,
    /// The resources the job holds on the host.
    pub resources: Resources,
}

/// The server's verdict on a result an agent reported (the wire's acknowledgement).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The result was recorded.
    Accepted,
    /// The result was recorded earlier.
    Duplicate,
    /// The lease has moved on, or the server holds no record of the run.
    Stale,
    /// The server holds no such lease.
    UnknownLease,
    /// The result does not match the lease it names.
    Mismatch,
}

/// The ground truth of one agent process: what it holds, not what it reported.
///
/// An agent that dies loses all of it; its replacement starts from
/// [`AgentTruth::new`]. The cell's [`Launch`] ledger is what survives.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentTruth {
    /// What the host can hold.
    pub capacity: Resources,
    /// The runs it runs now.
    pub running: BTreeMap<RunId, Running>,
    /// The runs it completed, with the virtual time each ended. No check reads it yet.
    pub completed: BTreeMap<RunId, FarmTime>,
    /// The results the server acknowledged, with its verdict. No check reads it yet.
    pub acknowledged: BTreeMap<RunId, Verdict>,
    /// The jobs it runs now, when it speaks the per-job messages.
    pub jobs: BTreeMap<JobId, RunningJob>,
}

impl AgentTruth {
    /// A fresh agent process on a host of `capacity`, running nothing.
    #[must_use]
    pub fn new(capacity: Resources) -> Self {
        Self {
            capacity,
            ..Self::default()
        }
    }

    /// This agent, also running `run` of `lease` with `resources`.
    #[must_use]
    pub fn with_run(mut self, run: RunId, lease: LeaseId, resources: Resources) -> Self {
        self.running.insert(run, Running { lease, resources });
        self
    }

    /// This agent, also running `job` under `allocation` with `resources`.
    #[must_use]
    pub fn with_job(mut self, job: JobId, allocation: AllocationId, resources: Resources) -> Self {
        let running = RunningJob {
            allocation,
            resources,
        };
        self.jobs.insert(job, running);
        self
    }
}

/// One launch of a run by an agent process, in the cell's ledger.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    /// The run launched.
    pub run: RunId,
    /// The host whose agent launched it.
    pub host: HostId,
    /// When.
    pub at: FarmTime,
}

/// One agent process the cell killed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Killed {
    /// The host whose process was killed.
    pub host: HostId,
    /// When.
    pub at: FarmTime,
}

/// A heartbeat in which an agent process reported its host ready for an exclusive
/// (whole-host) lease: what the agent sent, whether or not the server heard it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadyReport {
    /// The host whose agent sent it.
    pub host: HostId,
    /// The lease the report named, as the agent wrote it (the server writes a lease id
    /// `<term>-<sequence>`).
    pub lease: String,
    /// When the heartbeat was sent.
    pub at: FarmTime,
}

/// A start an agent process was sent, whether or not it launched the run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartSeen {
    /// The host whose agent it reached.
    pub host: HostId,
    /// The run it starts.
    pub run: RunId,
    /// The lease the run belongs to.
    pub lease: LeaseId,
    /// When it reached the agent.
    pub at: FarmTime,
    /// When the agent sent the heartbeat the start names, the one the server had taken
    /// when it queued the run (the hello that opened the session for a number the agent
    /// never sent): no later than the server's time when it queued the run.
    pub asked_at: FarmTime,
    /// The run's wall limit, as the start sets it.
    pub wall: Duration,
}

/// How the server told an agent to end a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum StopCommand {
    /// The wire's `stop`.
    Stop,
    /// The wire's `cancel`.
    Cancel,
}

impl StopCommand {
    /// A stable kebab-case name, as a violation writes it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            StopCommand::Stop => "stop",
            StopCommand::Cancel => "cancel",
        }
    }
}

/// A stop or a cancel that reached a run the agent process was running.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StopSeen {
    /// The host whose agent it reached.
    pub host: HostId,
    /// The run it ended.
    pub run: RunId,
    /// The lease the run belongs to.
    pub lease: LeaseId,
    /// When it reached the agent.
    pub at: FarmTime,
    /// Which command it was.
    pub command: StopCommand,
    /// When the run's own wall limit ran out: its launch plus the wall limit of its
    /// start.
    pub wall_ends_at: FarmTime,
}

/// One host as the server lists it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerHost {
    /// Whether the server holds the host lost (silent past its grace).
    pub lost: bool,
    /// The leases booked on the host, in the order they were booked.
    pub leases: Vec<LeaseId>,
    /// Whether the host is set aside (reserved, held whole, draining or releasing). No
    /// check reads it yet.
    pub set_aside: bool,
    /// Whether the host is leaving the pool: drained or being released by the scaler.
    pub leaving: bool,
    /// The whole-host request a reservation sets the host aside for, while the server
    /// holds one for it (the listing's `reserved_for`).
    pub reserved_for: Option<String>,
    /// The end of the host's tail when the reservation was made, on the cell's virtual
    /// clock; `None` without a reservation, or for one whose tail the server does not
    /// know.
    pub tail_end: Option<FarmTime>,
}

/// One lease the server's listing booked on one host, from the read of the listing that
/// first showed it there to the read that no longer did: how long the server booked it,
/// to within the time between two reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Booking {
    /// The host.
    pub host: HostId,
    /// The lease booked there.
    pub lease: LeaseId,
    /// The first read that showed it.
    pub from: FarmTime,
    /// The first read that no longer showed it; `None` while it is booked.
    pub until: Option<FarmTime>,
}

/// The server's own list of hosts, as the cell last read it from the registry.
///
/// The cell re-reads it at every client call and once per virtual second, not at
/// every step, so between two reads it can lag what the server holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerView {
    /// Every host the server lists.
    pub hosts: BTreeMap<HostId, ServerHost>,
    /// Each host the listing named again after it had named it, once per time again,
    /// in the order the listing named them. Empty for a listing that names every host
    /// once.
    pub repeated: Vec<HostId>,
}

impl ServerView {
    /// The view of a listing that names `listed` hosts in that order: the first entry
    /// of a host is its entry, and each later one is noted in [`ServerView::repeated`].
    #[must_use]
    pub fn from_listing(listed: impl IntoIterator<Item = (HostId, ServerHost)>) -> Self {
        let mut view = Self::default();
        for (host, entry) in listed {
            match view.hosts.entry(host) {
                Entry::Occupied(named) => view.repeated.push(named.key().clone()),
                Entry::Vacant(free) => {
                    free.insert(entry);
                }
            }
        }
        view
    }
}

/// Each lease `view` books, with its host.
fn booked(view: &ServerView) -> BTreeSet<(HostId, LeaseId)> {
    let pairs = view.hosts.iter().flat_map(|(host, listed)| {
        let leases = listed.leases.iter();
        leases.map(move |lease| (host.clone(), *lease))
    });
    pairs.collect()
}

/// Everything the cell observes at one virtual time.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Observations {
    /// The virtual time.
    pub at: FarmTime,
    /// Whether a fault (a partition, a dropped message, a killed process) is active.
    pub fault_active: bool,
    /// Since when no fault has been active: the end of the last fault, or the start of
    /// the run if there was none.
    pub quiet_since: FarmTime,
    /// The client driver's record of each item it submitted.
    pub items: BTreeMap<ItemId, ItemRecord>,
    /// The agent process of each host that has one now.
    pub agents: BTreeMap<HostId, AgentTruth>,
    /// The cell's launch ledger, oldest first; entries are only ever appended.
    pub ledger: Vec<Launch>,
    /// The runs that ended on their own, from the cell's ledger, oldest first.
    pub completions: Vec<Completion>,
    /// The verdicts the agents read on their results, from the cell's ledger, oldest
    /// first.
    pub verdicts: Vec<Acked>,
    /// The agent processes the cell killed, oldest first.
    pub kills: Vec<Killed>,
    /// The server's list of hosts.
    pub server: ServerView,
    /// Every booking the server's listing showed, in the order the reads first showed
    /// them; only ever appended to, and ended.
    pub bookings: Vec<Booking>,
    /// Every ready report an agent process sent, oldest first; only ever appended.
    pub ready: Vec<ReadyReport>,
    /// Every start that reached an agent process, oldest first; only ever appended.
    pub starts: Vec<StartSeen>,
    /// Every stop or cancel that reached a running run, oldest first; only ever
    /// appended.
    pub stops: Vec<StopSeen>,
    /// The items the client asked to cancel, with when it first asked.
    pub cancels: BTreeMap<ItemId, FarmTime>,
    /// Each change of a job's state the server's job table showed, in the order read; only
    /// ever appended.
    pub job_reads: Vec<JobRead>,
    /// Each change of an allocation's row the server's allocation table showed, in the
    /// order read; only ever appended.
    pub allocation_reads: Vec<AllocationRead>,
    /// When the server's job and allocation tables were last read; `None` before the
    /// first read, and for a server without the job service.
    pub tables_at: Option<FarmTime>,
    /// Every heartbeat a per-job agent sent, oldest first; only ever appended.
    pub job_beats: Vec<JobBeat>,
    /// Every answer a per-job agent read to a heartbeat, oldest first; only ever appended.
    pub job_replies: Vec<JobReply>,
    /// Every result a per-job agent made, oldest first; only ever appended.
    pub results_made: Vec<ResultMade>,
    /// The cloud of the server's elastic pool; `None` for a server without one.
    pub cloud: Option<CloudFacts>,
}

impl Observations {
    /// Takes `view` as the server's listing read at `now`: each booking it shows that the
    /// listing before did not is appended to [`Observations::bookings`] from `now`, and
    /// each one the listing before showed that it does not is ended at `now`.
    pub(crate) fn read_listing(&mut self, view: ServerView, now: FarmTime) {
        let old = std::mem::replace(&mut self.server, view);
        let (was, is) = (booked(&old), booked(&self.server));
        let new = is.difference(&was).map(|(host, lease)| Booking {
            host: host.clone(),
            lease: *lease,
            from: now,
            until: None,
        });
        self.bookings.extend(new);
        for (host, lease) in was.difference(&is) {
            let mut open = self.bookings.iter_mut().rev();
            let open = open.find(|booking| (&booking.host, booking.lease) == (host, *lease));
            open.into_iter()
                .for_each(|booking| booking.until = Some(now));
        }
    }

    /// Nothing observed, at virtual time zero, with no fault ever active.
    #[must_use]
    pub fn quiet() -> Self {
        Self::default()
    }

    /// These observations at virtual time `at`.
    #[must_use]
    pub fn at(mut self, at: FarmTime) -> Self {
        self.at = at;
        self
    }

    /// These observations with a fault active now.
    #[must_use]
    pub fn with_fault(mut self) -> Self {
        self.fault_active = true;
        self
    }

    /// These observations with the last fault ended at `since`: no fault is active
    /// now, and none has been since `since`.
    #[must_use]
    pub fn quiet_since(mut self, since: FarmTime) -> Self {
        self.fault_active = false;
        self.quiet_since = since;
        self
    }

    /// These observations with `truth` as the agent process of `host`, in place of any
    /// earlier one.
    #[must_use]
    pub fn with_agent(mut self, host: HostId, truth: AgentTruth) -> Self {
        self.agents.insert(host, truth);
        self
    }

    /// These observations with `item` asking for `ask`, keeping what it heard.
    #[must_use]
    pub fn with_item(mut self, item: ItemId, ask: Ask) -> Self {
        self.items.entry(item).or_default().ask = ask;
        self
    }

    /// These observations with the client hearing `event` about `item` at `at`, after
    /// everything it heard before. An item not yet recorded is recorded with the
    /// default [`Ask`]: a share of no resources, given up at time zero.
    #[must_use]
    pub fn with_event(mut self, item: ItemId, at: FarmTime, event: ClientEvent) -> Self {
        let heard = Heard { at, event };
        self.items.entry(item).or_default().heard.push(heard);
        self
    }

    /// These observations with `run` launched on `host` at `at` appended to the ledger.
    #[must_use]
    pub fn with_launch(mut self, run: RunId, host: HostId, at: FarmTime) -> Self {
        self.ledger.push(Launch { run, host, at });
        self
    }

    /// These observations with the agent of `host` having sent a ready report for
    /// `lease` at `at`, after the reports before.
    #[must_use]
    pub fn with_ready(mut self, host: HostId, lease: &str, at: FarmTime) -> Self {
        let lease = lease.to_owned();
        self.ready.push(ReadyReport { host, lease, at });
        self
    }

    /// These observations with `start` delivered, after the starts before.
    #[must_use]
    pub fn with_start(mut self, start: StartSeen) -> Self {
        self.starts.push(start);
        self
    }

    /// These observations with `stop` delivered to a running run, after the stops before.
    #[must_use]
    pub fn with_stop(mut self, stop: StopSeen) -> Self {
        self.stops.push(stop);
        self
    }

    /// These observations with the client having asked to cancel `item` at `at`, unless
    /// it asked earlier.
    #[must_use]
    pub fn with_cancel(mut self, item: ItemId, at: FarmTime) -> Self {
        self.cancels.entry(item).or_insert(at);
        self
    }

    /// These observations with the server listing `host` as set aside for the whole-host
    /// request `request`, its tail ending at `tail_end`.
    #[must_use]
    pub fn with_reservation(
        mut self,
        host: HostId,
        request: &str,
        tail_end: Option<FarmTime>,
    ) -> Self {
        let listed = self.server.hosts.entry(host).or_default();
        listed.set_aside = true;
        listed.reserved_for = Some(request.to_owned());
        listed.tail_end = tail_end;
        self
    }

    /// These observations with `run` ended on its own on `host` at `at` appended to the
    /// ledger's completions.
    #[must_use]
    pub fn with_completion(mut self, run: RunId, host: HostId, at: FarmTime) -> Self {
        self.completions.push(Completion { run, host, at });
        self
    }

    /// These observations with the agent of `host` reading `verdict` on `run`'s result
    /// at `at`, appended to the ledger's verdicts.
    #[must_use]
    pub fn with_verdict(
        mut self,
        run: RunId,
        host: HostId,
        verdict: Verdict,
        at: FarmTime,
    ) -> Self {
        let acked = Acked {
            run,
            host,
            verdict,
            at,
        };
        self.verdicts.push(acked);
        self
    }

    /// These observations with the agent process of `host` killed at `at`.
    #[must_use]
    pub fn with_kill(mut self, host: HostId, at: FarmTime) -> Self {
        self.kills.push(Killed { host, at });
        self
    }

    /// These observations with `cloud` as the elastic pool's cloud.
    #[must_use]
    pub fn with_cloud(mut self, cloud: CloudFacts) -> Self {
        self.cloud = Some(cloud);
        self
    }

    /// These observations with the server listing `host` as leaving the pool (drained or
    /// being released), keeping what it books there.
    #[must_use]
    pub fn with_leaving(mut self, host: HostId) -> Self {
        let listed = self.server.hosts.entry(host).or_default();
        listed.leaving = true;
        listed.set_aside = true;
        self
    }

    /// These observations with the server listing `host` as lost, or live, keeping
    /// what it books there.
    #[must_use]
    pub fn with_lost(mut self, host: HostId, lost: bool) -> Self {
        self.server.hosts.entry(host).or_default().lost = lost;
        self
    }

    /// These observations with the server listing `lease` as booked on `host`, after
    /// the leases it lists there already.
    #[must_use]
    pub fn with_booking(mut self, host: HostId, lease: LeaseId) -> Self {
        self.server
            .hosts
            .entry(host)
            .or_default()
            .leases
            .push(lease);
        self
    }
}

//! The feed loop: the scheduler, and the bookkeeping that carries its effects out in order.
//!
//! [`Engine`] feeds the scheduler an input, then a tick (so placement follows at once), and
//! carries out the effects one by one. It knows no wire message, no caller channel and no
//! cache: every effect that reaches outside is an [`Adapter`] call, made under the same
//! lock as the feed. A `Commit` is fed back to the scheduler as committed at once (the
//! control log is this process), but what the commit leads to (a `Start`, an answer) is
//! carried out only once the [`Writer`](crate::Writer) has taken the rows it settles
//! ([`Engine::release_unwritten`]). An `Answer` is returned for the caller to deliver
//! after the lock is released.
//!
//! `Engine` is not generic and takes `&mut dyn Adapter`, so every test binary's run of
//! any of its methods counts toward one instantiation of it.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use js_caps::NodeCaps;
use js_sched::{
    Cordon, DaemonInstance, Event, FARM_RERUNS, Input, KeyFloor, OpState, Prediction, Request,
    Requeue, Scheduler,
};
use js_store::Rows;
use js_types::sched::{
    Answer, ControlRecord, Effect, FarmTime, LeaseId, OperationId, Outcome, Refusal, Resources,
    StartLease, StateMachine, WaiterId, Waiting, WorkerId,
};

use crate::adapter::{Adapter, Link, Lowered, Stage, View};
use crate::core::{Admitted, Config, Held, NotQueued};
use crate::intake::Intake;
use crate::store::Unsettled;
use crate::write::{Attempt, Batch, Settles, Writer};

/// How many committed records the core holds for a writer that refuses them.
pub const HELD_CAP: usize = 4096;

/// The scheduler, and the bookkeeping between it and the effects it asks for.
pub(crate) struct Engine {
    pub(crate) sched: Scheduler,
    /// Takes the rows a committed record's effects settle.
    pub(crate) writer: Box<dyn Writer>,
    /// The newest stream of each worker that has registered; a worker's link outlives its
    /// stream.
    pub(crate) links: BTreeMap<WorkerId, Link>,
    pub(crate) next_stream: u64,
    pub(crate) next_waiter: u64,
    /// Nodes whose drain has paused, once logged.
    paused: BTreeSet<WorkerId>,
    /// The operations a `Start` was sent for, marked before the `Start` leaves and never
    /// cleared while the scheduler holds the operation: a drain ends the streams of an
    /// operation that waits for a worker only if it is not in here.
    pub(crate) ever_started: BTreeSet<OperationId>,
    /// The nodes the drain cordoned itself when it closed this core's intake, as opposed
    /// to an operator's cordon: an operation that waits only for these is queued, not
    /// stuck, since the other frontends take it.
    drain_held: BTreeSet<WorkerId>,
    /// The operation of each lease whose `Start` was sent, until its operation is answered
    /// or refused: a `Result` names a lease it was sent.
    pub(crate) sent: BTreeMap<LeaseId, OperationId>,
    /// The operation of each lease whose answer was settled, while the scheduler keeps the
    /// operation: a copy of its `Result` is a duplicate, not an unknown lease.
    pub(crate) answered: BTreeMap<LeaseId, OperationId>,
    /// The `Result`s taken but not acknowledged, by lease, with the worker that sent each:
    /// their finish waits in `unwritten`. The acknowledgement goes out when that finish is
    /// settled, and never if it is dropped instead.
    pub(crate) owed: BTreeMap<LeaseId, WorkerId>,
    /// The effects of committed records whose rows the writer has not taken, one batch per
    /// record, oldest first. Nothing is carried out (no `Start`, no answer) before its row
    /// is written, so a writer that refuses holds the later effects behind the earliest
    /// refused one.
    pub(crate) unwritten: VecDeque<Batch>,
    /// Writes given up on for good: refused for a reason a later try cannot overcome, or
    /// over the cap.
    pub(crate) write_drops: u64,
    /// The most committed records `unwritten` holds before a new one is dropped.
    pub(crate) held_cap: usize,
    /// A handover took the work from this core: it feeds the scheduler nothing and queues
    /// nothing.
    pub(crate) fenced: bool,
    /// A drain closed this core's intake: it places no work on its nodes, and goes on with
    /// what it holds.
    pub(crate) closed: bool,
    /// The state store's rows, if the core has a store ([`Core::open`](crate::Core::open)).
    pub(crate) rows: Option<Arc<Rows>>,
    /// The epoch the store was opened under, if the core has one.
    pub(crate) epoch: Option<u64>,
    /// How long the scheduler keeps a finished operation; the store keeps its `done/` row at
    /// least as long.
    pub(crate) finished_retention: Duration,
    /// The scheduler was told the state store is unavailable ([`Engine::watch_store`]).
    pub(crate) store_down: bool,
    /// A write failed because the store could not be reached, and none has been taken since.
    pub(crate) unreachable: bool,
    /// Leases given up that the store has not yet been told of, oldest first. Until it has,
    /// it still holds the lease and no queue row for the operation, so every one is written
    /// again before anything that follows it is ([`Engine::settle_requeues`]).
    pub(crate) unsettled: Vec<Unsettled>,
    /// When each node's session row in the store was last written, or renewed: the next
    /// renewal falls due a renewal interval later.
    pub(crate) renewed: BTreeMap<WorkerId, FarmTime>,
    /// When each node's session row was last written successfully, at its registration or
    /// a renewal. Its heartbeats are acknowledged only while that was lately.
    pub(crate) session_ok: BTreeMap<WorkerId, FarmTime>,
    /// The nodes whose `Hello` found the store unreachable, so this core has not claimed
    /// them, with the daemon process that said it ([`Engine::claim_pending`]).
    pub(crate) unclaimed: BTreeMap<WorkerId, DaemonInstance>,
    /// The operations the scheduler says no live worker can run now, with its reason: the
    /// ones whose stage [`Engine::stage_for`] decides again when what it depends on changes.
    pub(crate) stuck: BTreeMap<OperationId, String>,
    /// What the daemons of the other frontends offer, as the last look at the store's
    /// session rows read it ([`Engine::look_at_remote_sessions`]).
    pub(crate) remote: Vec<(NodeCaps, Resources)>,
    /// A write was refused because a newer writer holds the state store: nothing this core
    /// writes can succeed any more. Never cleared; unlike an outage, nothing ends it.
    pub(crate) superseded: bool,
    /// For each operation this core's callers wait on ([`Core::follow`](crate::Core::follow))
    /// that the last looks found neither unfinished nor ended, how many looks in a row. Only
    /// logged: no look count ends a caller's wait.
    pub(crate) untraced_kept: BTreeMap<OperationId, u32>,
    /// What the intake poll keeps between two looks at the state store
    /// ([`Core::intake`](crate::Core::intake)).
    pub(crate) intake: Intake,
    /// Size keys are on ([`Config::size_keys`]).
    pub(crate) size_keys: bool,
    /// Key floors the estimator lowered that the state store has not taken yet, at most one
    /// per size key ([`Engine::lower_key_floors`]).
    pub(crate) unwritten_floors: Vec<KeyFloor>,
}

impl Engine {
    /// An engine over a new scheduler, whose writer takes everything.
    pub(crate) fn new(config: &Config) -> Self {
        let mut sched = Scheduler::new(config.term);
        if let Some(load) = config.load {
            sched = sched.with_load_config(load);
        }
        if let Some(whole) = &config.whole_node {
            sched = sched.with_whole_node(whole.clone());
        }
        if let Some(curves) = &config.curves {
            sched = sched.with_curves(Arc::clone(&curves.0));
        }
        if let Some(bound) = config.memory_floors {
            sched = sched.with_memory_floors(bound);
        }
        if let Some(ladder) = &config.memory_ladder {
            sched.set_memory_ladder(ladder.clone());
        }
        let sched = sched
            .with_unservable_wait(config.unservable_wait)
            .with_finished_retention(config.finished_retention)
            .with_loss_requeues(config.loss_requeues)
            .recording_requeues();
        Self {
            sched,
            writer: Box::new(|_: &Effect, _: &Settles<'_>| Attempt::Taken),
            links: BTreeMap::new(),
            next_stream: 0,
            next_waiter: 0,
            paused: BTreeSet::new(),
            ever_started: BTreeSet::new(),
            drain_held: BTreeSet::new(),
            sent: BTreeMap::new(),
            answered: BTreeMap::new(),
            owed: BTreeMap::new(),
            unwritten: VecDeque::new(),
            write_drops: 0,
            held_cap: HELD_CAP,
            fenced: false,
            closed: false,
            rows: None,
            epoch: None,
            finished_retention: config.finished_retention,
            store_down: false,
            unreachable: false,
            unsettled: Vec::new(),
            renewed: BTreeMap::new(),
            session_ok: BTreeMap::new(),
            unclaimed: BTreeMap::new(),
            stuck: BTreeMap::new(),
            remote: Vec::new(),
            superseded: false,
            untraced_kept: BTreeMap::new(),
            intake: Intake::new(config.orphan_wait),
            size_keys: config.size_keys,
            unwritten_floors: Vec::new(),
        }
    }

    /// What an [`Adapter`] hook may read.
    pub(crate) fn view(&self) -> View<'_> {
        View::new(&self.sched, &self.links)
    }

    /// Feeds `event` to the scheduler (see [`Engine::carry_out`]), unless the core is
    /// fenced: then nothing is fed and there are no answers.
    pub(crate) fn feed(&mut self, a: &mut dyn Adapter, now: FarmTime, event: Event) -> Vec<Answer> {
        if self.fenced {
            return Vec::new();
        }
        self.carry_out(a, now, event)
    }

    /// Feeds an input that cannot lead to an answer: only a `Report`, a heartbeat (a
    /// lease lost to a proven restart or a replacement) and a tick (a silent lease)
    /// propose a result, and only a committed result answers; those three deliver their
    /// answers themselves.
    pub(crate) fn feed_quiet(&mut self, a: &mut dyn Adapter, now: FarmTime, event: Event) {
        let answers = self.feed(a, now, event);
        debug_assert!(
            answers.is_empty(),
            "an answer without a report: {answers:?}"
        );
    }

    /// [`Engine::feed`] on a core that is not fenced: feeds `event`, then a tick (so
    /// placement follows at once), and carries out the effects in order. Returns the
    /// answers, for the caller to deliver once the lock is released.
    fn carry_out(&mut self, a: &mut dyn Adapter, now: FarmTime, event: Event) -> Vec<Answer> {
        a.clock(now);
        self.hold_placements(now);
        self.watch_store(now);
        self.claim_pending(a, now);
        self.reserve_seqs();
        // Only the inputs whose callers deliver answers ([`Engine::feed_quiet`]) release
        // effects an earlier refusal held back: one of them may be an answer.
        let answering = matches!(
            event,
            Event::Tick | Event::Heartbeat { .. } | Event::Report { .. }
        );
        let event = self.held_event(event);
        let mut effects: VecDeque<Effect> = self.sched.apply(Input::new(now, event)).into();
        effects.extend(self.sched.apply(Input::new(now, Event::Tick)));
        self.log_requeues(a);
        if answering {
            // Before this input's own effects: they follow what was held back.
            for effect in self.release_unwritten(a).into_iter().rev() {
                effects.push_front(effect);
            }
        }
        let mut answers = Vec::new();
        while let Some(effect) = effects.pop_front() {
            match effect {
                Effect::Commit(record) => {
                    // A refusal for want of a daemon waits on if another frontend's could
                    // run the operation: it is not committed, and is proposed again after
                    // another unservable wait.
                    if self.held_for_other_frontends(&record) {
                        continue;
                    }
                    let grant = if let ControlRecord::Lease(grant) = &record {
                        a.offer(&self.view(), grant);
                        Some(grant.clone())
                    } else {
                        None
                    };
                    // Appended is committed; the effects of the commit (the `Start`, the
                    // answer) are carried out only once the writer has taken what they
                    // settle. What it refuses is kept and written again, in order, until
                    // it takes it.
                    let next = self.sched.apply(Input::new(now, Event::Committed(record)));
                    if self.unwritten.len() >= self.held_cap {
                        self.write_drops += 1;
                        tracing::error!(cap = self.held_cap, "records over the cap: one dropped");
                        continue;
                    }
                    self.unwritten.push_back(Batch {
                        effects: next.into(),
                        grant,
                    });
                    // Written at once when this input can deliver an answer, or when
                    // nothing older waits; else behind the records already held.
                    let released = if answering || self.unwritten.len() == 1 {
                        self.release_unwritten(a)
                    } else {
                        Vec::new()
                    };
                    effects.extend(released);
                }
                Effect::Start(start) => self.start(a, &start),
                Effect::Answer(answer) => {
                    self.failed(a, &answer);
                    answers.push(answer);
                }
                Effect::Waiting(waiting) => self.waiting(a, &waiting),
                Effect::Refuse(refusal) => self.refuse(a, &refusal),
            }
        }
        self.log_requeues(a);
        self.forget_dropped(a);
        answers
    }

    /// Logs the leases the scheduler has given up since the last call, one INFO line
    /// each, naming the operation as the adapter does. Called before any effect is carried
    /// out (an effect can finish an operation, and with a zero finished retention drop it
    /// and forget its callers) and again at the end of a feed. A requeue only names an
    /// operation the scheduler held when it was made; one dropped since is named by the
    /// adapter all the same, never looked up by index.
    ///
    /// A grant the writer has not taken for a lease the scheduler has given up is never
    /// written: it would put the lease back, held by no one.
    pub(crate) fn log_requeues(&mut self, a: &mut dyn Adapter) {
        let given_up = self.sched.take_requeues();
        self.settle_requeues(a, &given_up);
        // The queue row a give-up writes is this core's own: the intake poll finds the
        // operation queued as it left it, not queued again by another frontend.
        for requeue in &given_up {
            self.intake.rewrote(requeue.operation);
        }
        for requeue in given_up {
            a.requeued(&requeue);
            let Requeue {
                operation,
                lease,
                worker: node,
                reason,
            } = requeue;
            let farm_runs = self.sched.farm_runs(operation).map_or(0, <[_]>::len);
            let operation = a.name(&self.view(), operation);
            // One line: the coverage of a logged field counts only where it is logged.
            // `farm_runs` is the farm-fault budget spent so far, of `farm_budget` reruns.
            tracing::info!(%operation, %lease, %node, %reason, farm_runs, farm_budget = FARM_RERUNS, "lease given up; requeued");
        }
    }

    /// Forgets the callers, and their channels, of every finished operation the
    /// scheduler has dropped. Every input ends here.
    pub(crate) fn forget_dropped(&mut self, a: &mut dyn Adapter) {
        let (sched, intake) = (&self.sched, &self.intake);
        let held = |operation| sched.state(operation).is_some() || intake.keeps(operation);
        a.forget_dropped(&held);
        self.answered
            .retain(|_, operation| sched.state(*operation).is_some());
        self.ever_started
            .retain(|operation| sched.state(*operation).is_some());
    }

    /// Marks the operation of a committed lease started, then has the adapter send its
    /// `Start`.
    fn start(&mut self, a: &mut dyn Adapter, start: &StartLease) {
        // Marked before the `Start` leaves: a stream that reads the stage later never
        // sees an operation that has a `Start` in flight as one that cannot start.
        self.ever_started.insert(start.operation);
        self.sent.insert(start.lease, start.operation);
        a.start(&self.view(), start);
    }

    /// Logs an operation the scheduler finishes failed: the failure kind, and how much of
    /// the farm-fault budget and the memory ladder its runs used (`farm_runs` at
    /// `farm_budget` is the budget spent, so no rerun is left). Operations that complete
    /// are logged where their lease's result is taken.
    fn failed(&self, a: &dyn Adapter, answer: &Answer) {
        if let Outcome::Failed(failure) = answer.outcome {
            let operation = a.name(&self.view(), answer.operation);
            tracing::warn!(%operation, lease = %answer.lease, ?failure, farm_runs = answer.farm_runs.len(), farm_budget = FARM_RERUNS, memory_runs = answer.memory_runs.len(), "operation finished failed");
        }
    }

    /// Tells an operation's callers why it waits, or that it no longer waits for a
    /// worker that can run it. Logged, so an operator sees it too.
    fn waiting(&mut self, a: &mut dyn Adapter, waiting: &Waiting) {
        let operation = waiting.operation;
        let stage = match &waiting.reason {
            Some(why) => {
                if self.runnable_elsewhere(operation) {
                    tracing::info!(
                        %operation,
                        reason = %why,
                        "no worker of this frontend can run the operation; another frontend can"
                    );
                } else {
                    tracing::warn!(%operation, reason = %why, "no live worker can run the operation");
                }
                self.stuck.insert(operation, why.clone());
                self.stage_for(operation, why)
            }
            None => {
                tracing::info!(%operation, "a live worker can run the operation again");
                self.stuck.remove(&operation);
                Stage::Queued
            }
        };
        a.show(&self.view(), operation, &stage);
    }

    /// Whether a daemon session of another frontend, as the last look at the store read
    /// them, could run `operation`. Its work is taken by that frontend, which takes the
    /// queued operations it did not enqueue.
    fn runnable_elsewhere(&self, operation: OperationId) -> bool {
        self.sched.request(operation).is_some_and(|request| {
            self.remote
                .iter()
                .any(|(caps, capacity)| request.runnable_on(caps, capacity))
        })
    }

    /// The stage of `operation`, which no live worker of this frontend can run now (`why`).
    /// A worker of another frontend can run it when that frontend takes the queued
    /// operation; this frontend follows the state store until then. What each stage means
    /// to a caller:
    ///
    /// - `WaitingKept`: a drain keeps its streams. The operation has started before, or a
    ///   daemon session of another frontend could run it.
    /// - `Queued`: the only thing between it and a worker is this drain's own cordon, with
    ///   the intake closed, so it is queued for the other frontends like any queued work.
    /// - `Waiting`: a drain ends its streams. It never started, no daemon session of
    ///   another frontend could run it, and it is held by an operator's cordon (alone, or
    ///   beside the drain's) or by no worker at all.
    fn stage_for(&self, operation: OperationId, why: &str) -> Stage {
        let cordoned = self.sched.cordoned_could(operation);
        let only_drain = !cordoned.is_empty()
            && cordoned
                .iter()
                .all(|worker| self.drain_held.contains(worker));
        if self.ever_started.contains(&operation) || self.runnable_elsewhere(operation) {
            Stage::WaitingKept(why.to_owned())
        } else if only_drain {
            Stage::Queued
        } else {
            Stage::Waiting(why.to_owned())
        }
    }

    /// Decides again the stage of every operation that waits for a worker, after the
    /// daemons of the other frontends were read.
    pub(crate) fn republish_stuck(&mut self, a: &mut dyn Adapter) {
        let alive: BTreeSet<OperationId> = self.sched.queued().collect();
        self.stuck.retain(|operation, _| alive.contains(operation));
        for (operation, why) in self.stuck.clone() {
            let stage = self.stage_for(operation, &why);
            a.show(&self.view(), operation, &stage);
        }
    }

    /// Answers the callers of an operation the scheduler refused with the reason, and
    /// keeps them for the finished retention. Nothing is cached, so nothing is awaited.
    pub(crate) fn refuse(&mut self, a: &mut dyn Adapter, refusal: &Refusal) {
        tracing::warn!(operation = %refusal.operation, reason = %refusal.reason, "operation refused");
        // A lease given up before the operation was refused may still be listed.
        self.sent
            .retain(|_, operation| *operation != refusal.operation);
        a.refuse(&self.view(), refusal);
        self.forget_dropped(a);
    }

    /// Cordons, in the scheduler alone, every node this core has heard from once its intake
    /// is closed ([`Core::close_intake`](crate::Core::close_intake)), so that placement
    /// gives none of them work. Before every input: a node that registers after the intake
    /// closed is cordoned before the placement round that follows its registration. A node
    /// that is cordoned already (by this or by an operator) is left as it is.
    fn hold_placements(&mut self, now: FarmTime) {
        if !self.closed {
            return;
        }
        let nodes: Vec<WorkerId> = self
            .links
            .keys()
            .filter(|worker| self.sched.cordon(worker).is_none())
            .cloned()
            .collect();
        for worker in nodes {
            self.drain_held.insert(worker.clone());
            let none = self.sched.apply(Input::new(now, Event::Cordon { worker }));
            debug_assert!(none.is_empty(), "a cordon has no effect: {none:?}");
        }
    }

    /// The input to feed in place of `event`: on a core whose intake is closed an uncordon
    /// is a tick, since the scheduler places work on the node inside the uncordon itself,
    /// which would give a node this core was told not to use work. Any other event, and
    /// every event of a core whose intake is open, is returned as it is.
    fn held_event(&self, event: Event) -> Event {
        match event {
            Event::Uncordon { .. } if self.closed => Event::Tick,
            event => event,
        }
    }

    /// Where a request about to be queued goes: written to the store first, if the core has
    /// one ([`Engine::queue`]); then the twin the scheduler holds, if it has one, or the
    /// next operation it numbers; and the caller's number.
    ///
    /// # Errors
    /// The core is fenced, or the store did not take the request.
    pub(crate) fn admit(
        &mut self,
        a: &mut dyn Adapter,
        request: &Request,
        parcel: &[u8],
        size_key: Option<u64>,
        prediction: Option<Prediction>,
        now: FarmTime,
    ) -> Result<Admitted, NotQueued> {
        if self.fenced {
            return Err(NotQueued::Fenced);
        }
        // A core with size keys off ignores the key: nothing is written, nothing is booked.
        let size_key = size_key.filter(|_| self.size_keys);
        let waiter = WaiterId(self.next_waiter);
        self.next_waiter += 1;
        let held = self.queue(a, request, parcel, size_key, now)?;
        let twin = match held {
            Held::Here => self.sched.twin(request),
            Held::Elsewhere(operation) => Some(operation),
        };
        let admitted = Admitted {
            waiter,
            operation: twin.unwrap_or_else(|| self.sched.next_operation()),
            joined: twin.is_some(),
            held,
        };
        // Before the `Submit` is fed, so the first placement round books the key's floor.
        // The id is the one the `opkey/` row was written under (`queue` moved the scheduler
        // to it). A joined caller keeps the key of the operation it joined.
        if let Some(key) = size_key.filter(|_| !admitted.joined && held == Held::Here) {
            self.sched.set_size_key(admitted.operation, key);
        }
        // The cost likewise: a new operation is charged it from its first round. A twin keeps
        // its cost while it holds a lease; one that still waits takes the newer prediction.
        if let Some(prediction) = prediction.filter(|_| admitted.held != Held::Here) {
            let waiting = self.sched.state(admitted.operation) == Some(&OpState::Queued);
            if !admitted.joined || waiting {
                self.sched.set_cost(admitted.operation, prediction);
            }
        }
        Ok(admitted)
    }

    /// Takes the key floors the adapter's estimator wants lowered to the scheduler, and
    /// writes them down on a best-effort basis: a row the store refuses is kept and written
    /// again at a later tick ([`Engine::write_key_floors`]); until then a restart books the
    /// higher floor. A floor the scheduler holds above what the estimator saw before the
    /// decay is left alone: a kill the estimator never saw raised it since.
    pub(crate) fn lower_key_floors(&mut self, a: &dyn Adapter, lowered: Vec<Lowered>) {
        for Lowered {
            size_key,
            before,
            ask,
        } in lowered
        {
            if self.sched.key_floor(size_key).is_some_and(|f| f > before) {
                continue;
            }
            let Some(row) = self.sched.lower_key_floor(size_key, ask) else {
                continue;
            };
            self.unwritten_floors.retain(|f| f.key != size_key);
            self.unwritten_floors.push(row);
        }
        self.write_key_floors(a);
    }

    /// Writes the lowered key floors waiting for the store; those it refuses stay.
    pub(crate) fn write_key_floors(&mut self, a: &dyn Adapter) {
        let rows = self
            .rows
            .clone()
            .filter(|_| !self.fenced && !self.superseded);
        let Some(rows) = rows else {
            // No store to write to, or a newer writer holds it: nothing is kept.
            self.unwritten_floors.clear();
            return;
        };
        let sched = &self.sched;
        // A pending row is written only while the scheduler still holds the floor it lowered
        // to: a kill that raised it since has written the higher row (a restart loses what is
        // pending, and the higher row it leaves is the safe side).
        self.unwritten_floors
            .retain(|row| sched.key_floor(row.key).unwrap_or(0) == row.ask);
        self.unwritten_floors.retain(|row| {
            let summary = a.summary_name(row.key);
            let refused = rows.put_key_floor(row, summary.as_deref()).err();
            if let Some(error) = &refused {
                tracing::warn!(%error, "a lowered key floor was not written");
            }
            refused.is_some()
        });
    }

    /// Expires leases of silent workers and places queued work. A silent lease lost past
    /// its operation's farm reruns finishes the operation: the answers come back, as a
    /// heartbeat's do. Logs each drain that has paused at its deadline, once.
    pub(crate) fn tick(&mut self, a: &mut dyn Adapter, now: FarmTime) -> Vec<Answer> {
        let answers = self.feed(a, now, Event::Tick);
        // The lowered rows the store refused are written again; a fenced core drops them.
        if !self.unwritten_floors.is_empty() {
            self.write_key_floors(a);
        }
        let paused: BTreeSet<WorkerId> = self
            .links
            .keys()
            .filter(|w| matches!(self.sched.cordon(w), Some(Cordon::Paused { .. })))
            .cloned()
            .collect();
        for worker in paused.difference(&self.paused) {
            let leases = self.sched.leases_on(worker).len();
            tracing::warn!(%worker, leases, "drain paused at its deadline; leases run on");
        }
        self.paused = paused;
        answers
    }
}

/// The lease an effect starts, if it is a `Start`.
pub(crate) fn started(effect: &Effect) -> Option<LeaseId> {
    match effect {
        Effect::Start(start) => Some(start.lease),
        _ => None,
    }
}

/// The operation of an `Answer` or `Refuse` effect that is a finish, if it is one.
pub(crate) fn finish_of(effect: &Effect) -> Option<OperationId> {
    match effect {
        Effect::Answer(answer) => Some(answer.operation),
        Effect::Refuse(refusal) => Some(refusal.operation),
        Effect::Start(_) | Effect::Commit(_) | Effect::Waiting(_) => None,
    }
}

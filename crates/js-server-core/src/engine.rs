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

use js_sched::{Cordon, Event, FARM_RERUNS, Input, LOSS_REQUEUES, Request, Requeue, Scheduler};
use js_types::sched::{
    Answer, ControlRecord, Effect, FarmTime, LeaseId, OperationId, Outcome, Refusal, StartLease,
    StateMachine, WaiterId, Waiting, WorkerId,
};

use crate::adapter::{Adapter, Link, Stage, View};
use crate::core::{Admitted, Config, Fenced, NodeAction, UnknownNode};
use crate::write::{Attempt, Writer};

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
    ever_started: BTreeSet<OperationId>,
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
    pub(crate) unwritten: VecDeque<VecDeque<Effect>>,
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
}

impl Engine {
    /// An engine over a new scheduler, whose writer takes everything.
    pub(crate) fn new(config: &Config) -> Self {
        let sched = Scheduler::new(config.term)
            .with_unservable_wait(config.unservable_wait)
            .with_finished_retention(config.finished_retention)
            .with_loss_requeues(LOSS_REQUEUES)
            .recording_requeues();
        Self {
            sched,
            writer: Box::new(|_: &Effect| Attempt::Taken),
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
        self.hold_placements(now);
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
                    if let ControlRecord::Lease(grant) = &record {
                        a.offer(&self.view(), grant);
                    }
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
                    self.unwritten.push_back(next.into());
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
    fn log_requeues(&mut self, a: &mut dyn Adapter) {
        for requeue in self.sched.take_requeues() {
            for batch in &mut self.unwritten {
                batch.retain(|effect| started(effect) != Some(requeue.lease));
            }
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
        let sched = &self.sched;
        a.forget_dropped(&|operation| sched.state(operation).is_some());
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
                tracing::warn!(%operation, reason = %why, "no live worker can run the operation");
                self.stage_for(operation, why)
            }
            None => {
                tracing::info!(%operation, "a live worker can run the operation again");
                Stage::Queued
            }
        };
        a.show(&self.view(), operation, &stage);
    }

    /// The stage of `operation`, which no live worker can run now (`why`). What each stage
    /// means to a caller:
    ///
    /// - `WaitingKept`: a drain keeps its streams. The operation has started before.
    /// - `Queued`: the only thing between it and a worker is this drain's own cordon, with
    ///   the intake closed, so it is queued for the other frontends like any queued work.
    /// - `Waiting`: a drain ends its streams. It never started and is held by an operator's
    ///   cordon (alone, or beside the drain's) or by no worker at all.
    fn stage_for(&self, operation: OperationId, why: &str) -> Stage {
        let cordoned = self.sched.cordoned_could(operation);
        let only_drain = !cordoned.is_empty()
            && cordoned
                .iter()
                .all(|worker| self.drain_held.contains(worker));
        if self.ever_started.contains(&operation) {
            Stage::WaitingKept(why.to_owned())
        } else if only_drain {
            Stage::Queued
        } else {
            Stage::Waiting(why.to_owned())
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

    /// Where a request about to be queued goes: the twin the scheduler holds, if it has
    /// one, or the next operation it numbers; and the caller's number.
    ///
    /// # Errors
    /// The core is fenced.
    pub(crate) fn admit(&mut self, request: &Request) -> Result<Admitted, Fenced> {
        if self.fenced {
            return Err(Fenced);
        }
        let twin = self.sched.twin(request);
        let waiter = WaiterId(self.next_waiter);
        self.next_waiter += 1;
        Ok(Admitted {
            waiter,
            operation: twin.unwrap_or_else(|| self.sched.next_operation()),
            joined: twin.is_some(),
        })
    }

    /// Cordons, drains or uncordons `worker` (see `js_sched::Cordon`).
    ///
    /// # Errors
    /// `worker` has never registered.
    pub(crate) fn place(
        &mut self,
        a: &mut dyn Adapter,
        now: FarmTime,
        worker: &WorkerId,
        action: NodeAction,
    ) -> Result<(), UnknownNode> {
        if !self.links.contains_key(worker) {
            return Err(UnknownNode(worker.as_str().to_owned()));
        }
        let name = worker.clone();
        let event = match action {
            NodeAction::Cordon => Event::Cordon { worker: name },
            NodeAction::Drain(within) => Event::Drain {
                worker: name,
                deadline: now.saturating_add(within),
            },
            NodeAction::Uncordon => Event::Uncordon { worker: name },
        };
        tracing::info!(%worker, ?action, "operator action");
        self.feed_quiet(a, now, event);
        Ok(())
    }

    /// Expires leases of silent workers and places queued work. A silent lease lost past
    /// its operation's farm reruns finishes the operation: the answers come back, as a
    /// heartbeat's do. Logs each drain that has paused at its deadline, once.
    pub(crate) fn tick(&mut self, a: &mut dyn Adapter, now: FarmTime) -> Vec<Answer> {
        let answers = self.feed(a, now, Event::Tick);
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
fn started(effect: &Effect) -> Option<LeaseId> {
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

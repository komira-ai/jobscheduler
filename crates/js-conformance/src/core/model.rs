//! The reference core: the smallest loop that satisfies the scenarios, and the same loop
//! with one rule broken per [`Defect`]. Test code; see the module doc for what it is not.

use std::collections::{BTreeMap, VecDeque};
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use js_caps::NodeCaps;
use js_sched::{DaemonInstance, Event, Input, Recovered, RecoveredLease, Request, Scheduler};
use js_state::fake::{Fake, Flaw};
use js_state::{Cond, ManualClock, Store, Txn};
use js_types::sched::{
    ControlRecord, Effect, FarmTime, LeaseId, LeaseKind, OperationId, Outcome, Qos, Resources,
    ResultRef, StateMachine, WaiterId, WorkKey, WorkerId,
};

use super::{Answered, CoreHarness, Observed, Released, Stream};

/// The leader term every boot runs under. A restart is the same leader again.
const TERM: u64 = 1;

/// How many lease sequences a boot reserves in the store before using any.
const SEQ_BLOCK: u64 = 1 << 20;

const GIB: u64 = 1 << 30;

/// The job no worker can ever hold (it asks for more cores than any worker has), so the
/// scheduler waits and then refuses it. Lets the model's own tests reach the refusal path.
pub const UNPLACEABLE_JOB: u8 = u8::MAX;

/// A job that asks for all the cores of a worker (4), so no other job fits beside it
/// whatever the placement rule or its slack, and the next job goes to another worker. Lets
/// a scenario put one job on each of two workers without relying on how the scheduler
/// spreads or packs.
pub const WIDE_JOB: u8 = u8::MAX - 1;

/// The one rule a broken [`ReferenceCore`] breaks. Each is the cause of exactly one
/// scenario failing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Defect {
    /// A `Start` is released before its lease record is in the store.
    StartBeforeCommit,
    /// A finished operation's row stays in the store, so a restart takes it up again and
    /// answers it a second time.
    AnswerTwice,
    /// A restart numbers new leases from the highest lease still in the store, not from
    /// the store's reserved blocks, so the ids of finished leases come round again.
    ReusesSeqAfterRestart,
    /// A heartbeat from a replaced stream is fed to the scheduler.
    FeedsStaleHeartbeat,
    /// A record the store refuses is forgotten, with no retry.
    DropsUnwrittenOnStoreError,
    /// A restart forgets which operations hold a lease, so each is queued and placed again
    /// at once instead of waiting out the grace for its worker.
    RecoversWithoutGrace,
    /// A restart that finds no worker had registered forgets the operations still queued,
    /// so work that could not be placed before the crash is lost.
    ForgetsQueueWithoutWorkers,
    /// A restart that finds the store was refusing writes forgets the operations still
    /// queued, so work whose grant was never written is lost.
    ForgetsQueueAfterRefusal,
}

/// Something the loop still has to do, in order.
enum Item {
    /// An effect the scheduler asked for.
    Effect(Effect),
    /// A record to write that the scheduler has already been told is committed.
    Write(ControlRecord),
}

/// The reference loop: commit to the store, then release; recover from the store.
///
/// What the store holds: `op/<n>` (an unfinished operation), `lease/<n>` (its committed
/// lease), `meta/next_op` and `meta/next_seq` (the highest ids ever handed out or reserved).
pub struct ReferenceCore {
    defect: Option<Defect>,
    clock: Arc<ManualClock>,
    store: Fake,
    sched: Scheduler,
    now: FarmTime,
    held: VecDeque<Item>,
    refusals: u32,
    store_down: bool,
    streams: BTreeMap<String, Stream>,
    streams_made: u64,
    seen: Observed,
}

impl Default for ReferenceCore {
    fn default() -> Self {
        Self::new()
    }
}

impl ReferenceCore {
    /// A correct core over an empty store, at farm time 0.
    pub fn new() -> Self {
        Self::with_defect(None)
    }

    /// A core that breaks `defect` and nothing else.
    pub fn broken(defect: Defect) -> Self {
        Self::with_defect(Some(defect))
    }

    fn with_defect(defect: Option<Defect>) -> Self {
        let clock = Arc::new(ManualClock::default());
        let mut core = Self {
            defect,
            store: Fake::with_flaw(clock.clone(), Flaw::None),
            clock,
            sched: Scheduler::new(TERM),
            now: FarmTime::default(),
            held: VecDeque::new(),
            refusals: 0,
            store_down: false,
            streams: BTreeMap::new(),
            streams_made: 0,
            seen: Observed::default(),
        };
        core.boot();
        core
    }

    /// Builds the scheduler from what the store holds and reserves its sequence block.
    fn boot(&mut self) {
        self.sched = Scheduler::new(TERM);
        // The scheduler's grace for recovered leases runs from the last time it was told.
        self.sched.apply(Input::new(self.now, Event::Tick));
        let leases: BTreeMap<u64, RecoveredLease> = self
            .rows("lease/")
            .into_iter()
            .map(|row| {
                let [operation, seq, worker] = fields(&row);
                let lease = RecoveredLease {
                    lease: LeaseId::new(TERM, number(seq)),
                    worker: WorkerId::new(worker),
                    cost: None,
                };
                (number(operation), lease)
            })
            .collect();
        let first = self.first_seq(&leases);
        let recovered = self.rows("op/").into_iter().map(|row| {
            let [operation, waiter, job] = fields(&row);
            let held = leases.get(&number(operation)).cloned();
            Recovered {
                operation: OperationId(number(operation)),
                request: request(number(job) as u8),
                waiter: WaiterId(number(waiter)),
                lease: held.filter(|_| self.defect != Some(Defect::RecoversWithoutGrace)),
                losses: 0,
                fate: None,
            }
        });
        let operations: Vec<Recovered> = recovered.collect();
        let next_op = self.counter("meta/next_op");
        self.sched
            .recover(TERM, OperationId(next_op), first, operations);
        let block: Range<u64> = first..first + SEQ_BLOCK;
        self.sched.use_seq_block(block.clone());
        self.put(Txn::new().put("meta/next_seq", block.end.to_string(), Cond::Any));
    }

    /// Where this boot's lease numbers start.
    fn first_seq(&self, leases: &BTreeMap<u64, RecoveredLease>) -> u64 {
        match self.defect {
            Some(Defect::ReusesSeqAfterRestart) => {
                let highest = leases.values().map(|held| held.lease.seq).max();
                highest.map_or(0, |seq| seq + 1)
            }
            _ => self.counter("meta/next_seq"),
        }
    }

    /// Deletes the rows of every operation the store holds.
    fn forget_queue(&self) {
        let rows = self.store.scan_prefix("op/").expect("the fake store scans");
        let txn = rows
            .into_iter()
            .fold(Txn::new(), |txn, (key, _)| txn.delete(key, Cond::Any));
        self.put(txn);
    }

    fn put(&self, txn: Txn) {
        let done = self.store.txn(txn);
        done.expect("the fake store takes a write")
            .expect("no conflict");
    }

    fn counter(&self, key: &str) -> u64 {
        let found = self.store.get(key).expect("the fake store reads");
        found.map_or(0, |row| number(&text(&row.value)))
    }

    /// The values of the rows under `prefix`, in key order.
    fn rows(&self, prefix: &str) -> Vec<String> {
        let rows = self
            .store
            .scan_prefix(prefix)
            .expect("the fake store scans");
        rows.into_iter().map(|(_, row)| text(&row.value)).collect()
    }

    fn feed(&mut self, event: Event) {
        let effects = self.sched.apply(Input::new(self.now, event));
        self.held.extend(effects.into_iter().map(Item::Effect));
        self.drain();
    }

    fn front(&mut self, effects: Vec<Effect>) {
        for effect in effects.into_iter().rev() {
            self.held.push_front(Item::Effect(effect));
        }
    }

    /// Carries out what is held, in order, until it is empty or the store refuses.
    fn drain(&mut self) {
        while let Some(item) = self.held.pop_front() {
            match item {
                Item::Write(record) => {
                    if !self.write(&record) {
                        self.held.push_front(Item::Write(record));
                        return;
                    }
                }
                Item::Effect(Effect::Commit(record)) => {
                    if self.defect == Some(Defect::StartBeforeCommit) {
                        self.held.push_front(Item::Write(record.clone()));
                        let more = self.committed(record);
                        self.front(more);
                    } else if self.write(&record) {
                        let more = self.committed(record);
                        self.front(more);
                    } else if self.defect != Some(Defect::DropsUnwrittenOnStoreError) {
                        self.held.push_front(Item::Effect(Effect::Commit(record)));
                        return;
                    }
                }
                Item::Effect(Effect::Start(start)) => {
                    self.seen.released.push(Released {
                        stream: self
                            .streams
                            .get(start.worker.as_str())
                            .copied()
                            .unwrap_or_default(),
                        worker: start.worker.to_string(),
                        lease: start.lease,
                        operation: start.operation,
                    });
                }
                Item::Effect(Effect::Answer(answer)) => self.seen.answered.push(Answered {
                    operation: answer.operation,
                    waiters: answer.waiters,
                }),
                Item::Effect(Effect::Refuse(refusal)) => self.seen.answered.push(Answered {
                    operation: refusal.operation,
                    waiters: refusal.waiters,
                }),
                Item::Effect(Effect::Waiting(_)) => {}
            }
        }
    }

    /// Tells the scheduler `record` is committed and returns what that asks for.
    fn committed(&mut self, record: ControlRecord) -> Vec<Effect> {
        self.sched
            .apply(Input::new(self.now, Event::Committed(record)))
    }

    /// Writes `record`; `false` if the store refused (and the scheduler is told).
    fn write(&mut self, record: &ControlRecord) -> bool {
        if self.refusals > 0 {
            self.refusals -= 1;
            if !self.store_down {
                self.store_down = true;
                self.sched.store_unavailable();
            }
            return false;
        }
        let mut txn = Txn::new();
        if let ControlRecord::Lease(grant) = record {
            let row = format!("{},{},{}", grant.operation.0, grant.lease.seq, grant.worker);
            txn = txn.put(format!("lease/{:016}", grant.operation.0), row, Cond::Any);
        }
        if let Some(operation) = finished(record) {
            txn = txn.delete(format!("lease/{:016}", operation.0), Cond::Any);
            if self.defect != Some(Defect::AnswerTwice) {
                txn = txn.delete(format!("op/{:016}", operation.0), Cond::Any);
            }
        }
        self.put(txn);
        if self.store_down {
            self.store_down = false;
            self.sched.store_available(self.now);
        }
        true
    }
}

impl CoreHarness for ReferenceCore {
    fn submit(&mut self, waiter: u64, job: u8) -> OperationId {
        let request = request(job);
        // A request for work already in flight joins that operation: no new id, no new row.
        let twin = self.sched.twin(&request);
        let operation = twin.unwrap_or(self.sched.next_operation());
        if twin.is_none() {
            let row = format!("{},{waiter},{job}", operation.0);
            let txn = Txn::new()
                .put(format!("op/{:016}", operation.0), row, Cond::Any)
                .put("meta/next_op", (operation.0 + 1).to_string(), Cond::Any);
            self.put(txn);
        }
        self.feed(Event::Submit {
            waiter: WaiterId(waiter),
            request,
        });
        operation
    }

    fn worker_up(&mut self, worker: &str) -> Stream {
        self.streams_made += 1;
        let stream = Stream(self.streams_made);
        self.streams.insert(worker.to_owned(), stream);
        let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
            .expect("a node report")
            .with_drivers(["container"]);
        self.feed(Event::WorkerUp {
            worker: WorkerId::new(worker),
            instance: DaemonInstance::new("daemon"),
            predecessor_stopped: false,
            capacity: Resources::new(4_000, 8 * GIB),
            caps,
        });
        stream
    }

    fn heartbeat(&mut self, worker: &str, stream: Stream, running: &[LeaseId]) {
        let newest = self.streams.get(worker) == Some(&stream);
        if newest || self.defect == Some(Defect::FeedsStaleHeartbeat) {
            self.feed(Event::Heartbeat {
                worker: WorkerId::new(worker),
                running: running.to_vec(),
                load: None,
            });
        }
    }

    fn report(&mut self, operation: OperationId, lease: LeaseId) {
        let action_result = ResultRef::new([7; 32], 0);
        self.feed(Event::Report {
            operation,
            lease,
            outcome: Outcome::Completed { action_result },
        });
    }

    fn advance(&mut self, by: Duration) {
        self.clock.advance(by);
        self.now = self.now.saturating_add(by);
        self.feed(Event::Tick);
    }

    fn refuse_writes(&mut self, count: u32) {
        self.refusals = count;
    }

    fn crash_and_restart(&mut self) {
        let empty = Fake::with_flaw(self.clock.clone(), Flaw::None);
        self.store = std::mem::replace(&mut self.store, empty).restart();
        if self.defect == Some(Defect::ForgetsQueueWithoutWorkers) && self.streams.is_empty() {
            self.forget_queue();
        }
        if self.defect == Some(Defect::ForgetsQueueAfterRefusal) && self.store_down {
            self.forget_queue();
        }
        self.held.clear();
        self.refusals = 0;
        self.store_down = false;
        self.streams.clear();
        self.boot();
    }

    fn observed(&self) -> Observed {
        let mut rows = self
            .store
            .scan_prefix("lease/")
            .expect("the fake store scans");
        rows.sort_by_key(|(_, row)| row.version);
        let leases = rows.into_iter().map(|(_, row)| {
            let row = text(&row.value);
            let [_, seq, _] = fields(&row);
            LeaseId::new(TERM, number(seq))
        });
        Observed {
            stored_leases: leases.collect(),
            ..self.seen.clone()
        }
    }
}

/// The operation a record ends, if it ends one.
fn finished(record: &ControlRecord) -> Option<OperationId> {
    match record {
        ControlRecord::Result(result) => Some(result.operation),
        ControlRecord::Refusal(refusal) => Some(refusal.operation),
        _ => None,
    }
}

/// The request a job number stands for: one core, one GiB, a container; different numbers
/// are different work. The unplaceable job asks for more cores than any worker has and the
/// wide job for all of a worker's.
fn request(job: u8) -> Request {
    let cores = if job == UNPLACEABLE_JOB {
        1_000_000
    } else if job == WIDE_JOB {
        4_000
    } else {
        1_000
    };
    Request {
        key: WorkKey::new("job", format!("job-{job}")),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(cores, GIB),
        needs: js_caps::Request::default(),
        hermetic: true,
        do_not_cache: false,
        whole_worker: false,
    }
}

fn text(value: &[u8]) -> String {
    String::from_utf8(value.to_vec()).expect("a row this model wrote")
}

fn number(digits: &str) -> u64 {
    digits.parse().expect("a number this model wrote")
}

/// The three comma-separated fields of a row this model wrote.
fn fields(row: &str) -> [&str; 3] {
    let mut parts = row.splitn(3, ',');
    [(); 3].map(|()| parts.next().expect("a row of three fields"))
}

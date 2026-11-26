//! The delivery of jobs' completion callbacks (`docs/design/job-retry-and-completion-callback.md`,
//! sections 5.3 to 5.5): [`THREADS`] threads per server, off the tick's path, each trying one
//! due callback at a time.
//!
//! - **One try** ([`pass_with`], which [`Delivery::pass`] and the in-process seam make): claim one
//!   due callback from the store at its clock
//!   (`js_jobs::JobStore::claim_callbacks`, which holds it for this server for 30 s and expires
//!   one past its deadline instead), POST it with the [`Sender`] (no proxy, no redirect, no
//!   body read, [`SEND_LIMIT`]), and record the answer at the store's clock
//!   (`js_jobs::JobStore::answer_callback`): delivered, abandoned, or due again after a doubling
//!   wait. The `webhook-timestamp` is the claim's instant, in seconds. A claim that lapsed
//!   before its answer (another server took the row) records nothing: the other try counts. A
//!   store that fails records nothing either, and the row is tried again when its claim lapses.
//! - **The threads** ([`Deliverers`]): each tries again at once after a try, and otherwise
//!   waits [`IDLE`] for the next due callback. A stop ends each after its try in flight, which
//!   takes at most [`SEND_LIMIT`]. They run whether or not the job service takes callbacks, so a
//!   server of this release delivers the callback of a job another server took.
//! - **Where a call may go** ([`js_callback::AddressRule`]): the rule of section 5.3, with the
//!   cluster's own addresses denied besides: this server's listen addresses, its store hosts'
//!   ([`store_addresses`]) and the address each host's heartbeat comes from, kept in memory as
//!   they are learned (`Agents::cluster`).
//!
//! What is logged names the job, the result and the URL's host (the sender's lines), never the
//! URL's path or query, which may carry the receiver's own reference, and nothing of the key.
//! Each recorded answer is counted (`js_callback_deliveries_total{result}`).

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use js_callback::{
    AddressRule, CallbackAnswer, CallbackUrl, FailureClass, Resolve, Sender, SystemResolver,
    TlsRoots, check_url,
};
use js_jobs::{CallbackClaim, InstanceId, JobStoreError};
use js_state::StoreTime;
use tracing::{info, warn};

use crate::agents::Agents;
use crate::job_store::ServerJobs;
use crate::metrics::{Meter, callback_result};
use crate::util::blocking;

/// How many delivery threads a server runs (design section 4: fixed in code, not a setting).
pub(crate) const THREADS: usize = 8;

/// The longest one try may take, its lookup included.
pub(crate) const SEND_LIMIT: Duration = Duration::from_secs(10);

/// How long a thread whose last pass found nothing due waits before it looks again.
pub(crate) const IDLE: Duration = Duration::from_secs(1);

/// What a server's start does for its callbacks: makes the cluster's own addresses known to
/// the rule of `agents` (where it listens, `listening`, and the addresses of its store's hosts
/// `store_hosts`, resolved off the runtime's threads), and, when it holds the job tables, starts
/// [`THREADS`] delivery threads claiming as `claimer`.
pub(crate) async fn start(
    agents: &Arc<Agents>,
    listening: &[SocketAddr],
    store_hosts: Option<Vec<String>>,
    claimer: String,
) -> Option<Deliverers> {
    let hosts = store_hosts.unwrap_or_default();
    let resolved = blocking(move || store_addresses(&hosts, &SystemResolver)).await;
    let listened = listening.iter().map(SocketAddr::ip);
    for address in listened.chain(resolved) {
        agents.cluster.add_cluster_address(address);
    }
    let jobs = agents.jobs.clone()?;
    let sender = sender(agents.cluster.clone());
    let by = InstanceId::new(claimer);
    let delivery = Delivery::new(jobs, agents.meter().clone(), sender, by);
    Some(Deliverers::start(Arc::new(delivery), THREADS, IDLE))
}

/// The sender a server tries callbacks with: [`SEND_LIMIT`], `rule`, the system's resolver and
/// the WebPki roots.
pub(crate) fn sender(rule: AddressRule) -> Sender {
    Sender::new(
        SEND_LIMIT,
        rule,
        Box::new(SystemResolver),
        TlsRoots::webpki(),
    )
}

/// The addresses of the store's hosts `hosts` (as its URL names them), each of which is one of
/// the cluster's own: an address as written, a name as `resolver` resolves it within
/// [`SEND_LIMIT`]. A name that does not resolve adds nothing, and is logged.
pub(crate) fn store_addresses(hosts: &[String], resolver: &dyn Resolve) -> Vec<IpAddr> {
    let mut found = Vec::new();
    for host in hosts {
        let resolved = match host.parse::<IpAddr>() {
            Ok(address) => Ok(vec![address]),
            Err(_) => resolver.resolve(host, SEND_LIMIT),
        };
        match resolved {
            Ok(addresses) => found.extend(addresses),
            Err(error) => {
                warn!(%host, %error, "a store host did not resolve: callbacks are not denied its addresses");
            }
        }
    }
    found
}

/// What one delivery thread tries callbacks with: the job tables, the meter that counts each
/// answer, the sender, and the name this server claims callbacks by.
pub(crate) struct Delivery {
    /// The job tables, which hold each callback's row.
    jobs: Arc<ServerJobs>,
    /// Where each recorded answer is counted.
    meter: Meter,
    /// What tries a callback.
    sender: Sender,
    /// The name of this server's claims.
    by: InstanceId,
}

impl Delivery {
    /// Delivery from `jobs`, sending with `sender` and claiming as `by`, its answers counted by
    /// `meter`.
    pub(crate) fn new(
        jobs: Arc<ServerJobs>,
        meter: Meter,
        sender: Sender,
        by: InstanceId,
    ) -> Delivery {
        Delivery {
            jobs,
            meter,
            sender,
            by,
        }
    }

    /// One try of one due callback, when one is due (the module docs); whether it claimed
    /// one. A store that cannot be read claims none, and is logged.
    pub(crate) fn pass(&self) -> bool {
        let send = &mut |url: &CallbackUrl, claim: &CallbackClaim, timestamp: u64| {
            self.sender
                .send(url, &claim.key, &claim.webhook_id, timestamp)
        };
        pass_with(&self.jobs, &self.meter, &self.by, send)
    }
}

/// One try of one due callback, when one is due: the claim at the job tables' clock as `by`,
/// the check of its URL, `send` in place of the HTTP sender (handed the checked URL, the claim
/// and the `webhook-timestamp`, the claim's instant in seconds), and the recording and the
/// counting of its answer. The delivery threads ([`Delivery::pass`]) and the in-process seam
/// (`InProcess::deliver_callback`) both make their tries here. Whether it claimed one. A store
/// that cannot be read claims none, and is logged.
pub(crate) fn pass_with(
    jobs: &ServerJobs,
    meter: &Meter,
    by: &InstanceId,
    send: &mut dyn FnMut(&CallbackUrl, &CallbackClaim, u64) -> CallbackAnswer,
) -> bool {
    let claimed = jobs.now().and_then(|now| {
        let claims = jobs.claim_callbacks(by, now, 1)?;
        Ok((now, claims))
    });
    let (now, claims) = match claimed {
        Ok(claimed) => claimed,
        Err(error) => {
            warn!(%error, "the due callbacks could not be claimed");
            return false;
        }
    };
    let Some(claim) = claims.into_iter().next() else {
        return false;
    };
    deliver(jobs, meter, claim, now, send);
    true
}

/// Sends `claim`, claimed at `now`, with `send`, and records its answer.
fn deliver(
    jobs: &ServerJobs,
    meter: &Meter,
    claim: CallbackClaim,
    now: StoreTime,
    send: &mut dyn FnMut(&CallbackUrl, &CallbackClaim, u64) -> CallbackAnswer,
) {
    let job = claim.job.clone();
    let answer = match check_url(&claim.url) {
        Ok(url) => send(&url, &claim, now.as_duration().as_secs()),
        // A URL this build's submit would refuse (a later build's, or a row written past
        // it) is never sent: the try fails, and is made again until its deadline.
        Err(fault) => {
            warn!(%job, code = fault.code(), "a callback whose URL this build refuses is not sent");
            CallbackAnswer::Failed(FailureClass::Failed)
        }
    };
    let answered = jobs
        .now()
        .and_then(|at| jobs.answer_callback(claim, answer, at));
    let result = callback_result(&answer);
    match answered {
        Ok(()) => {
            meter.sink().callback_answered(&answer);
            info!(%job, result, "a callback was tried");
        }
        Err(JobStoreError::Stale { .. }) => {
            info!(%job, result, "a callback's claim lapsed before its answer was recorded");
        }
        Err(error) => {
            warn!(%job, result, %error, "a callback's answer was not recorded: it is tried again when its claim lapses");
        }
    }
}

/// Where the threads stand: whether they are to stop, whether a test woke them, and how many
/// passes they have made.
#[derive(Debug, Default)]
struct Turns {
    stop: bool,
    woken: bool,
    passes: u64,
}

/// What the threads wait on between passes.
#[derive(Debug, Default)]
struct Gate {
    turns: Mutex<Turns>,
    changed: Condvar,
}

impl Gate {
    fn lock(&self) -> MutexGuard<'_, Turns> {
        self.turns.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Counts a pass, then waits for the next one: none after a pass that claimed (`busy`),
    /// else until `idle` has passed, a wake, or the stop. Whether to make it.
    fn next(&self, busy: bool, idle: Duration) -> bool {
        let mut turns = self.lock();
        turns.passes += 1;
        self.changed.notify_all();
        let wait = if busy { Duration::ZERO } else { idle };
        let waiting = |turns: &mut Turns| !turns.stop & !turns.woken;
        let waited = self.changed.wait_timeout_while(turns, wait, waiting);
        let (mut turns, _) = waited.unwrap_or_else(PoisonError::into_inner);
        turns.woken = false;
        !turns.stop
    }

    /// Ends every thread after its pass in flight.
    fn stop(&self) {
        self.lock().stop = true;
        self.changed.notify_all();
    }
}

/// The delivery threads of a server.
pub(crate) struct Deliverers {
    gate: Arc<Gate>,
    threads: Vec<JoinHandle<()>>,
}

impl Deliverers {
    /// Starts `threads` threads that each try `delivery` (the module docs), waiting `idle`
    /// after a pass that found nothing due.
    pub(crate) fn start(delivery: Arc<Delivery>, threads: usize, idle: Duration) -> Deliverers {
        let gate = Arc::new(Gate::default());
        let threads = (0..threads).map(|_| {
            let (delivery, gate) = (Arc::clone(&delivery), Arc::clone(&gate));
            thread::spawn(move || while gate.next(delivery.pass(), idle) {})
        });
        Deliverers {
            threads: threads.collect(),
            gate,
        }
    }

    /// Stops the threads and waits for each to end its try in flight.
    pub(crate) fn stop(mut self) {
        self.gate.stop();
        for thread in std::mem::take(&mut self.threads) {
            // A thread that panicked has nothing left to stop.
            let _ = thread.join();
        }
        let passes = self.gate.lock().passes;
        info!(passes, "the callback delivery stopped");
    }
}

impl Drop for Deliverers {
    /// A server dropped without its stop ends its threads after their tries in flight, and
    /// waits for none.
    fn drop(&mut self) {
        self.gate.stop();
    }
}

#[cfg(test)]
#[path = "callbacks_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "callbacks_receiver.rs"]
pub(crate) mod receiver;

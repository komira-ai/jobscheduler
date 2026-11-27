//! Held heartbeats (design komira-ai/jobscheduler#903): the wakes that end the hold of a
//! heartbeat's reply as soon as a job is placed on its host.
//!
//! A heartbeat of a host whose agent reported [`js_agent_wire::HELD_BEAT_CAPABILITY`] is
//! watched ([`Wakes::watch`]) from before the store is read for its reply. When that reply has
//! nothing to send, the route holds it open ([`crate::agent_api`]) for at most [`hold_bound`];
//! a placement on the host ([`Wakes::wake`], which the pass calls for every placement it
//! commits) ends the hold, the reply is built again from the store and carries the start. A
//! wake that comes between the read and the hold is kept by the watch, so the hold ends at
//! once and none is lost. The agent sends its next heartbeat as soon as a held reply reaches
//! it, so one heartbeat of the host is nearly always waiting here.
//!
//! A heartbeat that lists a job as `starting` is held for [`starting_hold`] at most instead:
//! the agent sends that heartbeat as soon as its worker has taken the start, and the start's
//! `running` rides on the next one, so the hold is kept short, while a placement still wakes
//! it at once. A host that polls dispatch gets its starts from the poll, not in this reply, so
//! its beat with a start under way is not held for a bound but paced ([`Starts::paced`]): answered
//! at once after a quiet spell, and no more often than once per [`starting_hold`] while such
//! beats follow each other closely.
//!
//! The wakes are this process's, and a placement made by another server over a store they share
//! reaches them by that server's notice ([`crate::listening`]): a dispatch message written for
//! the host wakes its watch here as the pass's own placement does. A notice lost (the store one
//! process writes has none, and a listening connection that was down misses them) leaves the
//! start to the host's next heartbeat, at most one hold later. (A host that polls dispatch has a
//! held poll as well, watched by the wakes of [`crate::dispatch_state`]; the pass and a notice
//! wake both, and the poll rechecks the queue on a timer for what neither wakes.)

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use js_types::HostId;
use tokio::sync::{Notify, watch};

/// The longest a reply is held, whatever the interval: well inside the agent's ten-second
/// request limit (`js_agent::REQUEST_LIMIT`).
pub(crate) const MAX_HOLD: Duration = Duration::from_secs(5);

/// How long a reply with nothing to send is held: the pool's heartbeat `interval`, at most
/// [`MAX_HOLD`]. Beats arrive at most this far apart while they are held, never further apart
/// than the interval, so a host's silence is counted as it was.
pub(crate) fn hold_bound(interval: Duration) -> Duration {
    interval.min(MAX_HOLD)
}

/// The longest a reply is held while the beat lists a job as `starting`: the start's `running`
/// can reach the server only on the heartbeat after this one, so this is the most it waits for
/// the hold to end. It is long enough that a host with a start under way, which beats again at
/// once after each held reply, is not asked in a tight loop (five beats a second at most), and
/// short next to the agent's whole wait for the start. A placement ends the hold at once.
pub(crate) const STARTING_HOLD: Duration = Duration::from_millis(200);

/// How long a reply with nothing to send is held when the beat lists a job as `starting`:
/// [`STARTING_HOLD`], and never longer than [`hold_bound`] of the same `interval`.
pub(crate) fn starting_hold(interval: Duration) -> Duration {
    hold_bound(interval).min(STARTING_HOLD)
}

/// For how long, on the server's clock, after a hold first saw a job listed `starting` that job
/// takes the short hold ([`Starts::young`]): longer than nearly every start takes, so its
/// `running` is reported soon, and short enough that a start that waits for minutes (a provider,
/// an image) does not keep its host beating five times a second: after it the normal bound
/// applies, and `running` is reported at most one normal hold later.
pub(crate) const STARTING_WINDOW_MS: u64 = 2_000;

/// The jobs each host's held beats list as `starting`, each with the time (milliseconds, on the
/// server's clock) a hold first saw it so. Only the jobs the last held beat of a host listed are
/// kept, and a host whose last held beat listed none is not kept.
///
/// For a host that polls dispatch it also keeps when a beat of the host that listed a start was
/// last answered ([`Starts::paced`]), which spaces those answers, while that spacing lasts.
#[derive(Default)]
pub(crate) struct Starts {
    /// By host, by job id. Its lock is a leaf.
    seen: Mutex<BTreeMap<HostId, BTreeMap<String, u64>>>,
    /// By host, when (milliseconds, on the server's clock) a paced beat of the host was last
    /// answered; a host whose spacing has passed is forgotten at the next answer. Its lock is a
    /// leaf.
    answered: Mutex<BTreeMap<HostId, u64>>,
}

impl Starts {
    /// Records that a held beat of `host` lists the jobs `starting` as `starting` at `now_ms`,
    /// and says whether one of them was first seen so less than [`STARTING_WINDOW_MS`] ago (a
    /// job not seen before is first seen now). A job the beat does not list is forgotten, so one
    /// listed `starting` again later counts from then.
    pub(crate) fn young(&self, host: &HostId, starting: &[&str], now_ms: u64) -> bool {
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        let before = seen.remove(host).unwrap_or_default();
        let now: BTreeMap<String, u64> = starting
            .iter()
            .map(|id| (id.to_string(), before.get(*id).copied().unwrap_or(now_ms)))
            .collect();
        let young = now
            .values()
            .any(|first| now_ms.saturating_sub(*first) < STARTING_WINDOW_MS);
        if !now.is_empty() {
            seen.insert(host.clone(), now);
        }
        young
    }

    /// How long a beat of `host` that lists a start, arriving at `now_ms`, waits before it is
    /// answered: what is left of `spacing` since the last such beat of the host was answered
    /// ([`Starts::answered`]), nothing when that was `spacing` ago or more, or never. So the
    /// first of them after a quiet spell is answered at once, and those that follow it closely
    /// are answered no more often than once per `spacing`.
    pub(crate) fn paced(&self, host: &HostId, now_ms: u64, spacing: Duration) -> Duration {
        let answered = self.answered.lock().unwrap_or_else(PoisonError::into_inner);
        let spacing_ms = u64::try_from(spacing.as_millis()).unwrap_or(u64::MAX);
        let free_ms = answered
            .get(host)
            .map_or(0, |last| last.saturating_add(spacing_ms));
        Duration::from_millis(free_ms.saturating_sub(now_ms))
    }

    /// Records that a beat of `host` that listed a start was answered at `now_ms`, and forgets
    /// every host whose last answer is `spacing` or more before it (its next beat is answered at
    /// once either way), so the record holds only the hosts whose spacing still lasts.
    pub(crate) fn answered(&self, host: &HostId, now_ms: u64, spacing: Duration) {
        let mut answered = self.answered.lock().unwrap_or_else(PoisonError::into_inner);
        let spacing_ms = u64::try_from(spacing.as_millis()).unwrap_or(u64::MAX);
        answered.retain(|_, last| last.saturating_add(spacing_ms) > now_ms);
        answered.insert(host.clone(), now_ms);
    }

    /// How many hosts the pacing record holds.
    #[cfg(test)]
    pub(crate) fn paced_hosts(&self) -> usize {
        self.answered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

/// The heartbeats being watched, one per host, and how often a held reply began to wait.
#[derive(Clone)]
pub(crate) struct Wakes {
    /// The wake of each host whose heartbeat is watched. Its lock is a leaf.
    watched: Arc<Mutex<BTreeMap<HostId, Arc<Notify>>>>,
    /// How many times a held reply began to wait for a wake: the progress a test waits for.
    waits: Arc<watch::Sender<u64>>,
    /// The bound the last hold that began was given: what a test reads to see which it took.
    bound: Arc<Mutex<Option<Duration>>>,
}

impl Default for Wakes {
    fn default() -> Self {
        Wakes {
            watched: Arc::default(),
            waits: Arc::new(watch::Sender::new(0)),
            bound: Arc::default(),
        }
    }
}

impl Wakes {
    fn watched(&self) -> MutexGuard<'_, BTreeMap<HostId, Arc<Notify>>> {
        self.watched.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Watches the heartbeat of `host` until the watch is dropped. A later watch of the same
    /// host (a second heartbeat while one is held) takes over; the first is then woken no more
    /// and ends its hold at the bound.
    pub(crate) fn watch(&self, host: &HostId) -> Watch {
        let wake = Arc::new(Notify::new());
        self.watched().insert(host.clone(), Arc::clone(&wake));
        Watch {
            wakes: self.clone(),
            host: host.clone(),
            wake,
        }
    }

    /// Wakes the watched heartbeat of `host`, if there is one: a hold under way ends now, and
    /// one that has not begun ends as soon as it begins. Nothing for a host not watched.
    pub(crate) fn wake(&self, host: &HostId) {
        if let Some(wake) = self.watched().get(host) {
            wake.notify_one();
        }
    }

    /// Wakes every watched heartbeat once, as [`Wakes::wake`] wakes one: for when a wake may
    /// have been missed (the connection that hears other servers' notices was made again).
    pub(crate) fn wake_all(&self) {
        self.watched().values().for_each(|wake| wake.notify_one());
    }

    /// Counts that a held reply begins to wait for a wake: when its hold begins, and again
    /// after each wake that brought nothing to send.
    pub(crate) fn waiting(&self) {
        self.waits.send_modify(|waits| *waits += 1);
    }

    /// Notes that a hold begins with `bound`, before its first wait.
    pub(crate) fn holding_for(&self, bound: Duration) {
        *self.bound.lock().unwrap_or_else(PoisonError::into_inner) = Some(bound);
    }

    /// The bound the last hold that began was given, `None` before any.
    #[cfg(test)]
    pub(crate) fn last_bound(&self) -> Option<Duration> {
        *self.bound.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// How many times a held reply began to wait, as a receiver a test waits on.
    #[cfg(test)]
    pub(crate) fn waits(&self) -> watch::Receiver<u64> {
        self.waits.subscribe()
    }
}

/// The watch of one host's heartbeat ([`Wakes::watch`]).
pub(crate) struct Watch {
    wakes: Wakes,
    host: HostId,
    wake: Arc<Notify>,
}

impl Watch {
    /// Waits for a placement on the host since the watch began, or since the last wake this
    /// returned for.
    pub(crate) async fn woken(&self) {
        self.wake.notified().await;
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        let mut watched = self.wakes.watched();
        let ours = watched
            .get(&self.host)
            .is_some_and(|wake| Arc::ptr_eq(wake, &self.wake));
        if ours {
            watched.remove(&self.host);
        }
    }
}

#[cfg(test)]
#[path = "held_beat_tests.rs"]
mod tests;

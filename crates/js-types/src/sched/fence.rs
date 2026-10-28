//! Lease timing: the re-dispatch grace G on the scheduler's side and the self-fence T
//! on the worker's side.
//!
//! G also bounds how long the scheduler keeps a committed lease that a worker it still
//! hears from does not list as running ([`START_GRACE`]), and with T it bounds how late
//! a worker may act on a `Start` ([`START_VALIDITY`]).
//!
//! The scheduler re-dispatches a worker's leases once it has heard nothing from the
//! worker for G. A worker holding a [`FencePolicy::SelfFence`] lease stops it once its
//! newest acknowledged heartbeat was *sent* more than T ago. A heartbeat's send time is
//! no later than the moment the scheduler heard it, so the worker stops at most T after
//! the scheduler last heard it, and the scheduler re-dispatches no sooner than G after
//! that. `T + LEADER_LEASE_MARGIN < G` keeps the two copies apart even across a leader
//! change (leaders acknowledge only inside their leader lease).
//!
//! [`FencePolicy::SelfFence`]: super::FencePolicy::SelfFence

use std::time::Duration;

/// G: how long the scheduler waits after last hearing a worker before it re-dispatches
/// that worker's leases.
pub const LEASE_GRACE: Duration = Duration::from_secs(60);

/// How long after sending a lease's `Start` the scheduler keeps the lease while the
/// worker's heartbeats leave it out of their running set. Until then the `Start` may
/// still be on its way, and a heartbeat sent before it arrived rightly omits it; after
/// that the lease is taken as lost and its operation requeued. The fence
/// (`docs/design/scheduler.md#fencing-g-and-t`) has one wait before re-dispatch, G;
/// this is G, counted from the `Start`. It assumes a `Start` that arrives later than
/// that is not run: a worker that refuses a `Start` older than [`START_VALIDITY`] makes
/// it hold.
pub const START_GRACE: Duration = LEASE_GRACE;

/// T: how long after sending its newest acknowledged heartbeat a worker keeps running a
/// self-fenced lease.
pub const SELF_FENCE: Duration = Duration::from_secs(40);

/// The slack the safety argument keeps between T and G.
pub const LEADER_LEASE_MARGIN: Duration = Duration::from_secs(5);

// The safety condition `T + 5 s < G` (`docs/design/scheduler.md#fencing-g-and-t`).
// Changing a constant so that it fails is a compile error, not a silent overlap of two
// runs.
const _: () =
    assert!(SELF_FENCE.as_millis() + LEADER_LEASE_MARGIN.as_millis() < LEASE_GRACE.as_millis());

/// How long the scheduler keeps the leases of a node's replaced daemon process, counted
/// from when it last heard the node before another process registered as it Two processes may hold one node's certificate (a cloned machine, a second
/// daemon started, a node replaced while the old one runs), and the newer one cannot say
/// what the older runs. The older one's stream is not acknowledged once it is replaced,
/// so it stops self-fenced work at most T after the scheduler last heard it; the margin
/// is the one G keeps over T.
pub const HANDOVER_GRACE: Duration = SELF_FENCE.saturating_add(LEADER_LEASE_MARGIN);

const _: () = assert!(HANDOVER_GRACE.as_millis() < LEASE_GRACE.as_millis());

/// How long the scheduler keeps a whole-machine lease while its worker switches the
/// Mac's GUI session (it reported [`NodePhase::Preparing`](super::NodePhase::Preparing)
/// or [`NodePhase::Restoring`](super::NodePhase::Restoring) for it), counted from the
/// first report of that
/// phase. A switch is a userspace restart, which ends the daemon with everything else,
/// so the worker is silent for minutes; nothing of the lease runs during a switch (the
/// action has not started, or has ended with its result kept on disk), so keeping the
/// lease past G cannot run it twice. Past this bound a silent worker's lease goes back
/// to the queue. A daemon that dies while the action runs is no switch: the restarted
/// daemon kills the action's processes before its `Hello` and reports the run lost
/// (`Failure::Lost`, its result gone) once the node is restored, and the scheduler
/// runs it again as a farm fault; but until the daemon is back those processes live on
/// (the helper, not the daemon, runs them), beside a retry elsewhere once the lease is
/// given up. The daemon gives up waiting for the session sooner
/// (`kbf_driver_native`'s `SESSION_WAIT`), so it does not start the action after this.
pub const SESSION_SWITCH_GRACE: Duration = Duration::from_secs(600);

const _: () = assert!(SESSION_SWITCH_GRACE.as_millis() > LEASE_GRACE.as_millis());

/// W: how late a worker may act on a `Start`, counted from when it sent the newest
/// heartbeat the scheduler had heard when the `Start` went out. The `Start` left after
/// that heartbeat arrived, so a `Start` acted on inside W was in flight for less than W,
/// whatever the delay, and the worker measures it on its own clock.
///
/// Why `W + T + LEADER_LEASE_MARGIN < START_GRACE` keeps a late `Start` from running
/// beside its retry. A lease given up to silence is covered by the self-fence, and one
/// given up to a new session by the old stream having ended. That leaves a heartbeat of
/// the same session, taken [`START_GRACE`] or more after the `Start` was sent, that
/// leaves the lease out. A heartbeat sent after the worker started the lease
/// lists it, so that heartbeat was sent earlier, less than W after the `Start` left.
/// Heartbeats on a stream are taken in order, so no heartbeat sent after it is
/// acknowledged before the scheduler gives the lease up, and the worker's self-fence
/// stops the run less than `W + T` after the `Start` left: before the retry exists.
pub const START_VALIDITY: Duration = Duration::from_secs(14);

const _: () = assert!(
    START_VALIDITY.as_millis() + SELF_FENCE.as_millis() + LEADER_LEASE_MARGIN.as_millis()
        < START_GRACE.as_millis()
);

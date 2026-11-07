//! The scenarios of the whole-host fault family (`whole_host_faults.rs`) but the
//! cancelled request, which the replay tests also run (`whole_host_cancel.rs`).
//!
//! Sizes, in agent requests a seed (heartbeats every 5 s, the server's default, with the
//! hellos besides), each far under the 30,000 a seed of the cell's sizing rule:
//! - the silent hosts: [`SILENT_HOSTS`] for [`SILENT_LAST`] plus [`SILENT_SETTLE`],
//!   900 s, make 1,800;
//! - a refusal: [`REFUSE_HOSTS`] for [`REFUSE_SETTLE`], 600 s, make 480, five times;
//! - the drain: [`DRAIN_HOSTS`] for [`DRAIN_ASK_AT`] plus [`DRAIN_SETTLE`], 960 s, make
//!   768;
//! - the race: [`RACE_HOSTS`] for [`RACE_SETTLE`], 600 s, make 240, eleven times;
//! - the restarts: [`RESTART_HOSTS`] for [`RESTART_AT`]'s last plus [`RESTART_SETTLE`],
//!   1,000 s, make 600;
//! - the held lease: [`HELD_HOSTS`] for [`HELD_RESTART_AT`] plus [`HELD_SETTLE`], 960 s,
//!   make 384.

use std::time::Duration;

use js_agent_wire::RefusalReason;
use js_sim_cell::actions::{
    agent_prepare_delay, agent_refuses_prepare, agent_silent_on_prepare, agents, replace_agents,
    submit_work,
};
use js_sim_cell::{Expect, HowEnded, ItemId, Scenario, ServerSpec, Shape, Work};

/// A whole-node pool of `hosts` hosts.
fn pool(hosts: usize) -> ServerSpec {
    ServerSpec::whole_node_pool("burst", Shape::gpu_host(), hosts)
}

/// The silent hosts: ten hosts, the first three silent on every prepare.
const SILENT_HOSTS: usize = 10;
const SILENT: std::ops::Range<usize> = 0..3;
/// The pool's prepare deadline, host backoff and failures before a request is refused:
/// a short deadline, so that three of them pass well inside one backoff.
const SILENT_PREPARE: Duration = Duration::from_secs(60);
const SILENT_BACKOFF: Duration = Duration::from_secs(300);
const SILENT_MAX_REPLACE: u32 = 3;
/// The first requests, and their runs, short enough that every host that reports ready
/// is free again when the silent hosts' prepares fail.
const SILENT_FIRST: u32 = 8;
const SILENT_RUN: Duration = Duration::from_secs(10);
/// When every agent is made silent and the last request is made, and how long the cell
/// runs after it.
const SILENT_LAST: Duration = Duration::from_secs(300);
const SILENT_SETTLE: Duration = Duration::from_secs(600);

/// Proves the failed prepare of design section 5.4 and section 7's prepare deadline over
/// many hosts: a host whose agent never answers a prepare fails it at the pool's
/// `prepare_timeout`, the request goes to another host and is held there once that
/// host's agent reports ready, the failed host is left out of the request's tries for the
/// pool's `reserve_backoff`, and a request whose prepare fails `max_replace` times is
/// refused `exclusive-not-ready`. Ten hosts, the agents of the first three
/// ([`SILENT`]) silent on every prepare. At time zero [`SILENT_FIRST`] whole-host
/// requests, more than the hosts that answer, so at least one is committed on a silent
/// host: each such request fails its prepare after
/// [`SILENT_PREPARE`] and is held on a host that reported ready (`held-means-ready`
/// reads the report). At [`SILENT_LAST`] every agent goes silent and one more request
/// is made: it meets a silent host at every try and is refused after exactly
/// [`SILENT_MAX_REPLACE`] tries. Which request takes which host, and the order the
/// wait list serves (row 11, advisory), are not pinned. Red for a server that holds a
/// lease no agent reported ready, retries a request on the host whose prepare it failed
/// within the backoff (`kept-off-a-failed-host`, read from the server's listing), never
/// fails a silent prepare (no request would be held after a failed try), or refuses a
/// request after more or fewer failures than `max_replace` (`tries(item8)`).
pub fn silent_hosts() -> Scenario {
    let spec = pool(SILENT_HOSTS)
        .prepare_timeout(SILENT_PREPARE)
        .reserve_backoff(SILENT_BACKOFF)
        .max_replace(SILENT_MAX_REPLACE);
    let all = agents(0..SILENT_HOSTS);
    let run = Work::whole_host(SILENT_RUN);
    let last = ItemId(SILENT_FIRST);
    let tries = SILENT_MAX_REPLACE as usize;
    Scenario::new("a_silent_host_fails_the_prepare_and_the_request_goes_to_another_host")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(spec)
        .hosts("burst", SILENT_HOSTS)
        .at(Duration::ZERO, agent_silent_on_prepare(agents(SILENT)))
        .work(SILENT_FIRST, run.clone())
        .at(SILENT_LAST, agent_silent_on_prepare(all))
        .at(SILENT_LAST, submit_work(1, run))
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(
            SILENT_FIRST as usize,
            HowEnded::Succeeded,
        ))
        .expect(Expect::terminal_count(1, HowEnded::Failed))
        .expect(Expect::held_and_released(SILENT_FIRST as usize))
        .expect(Expect::held_after_a_failed_try(1))
        .expect(Expect::tries(last, tries))
        .expect(Expect::kept_off_a_failed_host(SILENT_BACKOFF))
        .expect(Expect::every_host_serving())
        .settle_within(SILENT_SETTLE)
}

/// A refusal: four hosts, four requests, the first host's agent refusing.
const REFUSE_HOSTS: usize = 4;
const REFUSE_RUN: Duration = Duration::from_secs(60);
const REFUSE_BACKOFF: Duration = Duration::from_secs(300);
const REFUSE_SETTLE: Duration = Duration::from_secs(600);

/// What the scenario expects of the hosts' service after `reason`. The match names every
/// variant of `RefusalReason` and no wildcard, so a reason the wire gains does not
/// compile here until the scenario says what it expects of it: `host-unsafe` taints the
/// host for good (whole-node design, the refusal table), so it alone ends out of service.
fn serving_after(reason: RefusalReason) -> Expect {
    match reason {
        RefusalReason::HostUnsafe => Expect::serving_but(&[0]),
        RefusalReason::NotQuiet
        | RefusalReason::NoiseOverBound
        | RefusalReason::SetupFailed
        | RefusalReason::Unsupported => Expect::every_host_serving(),
    }
}

/// Proves design section 5.4's refused prepare for `reason`: a host whose agent refuses
/// a prepare ends the lease at the refusal, the request goes to another host and is held
/// there once that host's agent reports ready, and the refusing host is left out of the
/// request's tries for the pool's `reserve_backoff` (excluded until then, for life for
/// `unsupported`, or tainted for `host-unsafe`). Four hosts, the first host's agent
/// refusing every prepare with `reason`, and four requests at time zero, so one is
/// committed on the refusing host. Red for a refusal the server
/// does not act on (that request would never be held: four held and released), a retry
/// on the refusing host inside the backoff (`kept-off-a-failed-host`), a lease held
/// without a ready report, and a `host-unsafe` refusal that leaves the host serving, or
/// another that does not (`hosts-not-serving`, `every-host-serving`).
pub fn a_refusal(reason: RefusalReason) -> Scenario {
    let name = "a_refused_prepare_goes_elsewhere_for_each_refusal_reason";
    let spec = pool(REFUSE_HOSTS).reserve_backoff(REFUSE_BACKOFF);
    let requests = REFUSE_HOSTS as u32;
    Scenario::new(&format!("{name}-{}", String::from(reason)))
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(spec)
        .hosts("burst", REFUSE_HOSTS)
        .at(Duration::ZERO, agent_refuses_prepare(agents(0..1), reason))
        .work(requests, Work::whole_host(REFUSE_RUN))
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(REFUSE_HOSTS, HowEnded::Succeeded))
        .expect(Expect::held_and_released(REFUSE_HOSTS))
        .expect(Expect::held_after_a_failed_try(1))
        .expect(Expect::kept_off_a_failed_host(REFUSE_BACKOFF))
        .expect(serving_after(reason))
        .settle_within(REFUSE_SETTLE)
}

/// The drain: four hosts held by four requests' long runs, and a fifth request.
const DRAIN_HOSTS: usize = 4;
const DRAIN_BUSY: Duration = Duration::from_secs(300);
const DRAIN_RUN: Duration = Duration::from_secs(60);
const DRAIN_ASK_AT: Duration = Duration::from_secs(60);
const DRAIN_SETTLE: Duration = Duration::from_secs(900);

/// Proves design section 6.4, "drain never cuts work": a whole-host request that finds
/// every host busy waits for a host's work to end, and nothing on a host is stopped for
/// it. Four hosts, each held by a request whose run lasts [`DRAIN_BUSY`]; at
/// [`DRAIN_ASK_AT`] a fifth request asks for a whole host. It waits (as built a held host
/// is never set aside: the server marks a host for the request, and
/// `drain-kills-nothing` watches it, once a host is released and restores) and is held
/// on a host only after every run launched there before its grant has completed. Every
/// run succeeds: none is cut. Red for a server that stops or cancels a run to make room
/// (the run would not succeed; `drain-kills-nothing` while the host is set aside), grants
/// the request before its host's run ended (`held-after-the-runs-on-its-host`,
/// `whole-host-is-exclusive`), or drops the waiting request.
pub fn a_drain() -> Scenario {
    let requests = DRAIN_HOSTS as u32;
    let waiting = ItemId(requests);
    Scenario::new("drain_waits_for_a_busy_host_and_cuts_no_run")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(pool(DRAIN_HOSTS))
        .hosts("burst", DRAIN_HOSTS)
        .work(requests, Work::whole_host(DRAIN_BUSY))
        .at(DRAIN_ASK_AT, submit_work(1, Work::whole_host(DRAIN_RUN)))
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(DRAIN_HOSTS + 1, HowEnded::Succeeded))
        .expect(Expect::held_and_released(DRAIN_HOSTS + 1))
        .expect(Expect::held_after_the_runs_on_its_host(waiting))
        .expect(Expect::every_host_serving())
        .settle_within(DRAIN_SETTLE)
}

/// The race: two hosts, the first host's agent reporting ready a set time after it is
/// told, around a short prepare deadline; the steps of the sweep, in whole seconds either
/// side of the deadline.
const RACE_HOSTS: usize = 2;
const RACE_PREPARE: Duration = Duration::from_secs(60);
pub const RACE_SWEEP: std::ops::RangeInclusive<i64> = -5..=5;
const RACE_BACKOFF: Duration = Duration::from_secs(300);
const RACE_RUN: Duration = Duration::from_secs(10);
const RACE_SETTLE: Duration = Duration::from_secs(600);

/// [`RACE_PREPARE`] moved by `step` whole seconds.
pub fn race_delay(step: i64) -> Duration {
    let deadline = RACE_PREPARE.as_secs().saturating_add_signed(step);
    Duration::from_secs(deadline)
}

/// Proves section 7's "Whole-host ready report races the prepare deadline" (the registry
/// is asked first and the lease expires only if there is still no answer): two hosts,
/// two requests at time zero, so one is committed on each; the
/// first host's agent reports ready `delay` after it was told to prepare, and the pool's
/// deadline is [`RACE_PREPARE`]. Whichever wins, the request on the first host is held
/// there with the host ready, or its prepare fails and it goes to the other host, never
/// both: no host books a second lease while a whole-host item holds it, a lease is held
/// only after a ready report for it, and the failed host is left out of the retry.
/// Which delay wins is not pinned (it rests on where the agent's heartbeats fall against
/// the server's ticks). Red for a ready report that wins a lease the server also fails
/// (the host restored under a held lease, then booked again: `whole-host-is-exclusive`),
/// a lease held on a late report (`held-means-ready` holds, but the request would
/// also be retried: `kept-off-a-failed-host` or a booking on the held host), and a
/// request lost in the race (two held and released).
pub fn a_race(delay: Duration) -> Scenario {
    let name = "a_ready_report_racing_the_prepare_deadline_never_double_books";
    let spec = pool(RACE_HOSTS)
        .prepare_timeout(RACE_PREPARE)
        .reserve_backoff(RACE_BACKOFF);
    let requests = RACE_HOSTS as u32;
    Scenario::new(&format!("{name}-{}s", delay.as_secs()))
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(spec)
        .hosts("burst", RACE_HOSTS)
        .at(Duration::ZERO, agent_prepare_delay(agents(0..1), delay))
        .work(requests, Work::whole_host(RACE_RUN))
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(RACE_HOSTS, HowEnded::Succeeded))
        .expect(Expect::held_and_released(RACE_HOSTS))
        .expect(Expect::kept_off_a_failed_host(RACE_BACKOFF))
        .expect(Expect::every_host_serving())
        .settle_within(RACE_SETTLE)
}

/// The restarts: three hosts, three requests, the first host's agent slow to prepare
/// and started again three times while its host is committed.
const RESTART_HOSTS: usize = 3;
const RESTART_PREPARE_DELAY: Duration = Duration::from_secs(200);
const RESTART_RUN: Duration = Duration::from_secs(60);
const RESTART_AT: [Duration; 3] = [
    Duration::from_secs(20),
    Duration::from_secs(60),
    Duration::from_secs(100),
];
const RESTART_SETTLE: Duration = Duration::from_secs(900);

/// Proves section 7's "Flapping agent during whole-host commit" as far as it is not
/// Open: the agent of a committed host says hello again several times before it reports
/// ready, and nothing is booked twice. Three hosts, three requests at time zero, so one
/// is committed on each; the first host's agent takes
/// [`RESTART_PREPARE_DELAY`] to prepare, longer than the restarts last, and is started
/// again with empty state ([`RESTART_AT`], `replace_agents`), each new process saying
/// hello once the server holds the host lost. Invariants only: whether the request is
/// then held on that host or fails and goes elsewhere, and when the commit's deadline
/// falls after a hello (section 8, row 26, and the open question of whether a hello
/// restarts a placed job's prepare), are not pinned. Red for a server that books the
/// host to a second lease across a hello (`whole-host-is-exclusive`,
/// `no-double-booking`), holds the lease on a ready report of an earlier process for
/// another lease (`held-means-ready`), leaves a booking behind (`bookings-match-agents`)
/// or never settles the request (`settles-when-quiet`).
pub fn restarts() -> Scenario {
    let first = agents(0..1);
    let requests = RESTART_HOSTS as u32;
    let [one, two, three] = RESTART_AT;
    Scenario::new("an_agent_that_restarts_while_the_host_is_committed_does_not_double_book")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(pool(RESTART_HOSTS))
        .hosts("burst", RESTART_HOSTS)
        .at(
            Duration::ZERO,
            agent_prepare_delay(first.clone(), RESTART_PREPARE_DELAY),
        )
        .work(requests, Work::whole_host(RESTART_RUN))
        .at(one, replace_agents(first.clone()))
        .at(two, replace_agents(first.clone()))
        .at(three, replace_agents(first))
        .settle_within(RESTART_SETTLE)
}

/// The held lease: two hosts held by two requests' long runs, a third request waiting,
/// and the first host's agent started again while its lease is held.
const HELD_HOSTS: usize = 2;
const HELD_BUSY: Duration = Duration::from_secs(300);
const HELD_RUN: Duration = Duration::from_secs(60);
const HELD_ASK_AT: Duration = Duration::from_secs(10);
const HELD_RESTART_AT: Duration = Duration::from_secs(60);
const HELD_SETTLE: Duration = Duration::from_secs(900);

/// Proves design section 8, row 25, as far as the lease front reaches it: a held
/// whole-host lease whose host's agent is started again with empty state is not booked
/// to a second lease. Two hosts held by two requests whose runs last [`HELD_BUSY`]; a
/// third request waits from [`HELD_ASK_AT`]; at [`HELD_RESTART_AT`] the first host's
/// agent is started again (`replace_agents`), says hello once the server holds the
/// host lost, holding no run. The hello ends the held lease's run (the item ends lost,
/// or failed, which is not pinned), so its client releases the lease; the waiting
/// request is then held on the restarted host and launches its run there
/// (`launched-on`), and the other host's run is not touched (two items succeed). The
/// row's hazard, the registry ending a held booking (a hello that voids `Ready`, then a
/// prepare deadline) that the lease table keeps, needs the client to keep the lease past
/// the hello's deadline; this front releases it within a poll of the run's end, so the
/// scenario shows the hazard does not arise along this path. Red for a host booked to
/// the waiting request while the restarted host's lease is still held
/// (`whole-host-is-exclusive`, `no-double-booking`), a lease held again without a ready
/// report, a booking left behind, or the other host's run cut.
pub fn held_lease_restart() -> Scenario {
    let requests = HELD_HOSTS as u32;
    Scenario::new("a_whole_host_lease_whose_agent_restarts_after_it_is_held")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(pool(HELD_HOSTS))
        .hosts("burst", HELD_HOSTS)
        .work(requests, Work::whole_host(HELD_BUSY))
        .at(HELD_ASK_AT, submit_work(1, Work::whole_host(HELD_RUN)))
        .at(HELD_RESTART_AT, replace_agents(agents(0..1)))
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(HELD_HOSTS, HowEnded::Succeeded))
        .expect(Expect::held_and_released(HELD_HOSTS + 1))
        .expect(Expect::launched_on(&[0], HELD_RESTART_AT))
        .expect(Expect::every_host_serving())
        .settle_within(HELD_SETTLE)
}

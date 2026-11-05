//! The whole-host placement and failover tests of the lease server's HTTP suite
//! (`crates/js-lease-server/tests/server/`), moved onto the cell: one scenario per moved
//! test, named by it, in which every assertion of the old test is an [`Expect`] judged at
//! the same virtual time, or at the end.
//!
//! The old tests played one or two hosts by hand over loopback HTTP, so each asserted one
//! interleaving. Here the same server runs under the kernel with the cell's agents, which
//! say hello, beat on their interval and answer the whole-host handshake themselves (the
//! old tests set a two-hour grace so that they need not beat), and every invariant of
//! `Invariant::ALL` is checked after every step. Each scenario keeps the old test's world:
//! the pool keys it set, its host count, its request (a whole-node lease of the share the
//! old `lease_body` asks for, under no request id or under `nightly-42`) and the virtual
//! time of every decision, counted from [`ASK`], the moment the first request is made
//! (the old tests' `T0`).
//!
//! Each runs in the old world first and then at [`SCALE_HOSTS`] hosts, each under every
//! value of `--placement`, over [`SEEDS`] seeds: the old test plus more. An expectation
//! that names a host, or holds only because the old world has one host, is added to the
//! old world alone, by [`World::Old`], and its doc comment says so; every other one, and
//! every invariant, holds at every scale.
//!
//! Each expectation that carries an old assertion is a function here whose doc comment
//! names the old test and quotes the assertion, so a search for the old test's name
//! finds its new home.
//!
//! Sizes, in agent requests a seed: at [`SCALE_HOSTS`] hosts beating every 5 s, a
//! scenario runs [`SETTLE`] past its last expectation, at most 200 s after [`ASK`]: under
//! 50 * 805 / 5 = 8,050 heartbeats a seed, with the fifty hellos besides; the old worlds,
//! one or two hosts, under 330.

use std::time::Duration;

use clap::ValueEnum;
use js_lease_server::Placement;
use js_registry::Exclusion;
use js_sim_cell::actions::{
    agent_prepare_delay, agent_silent_on_prepare, agents, cancel_work, items, submit_work,
};
use js_sim_cell::server::wall_time;
use js_sim_cell::work::default_needs;
use js_sim_cell::{
    Booked, Call, Expect, HostRef, HowEnded, ItemId, Listed, RecordFact, Scenario, ScenarioFailure,
    ServerSpec, Shape, WHOLE_CLASS, Work,
};
use js_types::sched::FarmTime;
use js_types::{ClassId, Request};
use serde_json::json;

/// How many seeds each world runs.
const SEEDS: u64 = 16;

/// The host count of the scaled worlds.
const SCALE_HOSTS: usize = 50;

/// How long a scenario runs after its last action or expectation, before the end check.
const SETTLE: Duration = Duration::from_secs(600);

/// When the first request of each scenario is made: the server's first tick, by which
/// every agent has said hello. The old tests' `T0`.
const ASK: Duration = Duration::from_secs(5);

/// The `prepare_timeout` every moved silent-prepare test set.
const PREPARE_TIMEOUT: Duration = Duration::from_secs(60);

/// The wall limit of the old request (`lease_body`'s `max_wall_seconds`).
const OLD_WALL: Duration = Duration::from_secs(7_200);

/// The request id the old `whole_request_id.rs` tests asked under.
const ID: &str = "nightly-42";

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// `n` seconds after [`ASK`].
fn after(n: u64) -> Duration {
    ASK + secs(n)
}

/// `n` milliseconds after [`ASK`].
fn after_ms(n: u64) -> Duration {
    ASK + Duration::from_millis(n)
}

/// A world a moved scenario runs in.
#[derive(Clone, Copy, Debug)]
enum World {
    /// The old test's host count, under this placement.
    Old(Placement),
    /// [`SCALE_HOSTS`] hosts, under this placement.
    Scale(Placement),
}

impl World {
    /// The host count, `old` in the old world.
    fn hosts(self, old: usize) -> usize {
        match self {
            World::Old(_) => old,
            World::Scale(_) => SCALE_HOSTS,
        }
    }

    fn placement(self) -> Placement {
        match self {
            World::Old(placement) | World::Scale(placement) => placement,
        }
    }
}

/// Every world, the old one first: the old host count under every placement the server
/// accepts, then [`SCALE_HOSTS`] under every one.
fn worlds() -> Vec<World> {
    let placements = Placement::value_variants();
    let old = placements.iter().map(|p| World::Old(*p));
    old.chain(placements.iter().map(|p| World::Scale(*p)))
        .collect()
}

/// `placement` as the server's command line spells it.
fn spelled(placement: Placement) -> String {
    let value = placement
        .to_possible_value()
        .expect("every placement has a name");
    value.get_name().to_owned()
}

/// How the scenario's agents answer a prepare.
#[derive(Clone, Copy, Debug)]
enum Answering {
    /// They never report ready, as the old tests' hand-played agents did not.
    Silent,
    /// They report ready at their next heartbeat.
    Ready,
}

/// A whole-node pool of `hosts` hosts under `world`'s placement.
fn whole_pool(hosts: usize, world: World) -> ServerSpec {
    ServerSpec::whole_node_pool("burst", Shape::gpu_host(), hosts).placement(world.placement())
}

/// The scenario `name` in `world`: the server `spec` and `hosts` hosts, whose agents
/// answer a prepare as `answering` says from the start.
fn base(
    name: &str,
    world: World,
    (spec, hosts): (ServerSpec, usize),
    answering: Answering,
) -> Scenario {
    let label = match world {
        World::Old(_) => "old",
        World::Scale(_) => "scale",
    };
    let named = format!("{name}-{label}-{}", spelled(world.placement()));
    let scenario = Scenario::new(&named)
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(spec)
        .hosts("burst", hosts)
        .settle_within(SETTLE);
    let all = agents(0..hosts);
    match answering {
        Answering::Silent => scenario.at(Duration::ZERO, agent_silent_on_prepare(all)),
        Answering::Ready => scenario.at(Duration::ZERO, agent_prepare_delay(all, Duration::ZERO)),
    }
}

/// A whole host asked for under no request id, given up `wall` after it is asked (the
/// lease's wall limit, and so part of the request the server keys it by), whose run ends
/// at once.
fn unnamed(wall: Duration) -> Work {
    Work::whole_host(Duration::ZERO)
        .without_request_id()
        .give_up_after(wall)
}

/// The request an item of [`unnamed`]`(wall)` makes of `spec`'s server, as the agent
/// registry keys a request asked for under no id: its content. The old tests'
/// `whole_request`, with the capabilities of `spec`'s pool.
fn content(spec: &ServerSpec, wall: Duration) -> Request {
    Request {
        class: ClassId::new(WHOLE_CLASS).expect("the whole-node class is a class id"),
        resources: default_needs(),
        capabilities: spec.shape().capabilities.clone(),
        max_wall: wall,
        labels: [("team".to_owned(), "infra".to_owned())].into(),
        validation_run: None,
        probe_host: None,
    }
}

/// The first acquire of item `item`.
fn ask(item: u32) -> Call {
    Call::acquire(ItemId(item), 1)
}

/// The wall time `n` seconds after [`ASK`], as the registry's exclusions read it.
fn wall(n: u64) -> js_lease::Timestamp {
    wall_time(FarmTime::default().saturating_add(after(n)))
}

// a_silent_agent_fails_the_prepare_and_the_retry_goes_elsewhere

/// The host count of `a_silent_agent_fails_the_prepare_and_the_retry_goes_elsewhere`.
const SILENT_RETRY_HOSTS: usize = 2;

/// Carries `a_silent_agent_fails_the_prepare_and_the_retry_goes_elsewhere`:
/// `assert!(named(&one) != named(&two), "{} {}", one.body, two.body);`, the first try
/// told to exactly one host: its lease is booked on one host (and `NoDoubleBooking` holds
/// it to one), one second after the ask.
fn first_try_booked_on_one_host() -> Expect {
    Expect::booked(ask(0), Booked::Somewhere)
}

/// Carries `a_silent_agent_fails_the_prepare_and_the_retry_goes_elsewhere`:
/// `assert!(listed(&h, &id).await.is_some());`, one second before the deadline.
fn silent_lease_listed_before_the_deadline() -> Expect {
    Expect::listed(ask(0), Listed::Pending)
}

/// Carries `a_silent_agent_fails_the_prepare_and_the_retry_goes_elsewhere`:
/// `assert_eq!(record(&h), None);`, one second before the deadline.
fn no_record_before_the_deadline(key: &Request) -> Expect {
    Expect::request_record(key, RecordFact::Absent)
}

/// Carries `a_silent_agent_fails_the_prepare_and_the_retry_goes_elsewhere`:
/// `assert!(listed(&h, &id).await.is_none(), "dropped at the deadline");`, at the
/// deadline.
fn silent_lease_dropped_at_the_deadline() -> Expect {
    Expect::listed(ask(0), Listed::Absent)
}

/// Carries `a_silent_agent_fails_the_prepare_and_the_retry_goes_elsewhere`:
/// `let kept = record(&h).expect("the failed prepare is recorded");` and
/// `assert_eq!(kept.prepare_failures, 1);`, at the deadline (a record that is not there
/// reads `absent`).
fn one_failed_prepare(key: &Request) -> Expect {
    Expect::request_record(key, RecordFact::PrepareFailures(1))
}

/// Carries `a_silent_agent_fails_the_prepare_and_the_retry_goes_elsewhere`:
/// `assert!(!kept.terminal);`, at the deadline.
fn not_terminal_after_one(key: &Request) -> Expect {
    Expect::request_record(key, RecordFact::Terminal(false))
}

/// Carries `a_silent_agent_fails_the_prepare_and_the_retry_goes_elsewhere`:
/// `assert_eq!(kept.exclusions, excluded.into());`, at the deadline: the host the lease
/// was booked on, and no other, left out until `reserve_backoff` after the deadline
/// (`T0 + 60_000 + 120_000`).
fn the_silent_host_excluded_for_the_backoff(key: &Request) -> Expect {
    let host = HostRef::BookedFor(ask(0));
    let until = Exclusion::Until(wall(60 + 120));
    Expect::request_record(key, RecordFact::Exclusions(vec![(host, until)]))
}

/// Carries `a_silent_agent_fails_the_prepare_and_the_retry_goes_elsewhere`:
/// `let reason = kept.last.expect("the failure is recorded").code;` and
/// `assert_eq!(reason, "prepare-timeout");`, at the deadline (no last failure reads
/// as the empty code).
fn the_failure_is_a_prepare_timeout(key: &Request) -> Expect {
    Expect::request_record(key, RecordFact::LastFailure("prepare-timeout".to_owned()))
}

/// Carries `a_silent_agent_fails_the_prepare_and_the_retry_goes_elsewhere`:
/// `assert!(on_first != on_second, "{} {}", one.body, two.body);` and
/// `assert_ne!(went, failed);`: the retry, the item's second acquire (made at the
/// deadline, once the client reads the lease gone), is booked on exactly one host, and
/// not the one the first try was booked on, one second after the deadline.
fn the_retry_goes_elsewhere() -> Expect {
    let retry = Call::acquire(ItemId(0), 2);
    Expect::booked(retry, Booked::Elsewhere(HostRef::BookedFor(ask(0))))
}

/// The scenario of `a_silent_agent_fails_the_prepare_and_the_retry_goes_elsewhere`: two
/// hosts whose agents never report ready; `prepare_timeout = 60s`,
/// `reserve_backoff = 120s`; one whole-node request with no request id at [`ASK`]. The
/// first try is dropped at the deadline as a failed prepare (one failure, the host left
/// out for the backoff, `prepare-timeout`), and the client's retry goes to the other
/// host. Every try then fails the same way, so the request uses up the default
/// `max_replace` and the client is refused: one item failed.
fn silent_retry(world: World, answering: Answering) -> Scenario {
    let hosts = world.hosts(SILENT_RETRY_HOSTS);
    let spec = whole_pool(hosts, world)
        .prepare_timeout(PREPARE_TIMEOUT)
        .reserve_backoff(secs(120));
    let key = content(&spec, OLD_WALL);
    let name = "a_silent_agent_fails_the_prepare_and_the_retry_goes_elsewhere";
    base(name, world, (spec, hosts), answering)
        .at(ASK, submit_work(1, unnamed(OLD_WALL)))
        .expect(Expect::answer(ask(0), 202, None))
        .expect_at(after(1), first_try_booked_on_one_host())
        .expect_at(after(59), silent_lease_listed_before_the_deadline())
        .expect_at(after(59), no_record_before_the_deadline(&key))
        .expect_at(after(60), silent_lease_dropped_at_the_deadline())
        .expect_at(after(60), one_failed_prepare(&key))
        .expect_at(after(60), not_terminal_after_one(&key))
        .expect_at(after(60), the_silent_host_excluded_for_the_backoff(&key))
        .expect_at(after(60), the_failure_is_a_prepare_timeout(&key))
        .expect_at(after(61), the_retry_goes_elsewhere())
        .expect(Expect::terminal_count(1, HowEnded::Failed))
}

/// Proves `a_silent_agent_fails_the_prepare_and_the_retry_goes_elsewhere` on the cell, in
/// the old world and at scale under every placement: a silent agent fails the prepare at
/// exactly `prepare_timeout`, the registry records it (one failure, not terminal, the
/// host excluded until `reserve_backoff` after the deadline, `prepare-timeout`), and the
/// retry goes to another host. Catches a lease the table stops at its deadline without
/// the registry hearing of a failed prepare, a lease left pending past its deadline, and
/// a retry that picks the host that just failed.
#[test]
fn a_silent_agent_fails_the_prepare_and_the_retry_goes_elsewhere() {
    for world in worlds() {
        silent_retry(world, Answering::Silent).run_seeds(SEEDS);
    }
}

// a_silent_host_is_left_out_of_the_retry_for_reserve_backoff

/// The host count of `a_silent_host_is_left_out_of_the_retry_for_reserve_backoff`.
const BACKOFF_HOSTS: usize = 1;

/// The wall limit, and so the give-up time, of every request of the backoff and
/// terminal scenarios: one prepare deadline, so that an item stops asking when its try
/// is dropped and the next ask is an item of its own, made when the old test made it.
const ONE_TRY: Duration = PREPARE_TIMEOUT;

/// Carries `a_silent_host_is_left_out_of_the_retry_for_reserve_backoff`:
/// `assert_eq!(beat(&h, &me).await.json()["exclusive"], json!(id));`, the agent told of
/// the lease: it is booked on the one host, one second after the ask. Old world only: it
/// names the host.
fn backoff_first_try_on_the_host() -> Expect {
    Expect::booked(ask(0), Booked::On(HostRef::Agent(0)))
}

/// Carries `a_silent_host_is_left_out_of_the_retry_for_reserve_backoff`:
/// `assert!(listed(&h, &id).await.is_none());`, at the deadline.
fn backoff_lease_dropped_at_the_deadline() -> Expect {
    Expect::listed(ask(0), Listed::Absent)
}

/// Carries `a_silent_host_is_left_out_of_the_retry_for_reserve_backoff`:
/// `assert_eq!(refused.status, 409, "{}", refused.body);` and
/// `assert_eq!(refused.json(), json!({ "error": "no-capacity" }));`, the ask made at the
/// deadline. Old world only: with more hosts another one takes it.
fn refused_at_the_deadline() -> Expect {
    Expect::answer(ask(1), 409, Some(json!({ "error": "no-capacity" })))
}

/// Carries `a_silent_host_is_left_out_of_the_retry_for_reserve_backoff`:
/// `assert_eq!(refused.status, 409, "{}", refused.body);`, the ask made one second before
/// the backoff runs out. Old world only: with more hosts another one takes it.
fn refused_a_second_before_the_backoff_ends() -> Expect {
    Expect::answer(ask(2), 409, None)
}

/// Carries `a_silent_host_is_left_out_of_the_retry_for_reserve_backoff`: the `assert!` of
/// `h.call("GET", "/v1/leases", None).await.json()["leases"]` `.is_empty()`, nothing placed
/// and nothing pending after the refused ask one second before the backoff runs out. Old
/// world only: with more hosts the asks are placed.
fn nothing_listed_while_excluded() -> Expect {
    Expect::no_leases_listed()
}

/// Carries `a_silent_host_is_left_out_of_the_retry_for_reserve_backoff`:
/// `assert_eq!(beat(&h, &me).await.json()["exclusive"], json!(retry));`, the ask made the
/// moment the backoff ran out placed on the host it left out, one second later. Old world
/// only: it names the host.
fn placed_there_once_the_backoff_ends() -> Expect {
    Expect::booked(ask(3), Booked::On(HostRef::Agent(0)))
}

/// The scenario of `a_silent_host_is_left_out_of_the_retry_for_reserve_backoff`: one host
/// whose agent never reports ready; `prepare_timeout = 60s`, `reserve_backoff = 120s`;
/// four items of one whole-node request with no request id, each its own ask: at [`ASK`]
/// (placed, and dropped at the deadline), at the deadline and one second before the
/// backoff runs out (refused `no-capacity` while the host is left out; the second is
/// cancelled a millisecond after its refusal, so that it asks no more), and the moment
/// the backoff runs out (placed there).
fn backoff(world: World, answering: Answering) -> Scenario {
    let hosts = world.hosts(BACKOFF_HOSTS);
    let spec = whole_pool(hosts, world)
        .prepare_timeout(PREPARE_TIMEOUT)
        .reserve_backoff(secs(120));
    let name = "a_silent_host_is_left_out_of_the_retry_for_reserve_backoff";
    let scenario = base(name, world, (spec, hosts), answering)
        .at(ASK, submit_work(1, unnamed(ONE_TRY)))
        .at(after(60), submit_work(1, unnamed(ONE_TRY)))
        .at(after(179), submit_work(1, unnamed(ONE_TRY)))
        .at(after_ms(179_001), cancel_work(items(2..3)))
        .at(after(180), submit_work(1, unnamed(ONE_TRY)))
        .expect(Expect::answer(ask(0), 202, None))
        .expect_at(after(1), Expect::booked(ask(0), Booked::Somewhere))
        .expect_at(after(60), backoff_lease_dropped_at_the_deadline())
        .expect(Expect::all_items_terminal());
    match world {
        World::Old(_) => scenario
            .expect_at(after(1), backoff_first_try_on_the_host())
            .expect(refused_at_the_deadline())
            .expect(refused_a_second_before_the_backoff_ends())
            .expect_at(after_ms(179_001), nothing_listed_while_excluded())
            .expect(Expect::answer(ask(3), 202, None))
            .expect_at(after(181), placed_there_once_the_backoff_ends()),
        World::Scale(_) => scenario,
    }
}

/// Proves `a_silent_host_is_left_out_of_the_retry_for_reserve_backoff` on the cell: the
/// exclusion is what keeps the request off the silent host. In the old world the one host
/// is refused `no-capacity` (nothing placed, nothing pending) from the deadline until
/// `reserve_backoff` has passed, and is placed there from the first moment it has; at
/// scale, under every placement, the first try is still dropped at the deadline. Catches
/// a failed prepare that excludes nothing (the ask at the deadline would be placed) and
/// an exclusion that lasts longer or shorter than `reserve_backoff`.
#[test]
fn a_silent_host_is_left_out_of_the_retry_for_reserve_backoff() {
    for world in worlds() {
        backoff(world, Answering::Silent).run_seeds(SEEDS);
    }
}

// a_request_is_terminal_after_max_replace_silent_prepares

/// The host count of `a_request_is_terminal_after_max_replace_silent_prepares`.
const TERMINAL_HOSTS: usize = 1;

/// When the second try is asked, after [`ASK`]: twenty seconds after the first deadline.
/// The old test asked it ten seconds after, once two hand-played heartbeats had restored
/// the host; the cell's agent restores on its own heartbeats, two intervals after it hears
/// the lease is gone, so the second try is asked one interval later.
const SECOND_TRY: u64 = 80;

/// Carries `a_request_is_terminal_after_max_replace_silent_prepares`:
/// `assert_eq!(beat(&h, &me).await.json()["exclusive"], json!(id));`, each try told to
/// the one host: its lease is booked there one second after its ask. Old world only: it
/// names the host.
fn try_on_the_host(item: u32) -> Expect {
    Expect::booked(ask(item), Booked::On(HostRef::Agent(0)))
}

/// Carries `a_request_is_terminal_after_max_replace_silent_prepares`:
/// `assert!(listed(&h, &id).await.is_none());`, each try's lease dropped at its deadline.
fn try_dropped_at_its_deadline(item: u32) -> Expect {
    Expect::listed(ask(item), Listed::Absent)
}

/// Carries `a_request_is_terminal_after_max_replace_silent_prepares`:
/// `assert_eq!(kept.prepare_failures, tries);`, at each try's deadline.
fn failures_after_tries(key: &Request, tries: u32) -> Expect {
    Expect::request_record(key, RecordFact::PrepareFailures(tries))
}

/// Carries `a_request_is_terminal_after_max_replace_silent_prepares`:
/// `assert_eq!(kept.terminal, tries == 2);`, at each try's deadline.
fn terminal_after_tries(key: &Request, tries: u32) -> Expect {
    Expect::request_record(key, RecordFact::Terminal(tries == 2))
}

/// Carries `a_request_is_terminal_after_max_replace_silent_prepares`:
/// `assert_eq!(refused.status, 409, "{}", refused.body);`, the next ask, made once the
/// last exclusion has run out.
fn refused_once_terminal() -> Expect {
    Expect::answer(ask(2), 409, None)
}

/// Carries `a_request_is_terminal_after_max_replace_silent_prepares`:
/// `assert_eq!(record(&h), None);`, a millisecond after the refused ask.
fn the_refusal_clears_the_record(key: &Request) -> Expect {
    Expect::request_record(key, RecordFact::Absent)
}

/// The scenario of `a_request_is_terminal_after_max_replace_silent_prepares`: one host
/// whose agent never reports ready; `prepare_timeout = 60s`, `reserve_backoff = 10s`,
/// `max_replace = 2`; three items of one whole-node request with no request id, each its
/// own ask: at [`ASK`] and at [`SECOND_TRY`] (each placed and dropped at its deadline, the
/// second making the request terminal with two failures), and ten seconds after the
/// second deadline, once its exclusion has run out (refused, which clears the record).
fn terminal(world: World, answering: Answering) -> Scenario {
    let hosts = world.hosts(TERMINAL_HOSTS);
    let spec = whole_pool(hosts, world)
        .prepare_timeout(PREPARE_TIMEOUT)
        .reserve_backoff(secs(10))
        .max_replace(2);
    let key = content(&spec, ONE_TRY);
    let second_deadline = SECOND_TRY + 60;
    let name = "a_request_is_terminal_after_max_replace_silent_prepares";
    let scenario = base(name, world, (spec, hosts), answering)
        .at(ASK, submit_work(1, unnamed(ONE_TRY)))
        .at(after(SECOND_TRY), submit_work(1, unnamed(ONE_TRY)))
        .at(
            after(second_deadline + 10),
            submit_work(1, unnamed(ONE_TRY)),
        )
        .expect(Expect::answer(ask(0), 202, None))
        .expect(Expect::answer(ask(1), 202, None))
        .expect_at(after(60), try_dropped_at_its_deadline(0))
        .expect_at(after(60), failures_after_tries(&key, 1))
        .expect_at(after(60), terminal_after_tries(&key, 1))
        .expect_at(after(second_deadline), try_dropped_at_its_deadline(1))
        .expect_at(after(second_deadline), failures_after_tries(&key, 2))
        .expect_at(after(second_deadline), terminal_after_tries(&key, 2))
        .expect(refused_once_terminal())
        .expect_at(
            after_ms((second_deadline + 10) * 1_000 + 1),
            the_refusal_clears_the_record(&key),
        )
        .expect(Expect::all_items_terminal());
    match world {
        World::Old(_) => scenario
            .expect_at(after(1), try_on_the_host(0))
            .expect_at(after(SECOND_TRY + 1), try_on_the_host(1)),
        World::Scale(_) => scenario,
    }
}

/// Proves `a_request_is_terminal_after_max_replace_silent_prepares` on the cell: with
/// `max_replace = 2` and agents that never report ready, the second missed deadline makes
/// the request terminal with two failures, and the next ask is refused 409 and clears the
/// record, after the exclusion has run out; in the old world and at scale under every
/// placement. Catches a deadline that is never counted (the request would be placed on
/// silent hosts without bound).
#[test]
fn a_request_is_terminal_after_max_replace_silent_prepares() {
    for world in worlds() {
        terminal(world, Answering::Silent).run_seeds(SEEDS);
    }
}

// release_of_a_pending_whole_node_lease_drops_the_reservation

/// The host count of `release_of_a_pending_whole_node_lease_drops_the_reservation`.
const RELEASE_HOSTS: usize = 1;

/// How long each held lease's run lasts: the holders release their hosts once it ends,
/// 80 s after [`ASK`] (held at the second tick, the run started at the next heartbeat,
/// its result reported at the heartbeat after its end and read at the client's next
/// poll).
const HOLD: Duration = Duration::from_secs(60);

/// When the client releases the waiting lease, in milliseconds after [`ASK`]: between the
/// tick that sets a released host aside for its request (85 s) and the tick that could
/// commit the request on it once the host is quiet (90 s at the earliest), off the 5 s
/// grid of ticks and heartbeats.
const RELEASE_AT_MS: u64 = 87_500;

/// When the request id is asked for again, after [`ASK`]: the host is quiet again.
const AGAIN: u64 = 100;

/// Carries `release_of_a_pending_whole_node_lease_drops_the_reservation`:
/// `assert_eq!(entry(&hosts(&h).await, "burst-1")["reserved_for"], ID);`, the host set
/// aside for the waiting request once the lease that held it is released, when the
/// waiting lease is released. Old world only: it names the host.
fn the_host_is_reserved_for_the_request() -> Expect {
    Expect::set_aside(0, Some(ID))
}

/// Carries `release_of_a_pending_whole_node_lease_drops_the_reservation`: the
/// `assert_eq!` of `(host.get("mark"), host.get("reserved_for"))` with `(None, None)`, a
/// millisecond after the release, with no tick between. Old world only: it names the
/// host.
fn the_host_is_no_longer_set_aside() -> Expect {
    Expect::set_aside(0, None)
}

/// Carries `release_of_a_pending_whole_node_lease_drops_the_reservation`:
/// `assert_eq!(registry.queue_position(&request_id()), None);`, a millisecond after the
/// release.
fn the_request_waits_no_more() -> Expect {
    Expect::waiting(ID, None)
}

/// Carries `release_of_a_pending_whole_node_lease_drops_the_reservation`:
/// `assert!(registry.reservations().is_empty());`, a millisecond after the release.
fn no_reservation_is_left() -> Expect {
    Expect::no_reservations()
}

/// Carries `release_of_a_pending_whole_node_lease_drops_the_reservation`:
/// `assert!(lease_ids(&h).await.is_empty());`, a millisecond after the release.
fn no_lease_is_left() -> Expect {
    Expect::no_leases_listed()
}

/// Carries `release_of_a_pending_whole_node_lease_drops_the_reservation`:
/// `assert_ne!(again, id);`: once the id is asked for again, the lease the first ask
/// named is not listed, so the new ask's lease, which is, is another one; a second after
/// the new ask.
fn the_new_ask_is_a_new_lease(waiter: u32) -> Expect {
    Expect::listed(ask(waiter), Listed::Absent)
}

/// Carries `release_of_a_pending_whole_node_lease_drops_the_reservation`:
/// `assert_eq!(reply.json()["exclusive"], json!(again), "{}", reply.body);`, the new
/// lease takes the host at once: it is booked there a second after the ask. Old world
/// only: it names the host.
fn the_new_lease_takes_the_host(again: u32) -> Expect {
    Expect::booked(ask(again), Booked::On(HostRef::Agent(0)))
}

/// The scenario of `release_of_a_pending_whole_node_lease_drops_the_reservation`: hosts
/// whose agents report ready at once, under the pool's default whole-node keys. At
/// [`ASK`] one whole-node lease with no request id holds each host for [`HOLD`] (the old
/// test's `hold`), and one under [`ID`] waits; once the holders release, the next tick
/// sets a host aside for it; the client then releases the waiting lease, which drops the
/// reservation at once; and a new ask under the same id is a new lease that takes a host
/// at once.
fn release(world: World, answering: Answering) -> Scenario {
    let hosts = world.hosts(RELEASE_HOSTS);
    let holders = u32::try_from(hosts).expect("a host count fits in an item number");
    let (waiter, again) = (holders, holders + 1);
    let named = Work::whole_host(HOLD).with_request_id(ID);
    let spec = whole_pool(hosts, world);
    let name = "release_of_a_pending_whole_node_lease_drops_the_reservation";
    let released = after_ms(RELEASE_AT_MS);
    let after_release = after_ms(RELEASE_AT_MS + 1);
    let scenario = base(name, world, (spec, hosts), answering)
        .at(
            ASK,
            submit_work(holders, Work::whole_host(HOLD).without_request_id()),
        )
        .at(ASK, submit_work(1, named.clone()))
        .at(released, cancel_work(items(waiter..waiter + 1)))
        .at(after(AGAIN), submit_work(1, named))
        .expect(Expect::answer(ask(waiter), 202, None))
        .expect(Expect::answer(
            Call::release(ItemId(waiter), 1),
            200,
            Some(json!({ "state": "freed" })),
        ))
        .expect_at(after_release, the_request_waits_no_more())
        .expect_at(after_release, no_reservation_is_left())
        .expect_at(after_release, no_lease_is_left())
        .expect(Expect::answer(ask(again), 202, None))
        .expect_at(after(AGAIN + 1), the_new_ask_is_a_new_lease(waiter))
        .expect_at(
            after(AGAIN + 1),
            Expect::booked(ask(again), Booked::Somewhere),
        )
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(hosts + 1, HowEnded::Succeeded))
        .expect(Expect::terminal_count(1, HowEnded::Cancelled));
    match world {
        World::Old(_) => scenario
            .expect_at(released, the_host_is_reserved_for_the_request())
            .expect_at(after_release, the_host_is_no_longer_set_aside())
            .expect_at(after(AGAIN + 1), the_new_lease_takes_the_host(again)),
        World::Scale(_) => scenario,
    }
}

/// Proves `release_of_a_pending_whole_node_lease_drops_the_reservation` on the cell: a
/// release of a waiting lease cancels its request at once. The host set aside for it is
/// listed with no mark and no `reserved_for` in the next call with no tick between, the
/// wait list no longer holds the id, the registry holds no reservation and nothing is
/// listed; a new ask under the same id is a new lease that takes the host at once; in
/// the old world and at scale (every host held, so the request waits) under every
/// placement. Catches a release that frees the lease and leaves the request waiting (the
/// host would stay set aside for no one), and an id remembered after its lease.
#[test]
fn release_of_a_pending_whole_node_lease_drops_the_reservation() {
    for world in worlds() {
        release(world, Answering::Ready).run_seeds(SEEDS);
    }
}

/// Asserts that each moved scenario keeps the sensitivity of the test it replaced: its
/// old-world case, under every placement, run with agents that answer the prepare the
/// wrong way (the three silent-prepare scenarios with agents that report ready, the
/// release scenario with agents that never do) fails a named expectation or invariant
/// instead of passing. Catches a moved scenario whose expectations hold whatever the
/// server decides.
#[test]
fn the_old_world_case_fails_when_the_decision_is_wrong() {
    let defective: [(&str, fn(World, Answering) -> Scenario, Answering); 4] = [
        ("silent_retry", silent_retry, Answering::Ready),
        ("backoff", backoff, Answering::Ready),
        ("terminal", terminal, Answering::Ready),
        ("release", release, Answering::Silent),
    ];
    for placement in Placement::value_variants() {
        for (name, scenario, answering) in defective {
            let failure = scenario(World::Old(*placement), answering)
                .try_run_seeds(1)
                .expect_err(name);
            let named = matches!(
                failure,
                ScenarioFailure::Unmet { .. } | ScenarioFailure::Violated(_)
            );
            assert!(named, "{name}: {failure}");
        }
    }
}

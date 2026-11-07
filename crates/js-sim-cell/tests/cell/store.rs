//! The store family's scenarios, shared by the family (`tests/store.rs`), which runs each
//! over every store the build has, and the golden hashes (`tests/replay.rs`), which pin the
//! restart's over SQLite: the server's state (its host bindings, its join tokens and its
//! scaler's records) in the store `--store` names, across a restart, a stop in a burst of
//! hellos, a second server on the same store, and a crash of the scaler's launch.
//!
//! Each takes the store it runs over and is named after it (`-sqlite`, `-postgres`). The
//! design's restart recovery (section 6.7) is a property of the store: a durable fact is
//! committed before it is acted on, a restart reads the store, hosts say hello again, and
//! nothing a client holds is lost. Each scenario states only what the server does today and
//! the design also promises.
//!
//! Sizes, in agent requests a seed, every host beating every 5 s (the server's default):
//! - the restart: nine hosts for about 330 s, about 600 heartbeats;
//! - the burst of hellos: twelve hosts for about 63 s, about 150;
//! - the second server: four hosts for about 120 s, about 100;
//! - the scaler's crash: one machine for at most about 1,000 s, about 200.

use std::collections::BTreeMap;
use std::time::Duration;

use js_sim::TraceHash;
use js_sim_cell::actions::{
    crash_at_next_acquire, join_agents, mint_join_token, restart_server, start_second_server,
    stop_server,
};
use js_sim_cell::bus::agent_name;
use js_sim_cell::{
    Cell, CloudEvent, Counter, ElasticPool, Expect, HostIs, HowEnded, OfferSpec, Route, Scenario,
    SeedRun, ServerSpec, Shape, Store, Work,
};

/// The pool of every scenario of the family.
pub const POOL: &str = "burst";

/// The family of the golden file's lines of the store scenarios.
pub const FAMILY: &str = "store";

/// How many seeds of the restart the golden file pins, from seed 0.
pub const GOLDEN_SEEDS: u64 = 2;

/// The restart's name, as [`restart`] names it before the store's.
const RESTART: &str = "a_restart_over_the_store_keeps_every_host_binding_and_lease";

/// The restart: six hosts of the pool's shared token, one host joined with a minted token
/// before the restart, and twelve runs of a minute, two waves over the hosts, so the first
/// wave is in flight at the restart.
const RESTART_POOL_HOSTS: usize = 6;
const RESTART_RUNS: u32 = 12;
const RESTART_RUN: Duration = Duration::from_secs(60);
const RESTART_AT: Duration = Duration::from_secs(30);
/// When two more hosts join after the restart: one with the token used before it, one with
/// the token minted then and never used.
const REJOIN_AT: Duration = Duration::from_secs(31);
const RESTART_SETTLE: Duration = Duration::from_secs(300);
/// The host that joined with the first token before the restart (`h006`), the host that
/// presents that token again after it (`h007`), and the host of the unused token (`h008`).
const JOINED: usize = RESTART_POOL_HOSTS;
const REUSED: usize = RESTART_POOL_HOSTS + 1;
const FRESH: usize = RESTART_POOL_HOSTS + 2;

/// The burst of hellos: twelve hosts, each with a token of its own minted at the start,
/// say hello at once; the bus takes 1 to 10 ms a message, so the server is stopped at 6 ms
/// between two of their hellos, and started again three seconds later.
const HELLO_HOSTS: usize = 12;
const HELLO_STOP_AT: Duration = Duration::from_millis(6);
const HELLO_RESTART_AT: Duration = Duration::from_secs(3);
const HELLO_SETTLE: Duration = Duration::from_secs(60);

/// The second server: four hosts and eight runs of ten seconds; a second server on the
/// store while the first runs them, and again once the first is stopped, before the first
/// is started again.
const SECOND_HOSTS: usize = 4;
const SECOND_RUNS: u32 = 8;
const SECOND_RUN: Duration = Duration::from_secs(10);
const SECOND_AT: Duration = Duration::from_secs(5);
const SECOND_STOP_AT: Duration = Duration::from_secs(60);
const SECOND_AGAIN_AT: Duration = Duration::from_secs(61);
const SECOND_RESTART_AT: Duration = Duration::from_secs(62);
const SECOND_SETTLE: Duration = Duration::from_secs(60);

/// The refusal of a second server on a store another server holds, as the server says it.
pub const WRITER_LOCK_REFUSAL: &str = "--store: another process holds the store's writer lock";

/// The scaler's crash: an elastic pool of at most one machine and one whole-host run of a
/// minute; the server crashes at its first acquire (about 5 s) and is started again at
/// 60 s, before the machine has booted (120 s after its launch).
const SCALER_MAX: u64 = 1;
const SCALER_RUN: Duration = Duration::from_secs(60);
const SCALER_RESTART_AT: Duration = Duration::from_secs(60);
const SCALER_SETTLE: Duration = Duration::from_secs(900);

/// Proves that a restart of the server over its store keeps every host binding and every
/// lease (design section 6.7): six hosts of the pool's token and one joined with a minted
/// token are seated before the restart with runs in flight; after it every one of them says
/// hello again and is seated once, under the same host id; the token used before the
/// restart is refused as used to another host after it (`h007` is answered 403 and never
/// seated) and the token minted and never used still admits its host (`h008`); every item
/// reaches one terminal answer and no run is launched twice (`at-most-one-launch`). The
/// cell's agents keep no host credential, so the binding is seen as the host seated again
/// under its token. Red for a store that loses a token's use (`h007` would be seated), one
/// that loses a mint (`h008` would be refused), one that loses a binding or a taint the
/// listing shows, and a restart that books a lease on another host (`no-double-booking`).
pub fn restart(store: Store) -> Scenario {
    let spec = ServerSpec::registered_pool(POOL, Shape::gpu_host(), RESTART_POOL_HOSTS + 3);
    Scenario::new("a_restart_over_the_store_keeps_every_host_binding_and_lease")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(spec)
        .over_store(store)
        .hosts(POOL, RESTART_POOL_HOSTS)
        .at(Duration::ZERO, mint_join_token(2))
        .at(Duration::ZERO, join_agents(0..1))
        .work(RESTART_RUNS, Work::run_for(RESTART_RUN))
        .at(RESTART_AT, restart_server())
        .at(REJOIN_AT, join_agents(0..2))
        .expect(Expect::all_items_terminal())
        .expect(Expect::host_state(JOINED, HostIs::Live))
        .expect(Expect::host_state(REUSED, HostIs::Absent))
        .expect(Expect::host_state(FRESH, HostIs::Live))
        .expect(Expect::serving_but(&[REUSED]))
        .expect_that(
            "the token used before the restart is refused",
            used_token_refused,
        )
        .witness("unknown_host", |c| c.get(Counter::UnknownHost) >= 1)
        .settle_within(RESTART_SETTLE)
}

/// Proves that a hello that consumes a join token commits the host's binding with it
/// (design section 8, row 33): twelve hosts, each with a token of its own minted through
/// the operator route, say hello at once, and the server is stopped between two of their
/// hellos; the store, read while it is down, holds a binding for every used token and a
/// used token for every binding to one, and the stop fell inside the burst (some tokens
/// used, not all); after the restart every host is seated and serving. Red for a server
/// that keeps a token's use or a binding out of the store (in memory, or in a file beside
/// the lease file), and for one that loses either across the restart.
pub fn hellos(store: Store) -> Scenario {
    let spec = ServerSpec::registered_pool(POOL, Shape::gpu_host(), HELLO_HOSTS);
    Scenario::new("a_hello_that_consumes_a_join_token_commits_the_binding_with_it")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(spec)
        .over_store(store)
        .at(Duration::ZERO, mint_join_token(HELLO_HOSTS))
        .at(Duration::ZERO, join_agents(0..HELLO_HOSTS))
        .at(HELLO_STOP_AT, stop_server())
        .at(HELLO_RESTART_AT, restart_server())
        .expect(Expect::every_host_serving())
        .expect_that("every used token has its binding", every_use_bound)
        .expect_that("the stop fell inside the burst of hellos", inside_the_burst)
        .settle_within(HELLO_SETTLE)
}

/// Proves that a second server on the store is refused while the first holds the store's
/// writer lock (design sections 6.7, step 1, and 10): started while the first runs work, it
/// is refused with the one line that names the lock, and the first is undisturbed (every
/// item succeeds and every host serves at the end); started once the first is stopped, it
/// starts (and is stopped again), and the first then starts again over the store. Over
/// Postgres the lock is the advisory lock, over SQLite the lock file beside the store. Red
/// for a lock that is not taken or not held, one that outlives its server, and a refusal
/// that disturbs the first.
pub fn second(store: Store) -> Scenario {
    let spec = ServerSpec::registered_pool(POOL, Shape::gpu_host(), SECOND_HOSTS);
    Scenario::new("a_second_server_over_the_same_store_is_refused_while_the_first_holds_the_lock")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(spec)
        .over_store(store)
        .hosts(POOL, SECOND_HOSTS)
        .work(SECOND_RUNS, Work::run_for(SECOND_RUN))
        .at(SECOND_AT, start_second_server())
        .at(SECOND_STOP_AT, stop_server())
        .at(SECOND_AGAIN_AT, start_second_server())
        .at(SECOND_RESTART_AT, restart_server())
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(
            SECOND_RUNS as usize,
            HowEnded::Succeeded,
        ))
        .expect(Expect::every_host_serving())
        .expect_that(
            "a second server is refused, then starts",
            refused_then_started,
        )
        .settle_within(SECOND_SETTLE)
}

/// Proves that the scaler's records survive a restart over the store and launch nothing
/// twice (design section 7, "Scaler double launch"; section 8, row 34): a whole-host run
/// asks an empty elastic pool of one machine for a host; the server crashes at the
/// scaler's first acquire, once the call has reached the cloud and before the server reads
/// its answer, and is started again a minute later; the restarted scaler finds the machine
/// its record names and makes no second acquire, the machine's agent joins with the token
/// the scaler minted before the launch, the run succeeds, and the cloud runs none of the
/// pool's machines at the end, under every invariant of an elastic pool. Red for a store
/// that loses the scaler's records, or the machine's token, across the restart (a second
/// acquire, a second machine, or a host that never joins).
pub fn scaler(store: Store) -> Scenario {
    let offers = vec![OfferSpec::new("sim_1x_a100", "zone-a", 120)];
    let pool = ElasticPool::ec2_like(POOL, offers, SCALER_MAX);
    let shape = Shape {
        capabilities: BTreeMap::new(),
        ..Shape::gpu_host()
    };
    Scenario::new("the_scaler_records_survive_a_restart_and_launch_nothing_twice")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(ServerSpec::elastic_pool(pool, shape))
        .over_store(store)
        .at(Duration::ZERO, crash_at_next_acquire())
        .work(1, Work::whole_host(SCALER_RUN))
        .at(SCALER_RESTART_AT, restart_server())
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(1, HowEnded::Succeeded))
        .expect_that("one acquire crashed the server", crashed_once)
        .expect_that("one acquire launched the one machine", acquired_once)
        .expect_that("the cloud runs none of the pool's machines", none_running)
        .settle_within(SCALER_SETTLE)
}

/// Each of `runs` (runs of the restart, over any store) by the name the golden file gives
/// the restart's run of its seed over SQLite, with its hash: the lines both stores are
/// held to.
pub fn golden_names(runs: &[SeedRun]) -> Vec<(String, TraceHash)> {
    let sqlite = Store::Sqlite.name();
    let each = |run: &SeedRun| (format!("{RESTART}-{sqlite}/seed-{}", run.seed), run.hash);
    runs.iter().map(each).collect()
}

/// Whether host `h007`, which presents the token `h006` used before the restart, was
/// answered 403 (`join-token-used`: a hello for another host than the one that used it)
/// and never 200.
fn used_token_refused(cell: &mut Cell) -> Option<String> {
    let hellos = |status| {
        let server = cell.server();
        server.map_or(0, |server| {
            server.answered(&agent_name(REUSED), Route::Hello, status)
        })
    };
    let (refused, admitted) = (hellos(403), hellos(200));
    let met = refused > 0 && admitted == 0;
    (!met).then(|| format!("answered 403 {refused} times and 200 {admitted} times"))
}

/// Whether every read of the store at a stop paired each used token with its binding
/// ([`js_sim_cell::StoreRead::unpaired`]), and the store was read at all.
pub fn every_use_bound(cell: &mut Cell) -> Option<String> {
    let reads = cell.store_reads();
    let found: Vec<String> = reads
        .iter()
        .flat_map(|read| match read {
            Ok(read) => read.unpaired(),
            Err(why) => vec![format!("the store could not be read: {why}")],
        })
        .collect();
    match (reads.is_empty(), found.is_empty()) {
        (true, _) => Some("the store was never read".to_owned()),
        (false, true) => None,
        (false, false) => Some(found.join("; ")),
    }
}

/// Whether the first read of the store found some of the burst's tokens used and not all:
/// the stop fell between two hellos.
fn inside_the_burst(cell: &mut Cell) -> Option<String> {
    let first = cell.store_reads().first().cloned().and_then(Result::ok);
    let used = first.map(|read| read.joins.values().filter(|host| host.is_some()).count());
    let between = used.is_some_and(|used| used > 0 && used < HELLO_HOSTS);
    (!between).then(|| format!("{used:?} of {HELLO_HOSTS} tokens used at the stop"))
}

/// Whether the second server was refused, naming the writer lock, while the first ran, and
/// started once the first was stopped.
fn refused_then_started(cell: &mut Cell) -> Option<String> {
    let seconds = cell.second_servers();
    let got: Vec<Option<&str>> = seconds.iter().map(|s| s.refused.as_deref()).collect();
    let wanted = [Some(WRITER_LOCK_REFUSAL), None];
    (got != wanted).then(|| format!("wanted {wanted:?}, got {got:?}"))
}

/// Whether the server crashed at exactly one acquire.
fn crashed_once(cell: &mut Cell) -> Option<String> {
    let calls = cell
        .cloud()
        .map(js_sim_cell::Cloud::calls)
        .unwrap_or_default();
    let crashes = calls.iter().filter(|call| call.event == CloudEvent::Crash);
    let crashes = crashes.count();
    (crashes != 1).then(|| format!("{crashes} acquires crashed the server"))
}

/// Whether the scaler made one acquire over the whole run, before and after the restart,
/// and the cloud launched one machine.
fn acquired_once(cell: &mut Cell) -> Option<String> {
    let cloud = cell.cloud();
    let calls = cloud.map(js_sim_cell::Cloud::calls).unwrap_or_default();
    let acquires = calls
        .iter()
        .filter(|call| matches!(call.event, CloudEvent::Acquire { .. }));
    let acquires = acquires.count();
    let launched = cloud.map_or(0, |cloud| cloud.fake().launched());
    let met = (acquires, launched) == (1, 1);
    (!met).then(|| format!("{acquires} acquires, {launched} machines launched"))
}

/// Whether the cloud runs a machine at the end: what it runs, when it runs any.
fn none_running(cell: &mut Cell) -> Option<String> {
    let running = cell.cloud().map(|cloud| cloud.fake().running().len());
    match running {
        Some(0) => None,
        Some(running) => Some(format!("it runs {running} machines")),
        None => Some("the cell has no elastic pool".to_owned()),
    }
}

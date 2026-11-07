# The simulation cell: writing, running and reading a scenario

`js-sim-cell` tests the scheduler's behaviour without hosts: the real server, with its job
service, runs under a deterministic kernel beside many fake agents and a client driver, and a set of
invariants is checked after every step. A scenario is a short builder chain that reads top
to bottom. This page is for someone who has not seen the cell: what it is, how to run a
scenario, how to sweep it over many seeds, what keeps a seed replayable, how to write one,
what each invariant means, how to read a failure, when a golden hash may move, what the
cell does not model, and which rows of section 8 of `docs/design/jobscheduler-design.md` a
scenario covers today.

`crates/js-sim-cell/tests/docs.rs` holds this page to the crate: it fails when an
invariant, a test file of the crate, a scenario named in the section 8 table, or the
example below no longer matches the code.

## What it is

One run of the cell is one seed of one scenario. The pieces, and how they talk:

```text
  client driver (JobFront)
      |  calls at exact virtual times, never dropped
      v
  server node                  bus                    agent nodes
  (the real server  <-- drop, delay, partition -->    h000, h001, ...
   and its job service)                               (FakeAgent behind the real wire)

  kernel (js_sim::Sim): the virtual clock, the event queue and the bus, all from the seed
  checker: the step invariants after every step; the end invariants once the run is quiet
```

The kernel (`js_sim`) owns the only clock and the only random source, both from the seed.
Each event it takes (a message delivered, a timer, a partition) goes to one node. The
server node and the agent nodes exchange one HTTP call as a request and its answer
(`js_sim_cell::Msg`). The client driver is not a node: the cell's run loop polls it at the
virtual times it asks for, and its calls go straight to the server's job service router (a
path under `/v1/jobs`). After
every kernel step, every poll and every action, the cell gathers what changed into its
observations and the checker tests the step invariants over them; at the end of the
scenario it tests the end invariants.

What is real, the production code as it runs in the server:

- the server's request handlers: the agent router and the client router of
  `js-lease-server`, built with its `in-process` feature, configured by the server's own
  command-line parser from a `ServerSpec`;
- the registry (`js-registry`): registration through the real hello and its limiter, loss
  after the grace, forgetting after the lost retention;
- the lease table (`js-lease`) and its file, which the server still keeps and times its
  saves on the cell's clock; no client of the cell reaches it;
- the store the server keeps its host bindings, join tokens and scaler's records in
  (`--store`, `server.store` for an elastic pool's server): by default a SQLite file in
  the server's directory, with `--allow-sqlite-store`, or a Postgres database of its own
  for each server node with the crate's `postgres` feature (`Store`,
  `ServerSpec::store`, `Scenario::over_store`), opened by the server from the store file
  and nothing else. A restart opens the same store; the cell reads it only while the
  server is down (`ServerNode::stored`), to check what a stop left in it;
- the scaler and placement, as the server's tick runs them every 5 s of virtual time. The
  cell's pool is one whose hosts register themselves, unless it is an elastic pool
  (`ServerSpec::elastic_pool`): then the server reads a configuration file whose one pool
  launches its machines through the scaler, on an elastic fake cloud (below), with the
  scaler's acquire ids, and so its machines' names, drawn from the seed;
- the wire's types and their checks (`js-agent-wire`): each agent builds the hello and
  heartbeat bodies a real agent builds and checks every answer with the wire's own
  `validate()`;
- the job service: its HTTP/JSON gateway (`--job-listen`, reached in process, never
  bound), its job and allocation tables (`js-jobs`, the SQLite store beside the lease
  file), the per-job heartbeat and the job pass of the tick, placing as `js-sched`
  (`--placement js-sched`, the one placement that places queued jobs; every cell's server
  runs so). The tables stamp their rows with the
  cell's clock and mint their allocation ids from js-jobs's scripted source over ids drawn
  from the seed, both given through the server's in-process seam, so a job run replays
  from its seed too.

What is simulated:

- the wire: the kernel's bus carries each request and each answer as one message. A
  dropped message is a connection reset, a delay is a slow network and a partition is a
  cut link. Duplication and reordering are off unless a scenario turns them on.
- the agents: the conformance suite's in-process agent (`FakeAgent`) behind the real
  wire, the agent a cell runs by the hundred. It runs nothing; a run ends by itself after
  the time its `Work` names. A host can instead run the real agent lifecycle of
  `js-agent` (`Scenario::real_hosts`, `AgentKind::Real`) over an in-memory container
  runtime (`js_conformance::agent::memory`), behind the same wire and session code: its
  heartbeats are built by the lifecycle and every reply is handed to it, and its runs end
  the same way. It is for a handful of hosts, since it writes its held list to disk at
  every step, and the agreement family holds the fake to it.
- time: virtual. The server reads a manual clock that the node sets to the kernel's time
  before every input. Usage recording, whose store reads the system's clock, is off.
- the client: `JobFront` submits each unit of `Work` as a job (`SubmitJob` under an id of
  its own, `sim-job-<n>`, then `GetJob` until it is terminal, `GetResult`, and `CancelJob`
  for a cancel or a give-up) and reports what it hears as client events. Its calls are
  never dropped, delayed or reordered.
- the per-job agents: each fake agent reports the per-job capability in its hello and speaks the per-job messages with a second `FakeAgent`, keyed
  by job id, as their engine: a start of a job it holds is a repeat, a stop of a job it
  does not hold retires the id, each result rides in every heartbeat until an answer
  acknowledges it, and each retired id until an answer acknowledges it. A real agent's
  node does not speak them.
- an elastic pool's cloud: the elastic fake (`js-elastic-fake`), its clock advanced to the
  virtual time of every input of the server, behind a recording wrapper the scaler is
  handed, which forwards every call and writes down each acquire, release and list with
  its arguments, its answer and the virtual time of its step (`CloudCall`). Each machine
  the scaler launches boots into a fake agent node (`m000`, `m001`, ... in launch order)
  the cloud's boot time later, its join token and host id read from the launch payload
  (`ElasticFake::user_data`); a machine the cloud releases, preempts or drops has its
  agent killed in the same step. The wrapper sends no reclaim notices: the server reads
  such a stream on a thread of its own, which would act at a moment of real time.

The crate keeps the kernel's rules: no clock, no random source and no hashed collection,
so a run replays from its seed. The crate doc of `js-sim-cell` says the same at more
length; the kernel itself is described in the crate doc of `js-sim`, and
`crates/js-sim/tests/toy_counter.rs` is the smallest world it runs.

## Run one

CI runs every test binary of the cell on each pull request, in the `test` job
(`cargo test --workspace --locked --no-fail-fast`) and again under coverage, every store
scenario over SQLite. The Postgres cases are built only with the crate's `postgres`
feature, and the `state-postgres` job runs them against its Postgres service
(`cargo test -p js-sim-cell --features js-sim-cell/postgres --locked --test store --test server`);
they need the server `KBF_TEST_POSTGRES_URL` names and fail, never skip, without it. Where
you build, one family runs with:

```sh
cargo test -p js-sim-cell --test smoke
```

and one scenario of it by its test name:

```sh
cargo test -p js-sim-cell --test smoke -- a_partition_of_ten_agents_that_heals_settles_every_run --exact
```

A run is a function of (scenario, seed). A scenario runs seeds `0..n` in order
(`Scenario::run_seeds`), each in a cell of its own, and stops at the first failure, so the
same command on the same code fails at the same seed with the same trace hash. The pull
request tests run a few seeds of each scenario; [Sweep many seeds](#sweep-many-seeds) runs
the smoke family over thousands more.

Each file directly under `crates/js-sim-cell/tests/` is one test binary. Files in the
subdirectories (`cell/`, `agent/`, `invariants/`) are shared code that a binary includes
with `#[path = ...]`; they are not binaries of their own.

| binary | file | what it holds |
|---|---|---|
| `smoke` | `tests/smoke.rs` | The smoke family: a hundred hosts settling four hundred runs, a lossy agent wire, and a partition of ten agents that heals; and the two negative cases of the per-job agents: agents that drop an unacknowledged result fail `result-kept-until-acknowledged`, and agents that relaunch a repeated job start fail `at-most-one-launch`, each with its correct twin. The scenarios are in `tests/cell/burst.rs`, `tests/cell/lossy_wire.rs` and `tests/cell/partition.rs`. |
| `sweep` | `tests/sweep.rs` | The smoke family over the seeds its flags name, one shard of a sweep; it has no harness, and a plain `cargo test` runs it with no flag, so it runs no seed ([Sweep many seeds](#sweep-many-seeds)). |
| `seed_range` | `tests/seed_range.rs` | `Scenario::run_seed_range` and the sweep, each shown to fail when it should: the seeds a range runs, a failing seed and its report in the panic, the shards of a sweep, flags that do not fit together, a run that is not a sweep, and each kind of failure printed with the command that replays it. |
| `job_front` | `tests/job_front.rs` | The job front, driven through a scripted client port: each answer of the job service mapped to its event, the submit's body (a whole host, or the work's own cpu and memory and wall limit), and what a cancel and a give-up do at each stage. |
| `broken_agent` | `tests/broken_agent.rs` | The negative cases of the harness: agents that lack a behaviour the server relies on must fail a named invariant, and correct agents must pass with the witness held. |
| `faults` | `tests/faults.rs` | The fault family: agents that die and return, agents killed for good, a 30 s partition that heals, a lossy wire, a silent host holding a job it never listed, a start after its stop, a restart of the server, and a forgotten host seated again, each over sixteen seeds; three negative cases with defective agents. |
| `fault_actions` | `tests/fault_actions.rs` | The machinery the fault family added: a restart of the server (refused, down, and up again), an agent killed for good, the count of a cut agent's unlisted run, and `Expect::launched_on`. |
| `builder` | `tests/builder.rs` | The scenario builder, shown to fail when it should: a witness no seed satisfies, an expectation not met, a violation and its report, an action at its virtual time, the seeds in order, a fault that never ends, hosts of an unknown pool, and a client call the bus's faults never reach. |
| `server` | `tests/server.rs` | The real server with fake agents in the cell, driven through the `Cell` API directly: registration, loss and forgetting, a job made through the job service's router on the real wire; and the store: the `--store` file the cell writes and the server takes, a host joined with a minted token, a read of the store while the server is down, a second server refused on the store of a running one, and, with the `postgres` feature, a run's database dropped when the run ends. |
| `agent` | `tests/agent.rs` | The agent node against a scripted stand-in for the server, its side of the whole-host handshake included. |
| `real_agent` | `tests/real_agent.rs` | The real agent node (the lifecycle of `js-agent` over the in-memory runtime) against the same scripted stand-in. |
| `agreement` | `tests/agreement.rs` | The agreement family: each scenario run with every agent fake and again with every agent real, and the client's outcome of every item compared seed by seed. |
| `invariants` | `tests/invariants.rs` | Each invariant over hand-built observations, with negative cases it must flag and correct siblings it must not. |
| `whole_host` | `tests/whole_host.rs` | The whole-host family: fifty whole-host jobs over twenty hosts. The scenario is in `tests/cell/whole_host.rs`. |
| `whole_host_faults` | `tests/whole_host_faults.rs` | The whole-host fault family: a drain behind busy hosts, a cancelled whole-host job (queued), and a placed job whose agent restarts, each over sixteen seeds. The scenarios are in `tests/cell/whole_host_faults.rs` and `tests/cell/whole_host_cancel.rs`. |
| `whole_host_parts` | `tests/whole_host_parts.rs` | The parts of the whole-host machinery, each shown to work and to fail: whole-host jobs placed in turn on one host, the record of a client cancel, the whole-host expectations (at the end and at a virtual time), and the server spec's class, placement, job service address and architecture. |
| `elastic` | `tests/elastic.rs` | The elastic family: the demand of a hundred queued whole-host jobs scales an empty pool of at most forty machines up and back to none, billed as EC2 bills and as Lambda bills, each over two seeds; and cells driven by hand: a machine that boots with the token of its launch payload, the recording wrapper's times, a pool that does not join, machines the cloud drops or preempts, a foreign machine, the operator's brake reset, and an acquire armed to crash the server (down from the tick of that acquire until a restart, after which no second acquire is made). The scenarios are in `tests/cell/elastic.rs`. |
| `store` | `tests/store.rs` | The store family: a restart over the store, a stop of the server in a burst of hellos that consume join tokens, a second server on the store of a running one, and a crash of the server at the scaler's acquire, each over sixteen seeds over SQLite and, with the `postgres` feature, over eight seeds over Postgres; and with the feature, the same seeds' trace hashes compared between the two stores. The scenarios are in `tests/cell/store.rs`. |
| `replay` | `tests/replay.rs` | Whether a seed replays, the failure report, and the golden trace hashes. |
| `docs` | `tests/docs.rs` | The check that holds this page to the crate. |

## Sweep many seeds

A few fixed seeds find only the races those seeds hit. The test binary `sweep`
(`tests/sweep.rs`, over `js_sim_cell::sweep`) runs the smoke family over a range of seeds
split into shards, one shard a run:

```sh
cargo test --release --locked -p js-sim-cell --test sweep -- --start 0 --count 400 --shards 4 --shard 1
```

The flags, all the binary's own (it has no harness, so libtest never reads them):

- `--count <n>`: how many seeds the sweep runs over all its shards (required);
- `--start <seed>`: the first of them (default 0);
- `--shards <n>` and `--shard <i>`: split the seeds into `n` contiguous shards, whose sizes
  differ by at most one, and run shard `i`, from 0 (default: one shard, all the seeds);
- `--scenario <name>`: run only that scenario of the family;
- `--help`: print the flags.

This is the one way to run a scenario over seeds other than the first few; there is no
environment variable. A run with none of these flags runs no seed and passes, saying so
on stderr: that is how `cargo test --workspace` runs the binary on every pull request, so
the pull request tests keep their few seeds. Flags that do not fit together (no shard, a
shard past the last, fewer seeds than shards, a range past the last seed, a scenario the
family does not have) are refused with exit status 2.

Each scenario runs over the shard's seeds with `Scenario::try_run_seed_range` and prints
`scenario <name>: <n> seeds passed`, or its failure as text (for a violation the whole
report of [Read a failure](#read-a-failure)) and under it the command that replays it:

```text
replay: cargo test --release --locked -p js-sim-cell --test sweep -- --scenario <name> --start <seed> --count 1
```

That is the failing seed alone; for a witness no seed of the shard satisfied, it is the
shard's whole range. A witness is judged over one shard's seeds, so a witness of a swept
family must hold in nearly every shard-sized range, not only somewhere in the sweep. The
binary goes on to the next scenario after a failure and exits 1 if any failed.

The workflow `.github/workflows/sim-sweep.yml` runs the sweep every night, and by hand
from the Actions tab (`workflow_dispatch`, with the first seed, the seed count and the
shard count as inputs). Each shard is a job of its own on a GitHub-hosted runner, and a
red job's log holds each failure and its `replay:` line. A nightly run starts at the run's
id (`github.run_id`), so each night sweeps seeds no night swept before; the binary's first
line of output (`sweep: seeds <a>..<b>, shard <i> of <n> of seeds ...`) names them. It is not a check of pull requests.

## The determinism lint

A sweep's failure is worth having only if its seed replays, and a seed replays only while
nothing outside it decides anything. `cargo xtask determinism`
(`crates/xtask/src/determinism.rs`) reads the source of `js-sim-cell` and of every
workspace crate it links, listed by hand in that file's `CRATES`, and fails on each of
these that its allow-list does not name: `SystemTime::now`, `Instant::now`, `getrandom`,
`thread_rng` and `Uuid::new_v4`. It reads code, not text: a path (qualified ones too, such
as `<Instant>::now`), a type, an import or a macro call's tokens, but not a comment or a
string; and it leaves out what only a test
build compiles (`#[cfg(test)]` items and test module files). The test
`cargo test -p xtask --test determinism` runs it on the tree on every pull request, beside
the layering test, and fails when `CRATES` is not exactly the crates the cell links.

Each entry of the allow-list, `ALLOWED` in the same file, names a file, the item the site
is inside (`SystemClock::now`: an `impl` block's type, then the function), the token and
how many sites (lines) of that item it allows, with a reason: the production clock or random source behind a port the cell fills with
its own, or code a cell run never reaches. A failure names each site no entry allows, as
`file:line: token in item`, each entry that matches no site (remove it: the call moved
or went), each entry whose count is not its number of sites (so a second read added to an
allowed function needs its own reason), and each entry with no reason. A new site gets an entry only when the cell
cannot reach it or reaching it decides nothing; otherwise the code takes its clock or its
random source from the cell, as the server's clock does. The lint looks for a name as it
is written: `use std::time::Instant as Clock;` then `Clock::now()` is not found, nor is a
clock or random source other than the five (`rand::random`, `Utc::now`), nor the code of a
crate outside the workspace.

## Write one

A scenario is data plus one call. This one is the lossy agent wire of the smoke family,
copied from the crate (`docs.rs` fails when the copy and the file differ, so it is the
code as it is):

```rust
// Copied from crates/js-sim-cell/tests/cell/lossy_wire.rs
use std::time::Duration;

use js_sim::{Chance, Faults};
use js_sim_cell::actions::set_bus_faults;
use js_sim_cell::{Counter, Expect, Scenario, ServerSpec, Shape, Work};

/// How many hosts, and so how many runs at once.
pub const HOSTS: usize = 20;

/// How many runs the client submits at time zero, and how long each lasts.
pub const RUNS: u32 = 60;
pub const RUN: Duration = Duration::from_secs(120);

/// When the client gives an item up.
pub const GIVE_UP: Duration = Duration::from_secs(1_800);

/// The share of the agent wire's messages lost, and for how long from the start.
pub const DROP_PERCENT: u32 = 10;
pub const LOSSY_FOR: Duration = Duration::from_secs(900);

/// How long the cell runs after the loss ends, before the end check.
pub const SETTLE: Duration = Duration::from_secs(1_200);

/// Proves that the client's work settles while one message in ten between the agents and
/// the server is lost: every item ends exactly once, no run is launched twice, no host
/// runs more than it holds, and once the loss has ended nothing is left booked or held.
/// The witness shows that loss reached a result: some result was sent again because the
/// answer that acknowledged it was lost. It turns red for a server that relaunches a run
/// whose start or result it hears again (`at-most-one-launch`), one that books a host
/// twice when an answer is lost (`no-double-booking`), and one that keeps a booking after
/// its release (`bookings-match-agents`, `settles-when-quiet`).
pub fn a_lossy_agent_wire_settles_every_run() -> Scenario {
    let lossy = Faults {
        drop: Chance::percent(DROP_PERCENT),
        ..Faults::default()
    };
    Scenario::new("a_lossy_agent_wire_settles_every_run")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(ServerSpec::registered_pool(
            "burst",
            Shape::gpu_host(),
            HOSTS,
        ))
        .hosts("burst", HOSTS)
        .bus(lossy)
        .work(RUNS, Work::run_for(RUN).give_up_after(GIVE_UP))
        .at(LOSSY_FOR, set_bus_faults(Faults::default()))
        .expect(Expect::all_items_terminal())
        .witness("result_resent", |c| c.get(Counter::ResultResent) >= 1)
        .settle_within(SETTLE)
}
// End of the copy.
```

Read it top to bottom: a server with one pool of twenty registered GPU hosts (the
heartbeat interval and grace are the server's defaults, 5 s and 15 s, unless
`ServerSpec::heartbeat` says otherwise), twenty correct agents, one message in ten on the
agent wire dropped from the start, sixty runs of 120 s submitted at time zero, the loss
ended at 900 s by an action, and then what the scenario expects: every item terminal, and
at least one seed in which some agent sent a result again (the witness). `settle_within`
is how long the cell runs after the last action before the end check.

The smoke family runs it over eight seeds:

```rust
// Copied from crates/js-sim-cell/tests/smoke.rs
#[test]
fn a_lossy_agent_wire_settles_every_run() {
    lossy_wire::a_lossy_agent_wire_settles_every_run().run_seeds(LOSSY_SEEDS);
}
// End of the copy.
```

What a scenario can say, all of it in `crates/js-sim-cell/src/scenario/`,
`crates/js-sim-cell/src/actions/`, `crates/js-sim-cell/src/work.rs` and
`crates/js-sim-cell/src/server/spec.rs`:

- the world: `.server(ServerSpec::registered_pool(pool, Shape::gpu_host(), count))`,
  `.hosts(pool, count)` for correct agents, `.broken_hosts(pool, count, defect)` for
  agents that lack a behaviour (a `js_conformance::agent::Defect`), `.real_hosts(pool,
  count)` for hosts running the real agent lifecycle, `.bus(faults)` for the
  bus's faults at the start, `.front(factory)` for a client other than `JobFront`, and
  `.scratch(env!("CARGO_TARGET_TMPDIR"))` for the server's files. For whole hosts,
  `ServerSpec::whole_node_pool(pool, shape, count)` with `Shape::cpu_host()`, the host with no
  GPU (a whole-host job is started only on a host with none); `.over_store(store)` after `.server(...)` to keep
  the server's state in `Store::Sqlite` (the default) or, with the `postgres` feature,
  `Store::Postgres` (the scenario's name gains the store);
- the work: `.work(count, Work::run_for(d))`, with `.give_up_after(d)` and `.needs(r)`;
  a whole host with `Work::whole_host(d)`, submitted as a job whose needs are
  `wholeHost`;
- the actions, at a virtual time after the start, with `.at(after, action)`:
  `submit_work`, `cancel_work(items(range))`, `kill_agents`, `replace_agents`,
  `kill_agents_for_good` (a host gone, not a fault), `partition_agents(agents, lasting)`,
  `set_bus_faults(faults)` and `restart_server()` (the server started again over its
  files and store with `--boot resume`); for the store, `stop_server()` (the server
  stopped between two inputs, its store read while it is down, until a `restart_server`),
  `start_second_server()` (a second server on the store, stopped again at once if it
  starts), `mint_join_token(count)` (one-time tokens through the operator route) and
  `join_agents(tokens)` (a host for each minted token, saying hello with it); for an
  elastic pool, `crash_at_next_acquire()` (the server stopped at the end of the tick in
  which the scaler's next acquire reached the cloud, answered as for a lost answer).
  Agents are named by
  the index they were added at (`agents(0..10)`), or drawn from the seed when the action
  runs (`drawn_agents(10)`);
- what is expected beyond the invariants: `Expect::all_items_terminal()`,
  `Expect::terminal_count(n, how)`, `Expect::host_state(index, state)` and
  `Expect::launched_on(&agents, after)` (each of those hosts launched a run at or after
  `after`, by the launch ledger), `Expect::held_and_released(n)` (n items were placed on a
  host and heard done with the job's result), and for whole hosts
  `Expect::every_host_serving()`, `Expect::serving_but(&agents)` and
  `Expect::held_after_the_runs_on_its_host(item)` (the item was placed only after every job
  launched on its host had made its result), judged once per
  seed after the end check with `.expect(e)`, or at a virtual time with
  `.expect_at(after, e)` (after the kernel's events and the actions of that time, before
  the client's calls of it);
- witnesses: `.witness(name, |counters| ...)`, a predicate over the agents' counters
  (`start_repeated`, `start_after_stop`, `start_dropped_late`, `result_resent`,
  `reply_invalid`, `cut_with_unlisted_run` (an agent that was cut off ran a job that none of its heartbeats had listed), `unknown_host`) added over one seed, which at least one seed must satisfy. A witness
  shows that the situation the scenario is about happened, so a scenario that passes
  because nothing happened fails instead;
- `.settle_within(d)`: the virtual time the cell runs after the last action before the
  end check (600 s unless the scenario says otherwise);
- `.run_seeds(n)`: run seeds `0..n`, and fail the test with the failure as text;
  `.run_seed_range(a..b)` does the same for the seeds of any range (`run_seeds(n)` is
  `run_seed_range(0..n)`), and `.try_run_seeds(n)` and `.try_run_seed_range(a..b)` return
  the failure instead.

The rules:

- Expect only what the server as built and the design both promise. Where they differ,
  the scenario states what both promise and leaves the rest to the invariants (the
  partition scenario of `tests/cell/partition.rs` does this for rows 4 and 35 of section 8).
- Never pin a behaviour that a row of section 8 marks Open.
- Time is virtual. Every duration in a scenario is virtual time; nothing in a scenario
  measures or waits for real time.
- No assertion of your own beyond `Expect`, the witnesses and the invariants. The
  negative cases of `tests/broken_agent.rs`, which check that a scenario fails and how,
  are the exception.
- One family is one test binary: a new family is a new file `tests/<family>.rs`, and it
  is named on this page as `` `tests/<family>.rs` `` (add a row to the table under "Run
  one"), or `docs.rs` fails. Code two binaries
  share goes in a subdirectory and is included with `#[path = ...]`, as
  `tests/cell/lossy_wire.rs` is.
- One action is one file: a new action is a file in `src/actions/` and one `pub use` line
  in `src/actions/mod.rs`. A choice it makes (which agents) is a literal or is drawn from
  the cell's random source, so it follows the seed.

## The invariants

Every scenario checks every invariant of `Invariant::ALL`. A violation is written as one
line, `<name> at <virtual ms>: <detail>`; hosts are `h000`, `h001`, ...; a lease is
`<term>.<sequence>`; a run is `run "<id>"`; an item is `item<n>`, numbered from zero in
submission order; a job is its id (`sim-job-<n>` for item `n` of the job front), and an
allocation its 32 hex digits (`<a>` below). The capacity check of `no-double-booking`
counts the jobs an agent runs beside its runs.

The job front hears no lease, and no whole-node lease is asked for: the lease table the
server still keeps is empty in every scenario of the cell. The checks over leases, runs and
ready reports (the rows from `no-lease-on-a-lost-host` through `marked-hosts-serve-no-ordinary-start`,
and the lease parts of the others) therefore find nothing to flag in a scenario, and
`tests/invariants.rs` runs them over hand-built observations. A whole-host item holds its host
from its `placed` to its `released`, as the job front hears them.

| invariant | the property | what a violation says | checked |
|---|---|---|---|
| `no-double-booking` | By the agents' own truth, no agent runs more than its capacity, no run is run by two agents at once, and no host runs a run of another lease while a whole-host item holds a lease on it. By the server's listing, a lease is booked on one host only, the host it was first booked on (across a restart too), and no host is listed twice. | `host h003 runs [run "a"; run "b"] needing res(cpu=2000 mem=2048 gpus=2) beyond its capacity res(cpu=8000 mem=17179869184 gpus=1) (over on gpus)`, or `run "a" runs on host h001 and on host h004`, or `host h002 runs run "a" of lease 1.7 while whole-host item3 holds lease 1.5 there`, or `lease 1.7 is booked on host h004, and was booked on host h001 first`, or `host h001 is listed twice` | every step |
| `at-most-one-launch` | Summed over the cell's launch ledger, each run is launched at most once, across agent deaths and replacements. | `run "a" launched on host h001 at 64010, already launched on host h001 at 12010` | every step |
| `one-terminal-answer` | Once the client has heard a terminal answer for an item, it hears no other terminal answer and no start for it. | `item3 heard terminal(failed) at 70000 after terminal(succeeded) at 65000` | every step |
| `settles-when-quiet` | Once no fault has been active for the scenario's settle time, every item is terminal (giving up counts), the client holds no lease, and no host's listing books a lease whose run still runs. | `item3 is not terminal`, `item3 still holds lease 1.7` or `host h002 still books lease 1.7` | at the end |
| `no-lease-on-a-lost-host` | A host the server lists as lost takes no new lease: no client is told an item was placed on it, and the listing never shows a lease booked on it for the first time. | `item3 was placed on host h002 at 64000 while the server lists it lost` or `lease 1.7 is booked on host h002 while the server lists it lost` | every step |
| `bookings-match-agents` | At the end, the server's listing books nothing on a host that its agent does not run, and no agent runs a run that the listing does not book: the leak detector. | `host h002 books lease 1.7 and its agent runs no run of it` or `host h002 runs run "a" of lease 1.7 and the server books no such lease there` | at the end |
| `no-result-lost` | From the cell's own ledger of completions and the verdicts the agents read, never the server's record: no run's result is accepted twice; every run an agent completed ends `succeeded` for its client, unless the client had its answer before the completion or the agent was killed between the completion and the first verdict on it; and no client hears `succeeded` for a run no agent completed. | `run "a" was accepted twice: on host h001 at 64010 and on host h001 at 69010`, `run "a" completed on host h001 at 64010 and item3 ended lost`, or `item3 succeeded and no agent completed its run "a"` | at the end |
| `whole-host-is-exclusive` | While a whole-host item holds its host (as the client asked for it, from its grant to its release), no other whole-host item holds the same host, and the server's listing books no other lease there. That the host runs only that lease's runs is `no-double-booking`. | `host h002 is held by whole-host item3 with lease 1.5 and by whole-host item4 with lease 1.6` or `host h002 books lease 1.7 while whole-host item3 holds lease 1.5 there` | every step |
| `held-means-ready` | A whole-host item is granted its lease on a host only after that host's agent sent a ready report naming exactly that lease (what the agent sent, not what the server took). | `whole-host item3 was granted lease 1.5 on host h002 at 41000 before the host's agent reported ready for that lease` | every step |
| `drain-kills-nothing` | No run on a host the server sets aside for a whole-host request is sent a stop or a cancel unless its own client asked first (it cancelled the item or sent its release), or the run's own wall limit had run out. | `host h002, set aside for request sim-item-7, was sent a cancel of run "a" of lease 1.4 at 52000 that its client did not ask for` | every step |
| `marked-hosts-serve-no-ordinary-start` | A host the server sets aside for a whole-host request is sent no start of a lease of an item that did not ask for a whole host, unless it is backfill that ends inside the host's tail. | `host h002, set aside for request sim-item-7 with its tail ending at 90000, was sent a start of run "a" of ordinary lease 1.4 at 52000 that may run until 120000` | every step |
| `one-allocation-per-job` | By the server's allocation table, a job has at most one allocation booked at a time, and an allocation id names one placement: never read for a second job or host, never read live again once read freed. By the agents' own truth, no job runs on two hosts at once. | `job sim-job-3 has allocations <a> and <b> booked at once`, or `allocation <a> of job sim-job-3 on host h001 was read at 64000 for job sim-job-4 on host h001`, or `allocation <a> of job sim-job-3 was read booked at 64000 after it was read freed`, or `job sim-job-3 runs on host h001 and on host h004` | every step |
| `lost-is-terminal` | A job the server's table read lost is never read in another state after, and an answer to a per-job heartbeat that listed it running, sent after it read lost, carries a stop of it. | `job sim-job-3 read running at 70000 after it read lost at 65000`, or `host h002 listed job sim-job-3 at 70000, after it read lost at 65000, and the answer carried no stop of it` | every step |
| `freed-only-when-confirmed` | An allocation is freed only on the host's word (a heartbeat whose listing left the job out, or named it retired, sent after the allocation was committed, after the host read the job's start and after it read the verdict on its result), when the server no longer lists the host, or for a whole-host prepare that failed before any launch: never on silence, never on an unknown. | `allocation <a> of job sim-job-3 on host h002 was read freed at 70000 with no listing of the host since 12000 that left the job out, while the server lists the host` | every step |
| `no-start-without-allocation` | Every job start an agent read names an allocation of that job on that host that the server committed no later than the agent read the start. | `host h002 was sent a start of job sim-job-3 under allocation <a> at 12000 that no allocation committed before it names` | every step |
| `result-kept-until-acknowledged` | A per-job agent sends a result it made in every heartbeat until it reads an answer that acknowledges it (unless its process was killed), and no job's result is accepted twice. | `host h002 sent a heartbeat at 70000 without the result of job sim-job-3 it made at 64000, before any answer acknowledged it`, or `the result of job sim-job-3 was accepted twice: on host h001 at 64010 and on host h001 at 69010` | every step |
| `instances-within-limits` | By the elastic fake itself, never the scheduler's records, the cloud runs no more of the scheduler's machines (launched by an acquire, not foreign) than the pool's `max_instances`. | `the cloud of pool burst runs 41 of this scheduler's machines, past its max_instances of 40` | every step |
| `only-own-machines-released` | Every release in the recording wrapper's calls names a machine the scheduler launched: one an acquire was answered with, or a list showed. | `a release of machine fake-0009 at 64000, which this scheduler never launched` | every step |
| `no-acquire-while-plan-only` | Once the pool's brake reads tripped (the scaler's state, read after every tick), the wrapper's calls gain no acquire until the operator resets the brake (`Cell::reset_brake`) or it no longer reads tripped. | `an acquire of offer sim_1x_a100 in zone zone-a at 64000, while the brake of pool burst has read tripped since 60000 with no reset` | every step |
| `throttle-honoured` | After the cloud answered an acquire with a 429 (a deferral of `THROTTLE_WAIT`, 30 s), the wrapper's calls show no acquire of the pool inside that wait. | `an acquire of offer sim_1x_a100 in zone zone-a at 70000, inside the 30s throttle wait of the 429 answered at 64000` | every step |
| `launched-machines-settle` | Once quiet, every machine the scheduler launched has joined and been released, is gone, or was preempted: none the cloud still runs has no work (its agent runs no run and no job) and no drain (its host is neither drained nor being released). | `machine m003 (fake-0004, host js-<12 hex>-<12 hex>) still runs with no work and no drain once the cell is quiet (it joined)` | at the end |

The step invariants are checked after every kernel step, every poll of the client and
every client call; the end invariants once, after the scenario has been quiet (no partition, no
killed agent, no drop, duplicate or reorder chance on the bus) for its `settle_within`.
The numbers and ids in the examples above show the shape of a message, not a recorded
run. The full statement of each invariant is its doc comment in
`crates/js-sim-cell/src/invariant.rs`.

## Read a failure

A seed that breaks an invariant fails the test with a report like this:

```text
scenario <name> seed <n> failed at check <k>: <invariant> at <virtual ms>: <detail>
trace hash <64 hex> (replayed: <64 hex>)
last 40 trace lines:
  <line>
  ...
```

It holds the scenario's name, the seed, the number of the check that failed (from 1), the
violation, the trace hash of the run, and the last forty trace lines. The run loop stops at
the first violation, so the last trace line is `<virtual ms> cell violation <invariant>`.
To get the lines, the cell runs the same seed a second time with its trace kept as text;
the hash of that second run is the one in brackets. The two are equal unless something
outside the seed (a clock, a thread, the order of a hashed collection, a random value)
reached a decision: a report whose two hashes differ has found nondeterminism, and that
is the bug to fix first.

The other ways a scenario fails are one line each:

- `scenario <name> seed <n> did not start: <error>`: the server refused its configuration,
  or the hosts name a pool the server does not have;
- `scenario <name> seed <n> failed expect <statement>: wanted <w>, got <g>`, for example
  `failed expect all-items-terminal: wanted 60, got 59`;
- `scenario <name>: witness <witness> held in none of <n> seeds; counted over them [...]`,
  with every counter added over all the seeds.

To replay a seed a test reported, run the same test again: seeds run from 0 in order, so it
fails at the same seed with the same hash. A seed a sweep reported is replayed by the
`replay:` line printed under its failure, which runs that scenario at that seed alone
([Sweep many seeds](#sweep-many-seeds)).

### Trace lines

Each trace line starts with the virtual time in milliseconds, except the first line of a
kept trace, `kbf-trace-format 1 sim`, the kernel's format line. The kernel writes these lines (`js_sim`; the values in
them are spelled as `js_sim::text` fixes):

| line | means |
|---|---|
| `<t> start <node>` | a node started |
| `<t> timer <node> tag=<n>` | a timer the node set fired |
| `<t> partition <groups>` | a partition took effect: groups of nodes joined by `,` and separated by `\|`, or `none` when healed |
| `<t> send #<m> <from>-><to> at=<t>` | message `m` was put on the bus, to arrive at `at`; `reordered` and `duplicate` follow when those faults hit it |
| `<t> deliver #<m> <from>-><to> <msg>` | message `m` arrived |
| `<t> lost #<m> <from>-><to> <why>` | message `m` was lost: `dropped`, `partitioned` or `unknown receiver` |

In the cell, `<msg>` is `call #<c> <path>` for a request (`/v1/agent/hello` or
`/v1/agent/heartbeat`) and `answer #<c> <status>` for its answer. `#<m>` is the kernel's
number of the message on the bus; `#<c>` is the agent's number of the call, repeated in
its answer. Neither the body nor the bearer is ever written.

The cell writes its own lines, `<t> cell ...`, for what the kernel does not see:

| line | means |
|---|---|
| `<t> cell client item<n> <event>` | the client heard `submitted`, `placed <host>`, `started`, `terminal <how>` or `released` for an item (the checker's events also include `pending`, `queued` and `release-sent`, which the job front never reports) |
| `<t> cell client item<n> cancel` | the scenario asked the client to cancel an item |
| `<t> cell client <METHOD> <status>` | a client call to the server and its status |
| `<t> cell kill <host>`, `<t> cell replace <host>` | an agent's process was killed, or a new one started |
| `<t> cell gone <host>` | the agent killed just before was killed for good: its host is gone, not a fault |
| `<t> cell restart server`, `<t> cell restart refused <flag>` | the server was stopped and started again over its files (`--boot resume`), or it refused to start again, naming the flag at fault |
| `<t> cell faults delay=<min>..<max>ms [<kinds>]` | the bus's faults changed; the kinds are among `drop`, `duplicate` and `reorder` |
| `<t> cell listing <host> live\|lost leases=<n> aside=<bool>` | the server's listing of a host changed |
| `<t> cell listing <host> forgotten` | the server no longer lists the host |
| `<t> cell launch <host>` | an agent launched a run or a job (an entry of the launch ledger) |
| `<t> cell cloud launch m<n>` | the cell first saw the elastic pool's cloud run the scheduler's machine number `n` (in launch order) |
| `<t> cell cloud boot m<n>`, `<t> cell cloud boot m<n> silent` | machine `n`'s agent started, its boot time after its launch; or, for a pool whose machines do not join, the boot that starts no agent |
| `<t> cell cloud release m<n>`, `<t> cell cloud preempt m<n>`, `<t> cell cloud drop m<n>` | the cloud stopped running machine `n`: released by the scheduler, preempted, or gone outside it; its agent is killed in this step |
| `<t> cell cloud brake reset <bool>` | the operator asked the scaler to reset the pool's brake, and whether the scaler took the pool's name |
| `<t> cell job <job> <state>` | the server's job table showed the job in a new state |
| `<t> cell allocation <job> <step> <host>` | the server's allocation table showed the job's allocation `booked`, `releasing` or `freed` on the host (never its id) |
| `<t> cell violation <invariant>` | the violation that stopped the run |

A worked example, three lines of the shape a lossy run writes (the numbers are
illustrative):

```text
65000 send #2911 h007->server at=65004
65004 deliver #2911 h007->server call #13 /v1/agent/heartbeat
65004 lost #2912 server->h007 dropped
```

At 65,000 ms agent `h007` puts its call 13, a heartbeat, on the bus as message 2911, due
at 65,004 ms. It arrives at 65,004 ms and the server handles it at once; the server's
answer, message 2912, is dropped as it is sent, so `h007` never hears it. Its next
heartbeat carries again what this one carried, and if this one carried a result, the
agent counts it as `result_resent`, the counter the lossy scenario's witness reads.

## Golden hashes

`crates/js-sim-cell/tests/golden/trace-hashes.txt` pins the trace hash of seeded runs:
family `js-sim-cell` holds the sixteen seeds of the replay scenario of `tests/replay.rs`,
family `smoke` the first four seeds of the lossy agent wire, family `real_agent` the
first two seeds of the agreement family's world without faults, run with real agents,
family `faults` the first four seeds of the restart of the server with runs in flight
(`tests/cell/server_restart.rs`), family `whole_host` the first two seeds of the fifty
whole-host jobs, and family `whole_host_faults` the
first two seeds of the cancelled whole-host job (`tests/cell/whole_host_cancel.rs`),
family `jobs` the first seed of jobs on a lossy agent wire (`tests/cell/jobs.rs`, eight
hosts), family
`elastic` the first seed of the EC2-like elastic pool scaled up to forty machines and back
to none (`tests/cell/elastic.rs`), and family `store` the first two seeds of the restart
over a SQLite store (`tests/cell/store.rs`). With the `postgres` feature,
`tests/store.rs` holds the same seeds over Postgres to the `store` lines, so one file pins
both stores. A trace hash covers the
cell's trace and the kernel's, so it moves when a decision of the server, the agents, the
client driver or the scenario builder changes, or when the text of a trace line changes.
It must not move for a change that only moves code. (The kernel has a golden file of its
own, `crates/js-sim/tests/golden/trace-hashes.txt`.)

`replay`'s tests `pinned_seeds_match_the_golden_file`,
`the_smoke_scenario_matches_the_golden_file`,
`the_real_agent_scenario_matches_the_golden_file`,
`the_whole_host_scenario_matches_the_golden_file`,
`the_cancelled_whole_host_scenario_matches_the_golden_file`,
`the_job_scenario_matches_the_golden_file`,
`the_elastic_scenario_matches_the_golden_file` and
`the_store_scenario_matches_the_golden_file` fail on a moved
hash and print the command
that regenerates the lines:

```sh
cargo test -p js-sim-cell --test replay -- --ignored regenerate_golden_hashes
```

It rewrites the lines of every family and leaves the rest of the file alone. Regenerate
only on purpose, and say in the pull request why each moved line moved: which decision
changed, or which trace text.

## What the cell does not model

The cell answers questions about scheduling decisions over the agent wire. It does not
model:

- **Duplicated or reordered HTTP messages.** HTTP carries a request and its answer once
  each, in order, on one connection, so the default bus (`Faults::default()`) never
  duplicates or reorders. A scenario that models a request retried over a second
  connection turns `reorder` or `duplicate` on for itself, with `.bus(...)` or
  `set_bus_faults`. Today two scenarios do: `an_agent_that_never_retires_a_stopped_run`
  in `tests/broken_agent.rs` and `a_stale_start_after_a_stop_does_not_run` in
  `tests/faults.rs` turn `reorder` on for every message from 2 s to 30 s. None
  turns `duplicate` on. A result or a stop sent again is covered by the server's HTTP
  tests (`crates/js-lease-server/tests/server/runs.rs`).
- **Lost client answers.** The client's calls are never dropped, delayed or reordered;
  only the agent wire is faulty. A submit sent again is the same job (the job front's id
  is its own), and row 1 of section 8 is Open.
- **TLS and authentication failures.** Every agent says hello with the one join token and
  the client sends the one client token. Covered by the server's tests
  (`crates/js-lease-server/tests/server/tls.rs`, `tls_reload.rs`, `auth.rs`,
  `joins_credential.rs`).
- **Connection limits.** There are no sockets. Covered by
  `crates/js-lease-server/tests/server/limits.rs`.
- **Disk and quota.** Covered by the server's disk tests
  (`crates/js-lease-server/tests/server/disk.rs`) and the agent's own tests
  (`crates/js-agent/tests/agent/disk.rs`, `crates/js-agent/tests/agent/quota/`).
- **Containers and the host operating system.** The agents run nothing; a run ends by
  itself after the time its `Work` names. Covered by the agent's own tests
  (`crates/js-agent/tests/`) and the conformance suite
  (`crates/js-conformance/tests/agent_suite.rs`).
- **Real clouds.** An elastic pool launches its machines on the elastic fake, never a
  real cloud; what the fake models and does not (its bill, its scripts, its lags) is in its
  crate doc. The cell scripts none of the fake's faults yet (refusals, lost calls, 429s,
  failed lists) and sends no reclaim notice.
  The fake itself is covered by its conformance suite
  (`crates/js-elastic-fake/tests/conformance.rs`) and the server's end-to-end test over it
  (`crates/js-lease-server/tests/server/elastic_fake_e2e.rs`).
- **Usage recording.** Its store reads the system's clock, so the cell turns it off.
- **A failed commit of the server's store.** The server opens its store from the store
  file and takes no store from its caller, so the cell cannot make one of its commits
  fail (design section 7, "State write fails when a job is placed", and section 8, row
  27, have no scenario). The server's own tests fail a commit through a wrapper over the
  in-memory fake (`crates/js-lease-server/src/agent_api_store_tests.rs`). A stop of the
  server falls between two inputs, never inside one, so a commit cut in half is not
  modelled either.
- **The store's clock.** The server opens a SQLite store on the system's clock, which it
  takes from no seam. It writes no key with a time to live, so no decision reads that
  clock; the cell's own read of the store, while the server is down, uses the cell's
  clock.
- **Jobs that book GPUs, and whole-host jobs on a host with GPUs.** The server sends no
  start for a job that books GPUs (it records no device indices), so the job front asks for a
  work's cpu and memory only, and a whole-host job (which books the whole host) is started
  only on a host with no GPU: the whole-host scenarios use `Shape::cpu_host()`. The job
  service's gRPC transport is covered where it lives (`crates/js-lease-server`); the cell
  reaches the HTTP/JSON gateway.
- **The whole-host handshake of a lease.** A whole-host job is prepared by its host's agent
  as a whole-node lease is (the agent goes `preparing` and reports ready), but the cell no
  longer scripts a slow, silent or refusing prepare, or a request id, a wait list or a
  mark: those belong to the lease routes.

## Which section 8 rows have a scenario

Rows of section 8 of `docs/design/jobscheduler-design.md` that a merged scenario covers,
and how much of each. A row not listed has no scenario.

| row | scenario | family file | what the scenario shows |
|---|---|---|---|
| 3 | `a_restart_of_the_server_with_runs_in_flight_double_books_nothing` | `tests/cell/server_restart.rs` | The server is stopped and started again over its files (`--boot resume`) with jobs in flight; nothing is launched twice, no job moves host, no host takes a new job while it runs a job of before the restart, and everything settles. What becomes of the jobs in flight (the row's subject) is not asserted. |
| 4 | `ten_of_two_hundred_agents_die_mid_run` | `tests/faults.rs` | Ten of two hundred agents die with their state, after the heartbeat that first listed their jobs, and new processes say hello holding nothing; every item is terminal, nothing runs twice, and the replaced hosts take work again (`launched_on`). That the jobs become lost with no result is not asserted. |
| 25 | `a_whole_host_job_whose_agent_restarts_after_it_is_placed` | `tests/cell/whole_host_faults.rs` | A placed whole-host job's agent is started again with empty state and says hello once its host reads lost; the hello ends the job's run, and a waiting job is then placed on the host and launches its run there. The scenario shows no double booking along this path and does not assert the job's end state. |
| 31 | `a_lossy_wire_launches_every_run_once` | `tests/faults.rs` | A fifth of the agent wire's messages lost and the rest delayed up to 3 s each way, with runs shorter than the heartbeat interval; the server sends starts again (witness `start_repeated`) and every run is launched once. Agents that run a repeated start fail `at-most-one-launch`. |
| 31 | `an_agent_that_relaunches_a_repeated_start` | `tests/broken_agent.rs` | The server sends a start again (runs shorter than the heartbeat interval on a bus slower than it); agents that run the repeated start fail `at-most-one-launch`, correct agents pass with the witness `start_repeated` held. The other half of the row, an agent restart that empties the retired list, has no scenario. |
| 33 | `a_hello_that_consumes_a_join_token_commits_the_binding_with_it` | `tests/cell/store.rs` | Twelve hosts with join tokens of their own say hello at once and the server is stopped between two of their hellos; the store, read while the server is down, holds a binding for every used token and a used token for every binding to one, and after the restart every host is seated and serving, over SQLite and Postgres. The row's failed bindings save is not run: the cell cannot fail a commit, and the hosts take no host credential. |
| 34 | `the_scaler_records_survive_a_restart_and_launch_nothing_twice` | `tests/cell/store.rs` | The server crashes at the scaler's first acquire, after the call reached the cloud and before its answer was read; the restarted scaler finds the machine its stored record names, makes no second acquire, and the machine's agent joins with the token minted before the launch, over SQLite and Postgres. That a retried launch's token is the one the machine booted with is not asserted: the scaler retries nothing here. |
| 35 | `a_silent_host_is_never_given_a_second_copy_of_its_run` | `tests/faults.rs` | Agents are cut off longer than the grace while they run a job no heartbeat of theirs had listed (witness `cut_with_unlisted_run`); the job is never run on another host. The start timeout of the row is not built and not asserted. |
| 38 | `a_stale_start_after_a_stop_does_not_run` | `tests/faults.rs` | The stale-start situation of the row below (the same settings), over sixteen seeds with every invariant of the fault family on. |
| 38 | `an_agent_that_never_retires_a_stopped_run` | `tests/broken_agent.rs` | Every item is cancelled while its start is still on the bus, and a start arrives after the stop that follows it; agents that do not retire the stopped job fail `at-most-one-launch`, correct agents pass with the witness `start_after_stop` held. That the allocation is freed in the same commit is not asserted. |
| 39 | `a_cancelled_whole_host_request_drops_its_place_and_the_host_serves_again` | `tests/cell/whole_host_cancel.rs` | Queued: one whole-host job cancelled while queued behind a held host ends cancelled, and a later whole-host job is placed on the host and launches its run. A cancel while the server has set a host aside for the job, and one while preparing, are not run. |
| 41 | `agents_killed_for_good_leave_no_booking_once_clients_release` | `tests/faults.rs` | Five of fifty agents die for good; their hosts read lost and take no new job, and once the clients give up and cancel, nothing stays booked on them. |

The partition scenario of `tests/cell/partition.rs` cuts agents off while they run, but asserts only
what the server as built and the design both promise (the invariants and every item
terminal). `a_thirty_second_partition_of_twenty_agents_loses_no_result` and
`a_forgotten_host_that_comes_back_is_seated_once` in `tests/faults.rs` cover section 7's
"Result lost" and section 6.3's retention, which are not rows of section 8.

The agreement family (`tests/agreement.rs`, its world in `tests/cell/real_agents.rs`)
checks every invariant of `Invariant::ALL` on both sides of each comparison, and beyond
them that each item's terminal is the same with fake and with real agents. It runs five
situations over eight seeds: no fault, a lossy wire (15% of messages dropped for 300 s,
witness `result_resent`), two of six agents cut off for 30 s, two agents killed and
replaced mid-run, and the stale start of `tests/broken_agent.rs` (witness
`start_after_stop`). It covers no row by itself: it shows that the fake answers as the
real agent does in the situations of section 7 ("Result lost", "Duplicate start", "Stop
of an unknown job"), and its replacement is that of rows 4 and 31 (a host whose
containers and retired list died with the agent), so a scenario that pins either row
with fake agents is held to the real one there.

The real side cannot run today: the cell's real agent node (`js_sim_cell::agent::real`) speaks
the per-run messages only and does not report the per-job capability, so the job service
places nothing on a real host and the real runs of the family (and the `real_agent` golden
family) end in `settles-when-quiet`. They stay in the family until the node speaks the
per-job messages of the lifecycle of `js-agent`.

The store family (`tests/store.rs`, its scenarios in `tests/cell/store.rs`) runs every
invariant of `Invariant::ALL` over SQLite and, with the `postgres` feature, over
Postgres. Beside rows 33 and 34 above it covers section 6.7's restart recovery over the
store (`a_restart_over_the_store_keeps_every_host_binding`: every host seated
once again under its id, a token used before the restart refused as used after it, an
unused one still admitting its host, nothing launched twice) and its first step, the
writer lock
(`a_second_server_over_the_same_store_is_refused_while_the_first_holds_the_lock`: a
second server refused with the line that names the lock while the first runs
undisturbed, and started once the first is stopped). What becomes of the runs in flight
at the restart is not asserted (row 3 is Open). With the feature it also holds the stand-in
to Postgres: `the_sqlite_and_postgres_stores_give_the_same_trace_hash` compares the trace
hashes of a fixed list of scenarios and seeds over the two stores.

The elastic family (`tests/elastic.rs`) covers no row of section 8 yet: row 16 (a scaler
tick skipped while the last is busy) needs a slow provider; the crash after a launch of row
34 is the store family's (above), not this family's. It covers section
7's "Scaler double launch" (each acquire id launched one machine, and the cloud never ran
more of the scheduler's machines than `max_instances`) and section 6.5's idle release (every
machine released once quiet), under every invariant of `Invariant::ALL`; its bill check
holds the cloud's clock to the server's, and says nothing of the scheduler's cost.

The whole-host family covers no row yet. Its fifty whole-host jobs wait in the queue and
are placed as hosts become quiet, but it pins neither the order the queue serves (row 11,
advisory as built) nor what happens when a request is not presented within `regain_grace`
(row 12); it has no failed prepare and no hello under a placed job, so it does not reach row
25 or 26. It holds the placement to the whole-host invariants above.

The whole-host fault family (`tests/whole_host_faults.rs`) runs every invariant of
`Invariant::ALL`. Beside rows 25 and 39 above it covers section 6.4's "drain never cuts
work" (a job behind busy hosts placed only after the jobs on its host ended, every run
succeeding). It pins neither the order the queue serves (row 11) nor which job takes which
host. The cell no longer scripts a failed prepare (silent hosts, a refusal, a ready report
racing the deadline, an agent that restarts while its host is committed): those scenarios
drove the whole-node lease's handshake through a request id and a wait list, which a job does
not have.

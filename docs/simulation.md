# The simulation cell: writing, running and reading a scenario

`js-sim-cell` tests the scheduler's behaviour without hosts: the real lease server runs
under a deterministic kernel beside many fake agents and a client driver, and a set of
invariants is checked after every step. A scenario is a short builder chain that reads
top to bottom. This page is for someone who has not seen the cell: what it is, how to run
a scenario, how to write one, what each invariant means, how to read a failure, when a
golden hash may move, what the cell does not model, and which rows of section 8 of
`docs/design/jobscheduler-design.md` a scenario covers today.

`crates/js-sim-cell/tests/docs.rs` holds this page to the crate: it fails when an
invariant, a test file of the crate, a scenario named in the section 8 table, or the
example below no longer matches the code.

## What it is

One run of the cell is one seed of one scenario. The pieces, and how they talk:

```text
  client driver (LeaseFront)
      |  calls at exact virtual times, never dropped
      v
  server node                  bus                    agent nodes
  (the real lease   <-- drop, delay, partition -->    h000, h001, ...
   server)                                            (FakeAgent behind the real wire)

  kernel (js_sim::Sim): the virtual clock, the event queue and the bus, all from the seed
  checker: the step invariants after every step; the end invariants once the run is quiet
```

The kernel (`js_sim`) owns the only clock and the only random source, both from the seed.
Each event it takes (a message delivered, a timer, a partition) goes to one node. The
server node and the agent nodes exchange one HTTP call as a request and its answer
(`js_sim_cell::Msg`). The client driver is not a node: the cell's run loop polls it at the
virtual times it asks for, and its calls go straight to the server's client router. After
every kernel step, every poll and every action, the cell gathers what changed into its
observations and the checker tests the step invariants over them; at the end of the
scenario it tests the end invariants.

What is real, the production code as it runs in the server:

- the server's request handlers: the agent router and the client router of
  `js-lease-server`, built with its `in-process` feature, configured by the server's own
  command-line parser from a `ServerSpec`;
- the registry (`js-registry`): registration through the real hello and its limiter, loss
  after the grace, forgetting after the lost retention;
- the lease table (`js-lease`), which times its saves on the cell's clock;
- the scaler and placement, as the server's tick runs them every 5 s of virtual time (the
  cell's pool is one whose hosts register themselves; no provider launches machines);
- the wire's types and their checks (`js-agent-wire`): each agent builds the hello and
  heartbeat bodies a real agent builds and checks every answer with the wire's own
  `validate()`.

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
- the clients: `LeaseFront` turns each unit of `Work` into the calls `js run` makes today
  and reports what it hears as client events. Its calls are never dropped, delayed or
  reordered.

The crate keeps the kernel's rules: no clock, no random source and no hashed collection,
so a run replays from its seed. The crate doc of `js-sim-cell` says the same at more
length; the kernel itself is described in the crate doc of `js-sim`, and
`crates/js-sim/tests/toy_counter.rs` is the smallest world it runs.

## Run one

CI runs every test binary of the cell on each pull request, in the `test` job
(`cargo test --workspace --locked --no-fail-fast`) and again under coverage. Where you
build, one family runs with:

```sh
cargo test -p js-sim-cell --test smoke
```

and one scenario of it by its test name:

```sh
cargo test -p js-sim-cell --test smoke -- a_partition_of_ten_agents_that_heals_settles_every_run --exact
```

A run is a function of (scenario, seed). A scenario runs seeds `0..n` in order
(`Scenario::run_seeds`), each in a cell of its own, and stops at the first failure, so the
same command on the same code fails at the same seed with the same trace hash.

Each file directly under `crates/js-sim-cell/tests/` is one test binary. Files in the
subdirectories (`cell/`, `agent/`, `invariants/`) are shared code that a binary includes
with `#[path = ...]`; they are not binaries of their own.

| binary | file | what it holds |
|---|---|---|
| `smoke` | `tests/smoke.rs` | The smoke family: a hundred hosts settling four hundred runs, a lossy agent wire, and a partition of ten agents that heals. |
| `broken_agent` | `tests/broken_agent.rs` | The negative cases of the harness: agents that lack a behaviour the server relies on must fail a named invariant, and correct agents must pass with the witness held. |
| `faults` | `tests/faults.rs` | The fault family: agents that die and return, agents killed for good, a 30 s partition that heals, a lossy wire, a silent host holding a run it never listed, a start after its stop, a restart of the server, and a forgotten host seated again, each over sixteen seeds; three negative cases with defective agents. |
| `fault_actions` | `tests/fault_actions.rs` | The machinery the fault family added: a restart of the server (refused, down, and up again), an agent killed for good, the count of a cut agent's unlisted run, and `Expect::launched_on`. |
| `builder` | `tests/builder.rs` | The scenario builder and the lease front, each shown to fail when it should. |
| `server` | `tests/server.rs` | The real server with real agents in the cell, driven through the `Cell` API directly: registration, loss and forgetting, a lease and a run on the real wire. |
| `agent` | `tests/agent.rs` | The agent node against a scripted stand-in for the server, its side of the whole-host handshake included. |
| `real_agent` | `tests/real_agent.rs` | The real agent node (the lifecycle of `js-agent` over the in-memory runtime) against the same scripted stand-in. |
| `agreement` | `tests/agreement.rs` | The agreement family: each scenario run with every agent fake and again with every agent real, and the client's outcome of every item compared seed by seed. |
| `invariants` | `tests/invariants.rs` | Each invariant over hand-built observations, with negative cases it must flag and correct siblings it must not. |
| `whole_host` | `tests/whole_host.rs` | The whole-host family: fifty whole-node requests over twenty hosts, run once per value of `--placement` (`Placement::value_variants()`), and two negative cases on the real server's history (an agent that reports ready for another lease, a server view that drains by cancelling). The scenario is in `tests/cell/whole_host.rs`. |
| `whole_host_faults` | `tests/whole_host_faults.rs` | The whole-host fault family: hosts silent on a prepare, a refused prepare for each refusal reason, a drain behind busy hosts, a ready report racing the prepare deadline (a scenario per second of the sweep), an agent that restarts while its host is committed, a cancelled request (queued and marked), and a held lease whose agent restarts, each over sixteen seeds under every value of `--placement`. The scenarios are in `tests/cell/whole_host_faults.rs` and `tests/cell/whole_host_cancel.rs`. |
| `whole_host_parts` | `tests/whole_host_parts.rs` | The parts of the whole-host machinery, each shown to work and to fail: the lease front's whole-host path, the actions that set how agents answer a prepare, request whole hosts and cancel a marked request, the whole-host expectations, `Scenario::under`, and the server spec's class, placement and whole-node keys. |
| `replay` | `tests/replay.rs` | Whether a seed replays, the failure report, and the golden trace hashes. |
| `docs` | `tests/docs.rs` | The check that holds this page to the crate. |

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
  bus's faults at the start, `.front(factory)` for a client other than `LeaseFront`, and
  `.scratch(env!("CARGO_TARGET_TMPDIR"))` for the server's files. For whole hosts,
  `ServerSpec::whole_node_pool(pool, shape, count)`, with `.prepare_timeout(d)`,
  `.reserve_backoff(d)` and `.max_replace(n)` for the pool's whole-node keys, and
  `.under(placement)` after `.server(...)` to run under a value of `--placement` (the
  scenario's name gains the placement);
- the work: `.work(count, Work::run_for(d))`, with `.give_up_after(d)` and `.needs(r)`;
- the actions, at a virtual time after the start, with `.at(after, action)`:
  `submit_work`, `cancel_work(items(range))`, `kill_agents`, `replace_agents`,
  `kill_agents_for_good` (a host gone, not a fault), `partition_agents(agents, lasting)`,
  `set_bus_faults(faults)` and `restart_server()` (the server started again over its
  files with `--boot resume`); for whole hosts, `request_whole_host`,
  `agent_prepare_delay`, `agent_refuses_prepare`, `agent_silent_on_prepare` and
  `cancel_when_set_aside(item, within)` (the item cancelled at the first read of the
  listing that shows a host set aside for it). Agents are named by
  the index they were added at (`agents(0..10)`), or drawn from the seed when the action
  runs (`drawn_agents(10)`);
- what is expected beyond the invariants: `Expect::all_items_terminal()`,
  `Expect::terminal_count(n, how)`, `Expect::host_state(index, state)` and
  `Expect::launched_on(&agents, after)` (each of those hosts launched a run at or after
  `after`, by the launch ledger), and for whole hosts `Expect::held_and_released(n)`,
  `Expect::every_host_serving()`, `Expect::serving_but(&agents)`,
  `Expect::tries(item, n)` (the item's request made exactly `n` tries: leases it was
  answered pending under), `Expect::held_after_a_failed_try(n)` (at least `n` items held
  after a try of theirs failed), `Expect::kept_off_a_failed_host(backoff)` (no try
  booked on a host where an earlier try of the same item failed sooner than `backoff`
  after, read from the server's listing to within one read) and
  `Expect::held_after_the_runs_on_its_host(item)`, judged once per
  seed after the end check;
- witnesses: `.witness(name, |counters| ...)`, a predicate over the agents' counters
  (`start_repeated`, `start_after_stop`, `start_dropped_late`, `result_resent`,
  `reply_invalid`, `cut_with_unlisted_run`, `unknown_host`) added over one seed, which at least one seed must satisfy. A witness
  shows that the situation the scenario is about happened, so a scenario that passes
  because nothing happened fails instead;
- `.settle_within(d)`: the virtual time the cell runs after the last action before the
  end check (600 s unless the scenario says otherwise);
- `.run_seeds(n)`: run seeds `0..n`, and fail the test with the failure as text.

The rules:

- Expect only what the server as built and the design both promise. Where they differ,
  the scenario states what both promise and leaves the rest to the invariants (the
  partition scenario of `tests/smoke.rs` does this for rows 4 and 35 of section 8).
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
submission order.

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

To replay a seed, run the same test again: seeds run from 0 in order, so it fails at the
same seed with the same hash. There is no flag that starts at a later seed.

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
| `<t> cell client item<n> <event>` | the client heard `submitted`, `pending` (a whole-host try answered pending), `queued`, `placed <host>`, `started`, `terminal <how>`, `release-sent` or `released` for an item |
| `<t> cell client item<n> cancel` | the scenario asked the client to cancel an item |
| `<t> cell client <METHOD> <status>` | a client call to the server and its status |
| `<t> cell kill <host>`, `<t> cell replace <host>` | an agent's process was killed, or a new one started |
| `<t> cell gone <host>` | the agent killed just before was killed for good: its host is gone, not a fault |
| `<t> cell restart server`, `<t> cell restart refused <flag>` | the server was stopped and started again over its files (`--boot resume`), or it refused to start again, naming the flag at fault |
| `<t> cell faults delay=<min>..<max>ms [<kinds>]` | the bus's faults changed; the kinds are among `drop`, `duplicate` and `reorder` |
| `<t> cell listing <host> live\|lost leases=<n> aside=<bool>` | the server's listing of a host changed |
| `<t> cell listing <host> forgotten` | the server no longer lists the host |
| `<t> cell launch <host>` | an agent launched a run (an entry of the launch ledger) |
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
whole-host requests under each value of `--placement`, and family `whole_host_faults` the
first two seeds of the cancelled whole-host request (`tests/cell/whole_host_cancel.rs`)
under each value of `--placement`. A trace hash covers the
cell's trace and the kernel's, so it moves when a decision of the server, the agents, the
client driver or the scenario builder changes, or when the text of a trace line changes.
It must not move for a change that only moves code. (The kernel has a golden file of its
own, `crates/js-sim/tests/golden/trace-hashes.txt`.)

`replay`'s tests `pinned_seeds_match_the_golden_file`,
`the_smoke_scenario_matches_the_golden_file`,
`the_real_agent_scenario_matches_the_golden_file`,
`the_whole_host_scenario_matches_the_golden_file_under_every_placement` and
`the_whole_host_fault_scenario_matches_the_golden_file_under_every_placement` fail on a moved
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
  only the agent wire is faulty. A repeated acquire is covered by the server's HTTP tests
  (`crates/js-lease-server/tests/server/whole_request_id.rs`); row 1 of section 8 is Open.
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
- **Real clouds.** The cell's pool registers its own hosts; no provider launches or ends a
  machine. Covered by the elastic fake's conformance suite
  (`crates/js-elastic-fake/tests/conformance.rs`) and the server's end-to-end test over it
  (`crates/js-lease-server/tests/server/elastic_fake_e2e.rs`).
- **Usage recording.** Its store reads the system's clock, so the cell turns it off.

## Which section 8 rows have a scenario

Rows of section 8 of `docs/design/jobscheduler-design.md` that a merged scenario covers,
and how much of each. A row not listed has no scenario.

| row | scenario | family file | what the scenario shows |
|---|---|---|---|
| 3 | `a_restart_of_the_server_with_runs_in_flight_double_books_nothing` | `tests/cell/server_restart.rs` | The server is stopped and started again over its lease file (`--boot resume`) with runs in flight; nothing is launched twice, no lease moves host, no host takes a new lease while it runs a run of before the restart, and everything settles. What becomes of the runs in flight (the row's subject) is not asserted: as built they are forgotten. |
| 4 | `ten_of_two_hundred_agents_die_mid_run` | `tests/faults.rs` | Ten of two hundred agents die with their state and new processes say hello holding nothing; every item is terminal, nothing runs twice, and the replaced hosts take work again (`launched_on`). That the runs become lost with no result is not asserted. |
| 25 | `a_whole_host_lease_whose_agent_restarts_after_it_is_held` | `tests/cell/whole_host_faults.rs` | A held whole-host lease's agent is started again with empty state and says hello once its host reads lost; the hello ends the lease's run, the client releases the lease, and a waiting request is then held on the host, under every placement. The row's hazard (the registry ending a held booking the lease table keeps) needs a client that keeps the lease past the hello's prepare deadline, which the lease front does not: the scenario shows no double booking along this path and does not assert the table's state, renew or a run call. |
| 31 | `a_lossy_wire_launches_every_run_once` | `tests/faults.rs` | A fifth of the agent wire's messages lost and the rest delayed up to 3 s each way, with runs shorter than the heartbeat interval; the server sends starts again (witness `start_repeated`) and every run is launched once. Agents that run a repeated start fail `at-most-one-launch`. |
| 31 | `an_agent_that_relaunches_a_repeated_start` | `tests/broken_agent.rs` | The server sends a start again (runs shorter than the heartbeat interval on a bus slower than it); agents that run the repeated start fail `at-most-one-launch`, correct agents pass with the witness `start_repeated` held. The other half of the row, an agent restart that empties the retired list, has no scenario. |
| 35 | `a_silent_host_is_never_given_a_second_copy_of_its_run` | `tests/faults.rs` | Agents are cut off longer than the grace while they run a run no heartbeat of theirs had listed (witness `cut_with_unlisted_run`); the run is never run on another host. The start timeout of the row is not built and not asserted. |
| 38 | `a_stale_start_after_a_stop_does_not_run` | `tests/faults.rs` | The stale-start situation of the row below (the same settings), over sixteen seeds with every invariant of the fault family on. |
| 38 | `an_agent_that_never_retires_a_stopped_run` | `tests/broken_agent.rs` | Every item is cancelled while its start is still on the bus, and a start arrives after the stop that follows it; agents that do not retire the stopped run fail `at-most-one-launch`, correct agents pass with the witness `start_after_stop` held. That the allocation is freed in the same commit is not asserted. |
| 39 | `a_cancelled_whole_host_request_drops_its_mark_and_the_host_serves_again` | `tests/cell/whole_host_cancel.rs` | Queued and marked: one whole-host request cancelled while queued behind a held host, and one cancelled at the first read of the listing that shows the host set aside for it; both end cancelled, no mark is left, and a later whole-host request is held on the host and launches its run, under every placement. In a whole-node pool there is no ordinary start, so a whole-host lease shows the host serving again. A cancel while preparing is not run. |
| 41 | `agents_killed_for_good_leave_no_booking_once_clients_release` | `tests/faults.rs` | Five of fifty agents die for good; their hosts read lost and take no new lease, and once the clients give up and release, nothing stays booked on them. |

The partition scenario of `tests/smoke.rs` cuts agents off while they run, but asserts only
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

The whole-host family covers no row yet. Its fifty requests wait on the server's wait list
and are presented again at every tick, but it pins neither the order the list serves
(row 11, advisory as built) nor what happens when a request is not presented within
`regain_grace` (row 12); it has no failed prepare and no hello under a held lease, so it
does not reach row 25 or 26. It holds every placement to the four whole-host invariants
above.

The whole-host fault family (`tests/whole_host_faults.rs`) runs every invariant of
`Invariant::ALL` under every value of `--placement`. Beside rows 25 and 39 above it covers
section 7's "Whole-host ready report races the prepare deadline" (a scenario per second
from five before the deadline to five after: held on that host or retried elsewhere,
never both), "Flapping agent during whole-host commit" (invariants only: when the
deadline falls after a hello is row 26 and is not pinned), section 5.4's failed prepare
(silent hosts and each refusal reason: the request held on a host that reported ready,
the failed host kept off for `reserve_backoff`, and a request refused after
`max_replace` tries) and section 6.4's "drain never cuts work" (a request behind busy
hosts held only after the runs on its host ended, every run succeeding). As built a held
host is never set aside for another request, so `drain-kills-nothing` watches the host
from the mark the server makes once it restores. It pins neither the order the wait list
serves (row 11) nor which request takes which host.

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
  wire. It runs nothing; a run ends by itself after the time its `Work` names.
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
| `builder` | `tests/builder.rs` | The scenario builder and the lease front, each shown to fail when it should. |
| `server` | `tests/server.rs` | The real server with real agents in the cell, driven through the `Cell` API directly: registration, loss and forgetting, a lease and a run on the real wire. |
| `agent` | `tests/agent.rs` | The agent node against a scripted stand-in for the server. |
| `invariants` | `tests/invariants.rs` | Each invariant over hand-built observations, with negative cases it must flag and correct siblings it must not. |
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

What a scenario can say, all of it in `crates/js-sim-cell/src/scenario/` and
`crates/js-sim-cell/src/actions/`:

- the world: `.server(ServerSpec::registered_pool(pool, Shape::gpu_host(), count))`,
  `.hosts(pool, count)` for correct agents, `.broken_hosts(pool, count, defect)` for
  agents that lack a behaviour (a `js_conformance::agent::Defect`), `.bus(faults)` for the
  bus's faults at the start, `.front(factory)` for a client other than `LeaseFront`, and
  `.scratch(env!("CARGO_TARGET_TMPDIR"))` for the server's files;
- the work: `.work(count, Work::run_for(d))`, with `.give_up_after(d)` and `.needs(r)`;
- the actions, at a virtual time after the start, with `.at(after, action)`:
  `submit_work`, `cancel_work(items(range))`, `kill_agents`, `replace_agents`,
  `partition_agents(agents, lasting)` and `set_bus_faults(faults)`. Agents are named by
  the index they were added at (`agents(0..10)`), or drawn from the seed when the action
  runs (`drawn_agents(10)`);
- what is expected beyond the invariants: `Expect::all_items_terminal()`,
  `Expect::terminal_count(n, how)` and `Expect::host_state(index, state)`, judged once per
  seed after the end check;
- witnesses: `.witness(name, |counters| ...)`, a predicate over the agents' counters
  (`start_repeated`, `start_after_stop`, `start_dropped_late`, `result_resent`,
  `reply_invalid`) added over one seed, which at least one seed must satisfy. A witness
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
  is named in the table under "Run one" above, or `docs.rs` fails. Code two binaries
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
| `no-double-booking` | By the agents' own truth, no agent runs more than its capacity, no run is run by two agents at once, and no host runs a run of another lease while a whole-host item holds a lease on it. | `host h003 runs [run "a"; run "b"] needing res(cpu=2000 mem=2048 gpus=2) beyond its capacity res(cpu=8000 mem=17179869184 gpus=1) (over on gpus)`, or `run "a" runs on host h001 and on host h004`, or `host h002 runs run "a" of lease 1.7 while whole-host item3 holds lease 1.5 there` | every step |
| `at-most-one-launch` | Summed over the cell's launch ledger, each run is launched at most once, across agent deaths and replacements. | `run "a" launched on host h001 at 64010, already launched on host h001 at 12010` | every step |
| `one-terminal-answer` | Once the client has heard a terminal answer for an item, it hears no other terminal answer and no start for it. | `item3 heard terminal(failed) at 70000 after terminal(succeeded) at 65000` | every step |
| `settles-when-quiet` | Once no fault has been active for the scenario's settle time, every item is terminal (giving up counts), the client holds no lease, and no host's listing books a lease whose run still runs. | `item3 is not terminal`, `item3 still holds lease 1.7` or `host h002 still books lease 1.7` | at the end |
| `bookings-match-agents` | At the end, the server's listing books nothing on a host that its agent does not run, and no agent runs a run that the listing does not book: the leak detector. | `host h002 books lease 1.7 and its agent runs no run of it` or `host h002 runs run "a" of lease 1.7 and the server books no such lease there` | at the end |

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
| `<t> cell client item<n> <event>` | the client heard `submitted`, `queued`, `placed <host>`, `started`, `terminal <how>`, `release-sent` or `released` for an item |
| `<t> cell client item<n> cancel` | the scenario asked the client to cancel an item |
| `<t> cell client <METHOD> <status>` | a client call to the server and its status |
| `<t> cell kill <host>`, `<t> cell replace <host>` | an agent's process was killed, or a new one started |
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
and family `smoke` the first four seeds of the lossy agent wire. A trace hash covers the
cell's trace and the kernel's, so it moves when a decision of the server, the agents, the
client driver or the scenario builder changes, or when the text of a trace line changes.
It must not move for a change that only moves code. (The kernel has a golden file of its
own, `crates/js-sim/tests/golden/trace-hashes.txt`.)

`replay`'s tests `pinned_seeds_match_the_golden_file` and
`the_smoke_scenario_matches_the_golden_file` fail on a moved hash and print the command
that regenerates the lines:

```sh
cargo test -p js-sim-cell --test replay -- --ignored regenerate_golden_hashes
```

It rewrites the lines of both families and leaves the rest of the file alone. Regenerate
only on purpose, and say in the pull request why each moved line moved: which decision
changed, or which trace text.

## What the cell does not model

The cell answers questions about scheduling decisions over the agent wire. It does not
model:

- **Duplicated or reordered HTTP messages.** HTTP carries a request and its answer once
  each, in order, on one connection, so the default bus (`Faults::default()`) never
  duplicates or reorders. A scenario that models a request retried over a second
  connection turns `reorder` or `duplicate` on for itself, with `.bus(...)` or
  `set_bus_faults`. Today one scenario does: `an_agent_that_never_retires_a_stopped_run`
  in `tests/broken_agent.rs` turns `reorder` on for every message from 2 s to 30 s. None
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
| 31 | `an_agent_that_relaunches_a_repeated_start` | `tests/broken_agent.rs` | The server sends a start again (runs shorter than the heartbeat interval on a bus slower than it); agents that run the repeated start fail `at-most-one-launch`, correct agents pass with the witness `start_repeated` held. The other half of the row, an agent restart that empties the retired list, has no scenario. |
| 38 | `an_agent_that_never_retires_a_stopped_run` | `tests/broken_agent.rs` | Every item is cancelled while its start is still on the bus, and a start arrives after the stop that follows it; agents that do not retire the stopped run fail `at-most-one-launch`, correct agents pass with the witness `start_after_stop` held. That the allocation is freed in the same commit is not asserted. |

The partition scenario of `tests/smoke.rs` cuts agents off while they run, which is the
situation of rows 4 and 35, but it asserts only what the server as built and the design
both promise (the invariants and every item terminal), so it covers neither row.

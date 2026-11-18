# A job's start: placement to a running container

Goal: the part of an action's launch that this repository owns, from the moment the server places a
job on a host to the moment its container runs, takes 100 ms or less at the median. This note says
where that time went before this change, what the change runs side by side or stops waiting for, how
the whole segment is now measured on one line per start, and what is left.

## Where the time went (measured)

The agent's `Started` lines (`docs/runbooks/start-timeline.md`) of js-agent 0.1.260 on five nodes, 887
starts over thirty minutes of a cold build, `journalctl -u js-agent-crun`:

| Segment | p50 | p90 | p99 | max |
|---|---|---|---|---|
| reply arrived to `created` | 660 ms | 4.6 s | 71 s | 107 s |
| of which `admitted` to `resolved` (providers, lookups) | 416 ms | 3.7 s | 24 s | |
| of which `dirs` to `created` (assembly, create) | 34 ms | | 39 s | |
| `crun`'s own `begin` to `running` | about 20 ms | | | |

Placement to the reply's arrival was not on the line: the start carried no time of the server's,
and the agent's `received_ms` is on its own monotonic clock.

## What was serial (read in the code)

A start with layers or mounts went through its waits one after another
(`lifecycle/machine/prepare.rs`, `providing.rs` before this change):

1. the providers: `Prepare` for every source (sources in parallel), then, once a source's `Prepare`
   was answered, its `Mount`;
2. only when every source had its directory: the lookups of all host paths (`Runtime::resolve_fs`);
3. only when the lookups were done: the run's scratch and lease directories (`make_dirs`);
4. then the assembly, then the create.

The images were asked at admission and pulled beside step 1, but nothing of steps 2 and 3 began
before the providers had all answered.

## What this change does

- **Every independent wait begins at admission.** In the worker's one step that admits a start: the
  providers are asked to prepare, the images are asked, the runtime begins the lookups of every host
  path that waits for no provider (`JobFs::without_providers`), and the directories are made. At each
  later step the providers, the images and the lookups are each followed; when the providers have
  answered, their directories are bound into the file system and the runtime begins the lookups of
  those alone (`Fs::resolve` keeps the lookups it began). The assembly is asked for when all three
  are done. A lookup refused while a provider is still out fails the start at once.
- **No wait in the path is the worker's step interval (1 s).**
  - A start with nothing to look up (image layers only, or only providers' sources before they
    answer) was answered `Pending` by `crun` on its first lookup call with no task queued to wake
    the worker, so its `resolved` waited for the next tick. `Fs::resolve` now answers at once when it
    has nothing to wait for.
  - A provider call that failed rang nothing, and was asked again at the next step, up to a second
    later. It now rings after a pause of its own (`CallBounds::retry`, 200 ms), and the ask that
    ring brings sends an unavailable call again at once; a refusal stays final.
- **The worker never waits behind another image's admission.** Both of a start's uses of its image
  on the worker thread (the assembly's pin and the create's) took the image budget's store-wide turn
  twice, and a pull's admission holds that turn while it measures a freshly unpacked image's tree and
  while it evicts. A start's use of an image the budget's book already counts now checks the store
  and touches the book, and takes no turn (`Images::use_held`). The prefetcher's pulls still take it.
- **One line per start holds the whole segment.** A job start carries the server's placement time
  (`JobStartSpec::placed_at_ms`, optional, from the job record's `placed_at`), and the timeline logs
  it with the reply's arrival in Unix time (`received_at_ms`) and the step `provided` (the last
  provider directory answered). `js-start-timeline` (crate `js-analysis`) prints the p50, p90, p99
  and maximum of every segment of the lines it is given.

## Decisions

- **The ledger's fsyncs stay.** The sidecar is written and synced before anything it names is made,
  and rewritten twice (supervisor named; running) with a sync each: six fsyncs a start. They are what
  lets a restarted agent adopt or remove what a crashed one made. `begin` to `running`, which holds
  all six with the supervisor's spawn, measured about 20 ms; the timeline's `recorded` and `running`
  steps show their share per start. They are the next agent-side candidate only if the farm table
  shows `begin->recorded` or `supervised->running` large.
- **The other sync writes stay, for the same reason.** The provider record (written and synced before
  the first `Prepare`, so a restart can release what a provider may hold) and the mount list of the
  assembly, which is on a `tmpfs` (`/run/js-agent/mounts`), where a sync costs nothing.
- **`Prepare` then `Mount` stays two calls.** The provider protocol answers `Prepare` at once and
  `Mount` when the tree can be opened; sending `Mount` before `Prepare` was answered would let the two
  arrive in either order. The second call costs one round trip on the node's socket and one step.
- **The beat after a start still waits the interval** (below): changing it changes the protocol, and
  its cost is not yet measured.

## The p99 tail: what it may be waiting on

Not yet proved either way: the lines measured above have neither `provided` nor the placement time.
The code gives three candidates, and the new line tells them apart.

1. **The provider's `Mount` on a cold store** (the tier store's tree attach). It shows as `provided`
   large and everything after it small. If so, the call and its latency go to the tier store: the
   agent's `ProviderCall::Mount` (`provider/mod.rs`, bound 30 s), answered when the tree can be
   opened.
2. **The worker held by the image budget's turn** (removed by this change). Its sign was a long
   `dirs->ready` (the assembly's pin) or `begin->image` (the create's), and, since one worker steps
   every start, a long `taken` on the starts that arrived meanwhile.
3. **A step interval or a heartbeat.** A failed provider call (removed by this change), the beat
   after a start (below), or a start dropped from a full worker queue and sent again on a later beat.
   Its sign is a long `placed->received`.

## What is left

- **The beat after a start.** A reply that carries a command does not say `beat_now`, so the agent
  waits its interval (5 s by default) before its next heartbeat, and a second job placed on that host
  within the interval waits for it (`docs/design/held-beat.md`). The bound is one interval; the farm
  table's `placed->received` will say how often it is paid. Removing it needs the server not to send a
  start again before the agent could have listed it, or the agent to beat once its worker has taken
  the start; either is a change of the held beat's protocol.
- **A placement on another server process** reaches the host only on its next heartbeat, up to one
  hold (5 s) later; the wakes are in-process (`held_beat.rs`).
- **The farm measurement.** Run a cold build on the farm with this agent and server, and print the
  table with `js-start-timeline` over each node's journal for the window; the rows are described in
  the runbook.

## Tests

The tests drive the lifecycle with counted, ordered fakes and an injected clock; none asserts a
duration: `tests/agent/lifecycle/concurrent_start.rs` and `timeline.rs` (the order of the waits
and the line), `crun/fs/fs_tests.rs` (the lookups), `crun/budget_tests.rs` (the turn),
`tests/agent/provider_client.rs` (the retry), the wire's previous-release test
(`previous_release_v0_1_208.rs`) and `js-analysis`'s timeline tests.

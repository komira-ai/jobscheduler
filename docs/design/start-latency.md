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

- **The ledger's fsyncs stay** (the second part below rewrites the sidecar once a start instead of
  twice, and makes the create off the worker). The sidecar is written and synced before anything it
  names is made, and rewritten once it runs. They are what lets a restarted agent adopt or remove
  what a crashed one made.
- **The other sync writes stay, for the same reason.** The provider record (written and synced before
  the first `Prepare`, so a restart can release what a provider may hold) and the mount list of the
  assembly, which is on a `tmpfs` (`/run/js-agent/mounts`), where a sync costs nothing.
- **`Prepare` then `Mount` stays two calls.** The provider protocol answers `Prepare` at once and
  `Mount` when the tree can be opened; sending `Mount` before `Prepare` was answered would let the two
  arrive in either order. The second call costs one round trip on the node's socket and one step.
- **The beat after a start still waited the interval** in this first part; the second part below
  ends that wait when the container runs, with no change of the protocol.

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

- **The beat after a start** (done in the second part below).
- **A placement on another server process** reaches the host only on its next heartbeat, up to one
  hold (5 s) later; the wakes are in-process (`held_beat.rs`).
- **The farm measurement.** Run a cold build on the farm with this agent and server, and print the
  table with `js-start-timeline` over each node's journal for the window; the rows are described in
  the runbook.

## Second part: the worker, the sidecar, the beat

### Measured on the farm (before)

js-agent 0.1.267 (with the first part), 343 jobs of a cold build over 25 minutes, read from the
agents' journals:

- **One worker steps every job of a host, and a start's create runs on it.** A start's
  `create_and_start` (the sidecar's record and two rewrites, each synced; the cgroup; the bundle;
  the supervisor) ran on the job worker's one thread. On a node whose SATA disk answered syncs in
  66 to 178 ms, every job of the host waited behind each start's syncs: the worker's queue of 16
  replies filled, the agent logged `the job worker is busy: an answer was dropped` 1,047 times, and,
  the dropped reply having said `beat_now`, it sent the next heartbeat after 0 ms, which the server
  answered with the same start again.
- **About 3 s of every job was waiting, on every node.** Placement to the reply's arrival was 0.9 s
  at the median (the heartbeat interval there is 1 s), the server heard `running` 1.4 s at the
  median after the container ran, and the result came 0.1 s after the job's end.

### Why, read in the code

- `Lifecycle::apply_at` called `Runtime::create_and_start` in the worker's order for a start that
  waits for nothing (`prepare_or_begin`), so the start was not even listed until its create returned,
  and nothing else of the host moved meanwhile.
- A heartbeat reply that carries a command never says `beat_now` (`agent_api/held.rs`, so a slow
  agent is not asked in a loop), and the session waited its interval after it. For that interval no
  heartbeat of the host was held at the server, so a placement on a busy host, which had just been
  given a start, waited for the next beat: the 0.9 s. The held-beat path works; a busy host was rarely
  in it. The heartbeat that told the server `running` was the one after that interval: the 1.4 s.
- `Session::beat_done` took the wait from `beat_now` before it knew whether the worker had taken the
  reply: a dropped reply that said `beat_now` was followed at once by the next heartbeat.

### What this part changes

- **The create runs off the worker** (`crun/create.rs`). `CrunRuntime` answers
  `Runtime::begin_create` with `Created::Pending` and makes and starts the lease on a pool of four
  threads (`js-create`); the lifecycle holds the start as `starting`, in the phase `Creating` with its
  slot booked, and asks `Runtime::poll_create` at each step; the pool rings the worker's wake when it
  answers. The worker keeps what reads its books (the checks, the name, refused while a create of it
  is under way, the sidecar's contents) and takes apart, on the worker as before, what a failed
  create left. A slow sync now holds its own start and nothing else. Other runtimes keep the default,
  which creates on the worker as before.
- **One sidecar rewrite a start.** The sidecar is recorded before anything is made and rewritten once,
  in the state running with the supervisor, after the network is wired: four syncs instead of six.
  The rewrite that named the supervisor while the start was still starting is gone: a teardown of a
  lease whose sidecar names no supervisor ends the supervisor the lease directory records (it did so
  already for a lease whose sidecar is gone), and that record is written before the rewrite was.
- **The beat after a start ends when the container runs.** The worker rings the sleeper's bell when
  what it publishes holds news for the server: a run or job that runs or ended, a new result, a newly
  retired job id, a taint or its clear (`worker/bell.rs`). A start just taken, listed as `starting`,
  is not news. The wait after an accepted heartbeat whose reply the worker took ends on the bell
  (`Session::beats_on_news`, `Sleeper::sleep_or_bell`); every other wait (a backoff, a retry, the
  drain's) does not. So the heartbeat after a start leaves the moment its container runs, carries
  `running`, and is then the heartbeat the server holds for the next placement on the host: a
  placement waits at most for the start before it, not for the interval.
- **A full worker is backed off from.** After a reply the worker had no room for, the next heartbeat
  waits the session's backoff (500 ms to 1 s at first, doubling while the worker stays full, reset
  by a reply it takes), not the interval and never 0 ms.

### Decisions

- **A pool of four threads, not a thread per start.** A start that hangs (a supervisor that never
  says it is ready is bounded at 30 s; a network attach by its route wait) holds a thread; four
  bound what a host does at once and what a hung disk can hold, and a fifth start waits its turn in
  the order it was begun. It is the size of the file system pool beside it.
- **The bell, not a second heartbeat.** While a heartbeat is held at the server the session thread
  waits in it; the agent sends no second request. A start's `running` rides on the heartbeat that
  leaves when the container runs, before the server holds one; a second start whose container runs
  while that heartbeat is held is reported when the hold ends, at most one hold later, as before.
- **No protocol change.** The server still answers a reply with a command without `beat_now`; the
  agent shortens only its own wait, and only on news, so an older server is served the same way and a
  slow agent is not asked in a loop.
- **A stop asked while the create runs waits for it.** The create cannot be called back, so the stop
  is carried to the made container, which is stopped as any running one is; a create that failed
  ends the start `failed_to_start`.

### Still to measure

The farm table after this agent ships (each segment's p50 and p90 on the slow node and a fast one,
from `js-start-timeline` over the journals of the same kind of cold build).

## Tests

The tests drive the lifecycle with counted, ordered fakes and an injected clock; none asserts a
duration: `tests/agent/lifecycle/concurrent_start.rs` and `timeline.rs` (the order of the waits
and the line), `crun/fs/fs_tests.rs` (the lookups), `crun/budget_tests.rs` (the turn),
`tests/agent/provider_client.rs` (the retry), the wire's previous-release test
(`previous_release_v0_1_208.rs`) and `js-analysis`'s timeline tests.

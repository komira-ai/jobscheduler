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
  the first `Prepare`, so a restart can release what a provider may hold; the third part below
  writes it right after the first `Prepare`s instead) and the mount list of the assembly, which is
  on a `tmpfs` (`/run/js-agent/mounts`), where a sync costs nothing.
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
- **The beat after a start ends when the container runs** (the third part below ends it when the
  worker has taken the start instead). The worker rings the sleeper's bell when
  what it publishes holds news for the server: a run or job that runs or ended, a new result, a newly
  retired job id, a taint or its clear (`worker/bell.rs`). A start just taken, listed as `starting`,
  is not news (the third part below makes it news). The wait after an accepted heartbeat whose reply the worker took ends on the bell
  (`Session::beats_on_news`, `Sleeper::sleep_or_bell`); every other wait (a backoff, a retry, the
  drain's) does not. So the heartbeat after a start leaves the moment its container runs, carries
  `running`, and is then the heartbeat the server holds for the next placement on the host: a
  placement waits at most for the start before it, not for the interval.
- **A full worker is backed off from, within the interval.** After a reply the worker had no room
  for, the next heartbeat waits a backoff of its own (500 ms to 1 s at first, doubling while the
  worker stays full, reset by a reply it takes), never 0 ms and never more than the interval, so a
  busy worker never makes the host miss its grace. The reply is still an accepted heartbeat: it ends
  a run of failed exchanges, so a failure after it waits the first failure's backoff.

### Decisions

- **A pool of four threads, not a thread per start.** A start that hangs (a supervisor that never
  says it is ready is bounded at 30 s; a network attach by its route wait) holds a thread; four
  bound what a host does at once and what a hung disk can hold, and a fifth start waits its turn in
  the order it was begun. It is the size of the file system pool beside it.
- **The bell, not a second heartbeat.** While a heartbeat is held at the server the session thread
  waits in it; the agent sends no second request. A start's `running` rides on the heartbeat that
  leaves when the container runs, before the server holds one; a second start whose container runs
  while that heartbeat is held is reported when the hold ends, at most one hold later, as before. (In
  the third part the heartbeat that leaves when the start is taken is held, briefly, and `running`
  rides on the one after it.)
- **No protocol change.** The server still answers a reply with a command without `beat_now`; the
  agent shortens only its own wait, and only on news, so an older server is served the same way and a
  slow agent is not asked in a loop.
- **A stop asked while the create runs waits for it.** The create cannot be called back, so the stop
  is carried to the made container, which is stopped as any running one is; a create that failed
  ends the start `failed_to_start`.

### Still to measure

The farm table after this agent ships (each segment's p50 and p90 on the slow node and a fast one,
from `js-start-timeline` over the journals of the same kind of cold build).

## Third part: delivery, the provider record, the worker's order, the lookups' threads

### Measured on the farm (before)

js-agent 0.1.280 (with both parts), one node over three hours, 554 starts, from the `Started` lines
(`placed_at_ms` to the runtime's `running`). That node took every job of the pool while these were
taken, so its load inflates them.

Read with `js-start-timeline` over that node's `journalctl -u js-agent-crun` for the three hours.

| Segment | p50 | p90 |
|---|---|---|
| placement to `running` | 515 ms | 930 ms (p99 1.8 s) |
| placement to the reply's arrival | 66 ms | 296 ms |
| arrival to `admitted` | 3 ms | 117 ms |
| `admitted` to `resolved` | 204 ms | 423 ms |
| `resolved` to `created` | 83 ms | 254 ms |
| the runtime's `begin` to `running` | 38 ms | 76 ms |

### Why, read in the code

- **The heartbeat after a start still waited.** After a reply with a command the agent waits the
  interval unless the bell rings, and a start just listed as `starting` did not ring it. No heartbeat
  of the host was held at the server until one of its starts ran, so a placement on a host with a
  start under way waited for that start's `running`. On a host that takes every job a start is
  nearly always under way; this is read in the code and is the likeliest cause of the delivery's
  p90, not measured apart.
- **The provider record's syncs came before the first `Prepare`.** `Lifecycle::provide` wrote the
  record (the file synced, renamed, the directory synced) on the worker and only then asked the
  providers, so those syncs were added to `admitted->provided` in full. A provider begins making its
  tree only when it has `Prepare`.
- **A waiting start was stepped behind other jobs' cleanup.** `Lifecycle::step` took each job, in
  the order they were listed, through its advance, its watch and its cleanup. A start is listed last,
  so the step that saw its lookups or its create answer ran every older job's cleanup first (its
  syncs, the container's removal, the scratch removal, the integrity check) before it marked
  `resolved` or asked the assembly. This is the worker's share of every step of a start on a busy
  host, and of the p90.
- **The lookups shared their threads with the assemblies.** The `js-paths` pool's four threads ran
  the lookups of host paths, the assemblies and the post-job checks in one queue. An assembly holds
  its thread for as long as its mounts take (for ever on a hung file system), so a burst of starts,
  or one hung provider tree, kept a later start's lookups waiting for a thread.

### What this part changes

- **A start taken is news.** The worker rings the bell when it lists a run or job as `starting`
  (`worker/bell.rs`), so the heartbeat after a start leaves as soon as the worker has taken it. It
  lists the start, so the server sends it no command and holds it: a placement on the host meanwhile
  is answered on that hold at once. The server holds a beat that lists a job `starting` for
  `held_beat::STARTING_HOLD` (200 ms, never more than the normal bound) instead of the normal
  bound, so the start's `running`, which rides on the next heartbeat, reaches the server at most
  that much after the container runs. It does so for `held_beat::STARTING_WINDOW_MS` (2 s, on the
  server's clock) from when a hold first saw that job `starting` (`held_beat::Starts`); after it
  the normal bound applies again.
- **`Prepare` first, then the record.** `Lifecycle::provide` remembers the sources for the release,
  asks each provider, and then writes the record, in the same call and before the worker steps
  again. A record that cannot be written still fails the start, and its cleanup releases what was
  asked. A crash between a `Prepare` and the record leaves a source that no record names: the sweep
  each provider is asked when the agent starts keeps only the runs the agent adopted, and a start
  that had no container is not one, so the sweep releases it.
- **Every start is advanced before any job is watched or cleaned** (`Lifecycle::step`): one pass
  advances every job, then a second watches and cleans each. A start that settled in the first pass
  is watched in the second, as before.
- **The lookups have their own threads** (`crun/fs/pool.rs`, now `pool.rs`): `js-lookups` for the lookups of host
  paths, `js-paths` for the assemblies and the checks, four threads each. A hung provider tree now
  holds at most four threads of each. (The sixth part gives `js-lookups` 80 threads.)

### Decisions

- **A short hold, not `beat_now` on a reply with a command.** The server sends a start again on
  every heartbeat until the agent lists it, so a `beat_now` there would ask a slow agent in a loop.
  A beat that lists the start is answered with nothing, and is held briefly.
- **200 ms for that hold.** A host with a start under way beats about five times a second, and each
  beat is a commit of the listing at the server (and was a new connection, until `js-client` kept
  its connection between requests, komira-ai/jobscheduler#1146). Shorter would add beats;
  longer would report `running` later. A placement ends the hold at once whatever its bound.
- **Only for 2 s of each start.** A start can wait minutes as `starting` (a provider's `Mount`, up to
  `--mount-provider-wait`; an image pull). Holding its host's beats short all that time would make
  about 600 beats for a two-minute wait instead of about 24. Nearly every start runs within 2 s
  (placement to `running` p99 1.8 s on the busy node), and one that takes longer is reported at most
  one normal hold after it runs. The window counts from the server's first sight of the job in a held beat, which needs no
  wire field; each server process keeps its own, and a host whose held beat lists no start is
  forgotten.
- **A constant, not a setting.** No operator has a reason to tune it apart from the interval; it is
  capped by the interval's own hold.
- **The record stays synced.** Only its place in the admission moves. A restart still finds the
  record of every start whose admission returned.
- **Two pools of four, not one larger pool.** One larger pool would still let assemblies take every
  thread; two keep a lookup from ever queueing behind an assembly.
- **`Mount` was still sent on the step after `Prepare` was answered** in this part. The eighth part
  below sends it from the provider client when `Prepare` answers.

### Compatibility and failure modes

- No wire, record or configuration format changes.
- A new agent with an older server: the beat after a start is held to the older server's normal
  bound (at most the interval, and 5 s), so `running` is reported up to that much later than before
  until the server is updated. A new server with an older agent behaves as before: that agent never
  sends a beat while its only change is a start taken.
- A start's `running` reaches the server up to 200 ms after the container runs while the host holds
  a beat, or one normal hold after it for a start that took longer than the window (before: at once,
  but only because no beat was held for the next placement).

### What is left

- The farm table after this agent and server ship, on the busy node and a node at normal load.
- `Mount` sent by the provider client as soon as `Prepare` is answered (one worker step a source):
  done in the eighth part below.
- The connection of each heartbeat (a new TCP and TLS handshake every beat, `js-client`). Built
  (komira-ai/jobscheduler#1146): `js-client` keeps one connection between requests, idle for at
  most 8 s (`IDLE_LIMIT` in `crates/js-client/src/client.rs`), so a host's beats ride one TCP and
  TLS connection until the server ends it. Still to measure: the start-latency gain on the farm.
- The p99: the integrity check's settle wait (1 s, on the worker, only after a job that left a
  process or mount behind) and the image budget's first walk after a restart, both on threads a
  start waits behind (done in the seventh part below).

## Fourth part: a finished job's cleanup off the worker

### Measured on the farm (before)

js-agent 0.1.288 (with the first three parts), a burst of about 8,900 starts over twenty minutes on
four nodes, from the agents' journals (`journalctl -u js-agent-crun`):

| Segment | p50 | p90 |
|---|---|---|
| arrival to `taken` (the reply waiting in the worker's queue) | 2.0 s | 7.0 s |
| placement to the reply's arrival | 116 ms | 10.3 s |

Both rows are `js-start-timeline`'s over the burst's `Started` lines (`received->taken` and
`placed->received`). The agents logged `the job worker is busy: an answer was dropped` 127 to 154
times per node in the same window (counted with `grep -c` over each node's journal). At light load
the same `received->taken` row is 0 ms p50 and 3 ms p90.

### Why, read in the code

- **The cleanup ran on the worker.** `Lifecycle::step` takes each finished job through its cleanup
  (`lifecycle/machine/cleanup.rs`), and two of its stages were synchronous calls on the worker's
  one thread: `Stage::Removing` called `Runtime::remove` (for `crun`: the end of the supervisor,
  `crun delete`, the unmount of the run's file system, the removal of its cgroup, of its lease
  directory with the writable layers and of its temporary directory, its pins and its synced
  sidecar), and `Stage::Scratch` removed the run's scratch directory and the directories beside it
  (one system call for each entry of the tree the job wrote).
- **A busy host finishes jobs several times a second**, and every step after an order took those
  removals on. A reply waited in the worker's queue for as long as they took (`taken`); once 16
  replies waited, the next was dropped and sent again after the session's backoff, which is the
  placement-to-arrival p90.
- The journal says when a job's cleanup events were printed, not how long each took, so the share
  of each removal in a step is read in the code, not measured. The line below measures it from now
  on.

### What this part changes

- **The container's removal runs off the worker.** `Runtime::removing` begins the removal, or
  follows the one under way, and answers `Removal::Pending` until it is done. `CrunRuntime` runs it
  on a pool of four threads (`js-remove`, `crun/remove.rs`): the run is taken out of the runtime's
  books and given to the thread, a removal that fails gives it back (the next removal goes over it
  again from the start, as before), and the pool rings the worker's wake when it answers, as the
  create does. While a removal is under way its lease's name is refused to a create and to a
  `Runtime::remove`. The trait's default removes on the caller's thread, so the other runtimes are
  unchanged; the VM router asks the runtime that holds the run.
- **The run's directories are removed off the worker.** The lifecycle removes them on a pool of four
  threads of its own (`js-scratch`, `Lifecycle::with_cleanup_threads`, `lifecycle/machine/run_dirs.rs`)
  and rings the wake when done. What the runtime mounted for a run it never made (`discard_fs`) is
  still discarded on the worker first: it is the runtime's own state. A removal that is done is not
  made again while the quota waits for the project's usage to read zero.
- **The order and the outcomes are unchanged.** The record that the checks are owed is written on the
  worker before either removal is asked, so a restart while one runs leaves the record, and the next
  start runs the owed check (tainting the host `check-owed` when it finds something or cannot run). A removal that fails taints `cleanup-failed` with the same check as before, and
  the slot stays booked; the job waits in its stage, with its slot booked, while a removal runs.
- **The cleanup is measured.** When a run's cleanup passes, the lifecycle logs
  `job: Cleaned { run_id, timeline: CleanupTimeline { steps, worker_ms } }`: the milliseconds after
  the run entered its cleanup at which the container (`removed`), the directories (`scratch`) and
  the checks (`checks`) were done, and the milliseconds the worker spent on the cleanup itself
  (`docs/runbooks/start-timeline.md`).

### Decisions

- **Pools of their own, not `js-paths`.** The `js-paths` threads run the assemblies, which hold a
  thread for as long as their mounts take (for ever on a hung file system), and a burst of starts
  keeps them busy. Removals queued there would make a start's assembly wait behind finished jobs'
  removals, moving the wait from `taken` to `ready`; the third part split the lookups from the
  assemblies for the same reason. Four threads each, the size of the pools beside them.
- **The scratch pool is the lifecycle's, not the runtime's.** The run's directories are the
  lifecycle's (its scratch root and files root), and the lifecycle runs on macOS too, where there is
  no `crun`; so the pool module moved from `crun/fs/pool.rs` to `pool.rs`.
- **No deadline.** A removal that hangs in the kernel holds its thread and its job, which stays in
  its cleanup with its slot booked, as a hung create holds its start; before, it held the whole
  worker until the watchdog ended the agent.
- **A plan setting, not always on.** `Plan::cleanup_threads` is four in the agent and 0 in the worker
  tests that look at a run's cleanup as soon as the worker settled; 0 removes on the worker as
  before.
- **The owed record, the checks and the quota stay on the worker.** The record must be on disk before
  anything is removed. The checks read the books of every job on the host. The quota's calls are
  system calls of microseconds that read and change the slot's state.

### Compatibility and failure modes

- No wire, record or configuration format changes; the journal gains the `Cleaned` line. The
  `Started` line is unchanged, so `js-start-timeline` reads it as before.
- A crash while a removal runs off the worker leaves what a crash during a removal on the worker
  left: the owed record (the next start runs the owed check) and the lease's sidecar (the start-up pass
  removes the lease).
- A thread that panics during a removal loses the run it was given: the removal answers a failure
  (the host is tainted `cleanup-failed`) and the next removal takes the lease apart by its sidecar.

### What is left

- The farm measurement after this agent ships: `received->taken` p50 in a burst of the same size,
  `the job worker is busy` counts, and the `Cleaned` lines' `worker_ms` against their steps.
- The post-job checks and the owed record's syncs still run on the worker, once for each job (the
  checks' settle wait no longer does: the seventh part).

## Fifth part: the held beat of a host that polls dispatch

### Why, read in the code

- **The agent tells the server `running` only on a heartbeat, and it has one heartbeat thread.** The
  session posts a heartbeat and waits for the reply (`run.rs`, `drive_holding`); the bell the worker
  rings when the container runs ends only the wait between two heartbeats, never a request in
  flight. So `running` reaches the server when the held request in flight at that moment returns.
- **The third part's hold.** A beat that lists a job `starting` is held up to `STARTING_HOLD`
  (200 ms) so that it is the heartbeat the server holds for the host's next placement, which a
  placement on the host answers at once. While it is held, `running` waits for it: up to 200 ms
  after the container runs, about 100 ms on average for a start that runs at a random point of
  the hold (inferred from the bound, not measured apart).
- **For a host that polls dispatch the hold delivers nothing.** Its starts come by its poll
  (`dispatch/`), and the beat's reply leaves out every start the dispatch queue carries
  (`per_job::commands_now`). So a placement on such a host wakes the held beat, the rebuilt reply is
  empty, and the beat is held on to its bound: the normal one (the interval, at most 5 s) for the
  idle host's beat that was waiting when the job was placed. The agent's one heartbeat thread stays
  in that request through the whole start, and `running` reaches the server up to that bound late.
  Every agent since the poll (komira-ai/jobscheduler#949) reports `dispatch-poll`, and a server that
  holds the job tables, which per-job starts need, serves the poll to it, so this is the path of
  every such host.

### What this part changes

Only the server's hold of a host that polls dispatch; a host that does not (an agent from before the
poll) is held as before, because the hold is how its starts reach it.

- **A placement ends the hold.** A wake whose rebuilt reply is still empty ends the hold, and the
  reply does not say `beat_now`. The agent's next heartbeat then leaves when its worker has news
  (the start taken, running or ended), or after the interval.
- **A beat with a start under way is paced, not held.** A beat that lists a job `starting`, however
  long it has been so, is answered at once when no such beat of the host was answered in the last
  `starting_hold` (200 ms) on the server's clock, and held for the rest of those 200 ms otherwise
  (`held_beat::Starts::paced`). Answered at once or at the end of its spacing, its reply does not
  say `beat_now`, so the agent's next heartbeat is the one that leaves when the container runs. A
  wake or a drain that ends such a hold answers as it does for any hold (a wake with nothing to
  send as above, a drain with `beat_now`). The third part's 2 s window does not apply to such a
  host: it kept a long start from beating five times a second, which a reply without `beat_now`
  cannot cause, and past it the beat was held to the normal bound, which would keep `running`
  back up to 5 s again.

One start on an idle host is then: the placement ends the idle beat's hold; the worker takes the
start and the beat that lists it `starting` is answered at once; the container runs and the beat
that lists it `running` leaves at once and is committed as it arrives.

### Decisions

- **Server side, no wire change.** The agent already beats again on news after a reply without
  `beat_now`, and every agent that reports `dispatch-poll` (komira-ai/jobscheduler#949) has the bell
  for news, a start taken among it (komira-ai/jobscheduler#930, komira-ai/jobscheduler#941), so it
  does so, so the server alone decides how soon a beat is answered. The agent's single heartbeat
  thread stays; a second heartbeat in flight would need the session's state (`seq`, the taint
  acknowledgements) kept per request.
- **Paced, not answered at once every time.** A burst of starts on a host makes news many times a
  second (each start taken, each running). Pacing answers the beats of such a host with a start
  under way no more often than once per 200 ms, which is how often the third part's hold answered them;
  the beats of a host with one start under way are not held at all.
- **The bound.** Against the third part, for a host that polls dispatch: the beats with a start
  under way are answered at most once per `starting_hold`, as they were, and each placement on this
  server ends at most one hold, so a host sends at most one beat more per placement on it. The
  agent's next beat after a paced or woken reply waits for news or the interval, never 0 ms.
- **The pacing record is per host, on the server's clock, in the process.** It is one number a
  host, kept only while the host's spacing lasts (each answer forgets the hosts whose spacing has
  passed); another server process paces on its own, and a beat answered there is not counted here.

### Compatibility and failure modes

- No wire, record or configuration format changes.
- An agent that does not report `dispatch-poll` is held as before (third part).
- A start whose news never comes (a start that hangs) leaves the agent's next beat to the interval,
  which is what a host with no held beat always had.
- A placement on another server does not wake this one's hold; as before, its start goes out on the
  poll, and the host's held beat ends at its bound.

### What is left

- The farm measurement after this server ships: the time from a container's `running` to the server's
  job state `running`, read from the job store's records against the agents' `Started` lines.

## Sixth part: the lookups' pool as wide as one start's host paths

### Measured on the nodes (before)

The `queue_ms` of the `Started` lines (the time the lookup a start waited on spent waiting for a
thread of `js-lookups`), one day of `journalctl -u js-agent-crun` on five nodes of one pool,
starts with a host path only:

| node | starts | p50 | p90 | p99 | over 20 ms |
|---|---|---|---|---|---|
| a | 6,670 | 0 | 302 | 3,124 | 25.3% |
| b | 6,421 | 0 | 91 | 1,450 | 16.4% |
| c | 6,968 | 0 | 230 | 2,892 | 21.5% |
| d | 6,336 | 0 | 216 | 1,031 | 23.3% |
| e | 6,760 | 0 | 225 | 4,891 | 24.5% |

The open itself (`open_ms`) was 82-96 ms p50 and 328-354 ms p90 on the same nodes, so a start that
waited for a thread waited behind four lookups of about that length. Measured from the same lines:
the most lookups whose `[queued_at_ms, ended_at_ms)` covered another's ask was 32-73 a node; with
each moved to begin when it was asked (inferred: as if no lookup had waited), 32-48. Each `Started`
line carries one lookup, the one its start waited on, so a start with several host paths counts once.

### Why, read in the code

- **Every pool had four threads** (`pool::THREADS`). A lookup is a blocking open of the host path
  beneath its root (on a tier store, the FUSE lookup of the tree) and holds its thread for as long
  as the open takes, so a fifth lookup, of the same start or another, waited for one to end.
- **Nothing bounds the starts in their lookups but the jobs the agent holds.** A job start's CPU
  and memory are the placement's to judge (`admit`, `placed`), so the agent holds up to
  `limits::MAX_HELD` (4096) jobs, any of which may be in its lookups. A pool wider than every lookup
  that could be in flight (4096 x 80) is not a bound.
- **One start's lookups are bounded:** at most `limits::MAX_LAYERS` (16) layers and
  `limits::MAX_MOUNTS` (64) mounts, each a host path at most, so 80.

### What this part changes

`js-lookups` has `LOOKUP_THREADS` = `MAX_LAYERS + MAX_MOUNTS` = 80 threads (`crun/fs/mod.rs`); every
other pool keeps `pool::THREADS` (4). A start's own lookups never wait for one another, and the
nodes' measured demand fits in the pool with room to spare.

### Decisions

- **One start's most host paths, not a guess.** It is the one bound the code gives on lookups; it is
  above what the nodes were measured to have in flight. It moves with the wire's limits.
- **Fixed, never made on demand.** A provider that hangs now holds up to 80 threads of
  `js-lookups`, and still no thread of `js-paths`, `js-create`, `js-remove` or `js-scratch`: its
  starts fail `input-unavailable` at their 60 s bound as before, and an assembly, a create, a removal
  or a cleanup is never behind it. A thread stuck in the kernel uses no CPU; its stack is reserved
  address space, touched only as far as the open goes. Before, four such threads stopped every
  lookup of every start on the host.
- **The other pools stay at four.** The `Started` line stamps the lookup's wait alone, so there is
  no measurement of a wait on another pool to widen it by.

### Compatibility and failure modes

- No wire, record or configuration format changes. The `Started` line is unchanged.
- An agent with more than 80 lookups in flight queues the rest as before, each bounded by its 60 s.

### What is left

- After deploy: the `queue_ms` of the `Started` lines, the target being a p90 under 20 ms on every
  node.

## Seventh part: the checks' settle wait and the budget's first walk off a start's threads

### Why, read in the code

- **The settle wait slept on the worker.** The cleanup's `Stage::Checks` called `CheckHook::run`
  on the worker's one thread (`lifecycle/machine/cleanup.rs`). When the first look of the process
  check saw a process of the job users' range that the baseline did not have, `Integrity` slept
  the settle wait (`--settle-ms`, 1 s by default, `ProcHost::settle`) and looked again
  (`integrity/compare.rs`). For that second every order, start and step of the host waited.
- **The image budget's first listing ran under the budget's turn.** `budget::load` counted the
  images the store held (a walk of every image's tree, `disk::held_images`) the first time a pull
  or a start's use of an image asked for it after the agent started, holding the turn every
  admission takes. Every pull, and the first start of each image, waited behind that walk.

### What this part changes

- **The checks answer the wait instead of sleeping.** `CheckHook::poll` asks the checks as a step
  that does not wait: the verdict, or `Checked::Wait(settle)` when the first look would look
  again. `Integrity` keeps the first look's processes for the run, and the next call for that run
  takes the second look and judges, with the lifecycle's books as they are then. The job waits in
  `Stage::Checks` with its slot booked and its owed record on disk until the wait has passed on
  the lifecycle's clock (`Job::checks_due`); the first step after that asks again. Meanwhile the
  worker takes orders, starts and steps as it does for a removal that runs off it. The trait's
  default is `run`, so the other hooks (the macOS scan, the tests' fakes) are unchanged.
- **The budget counts the store when the runtime starts.** `CrunRuntime` runs `load` on a blocking
  thread of its runtime as soon as its images are set up (`Images::count_held`). A start's use of
  an image before the listing has ended takes no turn: it is the store's own `ensure` (a stat when
  the image is there) and a use the book keeps for the listing, which gives it to the image when
  it counts it, so the order of eviction is what it was. A pull still lists the store first when
  the count at the start failed.
- **The cleanup's line.** The settle wait is now in the `Cleaned` line's `checks` step and not in
  its `worker_ms`.

### Decisions

- **One job's checks wait at a time.** A job that reaches its checks while another job's checks
  wait waits too, and is checked after it, in the same step once it is done. The checks compare
  the host with one baseline that each verdict moves; two looks interleaved would judge each
  job against a host the other had half settled. The cost is the second job's slot, booked up to
  one settle wait longer, never the worker.
- **No timer.** The job is asked again by the first step after the wait has passed, and the worker
  steps at least once each step interval (1 s), so the second look comes at most one interval
  late on an idle host, and at once on a busy one, which steps for every order. A wake rung at the
  wait's end would need a timer thread for a case (a process of the job user left as the job
  ended) that is rare.
- **The other checks keep `run` and its wait.** The re-check of a taint, a whole-node lease's
  checks and the checks owed at start run only when no job runs (`host_idle`, `is_quiet`, before
  the first start) and their callers need the verdict in the same call; they still wait on the
  caller's thread.
- **The count on a blocking thread, not a worker of the runtime.** The walk is a synchronous read of
  the store; on a task it would take one of the runtime's two workers for as long as it lasts.
- **A start before the listing ends does not measure its image.** It is counted by the listing when
  it was in the store, or, when the listing ended while the start fetched it, as a pull counts it
  (under the turn). An image a start fetched that the listing missed while it was under way is
  counted once a pull meets it, as an image another agent on the store pulled always was.

### Compatibility and failure modes

- No wire, record or configuration format changes. The `Started` line is unchanged; the `Cleaned`
  line keeps its fields, and only what its `worker_ms` holds changes, as above.
- A restart while a job's checks wait leaves the owed record, and the next start runs the owed
  check, as a restart during a check on the worker did.
- A store that cannot be listed at the start is logged (`the image budget: reading the image
  store`) and the first pull lists it again and fails with it, as before.
- `CheckHook::poll` and `Checked` are additions to js-agent's library API, with a default, so a
  hook written for the trait before builds and behaves as before.

### What is left

- After deploy: the p99 of the `Started` lines' `received->taken` on a node that runs jobs which
  leave processes ending as they end, and the first starts after an agent restart on a node with
  a full store, over `js-start-timeline`.

## Eighth part: `Mount` sent by the provider client when `Prepare` answers

### Why, read in the code

A source's `Mount` was sent only by the lifecycle: `Source::ask` (`lifecycle/machine/providing.rs`)
asks `Prepare` and, in the ask that is handed its success, asks `Mount`. The client
(`provider/mod.rs`) carried each call in a task of its own, and a `Prepare`'s success rang the wake,
so the `Mount` went out only once the worker had stepped for that ring: after whatever the step was
doing for other jobs, and after the steps of other starts queued before it. Each source waited for
one worker step between the two calls, with the provider idle meanwhile.

### What this part changes

- **The task that carried a `Prepare` that succeeded sends its source's `Mount`.** The answer of the
  `Prepare` is recorded and the `Mount` marked out in one turn of the flight table's lock
  (`Flights::finish`, with the call that follows), and the same task then sends it. The `Mount` is
  sent only once the `Prepare`'s answer is in, so the provider sees the two in the protocol's order.
- **The lifecycle is unchanged.** Its next step is handed the `Prepare`'s success and asks `Mount`,
  which finds the call out (`Pending`, nothing sent) or answered (the directory, handed over once).
  A `Mount` the lifecycle had asked already is not replaced.
- **One ring for the two calls.** A `Prepare` that is followed rings nothing; the `Mount`'s answer
  rings as any answer does (at once for a success, after the retry pause for a failure, which the
  lifecycle's ask then sends again as before).
- **A `Release` forgets the source's `Prepare` and `Mount`** (`Flights::forget`). A start stopped
  while its `Prepare` is out is released at its cleanup; without this its `Prepare` would still be
  followed by a `Mount` after the release. The answers it forgets are of calls no one asks again.

### Decisions

- **In the client, not the lifecycle.** The lifecycle only polls; the client is what knows the
  moment an answer arrives. The `Providers` contract says an implementation may send the `Mount`
  itself, so the lifecycle's fakes, which do not, keep the same behaviour.
- **Only after a success.** A refused `Prepare` ends the start, and an unavailable one is sent again
  by the lifecycle; a `Mount` after either would be refused or premature.
- **No chain past `Mount`.** Nothing follows a `Mount`, a `Release` or a `Sweep`.

### Compatibility and failure modes

- No wire, record or configuration format changes. The provider receives the same calls in the same
  order: `Mount` after `Prepare` was answered, now sooner.
- A provider that fails the `Mount` the client sent gets it again from the lifecycle's next ask, as a
  `Mount` the lifecycle sent would. (Later, komira-ai/jobscheduler#1274: after a `Mount` answered
  unavailable, the source's next ask, once its wait is over, sends its `Prepare` again and the
  `Mount` only once that is taken, so a provider that restarted and lost the source is asked from
  the start; `lifecycle/machine/providing.rs`.)

### What is left

- The farm measurement after this agent ships: `admitted` to `provided` on starts with providers.

## Tests

The tests drive the lifecycle with counted, ordered fakes and an injected clock; none asserts a
duration: `tests/agent/lifecycle/concurrent_start.rs` and `timeline.rs` (the order of the waits
and the line), `crun/fs/fs_tests.rs` (the lookups), `crun/budget_tests.rs` (the turn),
`tests/agent/provider_client.rs` (the retry), the wire's previous-release test
(`previous_release_v0_1_208.rs`) and `js-analysis`'s timeline tests. The third part adds
`tests/agent/lifecycle/providers.rs` (the record after the `Prepare`s, and its failure),
`concurrent_start.rs` (a start advanced before an older job's cleanup), `crun/fs/fs_tests.rs`
(lookups answered while every assembly or check thread is held), `worker/bell_tests.rs` and
`tests/agent/jobs/bell.rs` (a start taken rings the bell), and js-lease-server's
`agent_api_held_tests.rs` and `held_beat_tests.rs` (which bound a beat is held for). The fourth part
adds `tests/agent/lifecycle/removing.rs` (a start taken while a removal runs, the cleanup's
timeline, failures and a restart while a removal runs), `tests/crun_stand_in/removes.rs` (`crun`'s
removals off the worker), `lifecycle/machine/run_dirs.rs`'s unit tests and `tests/agent/vm/split.rs`
(the router). The fifth part adds js-lease-server's `agent_api_held_tests.rs` cases for a host that
polls dispatch (a placement ends the hold, a lone start's beat is answered at once and its `running`
committed as it arrives, close beats paced, the end of a paced hold and a start past the window, a
beat that lists no start, a drain), `held_beat_tests.rs`
(the pacing's arithmetic and its record), and changes `agent_api/dispatch_tests.rs`'s placement case to the
heartbeat being answered at the wake. The sixth part adds `crun/fs/lookups_tests.rs` (a start naming the
most host paths has all its lookups running at once, a lookup past the pool's threads waits for one
to end and its bound, lookups that hang hold no thread of the assemblies and checks), which takes
over `fs_tests.rs`'s case of four held lookups and a fifth. The seventh part adds
`tests/agent/integrity/poll.rs` (the first look answers the wait, the second judges, a call for
another run looks first, a second look that cannot read the processes, what changes between the
calls), `tests/agent/integrity/settle_steps.rs` (a job waits in its cleanup while a start takes its
slot, the leftover that stays taints, a second job is checked after the one that waits) and
`crun/budget_tests.rs` cases for a start before the listing (no turn and its use kept, an image the
store lacks, a listing that ends during the fetch) and the count at the start. The eighth part adds
`tests/agent/provider_client/chain.rs` (a `Mount` sent with no ask after the `Prepare` answered,
one ring for the two, a failed one sent again, a release that forgets its source) and
`provider/flight_tests.rs`'s cases of the call that follows (marked out with the success, none
after a refusal, a failure or a replaced call, never over a call of its own, none after a forget).

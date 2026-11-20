# Machines that run more than one platform, and the narrowest machine first

This file is the design for two connected changes, and it is the only place they are described:

1. **A machine can run more than one platform.** A Mac Studio runs macOS arm64 natively and Linux arm64 in a Linux guest. Its
   cores and memory are booked once, whichever platform uses them.
2. **Placement spends the narrowest machine first.** When several kinds of machine can run a job, the job goes to the machine
   that can do the fewest *other* things. A Linux arm64 job goes to a Linux-only arm64 server before a Mac, so the Mac stays free
   for the macOS work only a Mac can do. The rule is general: the same rule keeps a GPU machine free for GPU jobs.

It is a design only:

- Section 7 splits the implementation into pull-request-sized tasks.
- Section 8 lists the decisions that are the project owner's.

**How references read.**

- A section number such as "5.6" refers to [jobscheduler-design.md](jobscheduler-design.md).
- "Standing rule R1" is the table at the top of [design.md](../design.md). Rule ids without that word (T10, B8, B14, B20) are those
  of `AGENTS.md`.
- Every code reference was read in the tree this file was written against. A line number is approximate; the symbol is the
  anchor.
- The design is written against the current server: one registry per instance, which holds the hosts that said hello to it
  ([multi-instance.md](multi-instance.md) designs the move to shared node rows).

**The machines this is written for.**

| Machines | Count | Cores, memory | Platforms |
|---|---|---|---|
| Linux arm64 servers | 4 | 128, 256 GiB | `linux/arm64` only |
| Mac Studio | 4 | 28 to 32, 96 GiB | `macos/arm64` natively, `linux/arm64` in a Linux guest |
| Linux x86_64 nodes | 6 | as today | `linux/x86_64` only |

x86_64 Linux on the Macs is out of scope. Eight machines can run `linux/arm64`, and four of them run only that.

**"Machine" in this file** means the physical box that has the cores and memory. It is not `max_machines` (a pool's cap on hosts),
not a machine provider (5.6), and not the operating system's machine id the agent derives its host id from
(`crates/js-agent/src/facts.rs`).

## 1. What the code does today

### 1.1 Pools and admission

A pool has one shape, and a host is in one pool.

- **One shape.** `js_registry::PoolConfig` (`crates/js-registry/src/config.rs`) holds one `shape`. It is both the floor a hello
  must meet and what the pool books for every host it admits.
- **The floor check.** `Registry::admit` (`crates/js-registry/src/registry.rs`, near line 480) refuses a claim that does not
  `Shape::satisfies` the floor, with `shape-not-accepted` / `below-floor`. A host that offers more is booked at the shape.
- **One pool per host.** A hello names one pool, and a host id is one host in one pool (`Refusal::HostListed`).
- **What the placer sees.** `placement_shape` (config.rs, near line 404) starts from the pool's booked resources, never from the
  host's own numbers.
- **One memory percent.** `check_more` (`crates/js-lease-server/src/config/agent_setup.rs`) requires every pool's
  `memory_schedulable_percent` to equal the first pool's.
- **Several pools only when all are joined.** A server runs several pools only when every one is joined. An elastic pool is served
  alone (`SEVERAL_REGISTERING`, `crates/js-lease-server/src/file.rs`).
- **Today's model of a Linux guest.** `crates/js-registry/tests/mac_pool_shape.rs` models a Linux guest on a Mac as a second
  host in a second pool.
  - The native pool is shaped 10 cores and 36 GiB, the guest pool 14 cores and 40 GiB.
  - The operator picks the two shapes so their sum fits the smallest Mac.
  - Nothing in the code knows the two hosts are one machine, and nothing books the sum.

### 1.2 What a host advertises

A host advertises one platform.

- **One `os` and one `arch`.** The agent adds exactly one of each to its hello (`crates/js-agent/src/host.rs`, `capabilities`),
  and an operator cannot set either.
- **Matching.** `js_caps::NodeCaps` (`crates/js-caps/src/matching.rs`) holds one `arch` and one `os`, and `Request::unmet` compares
  them by equality.
- **The wire.** The capability map is flat `String -> String`: at most 64 entries, keys of at most 64 bytes, values of at most
  128 bytes (`crates/js-agent-wire/src/limits.rs`). Wire structs carry no `deny_unknown_fields`.
- **Protocol.** Within protocol major 1 a field is only added, and a receiver ignores what it does not know
  (`crates/js-agent-wire/src/protocol.rs`, [agent-wire.md](agent-wire.md)).
- **What a refused agent does.** A 403 with a code the agent cannot fix ends its session, and the program exits 2
  (`crates/js-agent/src/session.rs`, module header).
  - A 409 to a hello is retried with back-off.
  - The agent books its own runs against the one capacity it offers (`crates/js-agent/src/lifecycle/machine/mod.rs`, `booked`
    against the offer).

### 1.3 Placement

Placement is one scheduler round per queued job.

**The pass and the walk.** A pass (`SchedPlacer::place_queued`, `crates/js-sched-placer/src/jobs.rs`) walks the queue in
`js_jobs::queue_order`: higher priority, then earlier submit, then id. The walk is `js_sched::walk_in_order`
(`crates/js-sched/src/queue.rs`). A job placed nowhere closes every host it could use to the jobs behind it in that walk. "Could
use" is `usable` in `Engine::walk_holding`: capability and capacity, not current room.

**One round.** Each round chooses a host in `Servable::fit` (`crates/js-sched/src/servable.rs`, near line 301), in this order:

1. **Capable.**
   - The host is live and serving, and not cordoned.
   - It serves the lease kind and matches the job's platform (`js_caps::Request::matches`).
   - It is in the walk's open set.
2. **Room.** There are two paths:
   - Under admission by load (the default), the measured plus predicted CPU charge stays within `cores x (1 + slack)`, and memory
     stays within the schedulable share. `--placement-cpu-slack` defaults to 1.1x (`crates/js-sched/src/admission/config.rs`).
   - Without it, the free booking must fit (`Worker::has_room`, `crates/js-sched/src/scheduler/worker.rs`).
3. **Provider tier.**
   - Only the hosts of the first machine provider in the platform's order stay (`of_first_provider`,
     `crates/js-sched/src/provider_rank.rs`). It is applied on both paths of step 2.
   - It runs after the room filter, so a full host of an earlier provider never keeps a job off a free host of a later one.
   - The order (`rank::order_of`, `rank::rank`, `crates/js-lease-server/src/sched/rank.rs`) reads only whether a host's binding
     names a cloud instance: `static` before the pool's launching provider.
4. **Score.** Under load admission, `ScoreKey` (`crates/js-sched/src/load.rs`, near line 305) decides. Its fields, in order:

   | Field | What it measures |
   |---|---|
   | `slack` | overshoot into the slack zone |
   | `memory_unsafe` | padded memory above capacity at some slot |
   | `distrusted` | no usable reading, or a reading above the prediction |
   | `concentrated` | too much of one action key on the host |
   | `fit` | best fit: the tighter host wins |
   | `uncovered` | leases with no reading yet |
   | `larger_cpu` | the larger host wins |
   | `larger_near_headroom` | more CPU headroom in the near slots |
   | `hash` | a hash of the lease and the worker |
   | `worker` | the worker id |

   Otherwise the decision is the least `Load`: its share, then its lease count, then its name.

**Whole-host jobs.** They are marked on the host with the cheapest drain (`js_whole::cost::drain_cost`,
`crates/js-whole/src/cost.rs`): known tail first, then the shorter wait, the fewer leases, the provider, and the id.

**The commit.** A commit re-checks the host's live allocations in its own transaction (`JobStore::place_claimed`,
`crates/js-jobs/src/store/whole.rs`; `Capacity::has_room`, `crates/js-jobs/src/record.rs`).

- The capacity it checks against already includes the CPU slack (`capacity_of`, `crates/js-lease-server/src/sched/jobs_pass.rs`).
- It writes `hostbook/<host>` at the version it read. So two servers that place on one host conflict, and one of them tries
  again.

### 1.4 Nothing prefers a narrow machine

- No key in that order reads what else a host could run.
- A job without a GPU key matches a GPU host as readily as any other host.
- Two keys can work against a narrow server: `fit` prefers the host the job fills more tightly, which can be a smaller Mac guest,
  and `larger_cpu` prefers the larger host.
- Provider order cannot express the preference: a Linux server and a Mac that both joined with a pool token rank the same.
- The only "fewest accelerators" rule is in pass 2: `acquire::pick` (`crates/js-sched/src/acquire.rs`) prefers, after price, the
  offer with fewer accelerators when it launches a machine.

### 1.5 Usage and the operator's view

Usage and the operator's view are per host.

| Record | Keyed by |
|---|---|
| `ListHosts` `Host` (`crates/js-proto/proto/jobscheduler/v1/fleet.proto`) | no pool, platform or machine on the host |
| `UsageRecord` (`usage.proto`) | `host_id` |
| Node load rows (`usage/node/<host>/<hour>`, [usage-store.md](../usage-store.md)) | host |

The `js` CLI prints every proto field as a `path: value` line (`crates/js-cli/src/jobs/text.rs`), so a new field shows without CLI
code.

## 2. The model: a machine is the unit of capacity, a host is the unit of work

### 2.1 The two candidates

| | A. One host advertises a set of platforms | B. One host per platform, tied to one machine with a shared ledger |
|---|---|---|
| Wire | a new multi-valued platform key, matched as set membership in `js-caps` and `Shape::satisfies` | one optional `machine` field in a side's hello |
| Booking | free: one host, one book | a second level in the booking: the machine |
| Pool | a pool's floor and capabilities become per-platform lists | unchanged: each side joins a pool shaped for it |
| Taint, cordon, session, heartbeat, load reading, dispatch | one for both platforms | one per side, as today |
| Linux in a separate guest agent | the macOS agent must front the guest: a new protocol between them, and the guest's integrity checks, taint and load reported through it | the guest is an ordinary Linux host |
| Linux as a runtime inside the macOS agent | natural in the hello; the agent still needs two runtimes | the agent opens a second session for its second platform |
| "8 machines can run linux/arm64" | count hosts | count machines |

### 2.2 The choice: B

The code makes B cheaper and clearer, for four reasons.

1. **Everything that can go wrong is per host, and it stays per host.**
   - Taint, cordon, session, phase, load reading, the dispatch poll, allocations and usage rows are all keyed by `HostId`
     (section 1).
   - A Linux guest's integrity checks run in the guest. A tainted guest must stop Linux work without stopping macOS work, and the
     reverse. Under A, one taint stops both.
2. **The guest is an ordinary Linux host.** The Linux arm64 `js-agent` already registers on its own, with its own host id,
   `os=linux` and `arch=arm64`. That is what `mac_pool_shape.rs` models.
3. **Matching and pools do not change.** Under A, `NodeCaps`, `Request::unmet`, `Shape::satisfies`, the floor check and the pool's
   capabilities all become per-platform lists, and several reads assume one `os` (for example `ios.device` requires
   `os == macos`). Under B a host stays one platform.
4. **The one new thing is small and generic.** It is a ledger shared by the hosts of one machine, and it is the same for a guest
   agent and for a runtime inside the macOS agent.

What B costs:

- An agent that runs both platforms in one process opens two sessions (2.6).
- The operator's count is of machines, not hosts (6.5).

### 2.3 Terms

- **Machine.** The box with the cores and memory. Its id is the host id of its home side.
- **Side.** A host on a machine: its own host id, hello, session, pool and one platform.
- **Home side.** The side whose pool sets the machine's capacity, for example the macOS agent of a Mac. A host that names no
  machine is the home side of a machine of one side, which is every host today. A pool that some pool's `side_of` names is a
  **home pool**.
- **Other side.** A side whose hello names its machine, for example the Linux guest of a Mac. Its pool, a **side pool**, has
  `side_of` naming the home pool (6.3).
- **Ceiling.** The most an other side may book: its side pool's shape.

### 2.4 The ledger

For a machine `M` with home side `h` and other sides `s1..sn`, all sizes are scaled as the commit scales a host's today:

- memory by `memory_schedulable_percent`, the same for every pool;
- CPU by the slack (`capacity_of`).

**Machine capacity.** It is the booked shape of the home pool. It comes from configuration, so it is known while `h` is lost or has
not said hello since a restart.

**A placement on any side of `M`** books on two levels:

- The side's own room holds it. For `h` that room is the machine. For `si` it is `si`'s ceiling.
- The machine holds it: the bookings of every side of `M`, plus this one, fit the machine capacity.

**Held memory.** A side pool may declare `holds_memory = true` (6.3). It is for a warm guest, whose VM holds its configured memory
whether or not it runs a job. Then each machine with a bound side of that pool (2.5) counts the ceiling's memory as taken, whatever
is booked.

- So under a warm guest the macOS side's memory is fixed: the machine's memory minus the guest's.
- Only CPU moves with demand (4.2).
- The value is static configuration, so every server reads the same and the commit checks it.

**Load admission.** Each side's CPU charge is checked against its own room, as today. The engine also checks the sum of the
charges of all sides against the machine's scaled cores, so the slack setting holds per machine (B14).

- The charges are measured or predicted per side. A server sees only the readings of the sides it holds, so the machine sum is the
  engine's check, as all load admission is today.
- The bookings are the hard check, made in the commit.

**Whole-host jobs.**

- **On `h`.** It books the whole machine and is granted only when every side is empty.
  - Its mark is a mark on the machine: it closes every side to new placement, as a waiting job's closing does (3.4).
  - The drain cost is over all sides: the latest tail, the summed leases.
- **On `si`.** It books `si`'s ceiling on the machine and is granted when `si` is empty and the machine has room for the
  ceiling.
  - Its mark closes `si`.
  - While it waits, the home side takes only a placement that leaves the machine room for the ceiling.

### 2.5 Membership and the commit

**Which hosts are sides of which machine.**

- The membership row `machine/<machine>` records the machine's home pool and its sides. A home pool host's admitted hello writes
  the row if it is absent, and the row then lasts.
- A side's hello names its machine. The server admits it only if the machine's row exists and names the side pool's home pool.
  Every instance reads the row from the store. Otherwise the hello is refused `machine-unknown` (6.1).
- A typo or a stale `--machine` therefore never creates a machine with a fresh ledger.
- One machine has at most one side per side pool. A second host of the same side pool that names a machine with a side is
  refused `machine-side-taken` while the first is live or holds work. A rebuilt guest with a new host id joins once the old one
  is lost and holds nothing.
- The side's binding keeps its machine.
- The side's hello adds the side to the membership row at the version it read. So two instances admitting two sides at once
  cannot both pass the "one side per pool" check.

**The commit.** `JobStore::place_claimed` gains the machine capacity and the membership. In its transaction it does three things:

1. It reads the membership row and the live allocations of every member (`live_on`, the per-host read every release maintains),
   and checks the machine level.
2. It writes the side's `hostbook/<side>` row as today.
3. It also writes `hostbook/<machine>`, which is the home side's existing row, at the version it read. So two servers placing on
   two sides of one machine conflict, and one tries again.

Allocations stay keyed by host, and no allocation index is added. A release that does not know machines still writes and frees
allocations the new commit reads correctly.

### 2.6 The interface the Linux-on-Mac design must meet

The design of Linux on the Macs (a separate design) may run Linux in a guest with its own agent, or as a runtime inside the macOS
agent. Either way, the server sees exactly this:

1. **Each platform is a host session.** It has its own host id, hello, heartbeat, dispatch poll and pool, and one `os`, `arch` and
   `exec`.
   - A runtime inside the macOS agent opens a second session with a second host id. The agent then runs two lifecycle machines
     with separate capacities: the Linux one is its pool's ceiling, not the VM's size.
   - The agent's own checks stay per side. The server's ledger is the only check of the machine.
2. **The Linux side's hello names its machine.** It names the home side's host id, from a new agent flag `--machine <host id>` (a
   flag, as S2 asks). The macOS side names nothing, so a macOS agent of any release serves as a home side.
3. **The Linux side joins a side pool** (6.3). Its offer meets the ceiling, so a guest VM is sized at least to the ceiling. A warm
   guest's pool declares `holds_memory = true`. A runtime that starts one VM per job, sized to the job and destroyed at its end,
   does not.
4. **CPU readings are not counted twice.** If the macOS side ever reports a load reading, it excludes the CPU of the Linux side's
   VM processes. Today native macOS runs report no load (`NO_NATIVE_USAGE`, `crates/js-agent/src/platform/macos.rs`), so the macOS
   side is charged by prediction.
5. **Machine operations are per side.**
   - The server does not carry a cordon from one side to another.
   - A step that stops the whole machine (a reboot) drains every side, and each side's update hook cordons its own host.
   - A step that stops one side (restarting the macOS agent) leaves the other placing.
6. **Taint is per side.** A tainted guest is rebuilt and says hello again: with the same host id, as before, or with a new one
   under the rule of 2.5.

## 3. The preference: spend the narrowest machine first

### 3.1 The rule

**A machine's offers.** These are the scarce things the machine can give a job. They come from configuration:

- **Platform.** `os/arch` of the home pool and of every side pool that names it, from the pools' capabilities. For a machine of
  one side, it comes from the host's placement shape: the pool's capabilities, or the hello's for a pool that places by hello.
- **Accelerator.** `accelerator:<class>` for each of those pools whose shape books `gpus > 0`.

Every server therefore reads the same offers for a machine, including a server whose registry holds only one of its sides. A Mac
whose guest is stopped keeps both platforms.

**What a job uses on a side.** That side's platform, plus its accelerator if the job books GPUs.

**The job's breadth on a side.** The number of the machine's offers the job does not use there:

```
breadth(job, side) = | offers(machine(side)) \ uses(job, side) |
```

**The rule.** Of the hosts that can take the job now (capable and with room, steps 1 and 2 of 1.3), keep only those whose breadth
for the job is least, and let the existing order decide among them.

- The preference is a filter on hosts *with room*, so a full narrow machine never makes a job wait while a broad one has room.
- When the narrow machines are full, the job goes to the broader machine rather than waiting (3.5).

**Spill.** A job is *spill* on a side when a pool exists whose hosts could run it (platform, accelerator, network ceiling and
shape, from configuration, whatever its hosts' liveness, cordons and room) with a smaller breadth.

- A Linux arm64 job is spill on a Mac (breadth 1), because the Linux server pool has breadth 0. It stays spill while every Linux
  server is down or full.
- A macOS job on these Macs is not spill, because every pool that can run it has breadth 1. A macOS pool on machines without a
  guest would have breadth 0, and a macOS job would then be spill on the Macs that have one.

**Two instances of one rule.** In bold: the host used first.

| Job | Mac (`macos/arm64`, `linux/arm64`) | Linux arm64 server | GPU host (`linux/x86_64`, `accelerator:A`) | x86_64 node |
|---|---|---|---|---|
| Linux arm64, no GPU | breadth 1 | **0** | not capable | not capable |
| macOS arm64 | **1** | not capable | not capable | not capable |
| Linux x86_64, no GPU | not capable | not capable | 1 | **0** |
| Linux x86_64, one GPU of class A | not capable | not capable | **0** | not capable |

The GPU host is an example: none of the six nodes books GPUs today. The GPU pool of decision D32 ([design.md](../design.md),
section 15) is whole-node, and the narrowness tier leaves its quiet-host order unchanged among hosts of equal breadth.

**Derived, not configured.**

- The kinds that count (platform and accelerator) are fixed in code, in one function of `js-caps`.
- Device kinds such as attached iOS devices are not offers. They are reported at run time and differ per host, so servers would
  disagree about them. They can become offers once a pool declares them.
- A machine's offers are a pure function of the configured pools. They are computed once per pass for each host and sent to the
  scheduler, like the provider rank. The scheduler derives each job's breadth from them.

### 3.2 Where it sits, and the tie-breaks

The rule is a new tier in `Servable::fit`, right after the provider tier and before the score, on both paths. `of_first_provider`
runs on both today: servable.rs, near lines 331 and 335.

The full order for a share job:

1. Capable (1.3, step 1), including the walk's open set (3.4).
2. Room on the side, and room on the machine (2.4). The CPU slack is part of room.
3. Provider tier: the first machine provider in the platform's order.
4. **Narrowness tier: the least breadth.**
5. Score: `ScoreKey` under load admission, or the least `Load` without it. The last keys (`hash`, `worker`) make the order total.

For whole-host jobs, breadth is added after the provider:

- Quiet hosts: provider, then breadth, then id.
- Marks: drain cost (known tail, wait, leases; over all sides for a home side, 2.4), then provider, then breadth, then id.

**How it combines with the existing keys.**

- **Load and usage history.** Measured load and the predictions from usage history decide room (step 2), and the score among
  equally narrow hosts (step 5). The tier never admits a host the load predicate refused.
- **The CPU slack.** A host that admits the job only inside its slack (between 1.0x and 1.1x of its cores) has room, so the tier
  keeps it ahead of a broader host with more headroom. Its `ScoreKey.slack` then matters only among hosts of equal breadth. A Linux
  server therefore runs up to its slack before a Mac takes Linux work. Section 8 (O5) has the alternative.
- **The provider.** It stays the first key ("the provider is the first key, never a weight", the comment on `fit`). Today the
  question does not arise: a server with joined pools has no launching provider beside them (1.1). Section 8 (O1) settles it for
  when one does.
- **Whole-host reservation by least remaining time.** That order is unchanged. Breadth breaks a tie of drain cost, after the
  provider.
- **Farm reruns.** The `avoid` set (`Servable::avoided`, inside `pool_of`) runs before the tier, unchanged.
- **Probes.** Unchanged: a probe names its one host.

**Pass 2 and the scaler.**

- `acquire::pick` already prefers fewer accelerators among a provider's offers.
- The scaler's cover step (`cover`, `crates/js-scaler/src/scale.rs`) counts only free hosts of the pool it scales. Joined pools
  are not scaled, and an elastic pool is served alone (1.1), so nothing changes.
- If an elastic pool is ever served beside side pools, cover must count a free broad host as covering demand only after the narrow
  ones. That is noted for that change.

### 3.3 Worked examples on the eight arm64 machines

**The setup.**

- Pools, all joined:
  - `arm`: the four Linux servers, 128 cores and 256 GiB, `os=linux`, `arch=arm64`.
  - `mac` (home pool): the four Macs at the smallest Mac's 28 cores and 96 GiB, `os=macos`, `arch=arm64`. The 32-core Macs are
    booked at the pool's shape, as today (8, O6).
  - `mac-linux` (side pool, `side_of = "mac"`, `holds_memory = true`): a warm guest on each Mac, with a ceiling of 20 cores and
    64 GiB.
- What macOS has on each Mac:
  - CPU: from 8 cores (when Linux books its ceiling) to 28.
  - Memory: always 32 GiB, the machine's 96 less the guest's 64.
- Simplifications, so the numbers stay round:
  - The examples run with `memory_schedulable_percent = 100` and `--placement-cpu-slack 0`. With a percent `p`, every memory
    figure is multiplied by `p`. With the default slack, a server holds 17 of the Linux jobs below rather than 16, and a Mac's
    cores are 30.8 rather than 28.
  - Every machine is of one provider, `static`.
- Names: servers L1 to L4 and Macs M1 to M4. Each Mac Mi has home side `Mi` and guest side `Mi-linux`.
- Linux jobs are 8 cores and 16 GiB. macOS jobs are 6 cores and 8 GiB.

**Example 1: Linux work alone.**

- **Start.** Nothing runs. 50 Linux arm64 jobs arrive.
- **Placement.**
  - Each job's capable hosts are L1 to L4 (breadth 0) and M1-linux to M4-linux (breadth 1).
  - The tier keeps L1 to L4 while any has room. A server holds 16 jobs, so the four hold 64.
  - All 50 go to L1 to L4, and the score decides among the four.
- **Result.** The Macs take nothing. Without the tier, the `fit` key could put some on the Mac guests, which the job fills more
  tightly.

**Example 2: a burst of Linux jobs while macOS jobs run.**

- **Start.**
  - Each Mac runs 3 macOS jobs (18 cores; 24 of its 32 GiB).
  - L1 to L4 are full with 64 long Linux jobs.
- **A burst of 20 Linux jobs.**
  - L1 to L4 have no room, so the tier keeps the Mac guests.
  - Each Mac has 10 cores left on the machine, and each guest has 64 GiB of its own. So each guest takes one job (8 cores): 4
    placed, 16 waiting.
  - The waiting jobs are spill on the Macs. Each closes L1 to L4 to the jobs behind it, but not the Macs (3.4).
- **A macOS job arrives**, queued behind the 16.
  - Each Mac has 2 cores left, so it waits.
  - It is not spill, so it closes both sides of every Mac to every job behind it.
- **A Linux job ends on M1-linux**, freeing 8 cores on M1.
  - In the next pass the 16 Linux jobs walk first, and each could use M1-linux, so the first takes the freed room.
  - The macOS job waits only for the Linux jobs queued before it. No Linux job queued after it takes room on a Mac while it waits.
- **A Linux job ends on L2.** The queue head goes to L2 (breadth 0).
- **The wait is bounded.** The macOS job waits until a Mac has 6 cores free. That is bounded by the remaining run time of the
  jobs on the Macs and by the jobs queued ahead of it.
- **What macOS always keeps.** No Linux job on a Mac goes past the 20-core ceiling, so at least 8 cores of each Mac, and its
  32 GiB, are macOS's.

**Example 3, the reverse: a burst of macOS jobs while Linux work has spilled onto the Macs.**

- **Start.**
  - L1 to L4 are full.
  - Each Mac guest runs 2 Linux jobs (16 cores).
  - No macOS job runs, so each Mac has 12 cores and the macOS side's 32 GiB free.
- **10 macOS jobs arrive.**
  - Each Mac takes 2 (12 cores, 16 GiB): 8 placed and 2 waiting.
  - The 2 waiting jobs are not spill, so they close both sides of every Mac to every job behind them.
- **The Linux queue** waits for L1 to L4. A Linux job queued after the macOS jobs never takes a Mac while they wait.
- **A Linux job on M3-linux ends**, freeing 8 cores.
  - A Linux job queued before the macOS jobs walks first and takes the room if it fits.
  - Otherwise the first waiting macOS job takes 6 of the 8 cores.
- **How the room moves.** As the backlog drains, CPU moves from Linux to macOS one job end at a time. Memory does not move
  between the sides, because the guest is warm (4.2).

### 3.4 How the walk closes hosts

The walk closes, on behalf of a job that waits, the hosts it could use (`can_use` in `Engine::walk_holding`, jobs.rs, near line
367). With machines:

- **On a side where the waiting job is not spill,** it closes every side of that machine. The machine's capacity is what it waits
  for, and another side would consume it.
- **On a side where it is spill,** it closes nothing. The job waits for its narrow machines, not for this one. Spill is judged
  from configuration (3.1), so it does not change as narrow hosts go down or fill up.

"Could use" stays capability and capacity (`usable`). A side's capacity is its room on an empty machine. With a warm guest, the
home side's memory capacity is the machine's less the guest's. A job that could never fit there does not close the machine. It
waits as any job no host can hold waits, and it ends at its pool's `max_queue_wait` (`crates/js-lease-server/src/job_expiry.rs`).

`walk_in_order` itself does not change; only the `can_use` closure the placer passes to it does.

**How this reads against standing rule R1** ("no per-category tables and no reservation admission"):

- **The closing is the binding queue order applied to machines.** Nothing is held across passes, and a job ahead in the queue still
  walks first.
- **The ceiling is a pool's shape.** Each pool books its one shape, as every pool does today, and nothing keys capacity by a
  category of work.
- **Nothing is reserved.** No host is set aside for a job that is not placed. The one reservation stays the whole-node one.

### 3.5 When the narrow machines are full

**The default is to spill.** A job goes to the broader machine rather than waiting. Not waiting is worth more than keeping a broad
machine idle: the ceiling (2.4) and the closing (3.4) protect the broad machine's own work.

**Saying otherwise.** A pool or a job that must not take spill would need a key that does not exist yet. Its designed form:

- A pool key `spill = "refuse"`: its hosts take only jobs that are not spill on them.
- Or a job field.

It is built only when a use is named (B20; O4). With a refusing pool, a spill job waits for its narrow machines while they are down,
and its queue status says it waits for capacity, as any job does.

## 4. Demand-driven sharing on a two-platform machine

### 4.1 What the scheduler needs to know

| Fact | Where it comes from | How often it changes |
|---|---|---|
| Machine capacity | the home pool's shape | at a configuration change and restart |
| Each side's ceiling, and whether it holds memory | the side pool's shape and `holds_memory` | at a configuration change and restart |
| Which hosts are sides of which machine | the side's hello, kept in its binding and in `machine/<machine>` | at a hello |
| Each side's bookings | the live allocations of the side (the job store) | at every placement and every freed allocation |
| Each side's measured CPU | the side's load reading (`NodeLoad`), as today | at every heartbeat |

The scheduler needs no demand forecast and no split target. The split follows from the bookings: what one side does not book is
free for the other, within the ceiling.

### 4.2 How fast the split can change

- **CPU moves at the next placement pass after a booking ends.** A pass runs on every tick, and after every heartbeat that changes
  something (jobs.rs, module header). A guest's vCPUs are threads and hold no core when idle.
- **Memory moves only as the configuration says.**
  - With a warm guest (`holds_memory = true`), memory does not move: Virtualization.framework fixes a VM's memory at creation
    ([vm-backend.md](vm-backend.md) on resizing).
  - With one VM per job (`holds_memory` false), memory moves at each job's end.
  - A ballooned guest, whose held memory falls as it returns memory, would need a runtime value every server and the commit can
    read. That is a later, additive change, and the choice is O3.
- **A running job is never moved, shrunk or stopped to make room.** Its booking stays on the machine until it ends. "The other
  side wants it" is not an end reason.

### 4.3 What a running job on one side means for the other

A running job's booking counts on the machine, so the other side has that much less until the job ends. Nothing else crosses
sides:

- A Linux job's memory pressure, taint or crash is the guest's.
- The macOS side keeps its own runtime limits ([mac-runtime.md](mac-runtime.md)).

### 4.4 The guarantee that macOS work is not starved by Linux work on a Mac

1. **The floor.** Linux never books past its ceiling, so macOS always has at least the machine less the ceiling: 8 cores in the
   example. With a warm guest the macOS memory is fixed at the machine's less the guest's (32 GiB). Start checks hold the ceiling
   within the home pool's shape (6.3).
2. **Precedence in the queue.** A macOS job that waits closes the whole Mac to every job behind it (3.4), so no Linux job queued
   after it takes room on a Mac while it waits.
3. **A bounded wait.** A waiting macOS job is behind only the jobs queued before it. The room it waits for frees as the jobs on the
   Macs end, and every job has a wall-time limit.
4. **Spill is a second choice.** A Linux job reaches a Mac only when no Linux-only server open to it in the walk has room (3.1).
   A Linux job that waits closes the Linux servers to the jobs behind it (3.4). So a smaller Linux job behind it can spill to a
   Mac while a server has room the waiting job could not use. That is the queue order working, and the ceiling still bounds it.

What the guarantee does not give: a macOS job queued *after* Linux work of higher priority still waits for it. Queue order is the
one order of the queue (O7).

## 5. Compatibility

| Peer | Meets | Result |
|---|---|---|
| Old agent (no `machine`) | new server, any pool | It is the home side of a machine of one side, which is every host today. |
| Old agent in a side pool | new server | Refused `403 machine-required`. The agent ends its session naming the code and exits 2 (1.2). The operator restarts it with `--machine`. |
| New agent with `--machine` | old server, or a new server whose pool has no `side_of` | The field is ignored, and the guest is a host of its own pool with a static shape, as today. |
| New pool file (`side_of`, `holds_memory`) | old server | Refused at start: unknown key (`KEYS`, `crates/js-lease-server/src/file/joined.rs`). |
| Old pool file | new server | Unchanged. No machine has a second side. |
| New proto fields (6.5, 6.6) | old client | Skipped (proto3; the evolution rule of `service.proto`). |
| New client | old server | The new response fields are absent. A `machine_id` filter is dropped over gRPC (every record is returned, none carrying `machine_id`, which tells the client) and refused with a 400 over the HTTP gateway. |
| New rows `machine/<machine>`, a binding with a machine | previous release | It never reads the membership row. It reads a binding without the field. |
| Allocations written by the previous release | new server | Read as today: allocations stay keyed by host, and the machine check reads them by host (2.5). |

**The migration from today's two static pools** (the layout of `mac_pool_shape.rs`):

This runs once every task of 7.2 through `jsmp-place` has merged and been released.

1. **Roll every server to the new release.** Pool files are unchanged.
2. **Restart each guest agent with `--machine`.** Its pool has no `side_of` yet, so the field is ignored.
3. **Cordon every host of both Mac pools** (`CordonHost`).
   - The cordon is a store row that every instance's placement checks, so no server places on a Mac during the switch.
   - Running jobs keep running.
4. **Change the pool file in one edit, and restart every server.** The edit:
   - adds `side_of` and `holds_memory` to the guest pool;
   - raises the home pool to the whole Mac;
   - sets the guest pool to its ceiling.

   The shapes must change together. The start checks of 6.3 refuse a side pool larger than its home, and today's guest pool
   (14 cores, 40 GiB) is larger than today's native pool (10 cores, 36 GiB).

   A server with the old file and one with the new file keep two different ledgers for one Mac. The cordon of step 3 is what keeps
   them from booking over each other until every server has the new file.

   A binding with no machine that first names one records it; that is not `host-bound`.
5. **Uncordon the Mac hosts.** The ledger counts the jobs that kept running, because it sums the live allocations by host (2.5).

**Rollback** runs the same steps backwards, under the same cordon: restore the old file on every server, then roll the servers back.

**The behaviour change for existing pools.** The narrowness tier changes placement only where hosts of one platform differ in
offers:

- Today's farm has none, so its order is unchanged (test `an_old_farm_places_as_before`).
- A pool whose shape books GPUs beside a pool of the same platform that books none is the intended change: jobs without GPUs go to
  the hosts without GPUs first (test `a_job_without_a_gpu_goes_to_the_host_without_one_first`).

## 6. What changes, layer by layer

### 6.1 Wire (`js-agent-wire`) and agent

**`Hello`** gains `machine: Option<String>` (`serde(default, skip_serializing_if)`). It is a host id, checked by `Hello::validate`.

**New refusal codes:**

| Code | Status | When |
|---|---|---|
| `machine-required` | 403 | a side pool's hello names no machine |
| `machine-unknown` | 403 | no home pool host of that id has said hello (no `machine/<machine>` row naming the home pool) |
| `machine-side-taken` | 409 | the machine already has a live side of this pool, or one that holds work |

The agent retries a 409 to a hello with back-off (1.2), so a rebuilt guest joins once its predecessor is gone.

A hello naming a machine other than the one its binding records is refused with the existing `host-bound`.

**Agent.** A new flag `--machine <host id>` sets `Hello::machine`. With a guest agent the macOS agent changes nothing. A runtime
inside the macOS agent needs the second session and lifecycle machine of 2.6.

### 6.2 Registry (`js-registry`)

- **Record and binding.** `Record` and `Binding` gain `machine: Option<HostId>`, so a side put back after a restart
  (`restore_tainted`) keeps its machine.
- **Admission.** `Registry::admit` adds the checks of 6.1, in this order after the floor check: `machine-required`,
  `machine-unknown`, `machine-side-taken`. For a pool with no `side_of`, a named machine is ignored.
- **The membership row.** It is written with the binding (2.5).
- **The view.** `HostView` gains `machine` and `platform` (`os/arch` of the placement shape).
- **The placer seam.** `Placer` gains a default no-op `host_on(&self, host, machine: Option<&HostId>)`, called with `host_up`.
- **The pool config.** `PoolConfig` gains `side_of: Option<ProviderId>` and `holds_memory: bool`.

### 6.3 Pool file (`[[pool]]`, `kind = "joined"`)

Two optional keys:

```toml
[[pool]]
kind  = "joined"
name  = "mac-linux"
class = "share"
side_of      = "mac"        # optional: hosts of this pool are sides of machines whose home is in pool "mac"
holds_memory = true         # optional, only with side_of: the side holds its shape's memory on every machine it is bound to
shape = { cpu_millis = 20000, memory_bytes = 68719476736 }   # the ceiling of the Linux side of one Mac
capabilities = { os = "linux", arch = "arm64" }
max_machines = 4
```

**Checked once at start** (B20), each refused naming the pool and the key:

- `side_of` names a joined pool of the same file, and that pool has no `side_of`.
- The home pool and each side pool name `os` and `arch`, and no two pools of one machine name the same pair.
- A side pool's `cpu_millis` and `memory_bytes` are each at most the home pool's.
- The memory of all side pools with `holds_memory`, together, is less than the home pool's memory.
- A side pool books no GPU.
- `holds_memory` appears only with `side_of`.

The memory percent is already equal across pools (`check_more`). The `--agent-pool-config` file holds one pool, cannot have
sides, and refuses both keys.

### 6.4 Placement (`js-sched`, `js-sched-placer`, `js-jobs`, `js-caps`)

**`js-sched`.** It is a pure crate, so R4 holds: `BTreeMap`, no clock.

- **New per-worker inputs.** `Scheduler::set_machine(worker, Option<WorkerId>)` and `set_offers(worker, BTreeSet<Offer>)`,
  re-sent each pass like `set_provider_rank`. Breadth is derived per (worker, request) from the offers.
- **Room.** `Worker::has_room` and the load admission check the machine level, including held memory (2.4).
- **The tier.** `Servable::fit` gains the narrowness tier after `of_first_provider` on both paths.
- **Whole-host orders.** `drain_cost` and the quiet-host order gain breadth after the provider.
- **Whole-host on a machine.** Its marks and drain cost are over the machine (2.4).

**`js-caps`.** It gains `offers(&NodeCaps) -> BTreeSet<Offer>` (one pool's offers) and
`uses(&Request, &NodeCaps) -> BTreeSet<Offer>`, both pure.

**`js-sched-placer`.**

- `OpenHost` gains `machine` and the machine's offers. `jobs_pass.rs` fills them from the registry's view and the configured
  pools.
- `capacity` grows the machine capacity, the ceiling and the held memory for `place_claimed`, all scaled as `capacity_of`
  scales.
- `walk_holding`'s `can_use` closes by machine, and not on spill sides (3.4).

**`js-jobs`.** `place_claimed` gains the membership read, the machine check and the `hostbook/<machine>` write (2.5).

### 6.5 CLI and API output

**`fleet.proto` `Host`** gains three fields:

```
string machine = 13;          // the machine's id (its home side's host id); this host's own id when it is a home side
string platform = 14;         // "os/arch", e.g. "linux/arm64"
repeated string offers = 15;  // the machine's offers (3.1), sorted
```

The `/v1/hosts` JSON (`HostEntry`, `crates/js-lease-server/src/hosts.rs`) gains the same three.

**`service.proto` `ListHostsResponse`** gains a summary, counted over the machines with a live side in the server's registry:

```
repeated PlatformCount platforms = 2;
message PlatformCount { string platform = 1; uint32 machines = 2; uint32 only = 3; uint32 hosts = 4; }
```

- `machines`: the machines with a side of this platform.
- `only`: those whose offers are this platform alone.
- `hosts`: the sides of this platform.

What the operator sees (`js host list` prints every field as `path: value`):

```
platforms[0].platform: linux/arm64
platforms[0].machines: 8
platforms[0].only: 4
platforms[0].hosts: 8
platforms[1].platform: linux/x86_64
platforms[1].machines: 6
platforms[1].only: 6
platforms[1].hosts: 6
platforms[2].platform: macos/arm64
platforms[2].machines: 4
platforms[2].only: 0
platforms[2].hosts: 4
```

"8 machines can run linux/arm64, 4 of them only that." `hosts` is 8 because each Mac's Linux side is one host.

### 6.6 Usage accounting

- **Usage records.** `UsageRecord` gains `string machine_id = 23` and `string platform = 24`, written from the host's view when
  the record is written. An empty `machine_id` means the host is its own machine, as every host is before this change.
- **The usage request.** `GetUsageRequest` gains `string machine_id = 5`, a filter.
- **Node load rows** stay per host (`usage/node/<host>/<hour>`). A machine's load is the sum of its sides' rows, and no row format
  changes.
- **Attribution.** A booking belongs to the one side the job ran on. Breadth and spill are not recorded, since they derive from
  configuration.

### 6.7 What kbf sends to ask for a platform

Nothing new. A job names its platform as today:

- Job API: `Constraints.os` and `Constraints.arch` (`job.proto`). For example `os = "linux"`, `arch = "arm64"`, or
  `os = "macos"`, `arch = "arm64"`.
- Lease path: `os` and `arch` in the request's capabilities.

What kbf must keep doing:

- **Always send `arch`.** A Linux job with no `arch` is capable on x86_64 and arm64 hosts alike.
- **Send `exec` only when the action needs it.** A Mac's Linux side and a Linux server are both `exec = container`.

The preference is the server's: kbf names no pool and no machine.

## 7. Tests and tasks

### 7.1 Tests the implementation must pass

Every test asserts behaviour. Where time matters it uses the injected clock (`js_state::ManualClock`), and no test asserts elapsed
time (T10). The tests are Rust tests on fakes, so they run on the Linux x86_64 pull-request runners. The names below are the
tests' names.

**`js-caps`**

- `offers_count_the_platform_of_every_pool_of_a_machine`: a stopped side counts too.
- `offers_count_an_accelerator_only_where_the_pool_books_gpus`
- `a_job_uses_the_platform_of_its_side_and_the_accelerator_it_books`

**`js-sched`: narrowness**

- `a_linux_job_goes_to_the_linux_only_host_before_the_mac`
- `a_linux_job_goes_to_the_mac_when_every_linux_only_host_is_full`: placed in the same round, without waiting.
- `a_job_without_a_gpu_goes_to_the_host_without_one_first`
- `a_gpu_job_is_unchanged_by_the_tier`
- `narrowness_comes_after_the_provider`
- `narrowness_applies_with_admission_by_load_off`
- `equal_breadth_falls_to_the_score`
- `a_narrow_host_within_its_cpu_slack_is_chosen_before_a_broad_idle_host`
- `an_old_farm_places_as_before`: hosts of one platform each, with equal offers; choices equal the current goldens.

**`js-sched`: the machine ledger**

- `two_sides_of_a_machine_never_book_more_than_the_machine`
- `a_side_never_books_more_than_its_ceiling`
- `a_warm_guest_holds_its_memory_with_nothing_booked`
- `machine_cpu_admission_sums_the_charges_of_both_sides`: within the machine's scaled cores.
- `a_whole_host_job_on_the_home_side_closes_every_side_and_waits_for_all_to_empty`
- `a_whole_host_job_on_a_side_keeps_the_home_side_from_taking_its_ceiling`
- `whole_host_marks_break_a_drain_cost_tie_by_breadth_after_the_provider`

**`js-sched-placer`: the walk**

- `a_waiting_macos_job_closes_both_sides_of_a_mac_to_jobs_behind_it`
- `waiting_linux_jobs_never_close_a_mac_to_macos_jobs_behind_them`: the second and later waiting Linux jobs too, and with every
  Linux server cordoned.
- `a_waiting_linux_job_closes_the_linux_only_hosts_to_jobs_behind_it`
- `a_macos_job_larger_than_the_home_side_can_ever_hold_closes_nothing`

**`js-jobs`: the commit**

These use a fake store and two `JobStore` handles as two servers.

- `two_servers_placing_on_two_sides_of_one_machine_do_not_overbook_it`
- `a_placement_on_a_side_conflicts_with_one_on_the_home_side_read_at_the_same_version`
- `the_machine_check_counts_allocations_written_without_a_machine`: as a previous release writes them.
- `the_commit_accepts_a_machine_placement_inside_the_cpu_slack`

**`js-registry`**

- `a_side_hello_without_a_machine_is_refused_machine_required`
- `a_side_naming_a_machine_whose_home_never_said_hello_is_refused_machine_unknown`
- `a_home_hello_writes_the_membership_row_once`
- `a_second_side_of_one_pool_on_one_machine_is_refused_machine_side_taken`
- `a_rebuilt_side_joins_once_its_predecessor_is_lost_and_holds_nothing`
- `two_instances_admitting_two_sides_of_one_machine_admit_one`
- `a_side_naming_another_machine_than_its_binding_is_refused_host_bound`
- `a_binding_without_a_machine_records_the_first_one_named`
- `a_side_put_back_after_a_restart_keeps_its_machine`
- `a_machine_named_in_a_pool_without_side_of_is_ignored`

**`js-agent-wire`**

These extend `tests/previous_release_v0_1_208.rs`:

- `a_previous_release_hello_reads_as_a_home_side`
- `a_hello_with_a_machine_reads_in_the_previous_release`
- `a_hello_with_an_invalid_machine_is_refused_invalid_hello`

**`js-lease-server`: configuration**

- `side_of_naming_no_pool_is_refused_at_start`
- `side_of_naming_a_side_pool_is_refused_at_start`
- `a_side_shape_larger_than_its_home_is_refused_at_start`
- `side_pools_holding_all_the_home_memory_are_refused_at_start`
- `a_side_pool_booking_a_gpu_is_refused_at_start`
- `pools_of_one_machine_naming_one_platform_are_refused_at_start`
- `a_pool_of_a_machine_without_os_or_arch_is_refused_at_start`
- `holds_memory_without_side_of_is_refused_at_start`
- `the_agent_pool_file_refuses_side_of`

**`js-lease-server`: output and usage**

- `list_hosts_counts_machines_per_platform_and_those_that_run_only_it`: the 8 and 4 of 6.5.
- `a_host_lists_its_machine_platform_and_offers`
- `a_usage_record_names_its_machine_and_platform`
- `get_usage_filters_by_machine`

**`js-sim-cell`: the three examples of 3.3, as scenarios on the fake clock**

- `linux_work_alone_never_reaches_a_mac`
- `a_linux_burst_while_macos_runs_keeps_the_macos_floor_and_queue_order`
- `a_macos_burst_while_linux_has_spilled_takes_the_macs_back_as_linux_jobs_end`

The invariant "nothing double-booked" is extended to machines and checked on every step.

### 7.2 Tasks

Each task is one pull request. Each updates the docs it changes and moves its goldens in the same pull request (T19).

| Task | What | After |
|---|---|---|
| `jsmp-offers` | `js-caps` offers and uses, with tests | none |
| `jsmp-wire` | `Hello::machine`, the three refusal codes, the agent's `--machine`, the previous-release tests | none |
| `jsmp-ledger` | `js-sched` machine ledger: `set_machine`, room and load admission at the machine level, held memory, whole-host jobs on a machine | none |
| `jsmp-narrow` | `js-sched` narrowness tier on both paths, the whole-host orders, `set_offers` | `jsmp-offers` |
| `jsmp-pools` | `side_of` and `holds_memory`, start checks, registry admission, `machine` in the record and binding, the membership row, `HostView.machine` and `platform`, the `Placer` seam | `jsmp-wire` |
| `jsmp-commit` | `js-jobs` membership read, machine check and `hostbook/<machine>` write in `place_claimed`, the two-server tests | `jsmp-pools` |
| `jsmp-place` | the placer wiring: machine, offers, capacity, ceiling and held memory into `OpenHost` and the commit; the walk's closing by machine and spill | `jsmp-ledger`, `jsmp-narrow`, `jsmp-commit` |
| `jsmp-output` | `Host` fields, `ListHostsResponse.platforms`, `/v1/hosts` JSON, the `UsageRecord` and `GetUsageRequest` fields | `jsmp-offers`, `jsmp-pools` |
| `jsmp-sim` | the three `js-sim-cell` scenarios and the machine invariant | `jsmp-place` |

`jsmp-offers`, `jsmp-wire` and `jsmp-ledger` can land in parallel. A pool file names `side_of` only after `jsmp-place` is merged and
released (the migration of section 5).

`jsmp-pools` alone already refuses side hellos without a machine. Without the placer wiring of `jsmp-place` the server would not
book the machine. No pool file uses `side_of` before then.

## 8. Decisions for the project owner

Each decision has options and a recommendation. Everything else in this file follows from the code and the request.

**O1. The provider before narrowness, once a launching provider serves beside joined pools.**

- (a) **Provider first.** Recommended. It keeps the documented rule, and an owned broad machine is used before a rented narrow one,
  which the scaler can then release.
- (b) Narrowness first. Rent is spent while the Mac idles, and the Mac stays free for macOS work sooner.

**O2. The Linux ceiling on a Mac.** It sets the CPU floor macOS keeps and, with a warm guest, macOS's memory.

- (a) **20 cores and 64 GiB.** Recommended. macOS keeps at least 8 cores and has 32 GiB.
- (b) 14 cores and 48 GiB. macOS keeps 14 cores and has 48 GiB, and Linux spill gets less.
- (c) No ceiling below the machine. CPU protection then rests on queue precedence alone (4.4, item 2). It is not possible with a
  warm guest, whose memory must leave macOS some.

**O3. Whether memory moves between the sides of a Mac.**

- (a) **A warm guest (`holds_memory = true`), with CPU shared by demand and memory split statically.** Recommended for the first
  guest. It needs nothing beyond this design.
- (b) A ballooned guest, whose held memory the commit reads at run time. It is a later additive change.
- (c) One VM per job, sized to the job. Memory moves at every job end, at the cost of a VM start per job.

The Linux-on-Mac design chooses the runtime, and the server's side is set by `holds_memory`.

**O4. Who may refuse spill.**

- (a) **Nobody until a use is named.** Recommended (B20). Every job spills.
- (b) The pool key `spill = "refuse"` of 3.5, now. A pool that refuses makes Linux work wait for the servers while the Macs idle.

**O5. Whether the narrowness tier may spend the CPU slack.**

- (a) **Yes: slack is planned capacity.** Recommended. A Linux server runs up to its slack before a Mac takes Linux work.
- (b) No: a broad host with headroom below 1.0x comes before a narrow host that would enter its slack. The tier then ranks after
  the score's `slack` key.

**O6. Macs of different sizes.**

- (a) **Book every Mac at the smallest Mac's shape (28 cores).** Recommended. It is today's rule (`mac_pool_shape.rs`), and the
  32-core Macs keep 4 cores in reserve.
- (b) A pool per Mac size, with a side pool for each. It uses every core, at the cost of two more pools.

**O7. Whether macOS work on a Mac outranks Linux spill of higher priority.**

- (a) **No: queue order alone.** Recommended. Priority is the one order of the queue, and the floor of O2 bounds what Linux takes.
- (b) Yes: a waiting job that is not spill also closes the Mac to spill jobs ahead of it in the queue. That is a hold across passes,
  close to the reservation standing rule R1 forbids, so it needs a ruling first.

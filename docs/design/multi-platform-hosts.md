# Machines that run more than one platform, booked at what they report, and the narrowest machine first

This file is the design for three connected changes, and it is the only place they are described:

1. **A host is booked at what it reports.** Today a pool's shape decides what every host of the pool may book. After this change
   each host offers the cores and memory its agent reports, on Linux and macOS alike.
2. **A machine can run more than one platform.** A Mac Studio runs macOS arm64 natively and Linux arm64 jobs, each Linux job in a
   VM of its own. The machine's cores and memory are booked once, whichever platform uses them. The split between the two follows
   demand: Linux work may use a whole Mac when macOS work does not need it.
3. **Placement spends the narrowest machine first.** When several kinds of machine can run a job, the job goes to the machine that
   can do the fewest *other* things. A Linux arm64 job goes to a Linux-only arm64 server before a Mac, so the Mac stays free for
   the macOS work only a Mac can do. The rule is general: the same rule keeps a GPU machine free for GPU jobs.

It is a design only. Section 7 splits the implementation into pull-request-sized tasks. The owner's choices are recorded in section
8, and section 9 lists what they leave open.

**How references read.**

- A section number such as "5.6" refers to [jobscheduler-design.md](jobscheduler-design.md).
- "Standing rule R1" is the table at the top of [design.md](../design.md). Other rule ids (T10, B8, B14, B20) are those of
  `AGENTS.md`.
- Every code reference was read in the tree this file was written against. A line number is approximate; the symbol is the anchor.
- The design is written against the current server: one registry per instance, holding the hosts that said hello to it
  ([multi-instance.md](multi-instance.md) designs the move to shared node rows).

**The machines this is written for.**

| Machines | Count | Cores, memory | Platforms |
|---|---|---|---|
| Linux arm64 servers | 4 | 128, 256 GiB | `linux/arm64` only |
| Mac Studio | 4 | 28 to 32, 96 GiB | `macos/arm64` natively; `linux/arm64`, one VM per job |
| Linux x86_64 nodes | 6 | as today | `linux/x86_64` only |

x86_64 Linux on the Macs is out of scope. Eight machines can run `linux/arm64`, and four of them run only that.

**"Machine" in this file** means the physical box that has the cores and memory. It is not `max_machines` (a pool's cap on hosts),
not a machine provider (5.6), and not the operating system's machine id the agent derives its host id from
(`crates/js-agent/src/facts.rs`).

## 1. What the code does today

### 1.1 Pools, shapes and admission

**A pool has one shape, and it decides both admission and booking.**

- **Where the shape lives.** `js_registry::PoolConfig` (`crates/js-registry/src/config.rs`) holds one `shape`, read from the pool
  file:
  - `shape = { cpu_millis, memory_bytes, gpus, gpu_class }` of a `kind = "joined"` `[[pool]]`, a required key
    (`crates/js-lease-server/src/file/joined.rs`, where `shape` is read and refused as "required" when absent);
  - or the `[shape]` table of the one-pool `--agent-pool-config` file (`crates/js-lease-server/src/agent_pool.rs`).
- **The shape as a floor.** `Registry::admit` (`crates/js-registry/src/registry.rs`, near line 480) refuses a hello whose claim
  does not `Shape::satisfies` the shape, with `shape-not-accepted` / `below-floor`.
- **The shape as the booking.**
  - `PoolConfig::list_claimed` books every admitted host at `placement_shape(&self.booked, claim, by_hello)` (config.rs, near line
    372). Its resources are always the pool's `booked` resources, the shape cut by `memory_schedulable_percent`, and never the
    host's own numbers. `PoolConfig::new` says so: a job "is placed and sized against the booked shape, never against what a host
    says about itself".
  - A host that offers more is booked at the shape.
  - `crates/js-registry/tests/mac_pool_shape.rs` pins this rule for the Macs: a pool shaped at the smallest Mac (28 cores) books a
    32-core Mac at 28.
- **What reads the shape.**
  - The placer is told the host's booked shape (`Registry::admit` calls `tell_up` with `book.shape_of`).
  - The pass gives the commit `capacity_of(&view.shape, cpu_slack)` (`crates/js-lease-server/src/sched/jobs_pass.rs`).
  - The server sizes its usage limits from the largest pool shape (`crates/js-lease-server/src/agents.rs`, near line 279).
  - `check_pool` (`crates/js-lease-server/src/config/placement.rs`) requires the shape to name an `arch` unless the pool places by
    hello.
- **Pools beside each other.**
  - Every pool's `memory_schedulable_percent` must equal the first's (`check_more`, `crates/js-lease-server/src/config/agent_setup.rs`).
  - A server runs several pools only when every one is joined; an elastic pool is served alone (`SEVERAL_REGISTERING`,
    `crates/js-lease-server/src/file.rs`).
  - A hello names one pool, and a host id is one host in one pool (`Refusal::HostListed`).

### 1.2 What a host reports

- **Size.** The agent detects its cores and memory (`facts::detect`, `crates/js-agent/src/facts.rs`):
  - cores from `getconf _NPROCESSORS_ONLN` (the standard library's count when `getconf` fails);
  - memory from the system;
  - on Linux, lowered to the agent's own cgroup limits.
- **The offer.** `--cpu-millis` and `--memory-bytes` only lower it (`capped`, `crates/js-agent/src/host.rs`). It travels in the
  hello's `WireShape` (`cpu_millis`, `memory_bytes`, `gpus`, ...), and the registry keeps it as the host's `claim`.
- **When it is sent.** The offer is sent at hello only. A heartbeat carries no size.
- **Hostkeeper's facts file.** It gives the agent its host id (`--host-id-from-facts`), and no size.
- **Platform.** The agent adds exactly one `os` and one `arch` to its hello (`capabilities` in `host.rs`). `js_caps::NodeCaps`
  (`crates/js-caps/src/matching.rs`) holds one of each, and `Request::unmet` compares them by equality.
- **The wire.** The capability map is flat `String -> String`, with at most 64 entries, keys of at most 64 bytes and values of at
  most 128 bytes (`crates/js-agent-wire/src/limits.rs`).
- **Unknown fields.** Wire structs carry no `deny_unknown_fields`. Within protocol major 1 a field is only added, and a receiver
  ignores what it does not know (`crates/js-agent-wire/src/protocol.rs`).
- **Refusals.**
  - A 403 with a code the agent cannot fix ends its session, and the agent exits 2.
  - A 409 to a hello is retried with back-off (`crates/js-agent/src/session.rs`, module header).

### 1.3 Placement

**The pass and the walk.**

- A pass (`SchedPlacer::place_queued`, `crates/js-sched-placer/src/jobs.rs`) walks the queue in `js_jobs::queue_order`: higher
  priority, then earlier submit, then id.
- The walk is `js_sched::walk_in_order` (`crates/js-sched/src/queue.rs`). A job placed nowhere closes every host it could use to
  the jobs behind it in that walk.
- "Could use" is `usable` in `Engine::walk_holding`: capability and capacity, not current room.

**One round per job.** Each round chooses a host in `Servable::fit` (`crates/js-sched/src/servable.rs`, near line 301), in this
order:

1. **Capable.**
   - The host is live, serving and not cordoned.
   - It serves the lease kind and matches the job's platform (`js_caps::Request::matches`).
   - It is in the walk's open set.
2. **Room.**
   - Under admission by load (the default), the measured plus predicted CPU charge stays within `cores x (1 + slack)`, and memory
     stays within the schedulable share. `--placement-cpu-slack` defaults to 1.1x (`crates/js-sched/src/admission/config.rs`).
   - Without it, the free booking must fit (`Worker::has_room`, `crates/js-sched/src/scheduler/worker.rs`).
3. **Provider tier.**
   - Only the hosts of the first machine provider in the platform's order stay (`of_first_provider`,
     `crates/js-sched/src/provider_rank.rs`), on both paths of step 2.
   - A full host of an earlier provider never keeps a job off a free host of a later one.
   - The order (`rank::order_of`, `rank::rank`, `crates/js-lease-server/src/sched/rank.rs`) reads only whether a host's binding
     names a cloud instance.
4. **Score.** Under load admission, `ScoreKey` (`crates/js-sched/src/load.rs`, near line 305) decides. Its fields, in order:

   | Field | What it compares |
   |---|---|
   | `slack` | use of the slack zone |
   | `memory_unsafe` | whether memory would be unsafe |
   | `distrusted` | whether the host's reading is distrusted |
   | `concentrated` | whether the work would be concentrated on one host |
   | `fit` | best fit: the tighter host wins |
   | `uncovered` | load that no reading covers |
   | `larger_cpu` | the larger host wins |
   | `larger_near_headroom` | more headroom in the near term |
   | `hash` | a hash, for a stable order |
   | `worker` | the worker's name |

   Otherwise the least `Load` decides: its share, then its lease count, then its name.

**Whole-host jobs** are marked on the host with the cheapest drain (`js_whole::cost::drain_cost`, `crates/js-whole/src/cost.rs`),
by these keys in order:

1. known tail first;
2. the shorter wait;
3. the fewer leases;
4. the provider;
5. the id.

**The commit.**

- A commit re-checks the host's live allocations in its own transaction (`JobStore::place_claimed`,
  `crates/js-jobs/src/store/whole.rs`), against a capacity that includes the slack.
- It writes `hostbook/<host>` at the version it read, so two servers that place on one host conflict, and one tries again.

### 1.4 Nothing prefers a narrow machine

- No key in that order reads what else a host could run.
- A job without a GPU key matches a GPU host as readily as any other host.
- Two keys can work against a narrow server:
  - `fit` prefers the host the job fills more tightly;
  - `larger_cpu` prefers the larger host.
- Provider order cannot express the preference: two hosts that joined with pool tokens rank the same.
- The only "fewest accelerators" rule is in pass 2. `acquire::pick` (`crates/js-sched/src/acquire.rs`) prefers, after price, the
  offer with fewer accelerators.

### 1.5 Usage and the operator's view

Usage and the operator's view are per host:

- `ListHosts` `Host` (`crates/js-proto/proto/jobscheduler/v1/fleet.proto`) has no pool, platform or machine.
- `UsageRecord` (`usage.proto`) is keyed by `host_id`.
- Node load rows are kept per host (`usage/node/<host>/<hour>`, [usage-store.md](../usage-store.md)).

The `js` CLI prints every proto field as a `path: value` line (`crates/js-cli/src/jobs/text.rs`), so a new field shows up without
CLI code.

## 2. The model

### 2.1 Each host is booked at what it reports

**The booking.**

- A host's capacity is its own claim: the cores, memory and GPUs of its last hello's `WireShape`, with memory cut by
  `memory_schedulable_percent` and CPU scaled by the slack, as the pool's shape is cut and scaled today.
- There is no pool-wide size. Two hosts of one pool are booked each at its own size. A 32-core Mac is booked at 32 cores, and a
  28-core Mac at 28.

**What changes in the code.**

- **Registry.** `placement_shape` takes its resources from the claim instead of the pool's `booked` resources. Its capabilities
  are built as today.
- **Server.**
  - `capacity_of(&view.shape, ...)` then reads the host's own size with no further change.
  - The usage limits are sized from the largest size the wire accepts for a hello (`js_agent_wire::limits`), not from a pool shape.
- **Pool file.**
  - The pool's `shape` becomes optional. When a pool names one, it is a floor only: a hello that offers less is refused
    `below-floor`, as today. A pool file written before this change keeps its meaning for admission.
  - What a pool must still name is its capabilities: `os` and `arch`, which `check_pool` needs to know the pool's platform.
  - `mac_pool_shape.rs` changes with the rule it pins. Its test of a 32-core Mac then expects 32 cores booked.

**Where the numbers come from.**

- The agent's detection (1.2) is the host's report, the same code on Linux and macOS. It is what hostkeeper installs and starts on
  every host.
- The operator's `--cpu-millis` and `--memory-bytes` still only lower it, for example to keep cores for something else on the
  host.
- Hostkeeper's facts file adds nothing the server reads. Whether it should also carry the size is open (9).

**A wrong or shrinking report.**

- **A report larger than the host.** The agent reports what the operating system says, and flags only lower it, so a report
  above the hardware needs a broken system. The protections are the ones that hold today against a host under pressure:
  - The agent refuses a start it cannot hold, comparing its own offer with what it has booked.
  - Measured load above the prediction makes the host distrusted in the score (`ScoreKey.distrusted`).
  - The agent's memory backstop and a rerun on another host recover a job killed for pressure.
- **A report that shrinks.** A host's size changes only at a hello: memory removed, a cgroup limit lowered, a flag changed and the
  agent restarted.
  - The new hello books the host at the new size.
  - When its live allocations already exceed it, the host takes no new job until they fit.
  - No running job is stopped, since a booking ends only when its job ends.
  - The commit sees the new size in the same pass.
- **A report that grows.** It is used at the next hello.
- **Not designed here.** A size that changes without a hello is not designed; it would be an additive heartbeat field.

### 2.2 A machine is the unit of capacity, a host is the unit of work

| | A. One host advertises a set of platforms | B. One host per platform, tied to one machine with a shared ledger |
|---|---|---|
| Wire | a new multi-valued platform key, matched as set membership in `js-caps` and `Shape::satisfies` | one optional `machine` field in a side's hello |
| Booking | one host, one book | a second level of booking: the machine |
| Pool | a pool's capabilities become per-platform lists | unchanged: each side joins a pool of its platform |
| Taint, cordon, session, heartbeat, load reading, dispatch | one for both platforms | one per side, as today |
| Linux jobs in VMs the macOS agent starts | one runtime more in the macOS agent, and a platform set in its hello | one runtime more, behind a second session of the macOS agent |
| "8 machines can run linux/arm64" | count hosts | count machines |

**The choice is B**, for four reasons:

1. **The two platforms of a Mac differ in more than `os`.**
   - Their execution modes differ: `sandbox-exec` against `container`.
   - Their network ceilings differ. The experiment of
     [linux-containers-on-macos.md](experiments/linux-containers-on-macos.md) found no egress filter for the Linux VMs.
   - Their failure modes differ: the container tool's per-user services can fail while native runs go on.

   Under B each side has its own capabilities, ceiling, cordon and taint, and every per-host mechanism of section 1 stays as it is.
   Under A, one cordon or taint stops both.
2. **Matching and pools do not change.** Under A, `NodeCaps`, `Request::unmet`, `Shape::satisfies` and a pool's capabilities all
   become per-platform lists, and several reads assume one `os` (`ios.device` requires `os == macos`). Under B a host stays one
   platform.
3. **The one new thing is small and generic.** It is a ledger shared by the hosts of one machine. It is the same whether the Linux
   side is a runtime inside the macOS agent, as now, or a guest with an agent of its own, as a later option.
4. **The second session is cheap.** It is a second hello and heartbeat loop in the macOS agent. The agent's job worker, dispatch
   poll and runtimes are already behind interfaces (`trait Runtime`, [mac-runtime.md](mac-runtime.md)).

**What B costs.** The macOS agent runs two sessions, and the operator counts machines, not hosts (6.5).

### 2.3 Terms

- **Machine.** The box with the cores and memory. Its id is the host id of its home side.
- **Side.** A host on a machine: its own host id, hello, session, pool and one platform.
- **Home side.** The side whose report is the machine's capacity: the macOS session of a Mac.
  - A host that names no machine is the home side of a machine of one side, which is every host today.
  - A pool that some pool's `side_of` names is a **home pool**.
- **Other side.** A side whose hello names its machine: the Linux session of a Mac.
  - Its pool is a **side pool**, whose `side_of` names the home pool (6.3).

### 2.4 The ledger

**The two levels.**

- **Machine capacity.** It is the home side's reported size, scaled as in 2.1.
- **A side's own capacity.** An other side reports the size it may use. The Linux side of a Mac may use the whole Mac, so it
  reports the Mac's size. No side has a fixed share.
- **A placement on any side of a machine** must fit two things:
  - The side's own room: its capacity less its bookings.
  - The machine's room: the bookings of every side of the machine, plus this one, fit the machine capacity.

**What a Linux job books.** Its own resources, plus its VM's fixed memory overhead.

- The experiment measured about 95 MiB more than the job's memory.
- The Linux side reports the overhead in its hello as the report-only capability `vm.overhead_mib`, beside the existing `vm.*`
  report-only keys of `js-caps`. The placer adds it to the memory each job books on that side.
- A VM keeps the most memory its guest ever touched until it ends. One VM per job, sized to the job and destroyed at its end,
  therefore gives that memory back when the job ends, and its booking covers it until then.

**Load admission.**

- Each side's charge is checked against its own room, as today.
- The engine also checks the charges of all sides together against the machine's scaled cores, so the slack setting holds per
  machine (B14).
- A server sees only the readings of the sides it holds, so the machine sum is the engine's check, as all load admission is today.
  The bookings are the hard check, made in the commit.

**Whole-host jobs.**

- A whole-host job on any side books the whole machine. It is granted only when every side is empty, its mark closes every side to
  new placements, and its drain cost is taken over all sides: the latest tail and the summed leases.

### 2.5 Membership and the commit

**Which hosts are sides of which machine.**

- **The membership row.** `machine/<machine>` records the machine's home pool and its sides. A home side's admitted hello writes
  it if it is absent.
- **A side's hello.** It names its machine. The server admits it only if that row exists and names the side pool's home pool.
  Every instance reads the row from the store. Otherwise the hello is refused `machine-unknown`, so a typo or a stale `--machine`
  never creates a machine with a ledger of its own.
- **One side per side pool.** A second host of the same side pool that names a machine is refused `machine-side-taken` while the
  first is live or holds work.
- **Two instances at once.** The side's hello adds it to the row at the version it read, so two instances admitting two sides at
  once cannot both pass.
- **Restarts.** The side's binding keeps its machine, so a side put back after a restart (`restore_tainted`) keeps it.

**The commit.** `JobStore::place_claimed` gains the machine capacity and the membership. In its transaction it:

1. reads the membership row and the live allocations of every member (`live_on`, the per-host read every release maintains), and
   checks the machine level;
2. writes the side's `hostbook/<side>` row, as today;
3. writes `hostbook/<machine>`, the home side's existing row, at the version it read, so two servers placing on two sides of one
   machine conflict and one tries again.

Allocations stay keyed by host. A release that does not know machines still writes and frees allocations the new commit reads
correctly.

### 2.6 The interface the Linux-on-Mac design must meet

The owner's choices fix the first shape of Linux on the Macs: one Linux VM per job, started by the macOS agent. The experiment
used Apple's `container` tool, with a median of about 0.6 s from request to the job's first output, about 0.54 s of it the VM's
boot. Whatever runs the VMs, the server sees exactly this:

1. **Each platform is a host session.**
   - The macOS agent opens a second session with a second host id for its Linux runtime: its own hello, heartbeat, dispatch poll
     and pool, and one `os`, `arch` and `exec`.
   - Its two lifecycle machines keep separate bookings. The agent's own checks stay per side. The server's ledger is the only
     check of the machine.
2. **The Linux session's hello names its machine.**
   - It names the macOS session's host id, from a new agent flag `--machine <host id>`, a flag as S2 asks.
   - The macOS session names nothing, so a macOS agent of any release serves as a home side.
3. **The Linux session reports the Mac's size and its VM overhead** (`vm.overhead_mib`, 2.4). Its capabilities say what its VMs
   give a job. Its network ceiling is `none` until the VMs' egress is filtered, so a Linux job that needs egress is not capable
   there and never spills onto a Mac.
4. **CPU readings are not counted twice.**
   - Each VM's CPU is the Linux session's.
   - If the macOS session ever reports a load reading, it excludes the VMs.
   - Today native macOS runs report no load (`NO_NATIVE_USAGE`, `crates/js-agent/src/platform/macos.rs`), so the macOS side is
     charged by prediction.
5. **Machine operations are per side.**
   - The server does not carry a cordon from one side to the other.
   - A step that stops the whole Mac cordons both sessions.
   - A step that stops the Linux runtime alone cordons only the Linux session.
6. **A warm guest is a later option, not this design's.** A long-lived guest that keeps its memory would need the side to report
   the memory it holds, in a value every server and the commit read (a node-row column), and the ledger to count `max(booked,
   held)`. Nothing in this design needs it.

   It would be worth building only if the VM's start, about 0.5 s more than a Linux host's, is measured to matter for the jobs
   that spill onto the Macs.

## 3. The preference: spend the narrowest machine first

### 3.1 The rule

**A machine's offers** are the scarce things the machine can give a job. They come from configuration:

- **Platform.**
  - For a machine with sides, it is `os/arch` of the home pool and of every side pool that names it, from the pools'
    capabilities.
  - For a machine of one side, it comes from the host's placement shape: the pool's capabilities, or the hello's for a pool that
    places by hello.
- **Accelerator.** `accelerator:<class>` for each of those hosts whose reported size has `gpus > 0` and whose pool names a GPU
  class.

Every server therefore reads the same platforms for a machine, including a server whose registry holds only one of its sides. A Mac
whose Linux session is down keeps both platforms.

**What a job uses on a side.** That side's platform, plus its accelerator if the job books GPUs.

**The job's breadth on a side.** The number of the machine's offers the job does not use there:

```
breadth(job, side) = | offers(machine(side)) \ uses(job, side) |
```

**The rule.**

- Of the hosts that can take the job now (capable and with room, steps 1 and 2 of 1.3), keep only those whose breadth for the job
  is least, and let the existing order decide among them.
- The preference is a filter on hosts with room. A full narrow machine never makes a job wait while a broad one has room: the job
  goes to the broader machine (3.5).

**Spill.** A job is *spill* on a side when another pool's hosts could run it with a smaller breadth.

- "Could run" is judged from configuration: platform, accelerator, network ceiling. Liveness, cordons and room do not count.
- A Linux arm64 job is spill on a Mac (breadth 1), because the Linux server pool has breadth 0. It stays spill while every Linux
  server is down or full.
- A macOS job is not spill on these Macs, because every pool that can run it has breadth 1.

**Two instances of one rule.** In bold, the host used first:

| Job | Mac (`macos/arm64`, `linux/arm64`) | Linux arm64 server | GPU host (`linux/x86_64`, `accelerator:A`) | x86_64 node |
|---|---|---|---|---|
| Linux arm64, no GPU | breadth 1 | **0** | not capable | not capable |
| macOS arm64 | **1** | not capable | not capable | not capable |
| Linux x86_64, no GPU | not capable | not capable | 1 | **0** |
| Linux x86_64, one GPU of class A | not capable | not capable | **0** | not capable |

The GPU host is an example: none of the six nodes has a GPU today.

**Derived, not configured.**

- The kinds that count, platform and accelerator, are fixed in code, in one function of `js-caps`.
- Device kinds such as attached iOS devices are not offers. They are reported at run time and differ per host, so servers would
  disagree about them. They can become offers once a pool declares them.
- A machine's offers are computed once per pass for each host and sent to the scheduler, like the provider rank. The scheduler
  derives each job's breadth from them.

### 3.2 Where it sits, and the tie-breaks

The rule is a new tier in `Servable::fit`, right after the provider tier and before the score, on both paths (`of_first_provider`
runs on both today: servable.rs, near lines 331 and 335).

**The full order for a share job:**

1. Capable (1.3, step 1), including the walk's open set (3.4).
2. Room on the side, and room on the machine (2.4). The CPU slack is part of room.
3. Provider tier: the first machine provider in the platform's order.
4. **Narrowness tier: the least breadth.**
5. Score: `ScoreKey` under load admission, or the least `Load` without it. The last keys, `hash` and `worker`, make the order
   total.

**For whole-host jobs** breadth is added after the provider:

- Quiet hosts: provider, then breadth, then id.
- Marks: drain cost (known tail, wait, leases, over all sides of a machine), then provider, then breadth, then id.

**How it combines with the existing keys.**

- **Load and usage history.** Measured load and the predictions from usage history decide room (step 2), and the score among
  equally narrow hosts (step 5). The tier never admits a host the load predicate refused.
- **The CPU slack.**
  - A host that admits the job only inside its slack (between 1.0x and 1.1x of its cores) has room, so the tier keeps it ahead of
    a broader host with more headroom.
  - A Linux server is therefore used into its slack before a Mac takes Linux work (8).
  - `ScoreKey.slack` matters only among hosts of equal breadth.
- **The provider.** It stays the first key ("the provider is the first key, never a weight", the comment on `fit`), so an owned
  broad machine is used before a rented narrow one (8). Today the question does not arise: a server with joined pools has no
  launching provider beside them (1.1).
- **Whole-host reservation by least remaining time.** Unchanged. Breadth breaks a tie of drain cost, after the provider.
- **Farm reruns.** The `avoid` set (`Servable::avoided`, inside `pool_of`) runs before the tier, unchanged.
- **Probes.** Unchanged: a probe names its one host.

**Pass 2 and the scaler.**

- `acquire::pick` already prefers fewer accelerators.
- The scaler's cover step (`cover`, `crates/js-scaler/src/scale.rs`) counts only free hosts of the pool it scales. Joined pools are
  not scaled, and an elastic pool is served alone, so nothing changes.
- If an elastic pool is ever served beside side pools, cover must count a broad host only after the narrow ones.

### 3.3 Worked examples on the eight arm64 machines

**The setup.**

- **Pools, all joined.**
  - `arm`: the four Linux servers, `os=linux`, `arch=arm64`. Each reports 128 cores and 256 GiB.
  - `mac`, the home pool: `os=macos`, `arch=arm64`. In these examples M1 and M2 report 32 cores and M3 and M4 report 28. Each
    reports 96 GiB.
  - `mac-linux`, the side pool (`side_of = "mac"`): each Mac's Linux session. It reports its Mac's size.
- **The settings.** The examples run with `memory_schedulable_percent = 100` and `--placement-cpu-slack 0`, so the numbers stay
  round.
  - With a percent `p`, every memory figure is multiplied by `p`.
  - With the default slack, a server holds 17 of the Linux jobs below rather than 16.
- **The provider.** Every machine is of one provider, `static`.
- **Names.** The servers are L1 to L4 and the Macs M1 to M4. Each Mac has home side `Mi` and Linux side `Mi-linux`.
- **The jobs.**
  - A Linux job is 8 cores and 16 GiB. Its VM overhead of about 0.1 GiB is left out of the sums.
  - A macOS job is 6 cores and 8 GiB.

**Example 1: Linux work alone.**

- **The burst.** Nothing runs, and 50 Linux arm64 jobs arrive.
- **What the tier sees.**
  - Each job's capable hosts are L1 to L4 (breadth 0) and M1-linux to M4-linux (breadth 1).
  - The tier keeps L1 to L4 while any has room. A server holds 16 jobs, so the four hold 64.
- **Where the jobs go.** All 50 go to L1 to L4, and the score decides among the four. The Macs take nothing.
- **Without the tier.** `fit` could put some on the Macs, which the job fills more tightly.

**Example 2: a burst of Linux jobs while macOS jobs run.**

- **The start.** Each Mac runs 3 macOS jobs (18 cores and 24 GiB). L1 to L4 are full with 64 long Linux jobs.
- **A burst of 20 Linux jobs.**
  - L1 to L4 have no room, so the tier keeps the Macs.
  - M1 and M2 have 14 cores left, and M3 and M4 have 10. Each takes one Linux job (8 cores), so 4 are placed and 16 wait.
  - The waiting jobs are spill on the Macs, so each closes L1 to L4 to the jobs behind it, but not the Macs (3.4).
- **Three macOS jobs arrive** behind the 16.
  - M1 and M2 have 6 cores left, so the first two are placed there.
  - The third finds 2 cores on M3 and M4 and nothing on M1 and M2, so it waits.
  - It is not spill, so it closes both sides of every Mac to every job behind it.
- **A Linux job ends on M3-linux**, freeing 8 cores.
  - In the next pass the waiting Linux jobs walk first. Each could use M3-linux, so the first takes the room.
  - The macOS job waits only for the Linux jobs queued before it. No Linux job queued after it takes room on a Mac while it
    waits.
- **A Linux job ends on L2.** The head of the queue goes to L2 (breadth 0).

**Example 3, the reverse: a burst of macOS jobs after Linux work has filled the Macs.**

- **The start.**
  - Earlier, with no macOS work, Linux spill took M1 and M2 whole (4 jobs each, 32 cores) and M3 and M4 to 24 of 28 cores (3 jobs
    each).
  - L1 to L4 are full, and the Linux queue is now empty. This is the demand-driven split: Linux used the Macs while nothing else
    did.
- **10 macOS jobs arrive.** No Mac has 6 cores free, so all 10 wait. The first closes every Mac to every job behind it.
- **A Linux job ends on M1-linux**, freeing 8 cores. No Linux job is queued ahead of the macOS jobs, so the first macOS job takes 6
  of them.
- **The Linux jobs behind the macOS jobs.** Those that arrive later wait for L1 to L4 and never take a Mac while a macOS job waits.
- **How the room moves.** As the Linux jobs on the Macs end, every freed core goes to macOS, one job end at a time. Memory moves
  with it, because each Linux VM's memory returns when its job ends.

### 3.4 How the walk closes hosts

The walk closes, on behalf of a job that waits, the hosts it could use (`can_use` in `Engine::walk_holding`, jobs.rs, near line
367). With machines:

- **On a side where the waiting job is not spill,** it closes every side of that machine. The machine's capacity is what it waits
  for, and another side would consume it.
- **On a side where it is spill,** it closes nothing. The job waits for its narrow machines, not for this one. Spill is judged from
  configuration (3.1), so it does not change as narrow hosts go down or fill.

"Could use" stays capability and capacity (`usable`). A job too large ever to fit a machine closes nothing. It waits as any job no
host can hold waits, and it ends at its pool's `max_queue_wait` (`crates/js-lease-server/src/job_expiry.rs`).

`walk_in_order` itself does not change; only the `can_use` closure the placer passes it does.

**Read against standing rule R1** ("no per-category tables and no reservation admission"):

- The closing is the binding queue order applied to machines. Nothing is held across passes, and a job ahead in the queue still
  walks first.
- No capacity is set aside for a kind of work, and no host is set aside for a job that is not placed. The one reservation stays the
  whole-node one.

### 3.5 When the narrow machines are full

A job goes to the broader machine rather than waiting. No pool and no job can refuse that (8).

What protects a broad machine's own work is queue order: the closing of 3.4. Nothing reserves part of the machine.

## 4. Demand-driven sharing on a two-platform machine

### 4.1 What the scheduler needs to know

| Fact | Where it comes from | How often it changes |
|---|---|---|
| Machine capacity | the home side's reported size | at the home side's hello |
| Each side's capacity and VM overhead | the side's reported size and `vm.overhead_mib` | at the side's hello |
| Which hosts are sides of which machine | the side's hello, kept in its binding and in `machine/<machine>` | at a hello |
| Each side's bookings | the live allocations of the side (the job store) | at every placement and every freed allocation |
| Each side's measured CPU | the side's load reading (`NodeLoad`), as today | at every heartbeat |

The scheduler needs no demand forecast and no split. The split follows from the bookings: what one side does not book is free for
the other.

### 4.2 How fast the split can change

- **CPU and memory move at the next placement pass after a booking ends.** A pass runs on every tick, and after every heartbeat
  that changes something (jobs.rs, module header).
  - A Linux VM's cores are threads, which hold no core when idle.
  - Its memory returns to macOS when the VM ends with its job.
- **A running job is never moved, shrunk or stopped to make room.** Its booking stays on the machine until it ends. "The other
  side wants it" is not an end reason.

### 4.3 What a running job on one side means for the other

A running job's booking counts on the machine, so the other side has that much less until the job ends. Nothing else crosses
sides:

- A Linux job's memory limit, pressure and crash stay in its VM.
- The macOS side keeps its own runtime limits ([mac-runtime.md](mac-runtime.md)).

### 4.4 The guarantee that macOS work is not starved by Linux work on a Mac

1. **Precedence in the queue.** A macOS job that waits closes the whole Mac to every job behind it (3.4). No Linux job queued
   after it takes room on a Mac while it waits.
2. **A bounded wait.** A waiting macOS job is behind only the jobs queued before it. The room it waits for frees as the jobs on the
   Macs end, and every job has a wall-time limit.
3. **Spill is a second choice.** A Linux job reaches a Mac only when no Linux-only server open to it in the walk has room (3.1).
   - A Linux job that waits closes the Linux servers to the jobs behind it (3.4), so a smaller Linux job behind it can spill to a
     Mac while a server has room the waiting job could not use. That is queue order at work.

What it does not give:

- Linux work may hold a whole Mac while no macOS job is queued.
- A macOS job queued after Linux work of higher priority still waits for it, because queue order is the one order (8).

## 5. Compatibility

| Peer | Meets | Result |
|---|---|---|
| A pool file with `shape` | new server | The shape is a floor only. Hosts are booked at their reported sizes: larger than before on a host that offers more than its pool's shape. This is the intended change. |
| A pool file without `shape`, or with `side_of` | old server | Refused at start: `shape` is required and `side_of` is unknown (`crates/js-lease-server/src/file/joined.rs`). |
| Old agent (no `machine`) | new server, any pool but a side pool | It is the home side of a machine of one side, which is every host today. |
| Old agent in a side pool | new server | Refused `403 machine-required`. The agent ends its session naming the code and exits 2 (1.2). |
| New agent with `--machine` | old server, or a pool with no `side_of` | The field is ignored, and the host is a host of its own pool. |
| `vm.overhead_mib` | old server | An unknown capability: a node report skips keys it does not know (`crates/js-caps/src/report.rs`, module header), and nothing matches on it. The old server books no overhead, which matters only for a side pool, which it cannot load. |
| New proto fields (6.5, 6.6) | old client | Skipped (proto3; the evolution rule of `service.proto`). |
| New client | old server | The new response fields are absent. A `machine_id` filter is dropped over gRPC: every record returns, none carrying `machine_id`, which tells the client. Over the HTTP gateway it is refused with a 400. |
| `machine/<machine>`, a binding with a machine | previous release | It never reads the membership row, and it reads a binding without the field. |
| Allocations written by the previous release | new server | Read as today: allocations stay keyed by host, and the machine check reads them by host (2.5). |

**The rollout.** It runs once the tasks of 7.2 through `jsmp-place` are merged and released.

1. **Roll every server to the release.** Each host is then booked at its reported size, with no change to the pool files.
   - For a few passes, servers of both releases book a host differently: the old one at the pool's shape, the new one at the
     report.
   - The commit of each checks its own figure, and a pool's shape is the floor, so the report is never smaller than the shape.
     The difference therefore only lets the new release book more on a host that is larger than its pool's shape.
   - Rolling the release while those pools are cordoned removes even that window.
2. **Cordon the Mac hosts** (`CordonHost`).
3. **Add the side pool to the pool file** with `side_of = "mac"`, and restart every server.
4. **Start each Mac's Linux session** with `--machine`.
5. **Uncordon the Macs.**

The cordon keeps a server with the old file and one with the new from booking one Mac under two ledgers.

**Rollback.** Remove the side pool under the same cordon, then roll the servers back.

**The narrowness tier on existing pools.** It changes placement only where hosts of one platform differ in offers.

- Today's farm has none, so its order is unchanged (test `an_old_farm_places_as_before`).
- A host that reports GPUs, in a pool beside hosts of the same platform without them, is the intended change: jobs without GPUs go
  to the hosts without GPUs first.

## 6. What changes, layer by layer

### 6.1 Wire (`js-agent-wire`) and agent

- **`Hello`** gains `machine: Option<String>` (`serde(default, skip_serializing_if)`). It is a host id, checked by
  `Hello::validate`.
- **`js-caps`** gains the report-only key `vm.overhead_mib`.
- **New refusal codes:**

  | Code | Status | When |
  |---|---|---|
  | `machine-required` | 403 | a side pool's hello names no machine |
  | `machine-unknown` | 403 | no home pool host of that id has said hello (no `machine/<machine>` row naming the home pool) |
  | `machine-side-taken` | 409 | the machine already has a live side of this pool, or one that holds work |

  A hello naming a machine other than its binding's is refused with the existing `host-bound`.
- **The macOS agent** gains the second session for its Linux runtime, with `--machine` set to its own host id. The runtime itself,
  the VMs and their contract are the Linux-on-Mac design's.

### 6.2 Registry (`js-registry`)

- **The booking.** `placement_shape` takes its resources from the claim (2.1). `PoolConfig`'s shape becomes `Option<Shape>`, a
  floor when present.
- **Machine on the record.** `Record` and `Binding` gain `machine: Option<HostId>`.
- **Admission.** `Registry::admit` adds three checks after the floor check, in this order: `machine-required`, `machine-unknown`,
  `machine-side-taken`. For a pool with no `side_of`, a named machine is ignored.
- **The membership row** is written with the binding (2.5).
- **The view.** `HostView` gains `machine` and `platform` (`os/arch`).
- **The placer.** `Placer` gains a default no-op `host_on(&self, host, machine: Option<&HostId>)`, called with `host_up`.
- **Pool config.** `PoolConfig` gains `side_of: Option<ProviderId>`.

### 6.3 Pool file (`[[pool]]`, `kind = "joined"`)

`shape` becomes optional, and one key is added:

```toml
[[pool]]
kind  = "joined"
name  = "mac-linux"
class = "share"
side_of = "mac"            # optional: hosts of this pool are sides of machines whose home is in pool "mac"
capabilities = { os = "linux", arch = "arm64" }
max_machines = 4
```

**Checked once at start** (B20). Each check refuses naming the pool and the key:

- every pool names `os` and `arch` in `capabilities`, or places by hello;
- `side_of` names a joined pool of the same file, and that pool has no `side_of`;
- no two pools of one machine name the same `os` and `arch`;
- a side pool names no `shape` (its hosts report the machine's size).

The `--agent-pool-config` file keeps its required `[shape]`, now a floor, and refuses `side_of`.

### 6.4 Placement (`js-sched`, `js-sched-placer`, `js-jobs`, `js-caps`)

**`js-sched`** is pure, so R4 holds: `BTreeMap`, no clock.

- `Scheduler::set_machine(worker, Option<WorkerId>)` and `set_offers(worker, BTreeSet<Offer>)` are re-sent each pass, like
  `set_provider_rank`.
- `Worker::has_room` and the load admission check the machine level (2.4).
- `Servable::fit` gains the narrowness tier after `of_first_provider` on both paths.
- `drain_cost` and the quiet-host order gain breadth after the provider, and whole-host jobs on a machine book every side (2.4).

**`js-caps`** gains two pure functions: `offers(&NodeCaps) -> BTreeSet<Offer>` and `uses(&Request, &NodeCaps) -> BTreeSet<Offer>`.

**`js-sched-placer`.**

- `OpenHost` gains the machine and its offers, and `jobs_pass.rs` fills them from the registry's view and the configured pools.
- The placer adds a side's `vm.overhead_mib` to each job's memory booking there.
- The capacity it gives `place_claimed` grows by the machine's.
- `walk_holding`'s `can_use` closes by machine, but not on spill sides (3.4).

**`js-jobs`.** `place_claimed` gains three things: the membership read, the machine check and the `hostbook/<machine>` write (2.5).

**`js-lease-server`.**

- `capacity_of` reads the host's own size, through the registry's view.
- The usage limits are sized from the wire's largest accepted size (2.1).
- `check_pool` reads the pool's capabilities instead of a shape.

### 6.5 CLI and API output

**`fleet.proto` `Host`** gains three fields. The `/v1/hosts` JSON (`HostEntry`, `crates/js-lease-server/src/hosts.rs`) gains the
same three.

```
string machine = 13;          // the machine's id (its home side's host id); this host's own id when it is a home side
string platform = 14;         // "os/arch", e.g. "linux/arm64"
repeated string offers = 15;  // the machine's offers (3.1), sorted
```

`Host.shape` already shows a host's booked size, which is now its own.

**`service.proto` `ListHostsResponse`** gains a summary, counted over the machines with a live side in the server's registry:

```
repeated PlatformCount platforms = 2;
message PlatformCount { string platform = 1; uint32 machines = 2; uint32 only = 3; uint32 hosts = 4; }
```

What the operator sees (`js host list`):

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

"8 machines can run linux/arm64, 4 of them only that." `hosts` is 8 because each Mac's Linux session is one host.

### 6.6 Usage accounting

- **`UsageRecord`** gains `string machine_id = 23` and `string platform = 24`, written from the host's view when the record is
  written. An empty `machine_id` means the host is its own machine.
- **`GetUsageRequest`** gains `string machine_id = 5`, a filter.
- **Node load rows** stay per host. A machine's load is the sum of its sides' rows.
- **A booking** belongs to the one side the job ran on. A Linux job's booking includes its VM overhead, as booked.

### 6.7 What kbf sends to ask for a platform

Nothing new:

- **Job API.** `Constraints.os` and `Constraints.arch` (`job.proto`): `os = "linux"`, `arch = "arm64"`, or `os = "macos"`,
  `arch = "arm64"`.
- **Lease path.** `os` and `arch` in the request's capabilities.

kbf must keep doing three things:

- **Always send `arch`.** A Linux job with no `arch` is capable on x86_64 and arm64 hosts alike.
- **Send `exec` only when the action needs it.**
- **Send the network level the action needs.** A Linux job that asks for egress is not capable on a Mac's Linux session while that
  session's ceiling is `none` (2.6).

The preference is the server's: kbf names no pool and no machine.

## 7. Tests and tasks

### 7.1 Tests the implementation must pass

**How the tests are written.**

- Every test asserts behaviour.
- Where time matters, a test uses the injected clock (`js_state::ManualClock`), and no test asserts elapsed time (T10).
- The tests are Rust tests on fakes, so they run on the Linux x86_64 pull-request runners.
- The names are the tests' names.

**`js-registry` and `js-lease-server`: booking at the reported size**

- `a_host_is_booked_at_the_cores_and_memory_its_hello_reports`
- `two_hosts_of_one_pool_are_booked_each_at_its_own_size`
- `a_32_core_mac_is_booked_at_32_cores`: replaces the 28-core expectation of `mac_pool_shape.rs`.
- `the_memory_percent_cuts_the_reported_memory`
- `a_pool_shape_is_a_floor_only`: below is refused `below-floor`, and above is booked at the report.
- `a_pool_without_a_shape_admits_any_size`
- `a_host_that_says_hello_smaller_keeps_its_running_jobs_and_takes_none_until_they_fit`
- `the_commit_holds_a_placement_to_the_hosts_reported_size`
- `usage_limits_hold_the_largest_size_the_wire_accepts`

**`js-caps`**

- `offers_count_the_platform_of_every_pool_of_a_machine`: a down side counts too.
- `offers_count_an_accelerator_only_where_the_host_reports_gpus`
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
- `an_old_farm_places_as_before`: hosts of one platform each, with equal offers; the choices equal the current goldens.

**`js-sched`: the machine ledger**

- `two_sides_of_a_machine_never_book_more_than_the_machine`
- `linux_work_can_take_a_whole_mac_when_no_macos_job_waits`
- `a_linux_job_on_a_mac_books_its_memory_and_the_vm_overhead`
- `machine_cpu_admission_sums_the_charges_of_both_sides`: within the machine's scaled cores.
- `a_whole_host_job_on_either_side_closes_every_side_and_waits_for_all_to_empty`
- `whole_host_marks_break_a_drain_cost_tie_by_breadth_after_the_provider`

**`js-sched-placer`: the walk**

- `a_waiting_macos_job_closes_both_sides_of_a_mac_to_jobs_behind_it`
- `waiting_linux_jobs_never_close_a_mac_to_macos_jobs_behind_them`: the second and later waiting Linux jobs too, and with every
  Linux server cordoned.
- `a_waiting_linux_job_closes_the_linux_only_hosts_to_jobs_behind_it`
- `a_job_too_large_for_any_machine_closes_nothing`
- `a_linux_job_needing_egress_never_spills_to_a_mac_session_without_it`

**`js-jobs`: the commit**, with a fake store and two `JobStore` handles as two servers

- `two_servers_placing_on_two_sides_of_one_machine_do_not_overbook_it`
- `a_placement_on_a_side_conflicts_with_one_on_the_home_side_read_at_the_same_version`
- `the_machine_check_counts_allocations_written_without_a_machine`: as a previous release writes them.
- `the_commit_accepts_a_machine_placement_inside_the_cpu_slack`

**`js-registry`: sides**

- `a_side_hello_without_a_machine_is_refused_machine_required`
- `a_side_naming_a_machine_whose_home_never_said_hello_is_refused_machine_unknown`
- `a_home_hello_writes_the_membership_row_once`
- `a_second_side_of_one_pool_on_one_machine_is_refused_machine_side_taken`
- `a_side_joins_once_its_predecessor_is_lost_and_holds_nothing`
- `two_instances_admitting_two_sides_of_one_machine_admit_one`
- `a_side_naming_another_machine_than_its_binding_is_refused_host_bound`
- `a_side_put_back_after_a_restart_keeps_its_machine`
- `a_machine_named_in_a_pool_without_side_of_is_ignored`

**`js-agent-wire`**, extending `tests/previous_release_v0_1_208.rs`

- `a_previous_release_hello_reads_as_a_home_side`
- `a_hello_with_a_machine_reads_in_the_previous_release`
- `a_hello_with_an_invalid_machine_is_refused_invalid_hello`

**`js-lease-server`: configuration**

- `a_pool_without_os_or_arch_is_refused_at_start`
- `side_of_naming_no_pool_is_refused_at_start`
- `side_of_naming_a_side_pool_is_refused_at_start`
- `pools_of_one_machine_naming_one_platform_are_refused_at_start`
- `a_side_pool_with_a_shape_is_refused_at_start`
- `the_agent_pool_file_refuses_side_of`

**`js-lease-server`: output and usage**

- `list_hosts_counts_machines_per_platform_and_those_that_run_only_it`: the 8 and 4 of 6.5.
- `a_host_lists_its_machine_platform_and_offers`
- `a_usage_record_names_its_machine_and_platform`
- `get_usage_filters_by_machine`

**`js-sim-cell`: the three examples of 3.3**, as scenarios on the fake clock

- `linux_work_alone_never_reaches_a_mac`
- `a_linux_burst_while_macos_runs_keeps_queue_order_on_the_macs`
- `a_macos_burst_after_linux_filled_the_macs_takes_them_back_as_linux_jobs_end`

The invariant "nothing double-booked" is extended to machines and checked on every step.

### 7.2 Tasks

Each task is one pull request, and it updates the docs it changes and moves its goldens in the same pull request (T19).

| Task | What | After |
|---|---|---|
| `jsmp-reported` | booking at the reported size: `placement_shape` from the claim, the pool's `shape` optional and a floor, `check_pool` from capabilities, the usage limits, `mac_pool_shape.rs` and `configuration.md` moved to the new rule | none |
| `jsmp-offers` | `js-caps` offers and uses, and `vm.overhead_mib` | none |
| `jsmp-wire` | `Hello::machine`, the three refusal codes, the agent's `--machine`, the previous-release tests | none |
| `jsmp-ledger` | `js-sched` machine ledger: `set_machine`, room and load admission at the machine level, whole-host jobs on a machine | none |
| `jsmp-narrow` | `js-sched` narrowness tier on both paths, the whole-host orders, `set_offers` | `jsmp-offers` |
| `jsmp-pools` | `side_of`, start checks, registry admission, `machine` in the record and binding, the membership row, `HostView.machine` and `platform`, the `Placer` seam | `jsmp-reported`, `jsmp-wire` |
| `jsmp-commit` | `js-jobs` membership read, machine check and `hostbook/<machine>` write in `place_claimed`, the two-server tests | `jsmp-pools` |
| `jsmp-place` | the placer wiring: machine, offers, VM overhead and machine capacity into `OpenHost` and the commit; the walk's closing by machine and spill | `jsmp-ledger`, `jsmp-narrow`, `jsmp-commit` |
| `jsmp-output` | `Host` fields, `ListHostsResponse.platforms`, `/v1/hosts` JSON, the `UsageRecord` and `GetUsageRequest` fields | `jsmp-offers`, `jsmp-pools` |
| `jsmp-sim` | the three `js-sim-cell` scenarios and the machine invariant | `jsmp-place` |

`jsmp-reported`, `jsmp-offers`, `jsmp-wire` and `jsmp-ledger` can land in parallel.

`jsmp-pools` alone already refuses side hellos without a machine. Without the placer wiring of `jsmp-place` the server would not
yet book the machine, so no pool file names `side_of` before `jsmp-place` is released (the rollout of section 5). The macOS
agent's Linux session lands with the Linux-on-Mac design's runtime.

## 8. The owner's choices

These were questions in an earlier draft and are now settled. The design above follows them.

| Question | Choice |
|---|---|
| Provider order or narrowness first | The provider first, then narrowness. |
| A fixed share for Linux on a Mac | None. Linux may use a whole Mac when macOS work does not need it, and the split follows demand. |
| Memory between the sides | It follows demand, as CPU does. The first shape is one Linux VM per job, sized to the job. A warm guest is a later option (2.6). |
| A key to refuse spill | None. Every job may spill onto a broader machine. |
| The CPU slack | A Linux server is used into its slack before a Mac takes Linux work. |
| Booked shapes | None. A host offers what it reports, per host, on Linux and macOS alike (2.1). |
| macOS precedence over higher-priority Linux | None beyond queue order. |

## 9. Open for the lead

- **Hostkeeper's facts file.** The agent's own detection is the host's report, and it is all the server needs. Should hostkeeper's
  facts file also carry cores and memory, for a reader other than the agent? Nothing in this design needs it.
- **A pool's `shape`.** It is kept as an optional floor so that existing pool files keep their admission rule. Removing it outright
  would refuse those files at start.
- **The size between hellos.** A size that changes without a hello is not designed. Today a shrink is seen at the agent's next
  start.
- **Egress for the Linux VMs.** Until the Linux-on-Mac design filters their egress, the Linux session's ceiling is `none`, so Linux
  jobs that need egress do not spill onto the Macs.

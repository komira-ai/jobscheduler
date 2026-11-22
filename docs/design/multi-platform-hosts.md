# One js-agent per Mac, macOS first and Linux as an add-on; hosts booked at what they report; the narrowest host first

This is the design for a Mac that serves macOS jobs first and Linux jobs later, written to the owner's rulings of 2026-11-22. It has
two parts, in the order the owner set:

- **Part 1 (sections 2 and 3): native macOS jobs under the one-host shape.** A Mac is one host: one agent, one session, one book. It
  reports its cores and memory, and the server sees nothing else of it: no users, no slots. Section 2.4 says exactly what must
  change on the server before native jobs serve.
- **Part 2 (sections 4 to 6): Linux jobs on a Mac, as an add-on.** The same agent runs Linux jobs as containers in one long-running
  Linux VM it launches and stops itself, after Docker Desktop's model, with our own container launch. Part 2 only adds: the host's
  operating systems grow from `macos` to `macos` and `linux`, and a Part 1 host and an old server behave as before (4.1).

One rule spans both: placement spends the narrowest host first, so a Linux job goes to a Linux-only server before a Mac (section 6).
It is a design only; nothing here is built. Section 8 holds the questions still the owner's. Code is cited by file and symbol; line
numbers are approximate. Rule ids (T10, B8, B20) are those of `AGENTS.md`.

**The owner's rulings, and where each is carried.**

| # | Ruling | Section |
|---|---|---|
| 1 | The agent on a Mac schedules both macOS and Linux work, and that is all the surface it shows | 2, 4 |
| 2 | The server knows nothing of users; it sees total capacity. Users, slots and the one Linux VM are the agent's | 2, 5 |
| 3 | A vocabulary that maps cleanly onto the buck2 tags; Linux arm64 first, maybe ISA level; for x86-64, v1 to v3 | 3 |
| 4 | The VM's launch moves inside `js-agent`: no Lima, no separate launcher | 5.3 |
| 5 | The Linux VM does not count against Apple's two-VM limit for macOS guests | 5.3 |
| 6 | `js-agent` reports cores and memory, not hostkeeper | 2.2 |
| 7 | One `js-agent` on a Mac schedules both Linux containers and Mac sandboxes; no agent inside the VM | 5 |
| 8 | Copy Docker's VM on a Mac; the only difference is our own fast container launch | 5.1 |
| 9 | Firewall rules are around each container | 5.3 |
| 10 | Leftovers in the VM are handled as on a Linux server; the VM is reused (and, by ruling 12, stopped after an idle time) | 5.3 |
| 11 | A job carries its operating system and architecture; no separate container-platform field, so a client sends no `exec` | 2.3, 3 |
| 12 | Memory as Docker does it: a fixed VM memory limit, and an idle stop with a start on the next Linux job; agent settings only | 5.4 |
| 13 | Two parts: native macOS jobs first; Linux on a Mac as an add-on, purely additive | Part 1, Part 2 |
| 14 | The agent is in charge of the VM's life cycle; a container start while the VM is stopped, starting or stopping is accepted and waits | 5.5 |
| 15 | Lay out every route for input trees into the VM, and test; choose none yet | 8, Q3 |

**The machines.**

| Machines | Count | Cores, memory | Runs |
|---|---|---|---|
| Linux arm64 servers (still to be racked) | 4 | 128, 256 GiB | Linux arm64 |
| Mac Studio | 4 | 28 to 32, 96 GiB | macOS arm64 (Part 1); Linux arm64 in the VM (Part 2) |
| Linux x86_64 nodes | 6 | as today | Linux x86_64 |

## 1. What the code does today

- **The pool's shape is floor and booking.** `PoolConfig` (`crates/js-registry/src/config.rs`) holds one `shape`. The hello's
  judgement (`judge`, `crates/js-registry/src/registry/admit.rs`, near line 122) refuses a claim below it (`below-floor`), and
  `list_claimed` and `placement_shape` (config.rs, near lines 372 and 404) book every host at the pool's resources. A 32-core Mac in
  a pool shaped at 28 cores is booked at 28 (`crates/js-registry/tests/mac_pool_shape.rs`). The joined pool file requires `shape`
  (`crates/js-lease-server/src/file/joined.rs`).
- **The host reports its size, unused.** The agent detects cores and memory (`facts::detect`, `crates/js-agent/src/facts.rs`) and
  sends them in its hello's `WireShape`; the registry keeps them as the host's `claim`.
- **One operating system and one architecture per host.** The agent adds one `os` and one `arch` (`capabilities`,
  `crates/js-agent/src/host.rs`); `js_caps::NodeCaps` holds one of each and `Request::unmet` compares by equality
  (`crates/js-caps/src/matching.rs`). The only multi-valued capabilities are the member keys (`xcode`, `vm.image`).
- **Native jobs serve today through `exec`.** kbf asks for a Mac job with `os = macos`, `arch = arm64` and `exec = sandbox-exec`
  (komira-ai/kbf `crates/kbf-server/src/executor/mac.rs`, `mac_constraints`). A pool that declares a mode refuses a hello whose
  `runtime` differs (`HostContract::check`, `crates/js-caps/src/host.rs`). `job_start.rs`
  (`crates/js-lease-server/src/job_start.rs`, near line 209) makes a native start, with its `os` and `arch`, only when the job's
  `exec` is `sandbox-exec` and it names an image (its bundle); a container start carries no `os`.
- **A held agent refuses new starts with an existing code.** `host-held` (`crates/js-agent-wire/src/refusal.rs`): the host is being
  drained for an update. The server sends the job back to its queue and counts no failure.
- **Placement.** A pass walks the queue in priority, then submit order (`walk_in_order`, `crates/js-sched/src/queue.rs`); a job
  placed nowhere closes the hosts it could use to the jobs behind it. `Servable::fit` (`crates/js-sched/src/servable.rs`) chooses:
  capable, with room (load admission within `cores x (1 + slack)`, default 1.1x, and memory within the schedulable share, the
  measured working set included: `memory_is_filled_to_the_bookings_and_a_larger_working_set_still_refuses`,
  `crates/js-sched-placer/src/engine.rs`), the first machine provider (`of_first_provider`), then the score (`ScoreKey`,
  `crates/js-sched/src/load.rs`). Nothing prefers a narrow host.

# Part 1: native macOS jobs under the one-host shape

## 2. The host: one per Mac

### 2.1 What a Mac reports

One agent, one session, one book (rulings 1, 2). In Part 1 a Mac reports what it reports today: its cores and memory (`WireShape`),
`os = macos`, `arch = arm64`, `os_version` and its Xcodes. Users, pool slots and Seatbelt profiles are the agent's
([mac-one-program.md](mac-one-program.md)); the server sees none of them.

### 2.2 Booked at what it reports

Ruled (6), and not needed before native jobs serve (2.4):

- A host's capacity is its own claim: the cores, memory, GPUs and scratch disk of its hello, memory cut by
  `memory_schedulable_percent` and CPU scaled by the slack, as the pool's shape is cut and scaled today. A 32-core Mac is booked at
  32.
- The numbers are the agent's own detection, the same code on Linux and macOS. `--cpu-millis` and `--memory-bytes` still only lower
  them.
- The pool's `shape` becomes optional. When a pool names one it is a floor only (`below-floor` as today), so an existing pool file
  keeps its admission rule. Where a pool has no shape, the checks that read the floor read the claim: the GPU part of the host
  contract, the scratch-quota default against the disk, and the disk booked.
- A report changes only at a hello. A host whose new report is smaller keeps its running jobs and takes no new job until its
  bookings fit; a running job is never stopped.

### 2.3 What one host per Mac means in code

- **Cordon and taint are per host**: they stop the Mac's work, as today.
- **`exec` leaves the client** (ruling 11). The server makes a native start from the job's operating system (`macos`) and its
  bundle, instead of from `exec` and the bundle; `Constraints.exec` stays readable, so kbf's `sandbox-exec` keeps working, and is
  ignored once the operating system decides.
- **One booking.** The server books the reported size in one place.
- **A whole-host job on a Mac is a whole-host job**: it books the Mac.

### 2.4 What must change on the server before native jobs serve

**Nothing.** A Mac pool with `exec = sandbox-exec`, kbf's Mac constraints and `job_start.rs` serve native jobs today. The rest of
Part 1 is ruled and lands on its own, none of it a precondition:

| Change | Why | Needed for native jobs to serve |
|---|---|---|
| Booking at the reported size (2.2) | ruling 6 | no: the pool's shape books a Mac at the smallest Mac's size until then |
| A native start from `os = macos` instead of `exec` (2.3) | ruling 11 | no, while kbf sends `exec`; yes once a client stops sending it |
| The vocabulary of section 3 | ruling 3 | no |

## 3. The vocabulary of platforms

buck2 sends no tags of its own. A build's execution platform carries a set of properties, and the build service reads `OSFamily` and
`ISA` from it (komira-ai/kbf `docs/platform-properties.md`; buildrules `rules/platforms.bzl` and `rules/isa.bzl`).

| | buck2 (buildrules) | What the build service sends today | The job's constraint | What a host reports |
|---|---|---|---|---|
| Operating system | `:linux` / `:macos` (and the prelude's `os/constraints`) | `OSFamily` = `Linux` or `macOS`, mapped to `os` (`crates/js-caps/src/platform.rs`, `os_family`) | `os` (`linux`, `macos`) | `os` (a set in Part 2) |
| Architecture | `:x86_64` / `:aarch64` (the prelude's `arm64`) | a Linux job's: one executor-wide setting, not the action's (kbf `crates/kbf-server/src/executor/config.rs`, `arch`) | `arch` (`x86_64`, `arm64`) | `arch` |
| ISA level | `isa_<level>`, `exec_isa_<level>`; the property is `ISA=x86-64-v3`, `ISA=armv8.2-a`, or `x86-64` alone (`ISA_LEVELS`, buildrules `rules/isa.bzl`) | always empty (kbf `executor/config.rs`, `isa_level: String::new()`) | `isa_level`, at least, within the family | nothing: no agent detects or reports one |
| macOS version | none | none: the build service holds the least version itself and sends `os_version` empty (kbf `executor/mac.rs`, `mac_constraints`) | `os_version`, at least, by major | `os_version` on a Mac |

**Gaps found in code**: no agent detects or reports an ISA level (the levels exist: `IsaLevel`, `crates/js-types/src/isa.rs`); the
build service sends an empty ISA level and takes a Linux job's architecture from one executor-wide setting; a container job's start
carries no `os`; four spellings of one platform exist (`linux-arm64` / `darwin-arm64`, `linux_aarch64` / `macos_aarch64`,
`macos-arm64`, `os=macos arch=arm64`).

**The vocabulary is the owner's choice (V1, section 8).** The options:

- (a) **The two buck2 properties are the vocabulary**: the operating system (`OSFamily`) and the `ISA` value, which names the
  architecture and carries the level (`x86-64`, `x86-64-v3`, `aarch64`, `armv8.2-a`), spelled one way everywhere: the build
  service's request, the job's constraint and the host's report. *The lead's recommendation, recorded as the lead's.*
- (b) buck2's platform names (`linux_aarch64`, `macos_aarch64`, `linux_x86_64_v3`) as the one spelling.
- (c) Today's `os`, `arch` and `isa_level` keys, with the build service filling `arch` and `isa_level` from the action.

# Part 2: Linux jobs on a Mac, as an add-on

## 4. The host gains `linux`

### 4.1 What is added, and why it is purely additive

- **The hello.** The operating systems become a set: a Mac with its VM reports `macos` and `linux`, with one `arch` (`arm64`: the VM
  runs on the Mac's CPU, with no emulation). The set travels as one additive capability; `os` stays, naming the first of the set.
- **Matching.** A job asks for one operating system and one architecture. It matches when its operating system is in the host's set:
  `os` moves from `EXACT_KEYS` to `MEMBER_KEYS` (`crates/js-caps/src/matching.rs`), where `xcode` is.
- **The start.** The start's existing `os` field (`JobSpec::os`, `crates/js-agent-wire/src/command.rs`; absent on a legacy start) is
  now set for container jobs too, so the agent picks its runtime (5.2). `downgrade_for` (same file) already clears it for an agent
  that does not speak the per-job start.

Why nothing of Part 1 changes:

| Peer | Meets | Behaviour |
|---|---|---|
| A Part 1 Mac (`macos` only) | a Part 2 server | its set is `{macos}`: matched and placed exactly as in Part 1 |
| A Part 2 Mac (`macos`, `linux`) | a Part 1 or older server | the server reads `os = macos` and ignores the set. The Mac keeps reporting `exec = sandbox-exec` and its `runtime`, so `HostContract::check` admits it as before. It is a macOS host, as in Part 1, and gets no Linux job |
| A Linux host | either | its set is `{linux}`: unchanged |
| An old agent | a Part 2 start with `os` | ignores the field and runs its one runtime |

A Mac's agent advertises `linux` only once its VM path is configured and healthy (5.5): a Mac without it is a Part 1 host.

### 4.2 What one host with two systems means in code

- **Cordon and taint are per host.** A broken VM is the agent's alone to handle: it stops advertising `linux` (a new hello) until
  the VM is back, and macOS work goes on.
- **The network ceiling is per host.** The Mac reports the most both runtimes enforce (5.3).
- **The pool contract** accepts a host whose runtimes cover the pool's operating systems, instead of one equal mode.

## 5. The agent on a Mac

### 5.1 Docker's model, and ours

**Docker Desktop on a Mac**, from its own documentation: one Linux VM on Apple's Virtualization framework by default, run by the app
as the logged-in user ([Mac FAQ](https://docs.docker.com/desktop/troubleshoot-and-support/faqs/macfaqs/)); the engine inside the VM
and `com.docker.backend` on macOS driving it ([networking](https://docs.docker.com/desktop/features/networking/)); files by
VirtioFS; memory a fixed limit, by default half the host
([settings](https://docs.docker.com/desktop/settings-and-maintenance/settings/)), with no balloon named; memory back when Resource
Saver stops the VM after 5 idle minutes, 3 to 10 s to return ([Resource
Saver](https://docs.docker.com/desktop/use-desktop/resource-saver/)). Apple's own `container` tool documents the framework's
ballooning as partial.

**Our measurement of the same shape** ([linux-vm-on-mac-docker-model.md](experiments/linux-vm-on-mac-docker-model.md)): boot 7.9 to
8.1 s to placeable, once (5.1 there); a container start in the running VM 42 ms median from the agent receiving it to the container
running, 8 ms of it the runtime (5.2); idle CPU about 2.5% of one core (5.3). Apple's `container` tool, one VM per container, paid
about 0.54 s of boot per container ([linux-containers-on-macos.md](experiments/linux-containers-on-macos.md)).

| | Docker Desktop | This design | Difference |
|---|---|---|---|
| VMs | one Linux VM, Virtualization framework | the same | none |
| Who launches and stops it | the app | `js-agent` (rulings 4, 14) | ours is the one agent program |
| Inside the VM | the Docker Engine | `crun` and, by Q4, a small run-only program of ours or nothing of ours (5.3) | **our fast launch** (ruling 8), with the Linux agent's job contract and hardening |
| Files | VirtioFS | open: every route of Q3 is measured first (section 8) | open |
| Memory | a fixed limit; back when the idle VM stops | the same (ruling 12) | the start back is ours, about 8 s measured |
| Network | NAT through the backend | NAT, with rules around each container (ruling 9) | per-container rules, as on Linux |
| Reuse | reused, stopped when idle | the same (rulings 10, 12) | none |

### 5.2 Two runtimes behind one abstraction

`js-agent` drives jobs through one interface, `trait Runtime` (`crates/js-agent/src/lifecycle/runtime.rs`): `NativeRuntime` on
macOS, `CrunRuntime` on Linux ([mac-runtime.md](mac-runtime.md)). A Part 2 Mac agent holds both and picks per job from its operating
system: `macos` runs in the native sandbox (a slot user, a Seatbelt profile, the per-run supervisor of
[mac-one-program.md](mac-one-program.md)); `linux` is sent into the VM (5.3). The lifecycle, the dispatch poll and the one session
stay as they are; every job of either runtime books against the one capacity the agent reports.

### 5.3 The Linux VM and what is in it

- **Launch and stop.** `js-agent` launches the VM itself through the Virtualization framework, as `js-vmm` does for macOS guests,
  and stops it (rulings 4, 14). The VM is not a macOS guest, so it takes none of the two VM slots the agent's gate keeps for macOS
  guests (`VM_SLOTS = 2`, `crates/js-agent/src/vm/mod.rs`; ruling 5).
- **What is in the guest is not settled** (question Q4, section 8). Docker has its engine there. The CEO ruled a Linux `js-agent`
  driven by the macOS agent the wrong shape, and the lead has asked him which he means. What every choice has in the guest: a Linux
  kernel, a minimal root file system, `crun`, and the job's image on the VM's own disk. The choices differ in what of ours runs
  there (section 8, Q4).
- **How they talk**: a virtio socket (Q4 (a)), or the guest's stock `sshd` over the VM's network (Q4 (b)).
- **Firewall rules around each container** (ruling 9): the Linux agent's per-container `nft` rules
  ([agent-egress.md](agent-egress.md)), set in the guest for each container.
- **Leftovers** (ruling 10): the post-job checks run inside the VM, as on a Linux server; the VM is reused between jobs until its
  idle stop; a broken VM is rebuilt from its image while the agent advertises no `linux` (4.2).

### 5.4 The VM's memory, and the capacity the agent reports

As Docker does it (ruling 12), with two agent settings the server never learns: **a fixed memory limit** for the VM (the VM never
holds more; the guest keeps what it touched, up to it, until the VM stops: a 15 GB footprint after builds with under 1 GB used in
the guest, and the balloon gave nothing back, experiment 5.3) and **an idle time** after which the agent stops the VM with no Linux
job (Docker: 5 minutes), giving all its memory back to macOS.

The agent reports the Mac's whole cores and memory, and books every start against its own ledger, as every agent does:

- **While a Linux start is accepted, or the VM is starting, running or stopping**, its whole memory limit is taken from the Mac's
  memory. A macOS job fits the Mac's memory less the limit and the other macOS jobs; a Linux job fits the limit less the other Linux
  jobs. CPU is shared; both kinds book against the Mac's cores.
- **While the VM is stopped and no Linux start is accepted**, nothing is taken for it.
- **The server sees it only** through the agent's load reading, whose memory working set includes the VM process's footprint; the
  server's load admission already refuses a host whose measured working set leaves no room (section 1). Today native macOS runs
  report no load (`NO_NATIVE_USAGE`, `crates/js-agent/src/platform/macos.rs`), so the Mac agent's first reading is the VM's
  footprint.
- **A start the agent cannot hold** is refused `no-capacity`, as on every host. Examples: a Linux job larger than the limit, or a
  Linux job on a Mac whose VM is stopped while macOS jobs hold more than the Mac less the limit. The check runs before the start is
  accepted, so an accepted start is never left with a VM that cannot start. The operator sets the limit at least as large as the
  largest Linux job the Macs should take.

### 5.5 The VM's life cycle, the agent's states and the job's

The agent is in charge of the VM (ruling 14). A Linux start that arrives while the VM is stopped, starting or stopping is **accepted
and waits**, once it passes the fit check of 5.4 (which counts the VM's limit as taken from the moment the start is accepted). The
agent records the accepted start in its state root, as it records every accepted start, and starts the VM, or lets a stop finish and
starts it again.

**The states.**

- VM: **stopped**; **starting**; **running**; **stopping**, which is only the idle stop: no Linux job is waiting, starting or
  running.
- Agent: **serving**; **draining** for an update (held: a new start is refused `host-held`); **handing over** to a new agent;
  **restarting** after a crash.
- Job: **none** (no Linux job on the Mac); **waiting** for the VM (accepted, recorded, no container yet); **starting** (its
  container being made); **running**.

Some cells depend on the owner's question Q2 (section 8): what owns the VM process.

- Under Q2 (a) the agent's own process owns it, and the VM dies with it (measured, experiment 5.5). An update then drains, so
  handing over never happens on a Mac with Linux work.
- Under Q2 (b) a supervisor mode of `js-agent` owns it, and it outlives the agent.

The table assumes Q4 (a), a run-only program in the guest that supervises each run. Under Q4 (b), with nothing of ours in the guest,
a running job does not outlive the Mac agent's restart, so every "adopted" cell becomes `lost`.

| Agent | VM | No Linux job | Job waiting | Job starting | Job running |
|---|---|---|---|---|---|
| serving | stopped | idle: memory is all macOS's | the agent starts the VM | cannot occur: no container starts in a stopped VM | cannot occur while the VM is stopped (a VM that dies is the row below) |
| serving | starting | cannot occur: the agent starts the VM only for a waiting job | waits, then starts | cannot occur: not ready | cannot occur |
| serving | running | the idle time runs; when it passes, the agent stops the VM | starts at once | the container is made | runs |
| serving | stopping | the stop finishes | waits for the stop, then the agent starts the VM again | cannot occur: the idle stop begins only with no Linux job | cannot occur, for the same reason |
| draining | stopped | the agent exits for the update | accepted before the hold: the agent starts the VM and runs the job; the drain waits | cannot occur | cannot occur |
| draining | starting | cannot occur, as when serving | waits, then runs; the drain waits | cannot occur | cannot occur |
| draining | running | Q2 (a): the agent stops the VM, then exits. Q2 (b): it exits, leaving the VM to the new agent | starts and runs; the drain waits | runs to its end; the drain waits | runs to its end; the drain waits |
| draining | stopping | the stop finishes; the agent exits | waits, then the VM starts again for it | cannot occur | cannot occur |
| handing over (Q2 (b) only) | stopped | the old agent hands over | cannot occur: the old agent starts the VM and the job first | cannot occur | cannot occur |
| handing over | starting | cannot occur: the old agent finishes the start first | cannot occur | cannot occur | cannot occur |
| handing over | running | the VM is left to the new agent; its idle time starts again | cannot occur: started first | cannot occur: made first | the new agent adopts it through the VM's socket |
| handing over | stopping | cannot occur: the old agent finishes the stop first | cannot occur | cannot occur | cannot occur |
| restarting | stopped | idle | the new agent reads the record and starts the VM | Q2 (a): the start is made again from the record. Q2 (b): cannot occur | Q2 (a): lost with the VM (`lost`, run again). Q2 (b): cannot occur |
| restarting | starting | Q2 (b) only: the new agent waits for it, then lets the idle time run | Q2 (b): waits for the VM | cannot occur | cannot occur |
| restarting | running | Q2 (b) only: the idle time starts again | Q2 (b): starts | Q2 (b): the new agent asks the guest: a container made is adopted, one not made is made again | Q2 (b): adopted |
| restarting | stopping | Q2 (b) only: the stop finishes | Q2 (b): waits, then the VM starts again | cannot occur | cannot occur |

**Two transitions outside the table's states:**

- **The VM dies while the agent runs**, from a guest panic or macOS ending the VM process. Running jobs end `lost` and run again. A
  starting job's container is made again in a new VM, from its record. Waiting jobs wait for the restart. The agent starts the VM
  again; while it cannot, it stops advertising `linux` (4.2).
- **A start arrives during a hand-over.** The old agent is held, so the start is refused `host-held`. The new agent accepts starts
  once its hello is admitted.

**What the server sees.**

- A start that takes longer: bounded by a VM stop and start (about 1 s and 8 s measured). The pool's `start_timeout` must be longer.
- Nothing new.
- `host-held` for a start during a drain or a hand-over: an existing code, and the job goes back to the queue with no failure.
- `lost` for running Linux jobs in two cases: when the VM dies, and when the agent crashes under Q2 (a). These are the same outcome
  as any host whose runs die. Q2 (b) removes the second case; nothing removes the first.
- **A restart or hand-over also makes the host not placeable for a while.** The new agent's hello is refused `409 host-live` until
  the old session lapses: 20 to 62.5 s measured in the experiment (its 5.5 and section 7). This is today's behaviour for any agent
  restart.

**Why the abstractions do not conflict.** This is the owner's question: "if it's accepting and waiting and then an update comes
through".

1. An update first holds the host, so no new start is accepted (`host-held`).
2. Every start already accepted is finished by the old agent, whatever the VM's state.
3. The VM is stopped only with no Linux job in it.
4. The agent hands over or exits only with no job waiting or starting.

So no accepted start is left with no agent to finish it.

A crash is covered as follows. Waiting and starting jobs are made again from the record of accepted starts. Running jobs are lost
under Q2 (a), and adopted under Q2 (b).

## 6. The narrowest host first

### 6.1 The rule

**A host's offers** are the scarce things it can give a job: each operating system it runs, and `accelerator:<class>` when it
reports GPUs of a class. They come from the pools' configuration (the operating systems a pool's capabilities name), so every server
reads the same and a Mac whose VM is down keeps both. **A job's breadth on a host** is the number of the host's offers it does not
use: a Linux arm64 job has breadth 1 on a Mac and 0 on a Linux server; a job without a GPU, 1 on a GPU host. In Part 1 every Mac
offers only `macos` and the rule changes nothing.

**The rule**: of the hosts that can take the job now (capable and with room), keep those with the least breadth, and let the
existing order decide among them. When the narrow hosts are full, the job goes to a broader host rather than waiting. **Spill**: a
job is spill on a host when a pool exists whose hosts could run it with a smaller breadth, judged from configuration.

### 6.2 Where it sits

In `Servable::fit`, after the provider tier and before the score, on both paths: capable; room (the CPU slack is part of room, so a
Linux server is used into its slack before a Mac takes Linux work); the provider (`of_first_provider`); **the least breadth**; the
score (`ScoreKey` or the least `Load`), whose last keys make the order total. Whole-host jobs: quiet hosts by provider, breadth, id;
marks by drain cost, provider, breadth, id (`drain_cost`, `crates/js-whole/src/cost.rs`).

**The walk.** A job that waits closes, to the jobs behind it, the hosts on which it is not spill; on a host where it is spill it
closes nothing. So a waiting macOS job closes the Mac to the Linux jobs behind it, and a waiting Linux job does not close the Mac to
the macOS jobs behind it. Only the `can_use` closure passed to `walk_in_order` changes; nothing is held across passes (standing rule
R1 of [design.md](../design.md)).

### 6.3 Worked examples

Today a Mac is the only Linux arm64 host: the Linux arm64 servers are not racked. So, until they are, every Linux arm64 job goes to
a Mac, and the narrowness tier changes nothing. Once they are, with the settings of 5.4 (each Mac's VM limit 48 GiB; a Linux job of
8 cores and 16 GiB):

- **Linux work alone**: it goes to the Linux servers (breadth 0) while they have room; no Mac's VM starts.
- **Linux overflow while macOS runs**: each Mac takes Linux jobs only when the servers are full, its VM starting for the first
  (about 8 s); a macOS job that waits closes the Macs to every Linux job behind it.
- **The reverse**: as Linux jobs on a Mac end, their cores go to waiting macOS jobs; after the idle time the agent stops the VM and
  its 48 GiB return to macOS.

## 7. What changes in the server, and compatibility

| Part | Change |
|---|---|
| 1 | booking at the reported size, `shape` optional and a floor; a native start from `os = macos`, `exec` still read |
| 1 | the build service sends a job's operating system, architecture and ISA level from the action (V1) |
| 2 | the hello's operating-system set; `os` a member key; the pool contract over runtimes; the start's `os` for every job |
| 1, 2 | the narrowness tier and the walk's closing (section 6); `ListHosts` per host's systems, with "8 hosts run linux arm64, 4 only that"; `UsageRecord` gains the platform |

Compatibility of Part 2 on Part 1 is the table of 4.1. A pool file without `shape` is refused at start by an old server, so it is
never misread; new proto fields are skipped by an old client. The previous-release tests are named in section 9.

## 8. The owner's open questions

**Q1 is ruled** (ruling 12). His question "does it support balloon?": the Virtualization framework has a balloon device and the
guest obeys it (its free memory fell to 1.4 GiB), but the VM process on the Mac did not shrink at any step (it grew 1.76 GB while
the balloon inflated), and freeing memory in the guest gave nothing back; only stopping the VM gave memory back (experiment 5.3,
macOS 26.6).

**Q2. What owns the VM process, so what an agent restart or update does to Linux jobs** (5.5).

- (a) **The agent's own process.** An update drains Linux jobs first; a crash loses the running ones (`lost`, run again). Cost: a
  full drain at every update and lost work at every crash.
- (b) **A supervisor mode of `js-agent`**, which outlives the agent as the native per-run supervisor of
  [mac-one-program.md](mac-one-program.md) does; a new agent re-attaches. Cost: untested whether a Virtualization framework VM
  survives its launching process's exit when a child holds it.
- (c) **A separate launchd job.** Cost: a second program and launch context, against ruling 4.

**Q3. How a container in the VM reads an input tree.** Choose none until the test (ruling 15). The owner leans to (3), the VirtioFS
share; none is chosen until the proof's table exists. The proof task `mac-poc-vm-inside-agent` measures each on the 46,362-file
reference tree (walk and read, cold and warm, from a container), a large file's throughput, the Mac's CPU, and correctness of a
lazily fetched tree.

| Route | What exists | What would be new | What the proof must measure, beyond the common set |
|---|---|---|---|
| (1) the guest mounts the Mac's tierstore NFS export | tierstore's NFS front on the Mac: NFSv3 and MOUNT v3 on one port, read-only, any AUTH_SYS caller, listening on an IPv4 loopback address only (`fs.nfs.listen` refuses otherwise); the guest kernel's NFS client | a forwarder to the loopback listener or a change of the listen rule; a second, writable path for outputs | latency of a first read of an unfetched file; behaviour when the Mac's tierstore restarts |
| (2) a second tierstore with FUSE in the guest | tierstore with FUSE, as on a Linux node | tierstore in the guest image, its cache disk, keys and reachability of the store from the guest, and the Mac agent reaching a mount provider inside the guest | cache fill and size against the VM's memory limit; two caches for one tree |
| (3) a VirtioFS share of tierstore's mount point | VirtioFS in the Virtualization framework; tierstore's mount on the Mac | the share in the VM's configuration; whether VirtioFS passes a FUSE or NFS mount through correctly | the experiment's share read a small-file tree about 4.4 times slower than the VM's disk on plain APFS (5.4): the same through tierstore's mount |
| (4) a per-job copy onto the VM's disk (the baseline) | the copy itself (experiment 5.4: 17.5 s from a share) | nothing | copy time per job; the VM disk's churn and space |
| (5) a block device per tree, an image the VM attaches | nothing: nothing builds a tree image today | building an image (ext4 or erofs) per tree on the Mac; attaching it to a running VM | image build time; whether the framework attaches a device to a running VM at all; read speed |

**Q4. What of ours runs in the guest.** The CEO ruled a Linux `js-agent` driven by the macOS agent the wrong shape; the lead has
asked him which he means. The proof task `mac-poc-vm-inside-agent` runs the choices. The Linux agent's container code that each
needs in the guest:

- (a) **A small run-only program of ours**, whether a mode of our binary (as `__supervise-run` is today) or a separate one. In the
  guest: the per-run supervisor (`__supervise-run`, `crates/js-contain/src/oci/supervise.rs`) that is `crun`'s parent and writes the
  exit record; the per-container `nft` rules ([agent-egress.md](agent-egress.md)); the bundle store's unpack; the post-job checks
  (`crates/js-agent/src/integrity`). On the Mac: the OCI config `js-contain` builds, sent over a virtio socket. It never registers
  and holds no credential or server address. A run survives the Mac agent's restart, because its supervisor is in the guest.
- (b) **Nothing of ours: a stock channel such as the guest's `sshd`.** The Mac's `js-agent` runs `crun`, `nft` and the checks in the
  guest over it, and copies the config and the image's layers in. In the guest: stock `crun`, `nft` and `sshd` only. Cost: no
  supervisor in the guest, so a run's exit record and its survival rest on the `ssh` session, and the Mac agent's restart ends it; a
  key the Mac's agent holds for the guest; every container step is a remote command.

**V1. The vocabulary of platforms** (section 3).

## 9. Tests and tasks

Every test asserts behaviour, uses the injected clock (`js_state::ManualClock`) where time matters and asserts no elapsed time
(T10); all run on Linux x86_64, on fakes, except the macOS beta stage's VM tests.

**Part 1** (`js-registry`, `js-lease-server`): `a_host_is_booked_at_the_cores_and_memory_its_hello_reports`;
`a_32_core_mac_is_booked_at_32_cores`; `a_pool_shape_is_a_floor_only`; `a_pool_without_a_shape_admits_any_size`;
`a_host_that_says_hello_smaller_keeps_its_running_jobs_and_takes_none_until_they_fit`;
`a_macos_job_without_exec_gets_a_native_start`; `a_macos_job_with_exec_sandbox_exec_gets_the_same_start` (an older client).

**Part 2 additive** (`js-caps`, `js-registry`, `js-agent-wire`, `js-lease-server`):
`a_linux_job_matches_a_host_whose_set_holds_linux`; `a_part_1_mac_places_as_before_on_a_part_2_server`;
`a_part_2_macs_set_reads_as_macos_on_the_previous_release` (B8); `an_old_agent_ignores_the_starts_os` (B8);
`the_pool_contract_admits_a_host_whose_runtimes_cover_the_pools_systems`; `a_container_jobs_start_carries_its_os`.

**The agent's VM** (the Mac runtime's tasks): one test per cell of the table of 5.5, named `<agent>_<vm>_<job>` (for example
`serving_stopping_waiting_waits_for_the_stop_then_starts_the_vm`, `draining_stopped_waiting_starts_the_vm_and_the_drain_waits`,
`restarting_stopped_running_is_lost_under_agent_owned_vm`), a cell that cannot occur asserting the agent never reaches it; plus
`a_running_vm_takes_its_whole_memory_limit_from_macos_jobs`, `a_stopped_vm_takes_nothing`,
`the_vm_stops_after_the_idle_time_with_no_linux_job`, `a_linux_job_larger_than_the_vms_limit_is_refused_no_capacity`,
`a_new_start_during_a_drain_is_refused_host_held`, `a_start_during_a_hand_over_is_refused_host_held`,
`an_accepted_linux_start_reserves_the_vms_limit`, `a_vm_that_dies_loses_running_jobs_and_remakes_starting_ones`,
`under_q4_a_the_guest_program_is_given_no_credential_and_no_server_address`, `a_linux_vm_never_takes_a_macos_vm_slot`,
`a_broken_vm_drops_linux_from_the_hosts_set`.

**Narrowness and the walk** (`js-sched`, `js-sched-placer`): `a_linux_job_goes_to_the_linux_only_host_before_the_mac`;
`a_linux_job_goes_to_the_mac_when_every_linux_only_host_is_full`; `a_job_without_a_gpu_goes_to_the_host_without_one_first`;
`narrowness_comes_after_the_provider`; `narrowness_applies_with_admission_by_load_off`;
`a_narrow_host_within_its_cpu_slack_is_chosen_before_a_broad_idle_host`; `an_old_farm_places_as_before`;
`a_waiting_macos_job_closes_the_mac_to_jobs_behind_it`; `waiting_linux_jobs_never_close_a_mac_to_macos_jobs_behind_them`;
`a_working_set_that_holds_the_vms_memory_refuses_a_macos_job`; and the three examples of 6.3 as `js-sim-cell` scenarios with
"nothing double-booked" on every step.

**Tasks** (one pull request each; none started):

| Task | Part | What | After |
|---|---|---|---|
| `jsmp-reported` | 1 | booking at the reported size; `shape` optional, a floor | none |
| `jsmp-native-os` | 1 | a native start from `os = macos`, `exec` still read | none |
| `jsmp-isa` | 1 | ISA level detection and report | V1 |
| `jsmp-os-set` | 2 | the hello's operating-system set, `os` a member key, the pool contract over runtimes, the start's `os` | `jsmp-native-os` |
| `jsmp-narrow` | 1, 2 | offers, breadth, the tier, the whole-host orders, the walk's closing | `jsmp-os-set` |
| `jsmp-output` | 1, 2 | `ListHosts` systems and summary, `UsageRecord` platform | `jsmp-os-set` |
| `jsmp-mac-runtimes` | 2 | the Mac agent's two runtimes behind `Runtime`, the choice per job | `jsmp-os-set` |
| `jsmp-mac-vm` | 2 | the agent's VM launch and stop, the guest image and Q4's channel, per-container rules, the memory limit and idle stop, the ledger and load reading (5.4), the life cycle of 5.5 | `jsmp-mac-runtimes`, Q2, Q3, Q4, the proof task `mac-poc-vm-inside-agent` |
| `jsmp-sim` | 2 | the three scenarios and the invariant | `jsmp-narrow` |




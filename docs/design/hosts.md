# Hosts that run many jobs, spot hosts and several clouds (5.18)

Companion to [docs/design.md](../design.md), section 5.18. Section numbers refer to design.md. This file holds the text of the hosts
amendment: a provider brings hosts that run many jobs at once, not one machine or one VM per job; how a host's capacity is booked and
packed; VMs inside a host, per instance type; spot hosts; price and capacity as provider data; several clouds as several providers of
one ordered hierarchy; and what an elastic pool does when it is at its machine cap and the queue keeps growing (section 11). The
decisions are D56 to D65 ([decisions.md](decisions.md)); the risks are K42 to K48 ([risks-f.md](risks-f.md)).

## 1. The rule in one paragraph

A capacity provider brings **hosts**. A host is a machine that runs `js-agent` and serves a pool; it is sized to hold many jobs, and it
runs them at the same time and one after another. A *job run* is one container, or one VM, on a host. The provider launches a machine only
when the queued bookings, packed onto the hosts that already exist by the placement rule (6.5, section 3 below), do not fit; it never
launches a machine because one job arrived. Where the instance type allows it, one host also runs several VMs (a spot instance of a type
that supports nested virtualization, a bare-metal instance, a Mac): each VM *run* still gets its own fresh VM (D29), and many such VMs and
many containers share the one host, each booked against the host's capacity.

**Summary** (this text was section 5.18 of design.md). A container books its CPU and memory, a VM its CPUs, memory and one `vms` slot in
full (each VM run still gets a fresh VM, D29), a share lease the vector it names, a whole-node lease the whole host; the placement rule
packs them (6.6) and a machine is launched only for the hosts the packed bookings need. Whether a host can run VMs is a fact about its
launch option (an instance type: `virt` is `none`, `nested` or `bare-metal`; `vms` the slots), declared by the provider and bounded by the
agent's probe (D58). A **spot** host's interruption notice taints it as `reclaim`, tells consumers to leave (`host_leaving`), lets work
finish to the deadline, then reruns what is left elsewhere, through the taint, drain and host-lost paths that exist (D59). Price and
capacity are provider data used only to choose among launch options and equal providers, never to refuse or cap (D60, N4). `aws`, `gcp`,
`azure` and on-prem sources are providers in the one ordered hierarchy; clouds are values, an entry may be a tier of equals, and the walk
falls through as in D39 (D61). An elastic pool sizes its first instance to the first request and adds instances up to `max_machines`;
only at the cap, with the queue still growing, it launches the biggest type it can get and drains its smallest instance (jobs finish within a grace, the rest are requeued), and later
scales down only when the queue is empty and utilization is low (D62 to D65).

Three words used below. **Booking:** the part of a host's capacity (the resource vector of section 3 of design.md) that a lease or a run
holds. **Slot:** one unit of the `vms` dimension, the most VMs a host runs at once. **Launch option:** one way a provider can launch a
host for a pool's shape (an instance type and its terms); it is not the agent's *offer*, which is what a running host is willing to give.

## 2. What read as one machine or one VM per job, and now does not

The earlier text already reused a machine for many jobs in a row (5.10) and let VMs share a host with containers (5.15). Four places still
read as one job to a whole machine, and are changed in place:

- the ucdk Job classes were admitted `BookAll` (D7): a whole free host for each job. They will book their declared size under `Book` and
  share a host (D56, step 59);
- the scaler's demand was "machines' worth of the shape": it is now the hosts needed to hold the queued bookings after packing (5.8, section 3);
- a Job's size was "rounded up to a pool shape": the shape is the smallest host of a pool, and the size is the booking placed on a host
  that has room (ucdk.md);
- the text on VMs said what a fresh VM per run means for one run, and not that several such VMs are on one host at once (vm-backend.md,
  machines.md).

**What the code does today, so that this file is not read as already built.** No ucdk class is defined in this repository's code; a class
id that a pool file does not know loads as `BookAll` (`agent_pool.rs`), so a ucdk class named there would book a whole host until step 59
changes the ucdk admission. `vms` is not a `Dim` of `js-types` yet (`cpu_millis`, `memory_bytes`, `gpus`); it becomes one in step 59 or
60. The provider trait in code is the delegated `Provider` (`start`, `stop`, `observe`, `capacity`); `CapacityProvider`, `acquire`,
`events`, `launch_options`, `FirstFit`, cooldowns, the scheduler-recorded `reclaim` and `evict` taints, and the cap-time swap are design
(steps 59 to 64). Today's registry keeps its marks in memory only; making these taints durable is part of steps 62 and 64.

What does not change: a **fresh VM for each VM run**, cloned from its base image and destroyed after the run (D29); a **whole-node lease
books a whole host** (section 5); one reservation, one drain, the caps; the scheduler never sees an action of a consumer that holds
machine leases (9.1).

## 3. How a host's capacity is booked and packed

**The capacity.** A host's capacity is the vector its agent reports: `cpu_millis`, `memory_bytes`, `gpus`, `vms` (the dimensions of
section 3 of design.md; `vms` is added to the code's dimensions in step 59 or 60; the agent offers what it is willing to give, total less
booked, 5.15). The pool's shape is a floor, not the size: a host whose reported shape satisfies the pool's shape is admitted
(`Shape::satisfies`, registration.md), and a larger host simply holds more.

**The request vector.** Every lease, and every run the scheduler places itself, carries a request vector. It is what is booked:

| Job run | Books | Notes |
|---|---|---|
| a container | its `cpu_millis` and `memory_bytes` (and `gpus`) under `Book(Dims)` | memory is checked once against the host total and counted in the booking; containers may oversubscribe memory among themselves (6.3) |
| a VM | its whole CPUs, its configured memory and one `vms`, in full, from clone to destroy | no overcommit (5.15); a lease of several runs holds the size of its VMs between runs |
| a share lease (a consumer's machine lease of less than a host) | the vector the lease names | the consumer packs its own work inside it (9.1) |
| a whole-node lease | every dimension of the host | exclusive, quiet, reserved by a drain (section 5, 5.17) |

**Bin-packing is the placement rule.** There is no second rule for hosts the provider brought. For each queued request, in queue order:
*fit* keeps the capable hosts whose offer covers the booking on every gated dimension, has a free slot when the booking needs one, and
meets the request's constraints (6.1, 5.13); *rank* picks among them. In a pool that scales, rank is `FirstFit` (6.5): the request goes to
the first fitting host in hierarchy order and, inside one provider, in host order, so work packs onto the fewest hosts and the others go
idle and are released by the idle rule (5.8, 5.10). In a pool that does not scale, rank may stay `Spread`. The two passes of the
hierarchy (providers.md) run over this: a free host that fits is used before anything is launched.

**Demand for a new host** is found by the same rule. Take the queued bookings that fit no existing host, no host about to be freed and no
host still `registering`; place them, in queue order, with `FirstFit` onto empty hosts of the pool's shape; the number of empty hosts used is
the demand, at most `max_step` per tick and never beyond `max_machines` (5.8, 5.11). So a burst of 500 small jobs that fit two idle hosts
launches nothing, and 500 jobs that need nine hosts launch nine over the ticks, however many jobs each holds. A launch option larger than
the shape holds more than the demand assumed; the surplus is released by the idle rule when nothing uses it.

**Who packs.** Two levels, one rule. Where a consumer leases a share (9.1), the scheduler packs the *leases* onto hosts by this rule and
the consumer packs its *actions* inside its leases. Where the scheduler places the runs itself (the run service for ucdk, 11), it packs
the runs. A host can carry both kinds at once, since both are bookings against the same vector.

**Load-ranked placement is the consumer's inner level.** The proposal to place kbf actions by measured and predicted node load
(`placement-by-load.md` in jobscheduler PR 59) admits an action on a node the consumer already holds a lease on, from the node's measured
CPU and working set and the predicted cost of its leases. That is level 2 of 9.1, inside a lease; the scheduler never sees an action. It
does not replace `FirstFit` here: `FirstFit` ranks hosts for bookings at level 1, and for runs the scheduler places itself, by declared
vectors. The two compose, and the pool still packs onto the fewest hosts. If load-ranked admission is ever applied at level 1 in a
scaling pool, it must keep the property this section relies on, that new work goes to the first fitting host so the others go idle and
are released (5.8, 5.10); that is a decision for that proposal, and not made here.

**Usage attribution (12.2).** A host's instance-hours are attributed to the leases on it in proportion to their bookings over time, for
sizing and attribution only; nothing is charged.

## 4. VMs inside a host, per instance type

A VM needs a hypervisor on the host. Whether a machine has one depends on its type: a Mac has one; a bare-metal cloud instance has the
hardware; some virtual instance types allow **nested virtualization** (a VM inside a VM) and most do not; an on-prem hypervisor host
usually does. So VM support is a fact about a *launch option*, reported by its provider, and the placement rule reads it like any other
host fact (5.13).

**Two core facts** (added to the table in [capabilities.md](capabilities.md)):

| Key | Kind | Meaning |
|---|---|---|
| `virt` | `text` | `none`, `nested` or `bare-metal`: how a VM would run on this host. Absent means unknown, and unknown is unmet |
| `lifecycle` | `text` | `on-demand`, `spot` or `registered`: the terms the host was obtained on (section 6) |

`vms` (a `count`, existing) is the number of slots. Rules:

1. **The provider declares, per launch option.** Each launch option names its `virt` and the `vms` its host can run (config or the
   provider's own table), plus the base image a VM job needs (the existing `base_images` rule of 5.13). The scaler counts a VM request as
   demand only if a launch option's *declared* facts meet its `host` constraints, as for any constrained request (capabilities.md, rule 4).
2. **The agent decides what is true.** The agent probes whether it can run a VM (the hypervisor device is present and usable) and reports
   `vms` as the least of its `--vms` flag and what the probe allows. The declaration can lower nothing and raise nothing the probe denies:
   a host launched from a VM-capable option whose probe fails reports `vms = 0` and `virt = none`, serves containers only, and raises an
   alert. It is not a taint: nothing unsafe happened, the option was wrong. A request with `vms >= 1` simply does not match it.
3. **A VM request is a constraint, not a class of machine.** `vms >= 1` is the gate that exists; a consumer that cares how (say it wants
   bare metal for timing) adds `virt eq bare-metal`. `virt` and `lifecycle` are host facts, so a `guest` constraint on them is
   `constraint-bad-scope`, as for `vms`.
4. **Slots are a booked dimension.** A VM books one slot; the host's slots free after the VM is destroyed (the order of 5.15). A host of
   a nested-virtualization type may have few slots (the agent's `--vms`); the packing rule needs no special case for it.
5. **Trust.** `virt` is an unverified claim like every fact (K27), and bounded by the probe (rule 2). A consumer that needs real
   isolation leases from a pool whose operator it trusts.

A container-only option (`virt = none`, `vms = 0`) is a valid host for container jobs; there is no "VM pool" and "container pool" by
nature, only by the labels an operator puts on pools.

## 5. The whole-node class on a shared host

Nothing in 5.17 changes; the host is now shared in more ways, so the consistency is stated.

- A whole-node lease books **every dimension of one host**, all `vms` slots included, so it needs a host with no lease, no run and no VM.
  A host that carries many small bookings is drained before it can be given whole; the drain cost already ranks hosts by the latest end
  bound of everything on them (whole-node.md, section 3), so a host with many short jobs is cheaper to drain than one with a few long ones.
- In an elastic pool the whole-node request acquires a fresh host before it drains a busy one (D50), and the fresh host is reserved at
  registration. Nothing else is packed onto it.
- A whole-node lease on a spot host (section 6) can be lost to an interruption; it is the "loss" case of whole-node.md, section 9: the
  consumer repeats its measurement in a new lease. A consumer that cannot accept it constrains `lifecycle in {on-demand, registered}`, or the
  operator keeps spot launch options out of the pool's hierarchy.
- The caps (`max_whole_node` and the others) count hosts, whatever the host is packed with. The cap-time swap of section 11 never touches
  a host that holds a whole-node lease or a reservation, and is checked against the ceiling by host count.

## 6. Spot hosts

A *spot host* is a machine bought on terms under which the cloud may take it back, with a short notice or none. It is an ordinary
elastic host (providers.md) with `lifecycle = spot`, and the machinery below is the existing machinery, used in a new order.

**The notice.** The cloud announces a reclaim some time ahead (its length is the cloud's). A provider that can read it returns it from
`events()` (5.5, the optional reclaim-notice stream) as `Interruption { instance, deadline }`, the deadline in the provider's reading of
the cloud's clock converted to the scheduler's injected clock when the event is taken. A provider without events returns none; then the
agent is the second path: the cloud's shutdown signals the agent, which announces the existing phase `draining` (5.3). With neither, the
reclaim is a plain loss (below).

**What the scheduler does, in the tick that takes the notice** (under the table lock, recorded durably before it acts, like every
effect):

1. **Taint and cordon the host.** The host takes no new lease and no new run, and no request is queued toward it. A cordon alone is
   scheduler state that a restart forgets (absence.md); the notice must survive a restart, so the host is recorded as **tainted** with the
   reason `reclaim`, by the scheduler itself, as it records a host-job taint at grant (5.12). A tainted host is not capable (D41), is not
   released for idleness, and is never the target of a reservation; a reservation it held is dropped, without counting a give-up and without
   excluding another host (whole-node.md). A reclaim taint is the one taint that needs no operator: the instance is going and its id is
   never reused. A reclaim taint counts toward neither `max_taints_in_a_row` nor `max_boot_failures` (a flapping spot market is not a bad
   job class, K42).
2. **Drain, as the existing drain does.** Work on the host runs until its end or the deadline, whichever is first; nothing is requeued
   early for a run that can finish. The renewal answer of every lease on the host carries `host_leaving: true` and `leaves_at` (additive,
   R3; the sibling of `host_reserved`, whole-node.md), asking the consumer to place nothing new there and to release when its work is
   done; a consumer drains before it releases (9.1). A scheduler-placed run (ucdk) that has not started is requeued at once; one that is
   running is left to finish.
3. **Reschedule at the deadline.** Whatever is still on the host when the deadline passes, or when the provider reads the instance
   `Absent` with a reclaim reason, is lost: the lease is given up, the consumer reruns its placements elsewhere (9.1), and a
   scheduler-placed run is requeued, each against the platform-fault budget (8.1, as 5.3 says of a reclaim). Unknown stays unknown: the
   scheduler frees a booking only on Absent (5.6).
4. **Replace through the usual path.** The booking that was on the host is queued demand again, so the next tick's two passes run: a free
   capable host anywhere first, then an acquire in the first entry that can satisfy, under `max_step`, `cooldown`, `max_machines` and the
   shares. The option that was just interrupted is marked for the provider's capacity data (section 7), so the choice may go elsewhere.

**No notice** (a hard stop, a host lost) is the case that already exists: silence is Unknown, then the grace, then the lease is given up
and rerun (5.6, registration.md, Lifecycle). The notice only makes the end orderly.

**What is not here.** No cost guardrail and no spend cap: choosing spot is a provider and pool configuration (which options a pool's
providers carry) and a consumer constraint (`lifecycle`), never a scheduler budget (N4). The platform's quotas still arrive as a refused
acquire.

## 7. Price and capacity: provider data used for choosing

A provider reports, for each launch option, **data**; the scheduler reads it to choose among options and never to limit anything.

```rust
struct LaunchOption {
    id: OptionId,                  // opaque to the core (an instance type and terms, for aws)
    totals: Resources,             // what a host of this option offers; at least the pool's shape
    facts: Facts,                  // declared host facts: arch, os, virt, vms, lifecycle, ...
    price: Option<Price>,          // an amount per host-hour in the provider's unit, with the unit's name; absent when unknown
    capacity: Capacity,            // advisory signals, all optional
}
struct Capacity { available: Option<u64>, recent_refusals: u32, recent_interruptions: Option<Band> }
```

In configuration an option is a table of the provider (an `instance_types` entry stays the one-option shorthand):

```toml
[[provider.option]]
shape = "linux-x86-16"
id = "<instance-type>"                      # opaque to the scheduler; the provider's own name for the machine
facts = { virt = "nested", vms = 2, lifecycle = "spot" }
price = { amount = 0.31, unit = "provider-units-per-host-hour" }   # optional; a provider that can read a market overrides it each tick
```

The provider gains one call, `launch_options(shape) -> Vec<LaunchOption>` (read-only; a provider that returns none has one option made
from its configuration, as `instance_types` does today). `AcquireSpec` gains an optional `option` (additive). The set of options of a
provider for a shape is its **catalog**: cores, memory, GPUs and whether capacity (spot or on-demand) exists, all provider data.

**How it is used.**

- *Which option inside one provider.* Among the options whose `totals` hold the shape and whose declared `facts` meet the request's host
  constraints, the scaler takes the lowest `price` (absent last), then the best capacity signal (fewest recent refusals, fewest recent
  interruptions), then the lowest `id`. A `Refused(capacity)` or `Refused(quota)` cools down **that option** (the `cooldown` of 5.8); the
  next option is tried in the same tick; the provider's entry as a whole cools down only when every option does.
- *Which provider inside one tier* (section 8): the same order, over the options of the tier's providers whose unit of price is the same.
- *What it is never used for.* Not to refuse a request, not to stop a launch, not to sum a bill, not to compare a request's cost with a
  budget. There is no dollar model in the scheduler (N4). Price is a sort key.

Price and capacity data may change between ticks (a spot market moves); the scheduler takes one reading per tick, so a seeded run on the
injected clock chooses deterministically (R2).

## 8. Several clouds: providers in the ordered hierarchy

**Clouds are values.** `aws`, `gcp`, `azure` and an on-premises source are kinds of provider, each a crate behind the `CapacityProvider`
trait (5.5, 5.7). The core names none of them (the layering test of 10.1, planned with the providers, fails if it does). A cloud's account,
region, credentials role, launch image and instance-type table are values in its `[[provider]]` table; the same kind may appear twice
under two names (two regions, two accounts). Where clouds differ in what they can do, that is data in a launch option (`virt`,
`lifecycle`, `price`, `capacity`), never a branch on the cloud's name. The on-prem members are the `default` provider (hosts registered by
hand or by the operator's own tooling) and `vsphere`, `proxmox` and bare metal with power control when those kinds exist.

**One hierarchy.** A pool's `providers` is the ordered list of 5.8. To mix clouds, list them:

```toml
[[pool]]
name = "build"
shape = "linux-x86-16"
class = "action"
providers = ["site", ["aws-spot", "gcp-spot", "azure-spot"], "aws"]
max_machines = 64
provider_max_machines = { aws-spot = 24, gcp-spot = 24, azure-spot = 24, aws = 8 }
```

An entry may be a single provider name or a **tier**, an inner list of providers that are equals. A flat list is a list of one-provider
tiers, so every existing file means what it did. A provider appears at most once in the whole list. The members of a tier must report
price in the same unit (the file is refused otherwise).

**How it falls through.** The rules of D39 and providers.md hold, per tier:

1. *Use what exists.* Among the free capable hosts that fit, take the first in tier order, and inside a tier the first by `FirstFit` over
   hosts; a free host of a later tier is used before anything is launched.
2. *Launch only if nothing can serve now.* Walk the tiers in order. The first tier with a member that **can satisfy by acquiring** (it is
   enabled; not in an outage, brake or cooldown; some option meets the request; and an acquire is possible under its share and the pool's
   `max_machines`) takes the demand. Inside the tier the member and option are chosen by section 7. An earlier tier that cannot is passed
   over; a later one is asked only then.
3. *An unreachable provider* (an outage, a failed credential) cannot satisfy now: its pool share freezes (5.6) and the walk passes it. A
   refused acquire cools the option or provider down and the walk goes on in the same tick.
4. *Release goes the other way.* Packing prefers earlier tiers, so hosts of later tiers go idle first and are released first by the idle
   rule (5.10): on-prem is used before cheap spot, spot before on-demand, and the dearest capacity is the first given back.

**Why not order the whole list by price.** The order is the operator's statement of what to prefer (own hardware first, by contract,
by data location, by risk), and price alone would put on-prem hosts after any cheaper cloud and would change with the market. Price
chooses only among equals the operator named. Where data must stay in one cloud or region, the operator labels pools by site and the
consumer selects by label (6.2); the scheduler holds no data-location rule.

## 9. Tests, invariants, steps

Invariants, checked in seeded runs of the fake agent and the fake providers on the injected clock (R2); the numbering continues the
list of invariants.md and is local to this file until it is merged there:

- H1. **No host holds a booking above its offer on any dimension**, a VM's booking counted in full; slots used never exceed `vms`.
- H2. **A burst packs.** For any queued set that fits the free capacity of the existing hosts, no machine is launched; demand for new
  hosts equals the empty hosts the packing needs, never above `max_step` per tick or `max_machines`.
- H3. **A VM id serves one run** (D29) while several VM ids coexist on one host; each is destroyed before its slot frees.
- H4. **A VM request is never placed on a host with `vms = 0`**, whatever its launch option declared; a host whose probe fails reports
  zero slots and is not tainted for it.
- H5. **A reclaim notice taints the host in the tick it is taken**, recorded before any effect; the host takes no new lease or run after
  that tick, is not capable (D41), is not counted toward `max_taints_in_a_row`, and is not released for idleness; every lease still on it
  at the deadline is given up once, and no booking is freed on an Unknown observation.
- H6. **A whole-node lease never shares its host**, whatever packing the host had before the reservation (W1 of whole-node.md holds).
- H7. **Price never refuses.** Over seeded runs, the set of requests accepted does not depend on any price or capacity value; only the
  option chosen does.
- H8. **Tiers keep the order of D39.** A later tier is asked only when every earlier tier has no free fitting host and cannot acquire; a
  flat list behaves exactly as before the tier syntax.

Tests: the packing example (500 small jobs, two idle hosts, nothing launched; nine hosts' worth, nine launched over the ticks); a VM
request refused by `vms >= 1` on a container-only option and served by a bare-metal option; a nested-virtualization option whose probe
fails; a notice on a host with a short run (it finishes), a long run (lost at the deadline, requeued once against the platform-fault
budget), a not-yet-started run (requeued at once) and a reservation (dropped, no give-up counted); the notice lost across a restart (the
taint is read back); a spot interruption storm that does not trip the taint brake; two clouds of one tier, the dearer one chosen only
when the cheaper is refused; a provider outage in the first tier passed in the same tick; a flat file unchanged by the tier syntax. No
test asserts elapsed time (R2). The invariants and tests of the cap-time swap are in section 11.

Steps (Phase K of [extraction-steps-d.md](extraction-steps-d.md), steps 59 to 64).

## 10. Considered and rejected

- **One machine per job, terminated after it** (D23(a), already rejected: a boot for every job, and no reuse).
- **A VM per job on its own machine.** Boots a machine for each VM run; multiplies the boot of D29 by the machine boot (K25).
- **Reusing a VM to fit more on a host.** Rejected by D29: what remains of a guest after a job cannot be shown to be clean.
- **A price-ordered hierarchy** and **a spend cap or budget that stops launches** (N4): see section 8 and section 6.
- **A separate scheduler for spot hosts.** The reclaim path is the existing taint, drain and host-lost paths; a second path would rot.
- **Draining the small instances at the cap, with headroom for one extra host, and consolidating below the cap.** Considered for section
  11 and replaced by the simpler rule there: the drain needed a headroom slot (reserved, or one over the cap), bounds for long jobs and a
  precedence rule among taints, and consolidation needed logic of its own. Jobs are retryable, so the pool terminates and requeues.

## 11. An elastic pool at its machine cap

Until now an elastic entry launched machines of the pool's shape up to `max_machines` (5.8) and, at the cap, the queue waited. This
section says what the pool does about the size of its machines. Decisions D62 to D65; risks K47 and K48; step 64.

**Sizing the first instance and growing out.** The catalog (section 7) is the list of instance types a provider can launch for the
shape. When the pool has no machine of an entry, the first queued request that no host can serve takes the smallest option whose totals hold
its booking and whose declared facts meet its constraints (section 7's order breaks ties). More demand adds instances by the demand rule of
6.6, `max_step` and `cooldown`, up to `max_machines`. Nothing in the rest of this section applies while the pool is below its cap.

**Swap up: only at the cap, and only while the queue keeps growing.** The pool keeps a *pool sample* once a minute on the injected clock
(R2): the machines by option and state, the queued demand in hosts (6.6), the queue length, and the booked and total of each dimension.
(The design had no per-minute pool history: the usage record of 12.2 is per lease and the request history of section 7 is per class of
request. The sample is soft state in memory; a restart empties it, so the windows start again, and the effect is only that a swap comes
later.) The trigger is judged once per *observation window*, `swap_window` (default 10 minutes): at every sample of the window the pool was
at or above `max_machines` (the gate is `>= max_machines`, so the swap's own extra machine still counts as at the cap), and the queued
demand was above zero, and the **first** swap of an episode needs the queue to have grown strictly over the window (the last sample above
the first); a **repeat** after an action needs only that the queue has not eased (the last sample not below the first of the new window).
No second swap starts while a victim has not read back Absent (Unknown included). Then, in one tick:

1. *Choose the victim.* An elastic machine of the first elastic entry that can launch a larger option, preferring the smallest whose
   running jobs are expected to finish soonest. **The estimate** is the one the design already has for a drain, the *end bound*
   (whole-node.md section 3): for each lease and run on the host, its start plus its max wall time (12.1), minus now, and zero for a host
   with none; where a consumer or the run service has a better predicted remaining time for a job (the sizing history of section 7, or
   per-minute profiles if they are adopted; none is built into this design today), the smaller of the two is used. A job with no
   prediction is *unknown* and counts as its max wall time, so an unknown ranks as the slowest to finish, never the fastest. Hosts whose
   expected drain is within `swap_drain_grace` tie, and among the tied the smallest wins: smallest by `cpu_millis`, then memory, then
   fewest bookings, then lowest host id; a host whose expected drain is beyond the grace ranks after all of those, by expected drain. A registered `default` host is never chosen (the scheduler did not
   create it). **Never chosen:** a host that holds a whole-node lease or a reservation (the next smallest is taken); a host already tainted
   for a reclaim; a host holding a run that has already been evicted `max_evictions` (default 3) times; and a host whose removal would push
   the whole-node ceiling below the whole-node hosts reserved plus held (the ceiling counts hosts, `min(max_whole_node, capable_hosts -
   keep_for_ordinary)`, whole-node.md section 6, so a swap is checked by host count). If no host can be chosen, nothing happens.
2. *Choose the biggest option the provider can get.* Walk the entry's catalog from the biggest option (cores, then memory) downward and
   take the first that has capacity, meets the pool's constraints and is not cooling down. A type with no capacity (spot or on-demand) is
   skipped, so the walk falls through the catalog; the lifecycle is whatever the option declares, and a change from spot to on-demand is
   the operator's order in the hierarchy (section 8), not made here. If the biggest option obtainable is not larger than the victim, the
   pool stays as it is. **Thrash guard:** the swap fires only if the chosen option would hold at least one queued, unplaced request that no
   current host can take (its booking, constraints and class limits fit the option, and quota and the whole-node caps allow it). If the
   queue is blocked by something a bigger machine does not fix (a whole-node cap, a per-class limit, quota, a constraint only the current
   type meets), nothing is drained and nothing is launched.
3. *Launch first, then drain the victim (decided).* The launch is recorded and called (with the booking of the victim still counted);
   only when the provider answers `Accepted` is the victim marked to drain. A refused launch cools down that option by the existing
   per-option `cooldown` (section 7, 5.8) and the walk of item 2 continues in the same tick; if every option is refused nothing is
   drained and nothing is lost. Because of this order the count may be `max_machines + 1` until the victim reads back Absent, that is, for
   at most the drain grace. That is the one launch the swap is allowed over the cap; it is bounded by one swap per window, and launches
   stay refused at or above the cap otherwise.
   **Structural bound:** each swap replaces a machine with a strictly larger type or does nothing, so the swaps of one episode are finite
   (at most `max_machines` times the depth of the catalog). `max_evictions` is an extra bound on one request, not the only one.
4. *Wait one window.* The window starts when the new machine is up (registered and serving), not when the victim was marked. A drain in
   progress blocks the next swap, since no second swap starts while a victim has not read back Absent; so a repeat comes at the later of
   the window's end and the victim's disappearance. At the window's end the same test runs on the new samples; if the queue has eased,
   nothing more is done, and if it has not, the swap repeats with the next victim and the biggest option again. Overshoot is accepted:
   the pool may end with bigger machines than the load needed. There is no logic to consolidate machines or to undo an overshoot except
   the scale-down below.

**What the eviction means: drain, then terminate.** The victim stops taking new work, its running jobs are left to **finish** (a build must
not simply fail), and the machine is terminated when it empties or when the drain grace ends, whichever is first. Only jobs still running
at the end of the grace are put back in the queue to be retried elsewhere; they are never reported as failed. It uses the existing paths:

- *Nothing new is placed.* The scheduler records the victim durably as tainted with the reason `evict` (the mark of D59, which survives a
  restart), so no new lease or run lands on it, and tells consumers by `host_leaving` with `leaves_at` set to the end of the grace (D59).
  Its running work continues, as it does for a reserved host (whole-node.md section 4). The drain grace is the pool key
  `swap_drain_grace`, default 30 minutes, given in the configuration file or by flag (R4), never by an environment variable. It is shorter
  than a job's max wall time on purpose; the cost of a longer job is stated in K47.
- *Terminate.* When the host holds no lease and no run, or at the end of the grace, the scheduler calls `release` (terminate, then read
  back, 5.5). A host that empties is terminated at once, so most evictions cost no lost work.
- *The lost-host path, for what remains.* A host that reads back Absent is a lost host: the leases and runs still on it are given up and
  rerun, by the rule of 5.6 and absence.md. For a lease a consumer holds (kbf), the lease is lost and the consumer is told, and it reruns
  its placements in another lease (9.1). For a run the scheduler placed itself (ucdk), the run is requeued, a new attempt
  (retries-and-runs.md, 8.1, 8.3).
- *Fence and Unknown.* Nothing continues on a terminated machine; the agent's own fence (5.4) covers a machine that is cut off but not
  yet stopped. If the terminate reads back Unknown, nothing is freed and the leases stay booked (5.6); a run-on lease is given up only when
  the machine is proven stopped (registration.md, Lifecycle).
- *Budgets (D63).* An eviction is the scheduler's choice, not a fault of the work or of the platform. The attempt is recorded with the
  outcome `evicted` and **charged to no budget**: not the platform-fault budget, not the loss count that refuses a poison operation after
  three losses (absence.md), not a task failure (8.1). The consumer is told the reason, so kbf, whose budgets are its own, can also not
  charge it. A request evicted `max_evictions` times has its host passed over as a victim, which bounds how often one request can be hit.
  The count is kept per request id, as an additive field of the run record for a scheduler-placed run (an attempt ends `evicted`), and as
  an additive counter in the lease file for a lease a consumer holds (kbf): when the host of a lease is evicted, the counter of the
  lease's request id goes up, and a consumer that presents the same request id when it asks for a replacement lease (as it does for a
  whole-node request, whole-node.md section 3) carries the count with it; the record is dropped when the request ends. A consumer that
  presents no stable request id has no count across leases, and then only the structural bound above applies.
- *The cap.* An evicted host leaves the count when its terminate reads back Absent (5.11); until then, draining included, it counts, and
  the swap's launch is the one extra of item 3.

**Scale down: one signal, kept apart from the swap.** The only scale-down signal is that the queue has drained and utilization is low:
at every sample of `scale_down_window` (default 60 minutes) the queue was empty and the booked share of the pool (the tighter of CPU and
memory, over all machines) was below `scale_down_util` (default 30 percent). The pool then reduces by one machine per window: the
elastic machine with the least booked, biggest first on a tie, not holding a whole-node lease or reservation, is marked tainted with the
reason `retire` (no new work, no replacement, not counted toward `max_taints_in_a_row`), and is released when it empties (5.12, 5.8
Release). Nothing is killed. If the queue becomes non-empty before it empties, the mark is cleared (a `retire` mark is the scheduler's own;
an integrity taint is never cleared this way). A machine launched later is sized by the first-request rule, so the pool returns to small
types as demand falls. The two thresholds are separated in time (a 60-minute window against 10) and in load (a utilization floor against
a growing queue), and a window must hold at every sample, so one burst or one quiet minute never triggers either.

| Parameter | Default | Meaning |
|---|---|---|
| `swap_window` | 10 minutes | the observation window of the swap up, and the wait after each action |
| `scale_down_window` | 60 minutes | the window in which the queue must stay empty and utilization low |
| `scale_down_util` | 30 percent | the utilization below which a scale-down may start |
| `swap_drain_grace` | 30 minutes | how long a victim may finish its running jobs before it is terminated and the rest requeued |
| `max_evictions` | 3 | evictions of one request after which its host is not chosen as a victim (decided) |

All five are pool keys (R3: additive; absent keys mean the pool never swaps and never scales down by this rule, so a pool written before
this section is unchanged). A refused launch uses the existing `cooldown`.

**Long jobs.** A long job on a small machine does not stall the pool at its cap for longer than the grace: it keeps running until it
finishes or `swap_drain_grace` ends, then it is requeued as a new attempt, outcome `evicted`, charged to no budget, and counted toward
`max_evictions`. Lost work is only what a job had done past the grace (K47). The victim choice prefers hosts that will empty within the
grace, so most evictions lose nothing.

**Interaction with the whole-node class (5.17, whole-node.md).**

- A host with a whole-node lease or a reservation is never the victim and never retired (a held lease is never revoked, D49; nothing is
  stopped for a reservation, D48). A queued whole-node request that no live host is big enough for is demand like any other; the
  biggest option launched may serve it, and D50 commits the registered machine to the request.
- A whole-node request is not a reason to evict: eviction is judged on the pool at its cap with a growing queue, and the whole-node
  hosts it must spare reduce the choice.
- *Ceiling and caps by host count.* The swap leaves the host count unchanged once the victim is gone, but the check of item 1 uses the
  count at the moment of the swap. An eviction is not a reservation: it counts toward neither `max_draining` nor `keep_for_ordinary`, and
  an evicted or retiring host is not capable (D41), so it is never a reservation target.
- A request no catalog option could ever hold is refused `no-capable-host` at submit, as now.

**Spot hosts during the window.** Section 6 is unchanged and independent of the window. If the new biggest host receives a reclaim notice
or is lost before the window ends, it is handled as any reclaimed host (taint `reclaim`, drain, rerun). The pool is then below its cap, so
ordinary scale-out refills it by the demand rule, with the capacity signal of section 7 steering the provider away from the option just
interrupted; a swap happens again only if the pool is back at the cap and the trigger holds. A victim that has a reclaim notice is not chosen (item 1). If a retiring (scale-down) host receives a notice, `reclaim`
takes precedence over `retire`: the reason becomes `reclaim` (both kept), the notice's deadline applies, and its work follows section 6.
A `reclaim` notice on an `evict` host (draining): `reclaim` takes precedence, the earlier of the notice's deadline and the end of the
grace applies, and the work follows section 6.

Invariants (continuing H1 to H8):

- H9. **A swap happens only at the cap with a window that did not ease**: no victim is chosen unless at every sample of the last window the
  pool was at or above `max_machines` with demand above zero, and the queue grew strictly (first swap) or did not shrink (repeat), and the
  chosen option would hold a queued request no current host can take.
- H10. **One swap per window and per victim**, and the count never exceeds `max_machines + 1`, and exceeds `max_machines` only between an
  accepted swap launch and the victim's read-back Absent (a drain in progress blocks the next swap).
- H11. **The victim is never protected**: never a registered `default` host, never a host with a whole-node lease or reservation, never a
  `reclaim` host, never one whose removal pushes the ceiling (by host count) below reserved plus held, never one holding a run evicted
  `max_evictions` times.
- H12. **Launch before drain, drain before terminate**: no machine is marked to drain by a swap before its replacement launch is
  `Accepted`; a pool whose every option is refused drains nothing; the victim takes no new work after the mark; it is terminated when it
  is empty or at the end of `swap_drain_grace`, whichever is first.
- H13. **An eviction charges no budget and fails nothing**: a job that finishes within the grace ends with its own result; one still
  running at the end of the grace ends `evicted` (never `failed`), with no platform-fault count, no loss count, no task failure, and is
  requeued once; no lease on the host is lost without the host reading back Absent.
- H14. **Scale-down needs both signals** at every sample of its window (queue empty, utilization below the floor), kills nothing, and
  clears its mark when the queue becomes non-empty.

Tests: the pool at its cap with the queue growing for a full window selects a victim and the biggest option, and a window one sample
short does nothing; among victims whose expected drain is within the grace the smallest is chosen, one with a sooner expected finish
beats a smaller one beyond the grace, and a host with an unknown job ranks as its max wall time; a victim with a job that finishes
inside the grace takes no new work, lets the job finish with its own result and is terminated when empty; a job still running at the end
of the grace is requeued, ends `evicted` and not `failed`, and the machine is terminated; the window starts when the new machine is up,
and a draining victim blocks a second swap; the grace is read from the pool key and not from the environment; a queue that eases after the action ends the swap, and one that does not repeats it with the next
smallest; the biggest type without capacity is skipped and the next one taken, and when every type is refused nothing is terminated and
the refused options cool down; the smallest host holds a whole-node lease (the next smallest is taken) and a reservation (the same); a
removal that would push the ceiling below reserved plus held is skipped; a job on the victim still running past the grace spends
no platform-fault or loss count, and a request evicted `max_evictions` times protects its host; a terminate that reads back Unknown
frees nothing and starts no second swap; the new biggest host receives a reclaim notice mid-window, the pool is below its cap and
ordinary scale-out refills it with the capacity signal steering away; a queue blocked by a whole-node cap, a per-class limit, quota or a
constraint only the current type meets starts no swap and terminates nothing; a first swap needs a strictly growing queue and a repeat
only a queue that has not eased; the eviction count of a consumer lease with a stable request id protects its host after
`max_evictions`; the victim receives a notice before the choice and is skipped; a restart mid-window (the `evict` taint read back,
the windows restarted, no second swap in the restarted window); scale-down with the queue empty and low utilization marks one machine per
window, releases it when empty, kills nothing and clears the mark when a request arrives; a queue that is empty but utilization high does
not scale down. All on the injected clock (R2).

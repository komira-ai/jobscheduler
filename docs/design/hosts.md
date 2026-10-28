# Hosts that run many jobs, spot hosts and several clouds (5.18)

Companion to [docs/design.md](../design.md), section 5.18. Section numbers refer to design.md. This file holds the text of the hosts
amendment: a provider brings hosts that run many jobs at once, not one machine or one VM per job; how a host's capacity is booked and
packed; VMs inside a host, per instance type; spot hosts; price and capacity as provider data; and several clouds as several providers of
one ordered hierarchy. The decisions are D56 to D61 ([decisions.md](decisions.md)); the risks are K42 to K46 ([risks-f.md](risks-f.md)).

## 1. The rule in one paragraph

A capacity provider brings **hosts**. A host is a machine that runs `js-agent` and serves a pool; it is sized to hold many jobs, and it
runs them at the same time and one after another. A *job run* is one container, or one VM, on a host. The provider launches a machine only
when the queued bookings, packed onto the hosts that already exist by the placement rule (6.5, section 3 below), do not fit; it never
launches a machine because one job arrived. Where the instance type allows it, one host also runs several VMs (a spot instance of a type
that supports nested virtualization, a bare-metal instance, a Mac): each VM *run* still gets its own fresh VM (D29), and many such VMs and
many containers share the one host, each booked against the host's capacity.

Three words used below. **Booking:** the part of a host's capacity (the resource vector of section 3 of design.md) that a lease or a run
holds. **Slot:** one unit of the `vms` dimension, the most VMs a host runs at once. **Launch option:** one way a provider can launch a
host for a pool's shape (an instance type and its terms); it is not the agent's *offer*, which is what a running host is willing to give.

## 2. What read as one machine or one VM per job, and now does not

The earlier text already reused a machine for many jobs in a row (5.10) and let VMs share a host with containers (5.15). Four places still
read as one job to a whole machine, and are changed in place:

- the ucdk Job classes were admitted `BookAll` (D7): a whole free host for each job. They now book their declared size under `Book` and
  share a host (D56);
- the scaler's demand was "machines' worth of the shape": it is now the hosts needed to hold the queued bookings after packing (5.8, section 3);
- a Job's size was "rounded up to a pool shape": the shape is the smallest host of a pool, and the size is the booking placed on a host
  that has room (ucdk.md);
- the text on VMs said what a fresh VM per run means for one run, and not that several such VMs are on one host at once (vm-backend.md,
  machines.md).

What does not change: a **fresh VM for each VM run**, cloned from its base image and destroyed after the run (D29); a **whole-node lease
books a whole host** (section 5); one reservation, one drain, the caps; the scheduler never sees an action of a consumer that holds
machine leases (9.1).

## 3. How a host's capacity is booked and packed

**The capacity.** A host's capacity is the vector its agent reports: `cpu_millis`, `memory_bytes`, `gpus`, `vms` (the existing dimensions;
the agent offers what it is willing to give, total less booked, 5.15). The pool's shape is a floor, not the size: a host whose reported
shape satisfies the pool's shape is admitted (`Shape::satisfies`, registration.md), and a larger host simply holds more.

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
- The caps (`max_whole_node` and the others) count hosts, whatever the host is packed with.

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
from its configuration, as `instance_types` does today). `AcquireSpec` gains an optional `option` (additive).

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
trait (5.5, 5.7). The core names none of them (a layering test already fails if it does). A cloud's account, region, credentials role,
launch image and instance-type table are values in its `[[provider]]` table; the same kind may appear twice under two names (two regions,
two accounts). Where clouds differ in what they can do, that is data in a launch option (`virt`, `lifecycle`, `price`, `capacity`), never a
branch on the cloud's name. The on-prem members are the `default` provider (hosts registered by hand or by the operator's own tooling) and
`vsphere`, `proxmox` and bare metal with power control when those kinds exist.

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
test asserts elapsed time (R2).

Steps (Phase K of [extraction-steps-d.md](extraction-steps-d.md), steps 59 to 63).

## 10. Considered and rejected

- **One machine per job, terminated after it** (D23(a), already rejected: a boot for every job, and no reuse).
- **A VM per job on its own machine.** Boots a machine for each VM run; multiplies the boot of D29 by the machine boot (K25).
- **Reusing a VM to fit more on a host.** Rejected by D29: what remains of a guest after a job cannot be shown to be clean.
- **A price-ordered hierarchy** and **a spend cap or budget that stops launches** (N4): see section 8 and section 6.
- **A separate scheduler for spot hosts.** The reclaim path is the existing taint, drain and host-lost paths; a second path would rot.

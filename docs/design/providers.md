# The default provider, elastic providers and the provider hierarchy

Companion to [docs/design.md](../design.md), sections 5.5, 5.7 and 5.8. Section numbers refer to design.md. This file holds the text of the
amendment that made the self-registered pool the base, kept the operator-listed pool for tests, removed `min_machines`, and put providers in
an order.

## Kinds at a glance

| Kind | Role | Creates or destroys compute |
|---|---|---|
| `default` | The base provider (`DefaultProvider`). Hosts register themselves: a machine runs `js-agent`, says hello with the pool's join credential, and is a host of the pool. | Never. |
| `static` | **For testing only.** The operator lists hosts in the configuration; used by tests and by the single-host stepping stone (below). A production deployment declares none. | Never. |
| `aws`, later `gcp`, `azure`, `k8s`, `vsphere`, `proxmox` | Elastic providers: `acquire` boots a host, a machine that runs `js-agent` and many jobs at once, and that agent registers itself. | Yes: boots and terminates. |

A deployment always has a `default` provider. If the file declares none, one named `default` exists with no deny file. A pool admits
registering hosts only when it has a `join_token_file` (registration.md); without one it has no registered hosts of its own, and only
machines an elastic provider launched can join it.

## The DefaultProvider

**What it is.** The set of hosts that registered themselves with the scheduler. Its core is the merged `js-registry` (`RegisteredProvider`,
`merged-code.md`). It answers for a pool through the same trait as every other kind (5.5), with these fixed answers:

- `acquire` is `Refused(capacity)`: it has nothing to launch. The scheduler never asks it to (a provider says whether it can launch; the
  scheduler does not branch on the kind).
- `release` terminates nothing. A host leaves only when its agent deregisters (a clean exit, or the end of a drain), when the operator
  removes or denies it (the deny file, registration.md), or when it ages out of the lost window (registration.md, Lifecycle). The scaler
  never releases an idle registered host: idle release (5.10) is for machines the scheduler launched.
- `list` and `observe` are the host table. Present while heard; Unknown on silence, never Absent; Absent only on deregister or removal (5.6).
- Identity and admission are those of registration.md (join credential, pool exists, tainted set, shape satisfied, `max_machines`).

**Capable host.** A host of the pool is *capable* of a request when it could serve it once it is free: it is in the table and not tainted,
not denied, not draining and not `releasing`; its total shape (not its free room) holds the request's booking (a `BookAll` or `Whole` class needs the
host to exist; a `Book` class needs the total to hold the booking); and its latest facts satisfy every constraint of the request (5.13).
Busy hosts are capable. Lost hosts (silent past the grace, inside the retention window) are capable: the agent registers again with the
same id and the work waits for it, which is the same stance as 5.6 (silence is Unknown, not Absent). Cordoned and quarantined-by-pressure
hosts are capable: both end without anyone replacing the machine. **Tainted hosts are not capable**: a taint ends only when an operator
reimages the machine and clears it (machines.md, 5.12), so waiting for it would be an unbounded wait for an unplanned human action; the
caller should be told. Draining hosts are not capable for a new request, because they are leaving. A host *reserved* for a whole-node request (5.17) is not
draining in this sense: it is not leaving, it stays capable, and it is simply not available to a second reservation or to new work until
the reservation ends. For a whole-node request, "could ever serve" also needs the pool to offer the class (whole-node.md).

**Admission against registered hosts (D35, settled).** Let the *hierarchy* of the request's pool be its ordered providers (below).

1. If some host of the pool is capable, the request is accepted and queued; it is placed when a capable host is free (6.1).
2. Otherwise, if an elastic provider of the hierarchy *could launch* a machine that meets the request (the pool's shape holds the booking;
   for a guest constraint, one of the pool's `base_images` can meet it, 5.13), the request is accepted and its demand is served by an
   acquire (below). *Could launch* is a precise test, used here and in the `regain_grace` re-check: the entry is enabled and has a
   provider share greater than 0, and a launch is possible once every slot that can free is counted as freed. Slots held by in-flight
   acquires, busy hosts, lost hosts and elastic hosts count as freeable (each ends or is released by a rule that needs no human).
   Slots held by tainted or denied registered hosts do not: they stay held until an operator acts (5.12). The pool's `max_machines`
   (which counts every machine, tainted ones included, 5.11) and the entry's `provider_max_machines` must each leave room for the
   booking after only the freeable slots are counted as free. If they cannot, the request has no capable path and is refused.
3. Otherwise the request is refused at once with the plain constant `no-capable-host`. It is never queued: no registered host, now or
   after any bounded wait, could run it, and the caller can act (register a host, change the request, choose another pool).

A pool at `max_machines` with elastic providers is not refused when the slots that can free would cover the booking (it is capable in
principle, and waits, 8.2); a pool whose cap is held entirely by tainted or denied hosts, or whose only fitting elastic entry has a share
of 0 or is disabled, is refused with `no-capable-host`, because no acquire could ever happen and the wait would never end. A request that names a pool or
selectors that match no pool is refused at selection, as today, with the existing code. The refusal covers the request's constraints, its
shape and its class together: a GPU request (D32) in a pool without a registered GPU host is refused, not queued.

*Considered and rejected (D35).* Refusing whenever no host is free now: a busy fleet then looks like a broken one, and every caller would have to build the queue the scheduler exists to provide.

**What happens to queued requests when capability goes.** A queued request is re-checked in the tick at which the table changes (a
deregister, a removal, a taint, an eviction, an age-out). One whose last capable path has gone (no capable registered host, and no
elastic entry that *could launch*, as defined in admission step 2; a pool whose cap becomes held entirely by non-freeable hosts has lost
its last path too), is *not* refused at once: a host that restarts deregisters and registers again within moments, and refusing every queued
request across a restart would turn routine maintenance into failures. It stays queued for `regain_grace` (a pool key; default 5 minutes
on the injected clock, counted from the tick the last capable host left). If a capable host registers within it, the request proceeds
untouched. When the grace passes, the request ends as Refused with `no-capable-host` and the reason `capacity-left`, the same constant as
a refusal at submit, so a caller handles one code. A tainted last host starts the same grace (an operator can clear it quickly); a
longer outage is a refusal, not a wait for ever. Leases already held on a host that is then tainted or evicted follow 5.6 and 5.12
unchanged; only requests not yet placed are affected. New submissions get no grace: a request submitted while the only capable host is
restarting is refused and the caller retries, which is the price of an answer that never hangs.

## Elastic providers

An elastic provider is a `CapacityProvider` that can launch (5.5). It launches **hosts**: a machine is sized to hold many jobs at once, and
a launch answers the packed demand of the queue, never one job (hosts.md, section 3). The scheduler's single interaction is:

1. **Acquire.** The scheduler records the intent (acquire id, pool, entry, shape) durably, then calls `acquire`. The provider boots a
   machine from the image its configuration names; the image carries `js-agent`, the scheduler's address and identity to pin, and the pool
   name. **No secret** is baked in or passed in the launch request (K6).
2. **The machine registers itself.** The agent says hello. For a launched machine the proof is the cloud's signed instance identity
   (registration.md, Aws instances), not a join token: the provider verifies it and matches it to the pending acquire (a provider kind
   implements a verify step beside `acquire`; a kind that cannot prove identity cannot be an elastic provider). The first matching hello
   completes the acquire: the instance leaves `registering` and is a host of the pool.
3. **The provider's job ends there**, at boot plus registration, or at failure: a refusal, a boot that never says hello within
   `boot_timeout` (released, reaped, counted by the failure brake of 5.8), or a hello that does not match.
4. **A registered elastic host is an ordinary host of the pool.** Placement, reuse, caps, taint, leases and fencing treat it exactly as a
   registered `default` host. The only difference is that it carries an instance id, so the scheduler may release it: the idle rule of 5.10
   terminates it (`release`, then read-back), and a taint replaces it (5.12).

## The provider hierarchy

**Configuration.** A pool has `providers`, an ordered list of provider names, and optionally `provider_max_machines`, a table of per-provider
caps. The single `provider` key stays as shorthand for a one-element list, with one rule: if it names an elastic provider, the hierarchy is
`default` followed by it (the default hierarchy), so that registered hosts are preferred and the elastic provider tops up. To forbid
registered hosts in such a pool, give it no `join_token_file`. A pool of one `default` provider has no fall-through.

```toml
[[pool]]
name = "linux-build"
shape = "linux-x86-16"
class = "action"
providers = ["registered", "cloud"]         # where capacity comes from, first to last
max_machines = 24                           # the pool's total, every provider counted (5.11)
provider_max_machines = { cloud = 8 }       # a share of the total; an entry without one is bounded by the total alone
join_token_file = "<path-to-token-file>"
```

**Placement over the hierarchy.** For a queued request, each tick, in two passes:

1. *Use what exists.* Among the pool's capable hosts that are free now and serving, take the first in hierarchy order (within one
   provider, `Rank`, 6.5). Registered capacity of the first provider is tried first, then the second's, and so on. A free machine of a
   later provider is used before anything is launched: launching while a machine sits idle is waste.
2. *Launch only if nothing can serve now.* If no free capable host exists in any entry and no instance in flight (`registering`) can serve
   the request, walk the entries in order. The first entry that **can satisfy by acquiring** takes the demand: it is an elastic provider,
   it is enabled and not in cooldown or the failure brake or an outage, and an acquire is possible under its `max_machines` share and the
   pool's. That entry gets one acquire for the demand (bounded by `max_step`, 5.8). An earlier entry that cannot is passed over; a later
   one is asked only then.

**"Cannot satisfy now"** for an entry means exactly: it has no free capable host, and either it cannot launch (the `default` and `static`
kinds) or no acquire is possible (it is at its share of `max_machines` or at the pool's, disabled, cooling down, braked, or its provider
is unreachable). A busy capable host does not satisfy a request now. So with registered hosts all busy and `cloud` under its cap, a request
goes to `cloud`; with `cloud` at its cap too, it waits in the queue (8.2) for whichever frees first. A pool whose hierarchy is only
`default` waits for a registered host.

**Caps.** `max_machines` is the pool's: it counts every machine in every state, every provider, including in-flight acquires and tainted
hosts (5.11). `provider_max_machines.<name>` bounds the machines of one provider within the pool and is never above the pool's. A
registered host that says hello when the pool is at `max_machines` is refused with `pool-full`, so set the total with the registered
hosts in view. Only the scheduler's own launches are ever refused for the cap; there is no minimum anywhere, and no machine is kept warm.

**A host frees while a machine is booting.** The queued request that was waiting on an acquire is placed on the host that freed, first
in hierarchy order (pass 1), and its `acquiring` state ends. The booting machine is **kept**: its acquire was recorded before the call,
it is paid for by then, and it is simply a machine of the pool without work. When it registers it is an ordinary host and the idle rule
(5.10) releases it after `idle_timeout` if nothing needs it; the idle clock starts at its first hello. Releasing it mid-boot would add a
terminate that can read back Unknown, would discard minutes already spent, and would re-launch at the next burst after a `cooldown`; keeping
it bounds the waste to one boot plus one idle period, and any request queued meanwhile uses it. A boot that fails is handled by the usual
boot-failure rule.

**Tainted and lost hosts in the hierarchy.** A tainted host is replaced by an acquire in the hierarchy only if it was an elastic host
(5.12); a tainted registered host is simply gone from the capable set, and the elastic entries (if any) serve the demand. Demand is
computed over the whole pool, not per entry.

**Several clouds, tiers, spot hosts.** `aws`, `gcp`, `azure` and on-prem sources are providers of this one hierarchy; an entry may be a
tier of equal providers, among which price and capacity data choose; a spot host's reclaim notice taints, drains and reschedules through
the paths above. Rules, configuration and tests: [hosts.md](hosts.md), sections 6 to 8 (D59 to D61).

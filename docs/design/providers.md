# Machine providers: the hosts that register, the providers that launch, and their order

Companion to [docs/design.md](../design.md), sections 5.5, 5.7 and 5.8. Section numbers refer to design.md. This file first held the text
of the amendment that made the self-registered pool the base, kept an operator-listed pool for tests, removed `min_machines`, and put
providers in an order. What the code does today is below; the configuration that amendment designed is under "Superseded" at the end.
The current design of platforms, machine providers and pools is
[jobscheduler-design.md](jobscheduler-design.md), sections 3.5, 4.2 and 5.6.

## Kinds at a glance

The server compiles in the machine provider kinds below and one platform kind, `default` (`Kinds::standard`,
`crates/js-lease-server/src/kinds.rs`; `static` is built into the file reader, `crates/js-lease-server/src/file/platform.rs`).

| Kind | Role | Creates or destroys compute |
|---|---|---|
| `static` | The hosts that register themselves: a machine runs `js-agent`, says hello with its host certificate, and is a host of the pool. The registry's host table behind `CapacityProvider` (`StaticCapacity`, `crates/js-registry/src/capacity.rs`). A `[[machine_provider]]` of this kind takes only `name` and `kind`; it lists no hosts. | Never. |
| `lambdalabs`, `ec2`, `gce`; later kinds named only (`azure`, `k8s`, `vsphere`, `proxmox`) | Elastic providers: `acquire` boots a host, a machine that runs `js-agent` and many jobs at once, and that agent registers itself. | Yes: boots and terminates. |

`default` is not a machine provider: it is the platform kind, a `[[platform]]` that names machine providers in priority order. The
operator-listed hosts-file pool (the old `static` provider kind with a `hosts_file`) is refused at start with a migration message
(`crates/js-lease-server/src/file/agentless.rs`, komira-ai/jobscheduler#840); its test stand-in is `js-pool-fake`.

Provider crates in code today:

| Crate | Kind | State |
|---|---|---|
| `js-provider-lambdalabs` | Lambda Labs | Implements `CapacityProvider`: catalog, acquire, list, release. |
| `js-provider-ec2` | AWS EC2 | Implements `CapacityProvider` (`kind = "ec2"`, `crates/js-provider-ec2/src/provider.rs`): launch with `RunInstances` for one instance from the image of the offer's architecture, tagged with the scheduler, the acquire and the host name and carrying a client token derived from the scheduler and the acquire, so a repeat of one acquire is the same launch; for a Capacity Block offer, buy the block (`PurchaseCapacityBlock`, found again by its tags before a second purchase) and launch into it; list, release (terminate only an instance that is ours, then read it back), observe and verify (the signed instance identity document, checked against AWS's key, the instance, the region and the image). The catalog, `Ec2Catalog::catalog`, reads instance types, zone offerings, on-demand, spot and Capacity Block prices and vCPU quotas and translates them into offers in code. The port is `Ec2Api` with an in-memory fake (`FakeEc2`); the client is SigV4-signed HTTPS (`HttpEc2`) over the provider-standard `AWS_*` variables or the instance role. A pool's provider is built disabled and in dry-run unless the pool's `launch` is `live`. Launch landed in komira-ai/jobscheduler#727, Capacity Blocks in komira-ai/jobscheduler#767. |
| `js-provider-gce` | GCP Compute Engine | Implements `CapacityProvider` (`kind = "gce"`): launch from the image of the offer's architecture with the payload in the metadata item `user-data`, idempotent on the acquire id, list, release and verify. The catalog, `GceCatalog::catalog`, reads machine types (with their built-in accelerators and architecture), the accelerator types each zone offers to attach to N1, Cloud Billing SKU prices and region quotas through a read-only port (`GceApi`, with an in-memory fake) and translates them into offers in code, for the provisioning models (`on-demand`, `spot`) each series is sold on. The client is HTTPS over the application-default credentials. |

A pool that names its provider directly (`provider = { kind = "lambdalabs", ... }`) is read as three things the loader makes, each
named with a leading `+`: a machine provider from the table, the `static` machine provider, and a `default` platform that names the two,
`static` first (`crates/js-lease-server/src/file/platform.rs`). A host joins a pool by its host certificate (registration.md); a
`join_token_file` is accepted and ignored with a warning.

## The registered hosts (`static`)

**What it is.** The set of hosts that registered themselves with the scheduler: the registry's host table (`js-registry`) behind the
same `CapacityProvider` trait as every other kind (5.5), with these fixed answers (`crates/js-registry/src/capacity.rs`):

- `acquire` is refused with `capacity` for every request and changes nothing: it has nothing to launch, and the caller falls through to
  its next provider.
- `release` terminates nothing (it answers that the machine is not one it launched). A host leaves the table by the operator's retire,
  release or removal, or by the lost-host retention. The scaler never releases an idle registered host: idle release (5.10) is for
  machines the scheduler launched.
- `observe` is the host table: Present while the host is live, Unknown while it is in the table and silent, Absent only once the table
  no longer holds it (5.6). A host its placer holds work on is never forgotten.
- Identity and admission are those of registration.md (join credential, pool exists, tainted set, shape satisfied, `max_machines`).

**Capable host.** A host of the pool is *capable* of a request when it could serve it once it is free: it is in the table (a tainted host is capable: its taint ends, and placement skips it meanwhile),
not draining and not `releasing`; its total shape (not its free room) holds the request's booking (a `BookAll` or `Whole` class needs the
host to exist; a `Book` class needs the total to hold the booking); and its latest facts satisfy every constraint of the request (5.13).
Busy hosts are capable. Lost hosts (silent past the grace, inside the retention window) are capable: the agent registers again with the
same id and the work waits for it, which is the same stance as 5.6 (silence is Unknown, not Absent). Cordoned and quarantined-by-pressure
hosts are capable: both end without anyone replacing the machine. Tainted hosts are capable too: an agent clears a residue or unchecked
taint itself after clean re-checks, and the operator's `ClearHostTaint` clears any taint (taint-self-clearing.md), so a request waits for
the clear as it waits behind a cordon; a taint that needs a reimage (a host job's) makes that wait as long as the reimage. Draining hosts are not capable for a new request, because they are leaving. A host *reserved* for a whole-node request (5.17) is not
draining in this sense: it is not leaving, it stays capable, and it is simply not available to a second reservation or to new work until
the reservation ends. For a whole-node request, "could ever serve" also needs the pool to offer the class (whole-node.md).

**Admission (D35, settled).** The request's pool reaches its machine providers in order: the order the pool lists its `platforms`, and
each platform its `machine_providers` (jobscheduler-design.md, 5.6).

1. If some host of the pool is capable, the request is accepted and queued; it is placed when a capable host is free (6.1).
2. Otherwise, if a launching machine provider of the pool *could launch* a machine that meets the request, the request is accepted and
   its demand is served by an acquire (below). *Could launch* means the provider is enabled, its catalog has an offer that satisfies the
   request inside the pool's `provider_limits` for it (`allowed_instance_types`, `max_price_cents_per_hour`, `max_machines`), and the
   pool's `max_machines` and the provider's `max_machines` share leave room. A catalog that cannot be read counts as *may offer*.
3. Otherwise the request is refused at once with the plain constant `no-capable-host`. It is never queued: no registered host, now or
   after any bounded wait, could run it, and the caller can act (register a host, change the request, choose another pool).

A request that names a pool or selectors that match no pool is refused at selection, as today, with the existing code. The refusal
covers the request's constraints, its shape and its class together: a GPU request (D32) in a pool without a registered GPU host and with
no offer that has the GPU is refused, not queued.

*Considered and rejected (D35).* Refusing whenever no host is free now: a busy fleet then looks like a broken one, and every caller would have to build the queue the scheduler exists to provide.

**What happens to queued requests when capability goes.** For a whole-node pool, a waiting request whose last capable host went away
(it deregistered or aged out; a tainted last host is still capable) keeps its place for the pool's `path_grace` (default 15 s; the
registry's `regain_grace`), so a host that restarts and registers again within moments takes it; when the grace passes the request is
refused `no-capable-host`, the same constant as a refusal at submit (`crates/js-lease-server/src/file.rs`, `registry_pool`). A queued job
of the job tables is checked against capability only at submit; once queued it waits, and ends as failed to start with `no-capacity`
when the pool's `max_queue_wait` (default one hour) passes. Leases already held on a host that is then tainted or evicted follow 5.6 and
5.12 unchanged.

## Elastic providers

An elastic provider is a `CapacityProvider` that can launch (5.5). It launches **hosts**: a machine is sized to hold many jobs at once, and
a launch answers the packed demand of the queue, never one job (hosts.md, section 3). The scheduler's single interaction is:

1. **Acquire.** The scheduler records the intent (acquire id, pool, entry, shape) durably, then calls `acquire`. The provider boots a
   machine from the image its configuration names; the image carries `js-agent`, the scheduler's address and identity to pin, and the pool
   name. The image holds **no secret** (K6). Every launch today, whatever the kind, carries a launch payload (`user_data`, or the `user-data` metadata item on
   Compute Engine) that, by the design of pools-providers-network.md (7.2 and 9),
   held a one-time join token valid for minutes, and a network key where the pool's network mode needs one (the join token and its host credential
   are removed by G3: registration.md, Host certificate; nothing replaces the token until G9). The reason is requirement 8 of pools-providers-network.md: the alternative, a later push over
   ssh, needs an inbound port open for the instance's whole life.
   The scaler no longer mints a token for an acquire.
2. **The machine registers itself.** The agent says hello (with a certificate, from G9 for a launched machine), and the first hello that matches it completes
   the acquire: the instance leaves `registering` and is a host of the pool. Each provider also implements `verify_instance`, the check
   that matches a hello to the launch it came from (`ec2` and `gce` check the cloud's signed instance identity when the hello carries
   one, registration.md, Aws instances); the server does not call it yet, so the signed identity does not replace the host certificate today.
3. **The provider's job ends there**, at boot plus registration, or at failure: a refusal, a boot that never says hello within
   `boot_timeout` (released, reaped, counted by the failure brake of 5.8), or a hello that does not match.
4. **A registered elastic host is an ordinary host of the pool.** Placement, reuse, caps, taint, leases and fencing treat it exactly as a
   registered host of the `static` provider. The only difference is that it carries an instance id, so the scheduler may release it: the idle rule of 5.10
   terminates it (`release`, then read-back), and a taint replaces it (5.12).

### Reading why a provider launches nothing

`ListProviders` (`GET /v1/providers`, operator only) tells, for the provider of each elastic pool, what the service holds that keeps a
queued job waiting, besides its checks and machines. Each field is read when the call is answered from the part of the server that owns it;
none is a copy kept for the call. A field that is empty or absent means the service holds nothing of that kind.

| Field | What it says | Read from |
|---|---|---|
| `catalog` | when the catalog in use was fetched (`fetched_at_ms`) and how many offers it holds (`offer_count`); absent before the first fetch. A failed fetch keeps the last good catalog, so the time does not move | the catalog cache |
| `passed_over` | each instance type not asked for in a region after a `quota` refusal, until `until_ms` (`quota_backoff` after the refusal), for every waiting job | pass 2's failover memory |
| `declined` | each offer refused for `capacity` in the attempt in progress of a waiting request, with the request and when. An attempt that ran out of offers drops its list, and its next one asks every offer again | pass 2's failover memory |
| `brake` | the failure brake of the pool: the failures counted in a row, and `engaged` with `engaged_at_ms` once the count tripped the pool. Only `POST /v1/pools/{pool}/brake/reset` disengages it | the scaler's records |
| `cooldown_until_ms`, `cooldown_reason` | the pool's cool-down while it runs, and why (`boot_failed`, `denied`, `shape`, or `quota` from a records file of an earlier release) | the scaler's records |
| `launching` | each acquire whose call to the provider is out, with the machine's name, the offer, `started_at_ms` and `age_ms` at the time of the answer | the scaler, while the call is out |

The scaler's records and its failover memory are read as the last finished tick left them; the launches out are read live, so a call that
hangs shows with a growing age while its tick waits. Nothing here changes how a provider is chosen; it only shows what the choice read.

## The order of machine providers

**Configuration.** A pool reaches machine providers through platforms: `[[machine_provider]]` tables (a `name` and a `kind`, plus the
kind's own keys), `[[platform]]` tables of kind `default` whose `machine_providers` lists them first to last, and the pool's `platforms`,
first to last. A pool's per-provider caps and limits are its `[pool.provider_limits.<name>]` tables. The single `provider` key is the
shorthand above: it makes a `default` platform of the `static` provider followed by the launching one, so registered hosts are preferred
and the elastic provider tops up; it is refused beside `platforms`. A `join_token_file` is accepted and ignored with a warning: a host is admitted by its host certificate. The keys and their checks are in `crates/js-lease-server/src/lib.rs` and jobscheduler-design.md, section 4.2
(komira-ai/jobscheduler#678).

```toml
[[machine_provider]]
name = "registered"
kind = "static"                             # the hosts that register themselves

[[machine_provider]]
name   = "cloud"
kind   = "ec2"
region = "<region>"
images = { x86_64 = "<image-id>" }

[[platform]]
name              = "ours"
kind              = "default"
machine_providers = ["registered", "cloud"] # where capacity comes from, first to last

[[pool]]
name            = "linux-build"
class           = "burst-host"
platforms       = ["ours"]
max_machines    = 24                        # the pool's total, every provider counted (5.11)
join_token_file = "<path-to-token-file>"      # accepted and ignored
# max_spend_per_hour, max_spend_per_day, idle, max_age, min_host_lifetime and the other keys an elastic pool must set
# (crates/js-lease-server/src/lib.rs), and server.max_lease_wall

[pool.provider_limits.cloud]
max_machines = 8                            # a share of the total; a provider without one is bounded by the total alone
```

**Placement over the order.** For a queued request, each tick, in two passes (jobscheduler-design.md, 5.6, which holds the rules in
full):

1. *Use what exists.* Among the pool's capable hosts that are free now and serving, take the first in machine-provider order (within one
   provider the engine's score decides). A free machine of a later provider is used before anything is launched: launching while a
   machine sits idle is waste.
2. *Launch only if nothing can serve now.* If no free capable host exists in any provider and no machine already booting can serve the
   request, walk the launching providers in order. The first that has an offer that satisfies the request inside the pool's limits, is
   enabled, not cooling down and not braked, and has room under its `max_machines` share and the pool's, gets the acquire (the scaler
   makes at most `max_step` launches a step, 5.8). A `capacity` refusal falls through to the provider's other offers and then to the next
   provider.

**"Cannot satisfy now"** for a provider means exactly: it has no free capable host, and either it cannot launch (the `static` kind) or no
acquire is possible (it is at its share of `max_machines` or at the pool's, disabled, cooling down, braked, or its provider is
unreachable). A busy capable host does not satisfy a request now. So with registered hosts all busy and `cloud` under its cap, a request
goes to `cloud`; with `cloud` at its cap too, it waits in the queue (8.2) for whichever frees first. A pool whose platforms reach no
launching provider is refused at start (`crates/js-lease-server/src/file/platform.rs`); a pool of registered hosts alone is a
`kind = "joined"` pool, which waits for a registered host.

**Caps.** `max_machines` is the pool's: it counts every machine in every state, every provider, including in-flight acquires and tainted
hosts (5.11). `provider_limits.<name>.max_machines` bounds the machines of one provider within the pool; one above the pool's is refused
at start (`crates/js-lease-server/src/file/limits.rs`). A registered host that says hello when the pool is full is refused with
`pool-full`, so set the total with the registered hosts in view. Only the scheduler's own launches are ever refused for the cap; there is
no minimum anywhere, and no machine is kept warm.

**A host frees while a machine is booting.** The queued request that was waiting on an acquire is placed on the host that freed, first
in machine-provider order (pass 1), and its `acquiring` state ends. The booting machine is **kept**: its acquire was recorded before the
call, it is paid for by then, and it is simply a machine of the pool without work. When it registers it is an ordinary host and the idle
rule (5.10) releases it once it has been idle for the pool's `idle` and has lived `min_host_lifetime` (both required of a pool of
the built launching kinds, with `max_age`). Releasing it mid-boot would add a terminate that can read back Unknown, would discard minutes already spent, and
would re-launch at the next burst after a cool-down; keeping it bounds the waste to one boot plus one idle period, and any request queued
meanwhile uses it. A boot that fails is handled by the usual boot-failure rule.

**Tainted and lost hosts.** A tainted host is replaced by an acquire only if it was an elastic host (5.12); a tainted registered host is
not placed on, but stays in the capable set at admission, and the launching providers (if any) serve the demand. Demand is computed over
the whole pool, not per provider.

**Several clouds, tiers, spot hosts.** `ec2`, `gce`, `lambdalabs` and later kinds are machine providers of this one order; a spot host's
reclaim notice taints, drains and reschedules through the paths above. Rules, configuration and tests: [hosts.md](hosts.md), sections 6
to 8 (D59 to D61).

## Superseded (2026-11-24): the provider hierarchy of the amendment

The amendment this file first held configured a pool with `providers` (an ordered list of provider names), `provider_max_machines` (a
table of per-provider caps) and a `regain_grace` of 5 minutes, named the registered hosts the `default` provider (a `DefaultProvider`
with a deny file), and kept `static` as an operator-listed pool for tests. The code replaced that configuration with
`[[machine_provider]]`, `[[platform]]`, a pool's `platforms` and `provider_limits` (komira-ai/jobscheduler#678), made `static` the
kind of the hosts that register and refused the operator-listed pool (komira-ai/jobscheduler#840), and spells the grace `path_grace`
(default 15 s). The rules of the amendment that survived are stated above in the code's terms; the rest is in this file's git history.

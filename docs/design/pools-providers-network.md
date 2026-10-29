# DESIGN: pools, providers and host networking

Status: draft for review. Written against `origin/main` of komira-ai/jobscheduler at
73132d5f7 (includes #186, #195 bundled agent roots, #196 token revoke on terminate).
Every claim about existing code names its file. A claim marked PROPOSAL is a choice the
requirements do not make; each one is listed under Open questions (section 14).

## 1. Summary

Today the server leases whole hosts. Hosts come from two places: a static file
(`--pool-config`, `crates/js-pool-static`) and agents that register themselves into one
pool (`--agent-pool-config`, `--agent-token-file`,
`crates/js-lease-server/src/agent_pool.rs`). A Lambda provider exists
(`crates/js-provider-lambdalabs`) that can launch and terminate instances, but nothing
calls it: its docs say "The provider is not wired to a scheduler"
(`crates/js-provider-lambdalabs/src/lib.rs`). One-time join tokens can be minted and
revoked (`crates/js-lease-server/src/join_tokens.rs`, `join_api.rs`). There is no scaler,
no network layer, no pool config file, and no status page.

The design adds, in this order:

1. `js-elastic`: the provider-neutral version of the Lambda `Elastic` trait (acquire, list,
   release) plus `Offer`, `Demand`, `Bootstrap` and `Secret`. The lease-level
   `js_provider::Provider` trait does not change.
2. In `js-types`: the job types (`ContainerSpec`, `Requirements`) of section 3.1.
3. A launch payload (`user_data` for Lambda) and a no-inbound firewall attach in the Lambda crate,
   and a bundled-roots HTTPS client so a pool needs no CA file. The service renders the payload;
   the provider only delivers it.
4. A `network` module in `js-lease-server`: ONE service-level layer with modes `default` and
   `tailscale`. A mode adds boot steps and a per-launch secret to the boot plan; no provider sees
   it. It becomes its own crate only when a mode needs a dependency the server should not carry.
5. A TOML config file (`--config`), a compiled-in kind registry, and start-up validation
   that names pool and field.
6. A per-pool background check loop, and a status API and page, served only on the private address.
7. Registry changes: a read of waiting demand, a drain mark, a remove, a `Launchable`
   hook, and a pool that needs no shared join-token file.
8. `js-scaler`: a pure step function with an injected clock, a durable record of every
   acquire written before the call, and a shell in `js-lease-server` on the existing tick.
   Two new crates in all (`js-elastic`, `js-scaler`).
9. The bootstrap renderer (service side), the Lambda host-prep data, and an end-to-end test with fakes.
10. Server flags move into `[server]`.

The first slice keeps what the requirements and the double-launch, spend and secret
requirements need: record before acquire, caps, drain, revoke, a masked plan, and a separate
agent port. Price-drift marking, offer suppression with backoff, the boot-failure brake,
`zeroize`, `RLIMIT_CORE`, a panic hook, a loop-device state root as a default, a separate
network crate and the orphan adoption machine beyond "count and report" are under "Later" in
section 12.

The first slice serves one elastic pool whose class books a whole host (`whole-node` or
`gpu-host`). That is what the consumer sends today and it keeps one waiting request equal to one
machine. Several elastic pools in one server, and classes that book part of a host, are
later work, refused at load with a message that says so (the way `agent_pool.rs` refuses a
mixed pool today, `MIXED_UNSERVED`). Section 6.5 says what that costs and how it is lifted.

The answer to "do we build a service image with secrets in it" is no. The image is
generic. Secrets are files mounted at run time and named by path in the config (section 9).

### 1.1 Where each requirement is followed

| requirement | followed in |
|---|---|
| 1 job carries a container; never a cloud, AMI or type | 3.1, 4.3 |
| 2 pools carry the host; Lambda stock image plus built-in template | 3.2, 4.2, 5.3, 7.3 |
| 3 host contract, reported values, never provider names | 3.6, 7.3, 6.2 (Booting) |
| 4 macOS native under sandbox-exec; pool declares mode | 3.1, 3.6, 4.2 (`exec`) |
| 5 agents dial out; no Tailscale or host dependency in the core | 3.4, 8.6 |
| 6 one network layer, modes `default` and `tailscale` | 3.4, 5.4, 4.2 |
| 7 Tailscale OAuth client, per-launch key, ACL on tag | 8 |
| 8 Lambda launch: user_data secrets, one ssh key, no-inbound ruleset, token binding | 5.3, 7 |
| 9 config file only; refuse bad config; background checks; status | 4.4, 10 |
| 10 about ten lines, nothing hand-made | 4.1 |
| 11 no secret in an image | 9 |
| 12 compiled-in kind registry | 5.1 |
| 13 pools state offers, derived from a catalog | 3.5, 4.5 |
| 14 scaler: registered first, pending, offers, caps, idempotent acquire, drain, release, revoke | 6 |
| 15 operator go for first launch; hard spend cap | 4.1, 6.2, 9, 13 |

Deviations a reviewer should see (each is also under Open questions 20):

- Requirement 1 and 4 (lifted by PR 11): a job carries disk and may require an execution mode.
  The lease request carries them as the capabilities `disk-gib` and `exec` (3.1), beside `os`
  and `arch`, and `Offer::fit` refuses an offer whose os, arch, mode or disk differs from or
  falls short of the request's, naming the dimension.
- Requirement 14 says "elastic pools" (plural). The first slice serves one registering pool because
  `Registry::new` takes one `PoolConfig` (`crates/js-registry/src/registry.rs`). `js-scaler`
  takes a list of pools; the registry is the limit (6.5, PR 12).
- Requirement 13: the gpu class comes from a `types` row the operator writes, not the catalog (3.5),
  and the "about ten lines" of requirement 10 depend on that row (4.1).
- Requirement 8: "single-use" holds for registration only until the Welcome credential exists (7.2).
  The credential is in the first slice, before any live launch.
- Requirements 6 and 9: modes are `default` and `tailscale` only; there is no status token, no new
  client key and no public listener (3.4, 10.2).

## 2. Goals and non-goals

Goals: the fifteen requirements (table 1.1). Non-goals for the first slice:

- AWS, GCP, Azure and k8s providers (the registry has room; none is written), and an `exec` kind
  for out-of-tree providers (later work, requirement 12).
- macOS elastic pools. macOS pools are static or agent-registered; the execution mode is in the
  offer, demand, pool and hello so the seam exists (requirement 4).
- Several elastic pools in one server, and part-host classes (6.5).
- A job queue of our own (demand is the registry's wait list), a user-written bootstrap script
  (requirement 10), and spot or preemption policy beyond treating a preempted instance as lost.
- Any new in-service client or status authentication (section 12). Network code in any provider:
  there is none, by design (3.4, 5.4).

## 3. The model

### 3.1 Jobs (requirements 1 and 4)

A job needs a container (image, command), resources and a platform. Today these sit in two types:

- Run start: `RunBody` (`crates/js-lease-server/src/run_api.rs`) and the wire command
  (`crates/js-agent-wire/src/command.rs`) carry the image (`name[:tag][@digest]`, grammar in
  `crates/js-agent-wire/src/image.rs`), arguments and limits.
- Lease request: `js_types::Request` (`crates/js-types/src/request.rs`) carries `Resources` (cpu,
  memory, gpus, gpu class) and required `Capabilities`; `Shape::satisfies` in
  `crates/js-types/src/shape.rs` is the match. Platform is already a capability: the agent detects
  `os` and `arch` and they "cannot be set" by flag (`crates/js-agent/src/cli.rs`, `--capability`).

PR 1 adds `crates/js-types/src/job.rs`, naming the contract once:

```rust
pub struct ContainerSpec { pub image: ImageDigestRef, pub command: Vec<String> }  // digest in the type
pub struct Requirements {
    pub resources: Resources,            // existing: cpu, memory, gpus, gpu class
    pub disk_bytes: Option<u64>,         // new
    pub os: Option<String>, pub arch: Option<String>,      // "linux"/"macos", "x86_64"/"aarch64"
    pub exec: Option<ExecMode>,          // Container | SandboxExec; None = either
}
impl Requirements { pub fn from_request(request: &Request) -> Self; }  // os, arch, disk (disk-gib),
                                                                       // exec from the required caps
```

`js-elastic::Demand` is an alias of `Requirements`, so the scaler and the matcher read one
definition. `ContainerSpec` maps onto the existing run body: `RunBody` gains no field in the
first slice, its `image` is parsed into `ImageDigestRef` at the edge, and a digest-less image is
refused at run start with `image-digest-required` (422), for every pool: agents pull what a host
lacks, so a tag could change under a fleet. This is unconditional and not the `require_digest` pool
setting of 14 Q16; the agent refuses a tag-only start as well.

A lease request carries `Resources` plus `Capabilities`, and `Requirements::from_request` reads
the rest from the capabilities it requires. `os` and `arch` are read from `os` and `arch` (PR 1),
`disk_bytes` from `disk-gib` (whole GiB, at least the amount; `0` asks for nothing) and `exec` from
`exec` (`container` or `sandbox-exec`); a request that names a key it does not need leaves
the field `None`, which is "any". A `disk-gib` that is not a whole number of GiB, or an `exec`
that names no mode, is a 400 on `POST /v1/leases` and is refused by `Request::validate`. `Offer::fit`
refuses an offer whose os or arch differs, whose mode differs, or whose disk is below the
request's, naming `Dimension::Os`, `Arch`, `Exec` or `DiskBytes` (PR 11 lifted the earlier
deviation from requirements 1 and 4). A job never contains an AMI, a cloud name or an instance
type; 4.3 shows where those live.

### 3.2 Pools (requirement 2)

A pool is one `[[pool]]` table: a name, a class, a cap, a provider, a network, an idle
time, an execution mode. Today the server has exactly one registering pool: `Registry::new`
takes one `PoolConfig` (`crates/js-registry/src/registry.rs`, built in
`crates/js-lease-server/src/agents.rs`). The first slice keeps that: at most one pool whose
provider is elastic or `agent`, plus any number of `static` pools.

This replaces the provider hierarchy of `docs/design/providers.md` (`providers = [...]` per
pool, `provider_max_machines`) for the first slice. A pool has one provider. Choosing among
pools is the consumer's job through labels, as `docs/design/configuration.md` says for the consumer.
See Open questions 7.

The pool carries the host. For Lambda that is the provider table (instance types, regions)
and nothing about an image, since Lambda has none; the stock image plus the template of 7.3
is the host. For AWS (later) the pool's provider table would carry `ami` or
`launch_template`; that is the only place an AMI appears anywhere.

### 3.3 Providers

Three things are called a provider in the code and must not be merged:

| name | file | calls | who calls it |
|---|---|---|---|
| lease-level `Provider` | `crates/js-provider/src/lib.rs` | `start`, `stop`, `observe`, `capacity` per lease, synchronous | the lease table, under its lock |
| `CapacitySource` | `crates/js-lease-server/src/capacity.rs` | builds a `Provider` once at start | `Config` |
| `Elastic` | `crates/js-provider-lambdalabs/src/elastic.rs` | `acquire`, `list`, `release` of machines | nothing yet |

An elastic provider does not implement `js_provider::Provider`. It launches machines; those
machines register through the agent protocol and become hosts of the pool, which the
registry already serves as a `Provider` (`crates/js-registry/src/provider.rs`). The lease
table never waits on a cloud call. The Lambda crate says the same ("It does not implement
the delivered whole-host `Provider` trait, which is called under the lease table's lock",
`lib.rs`). The scaler calls the elastic trait from its own task with no lock held.

### 3.4 Network (requirements 5, 6)

How an agent on a new host reaches the scheduler is decided by one service-level layer: the
pool's network mode. The agent always dials out (`--server`, `crates/js-agent/src/cli.rs`). The
scheduler has no public listener in this design. It listens on a private address (same LAN, a
VPC, or a tailnet). Agent routes are served on their own port (`server.agent_listen`, required
when an elastic pool exists) and `advertise_url` names that port. The client, operator and
status routes are served only on `server.listen`. Who may reach each port is decided by the
network around the service (tailnet ACLs, or a proxy in front), not by a new key or token inside
it (9): the ACL gives a launched host the agent port and nothing else. The server cannot check
that its bind address is private (a wildcard bind is normal in a VPC or a container); it logs a
warning and shows it in status when `listen` or `agent_listen` is a wildcard, and keeping the
network private is the operator's part. Modes:

- `default`: the host uses the network it already has: the same LAN, a VPC, or a VPN the image
  joins on its own. The mode adds nothing: no boot step, no secret. Agents trust public roots
  with no file (`--ca-cert` is optional since #195, `crates/js-agent/src/lib.rs`; only the help
  comment in `cli.rs` is stale and is fixed in PR 10).
- `tailscale`: the host joins a tailnet first with a key minted for that launch;
  `advertise_url` is then a tailnet name.

A mode contributes two things to a launch's `BootPlan`: boot steps, and a per-launch secret
(5.4). The service-side layer also has two lifecycle hooks, `check` (can this mode issue what a
launch needs) and `forget` (cleanup after release). They belong to the service, are not
contributions to the plan, and never reach a provider. A provider receives one opaque boot payload
rendered from the plan (for Lambda, `user_data`) and never branches on the mode. No provider crate
contains network code, and a new provider gets every mode without writing any. Nothing in the
core imports a mode (8.6).

### 3.5 Offers (requirement 13)

An offer is what one launchable thing would give: the provider's id for it, the zone, cpu,
memory, disk, gpu count and class, os, arch, execution mode, and a price. The scaler decides
whether an offer fits a job before launching by comparing numbers. The provider fills
offers from its catalog (`LambdaApi::instance_types`,
`crates/js-provider-lambdalabs/src/api.rs`, which returns vcpus, memory, storage, gpus,
architecture, price and regions with capacity now, `src/model.rs`).

The pool is catalog-derived except for one value. The Lambda crate says the GPU is "free
text. Do not parse it: the GPU class comes from the agent" (`model.rs`,
`InstanceTypeOffer::gpu_description`). So the gpu class of a type is a row the operator
writes (`types = [{ name, gpu, gpus }]`, today `gpu_class` in `options.rs`). This is a
named narrowing of requirement 13. Two guards keep it honest:

- A row that disagrees with the catalog on gpu count is skipped with a log line, as
  `select.rs` does.
- The first hello is compared with the offer (6.2, Booting): a host that reports a
  different class or less than its offer starts the boot-failure cooldown, and is drained
  and then released once it is quiet.

If Lambda's catalog ever gives a machine-readable class, the `gpu` field becomes optional
and the rest of the design is unchanged.

### 3.6 Host contract (requirements 3, 4)

Any host image, pre-baked or bootstrapped, must end up running the agent and a container
runtime (macOS: a sandbox-exec profile instead), and for a GPU pool the NVIDIA driver and
container toolkit. The scheduler never trusts the pool's word or the provider's name. The
agent reports in its hello, as capabilities declared in `crates/js-caps`:

- `runtime` (text): `container` or `sandbox-exec`, set only if the binary the agent will run
  jobs with was found and ran (`--container-bin` in `crates/js-agent/src/cli.rs`).
- `gpu-toolkit` (flag): set only if the toolkit hook works, reported only when gpus > 0.
- `exec` (text): the mode the agent runs jobs in. PR 6 reports it; PR 11 lets a job require it.
- `disk-gib` (whole number): the free space of the volume that holds `--scratch-root`, where
  every run gets its directory, in GiB rounded down, read once when the hello is made with
  `df -P -k` (the same on Linux and macOS). An agent with no `--scratch-root`, or whose volume
  cannot be read (a warning says why), reports none, and a job that requires disk is not
  placed on it. Like the others, an operator cannot set it.

A pool declares `exec = "container"` (default) or `"sandbox-exec"`. The pool floor (4.5)
requires `runtime` to equal the pool's `exec`, and for a pool with gpus, `gpu-toolkit`.
Admission refuses with two new codes beside `SHAPE_NOT_ACCEPTED`
(`crates/js-agent-wire/src/refusal.rs`): `host-contract` (runtime or toolkit missing) and
`execution-mismatch` (reported mode differs from the pool's). Both name the field. A job that
requires a mode (`Requirements.exec`) is matched against the reported value, never against
the pool's name or the cloud: admission holds each host to the mode it reports, and a pool that
declares `exec` books its hosts with that mode among their capabilities, so a job that requires
`sandbox-exec` is never placed on a container host. A pool that declares no mode books none,
and a job that requires one finds no host there unless the pool's `[capabilities]` name `exec`
(the join then compares each host's reported value with it). `disk-gib` is a minimum everywhere
(`Shape::satisfies`, `js_caps::Request`): a pool that lists `disk-gib` in its `[capabilities]`
admits only hosts that report at least that much, and serves jobs that require up to it.

Two limits, stated plainly:

- Registry pools place jobs by the pool's booked shape. A host's reported `disk-gib` is only an
  admission floor against the pool's promise, and never places a job by itself. A job with
  `disk-gib` on a pool whose `[capabilities]` omit `disk-gib` gets no fit.
- Disk is read once, at hello, so it goes stale afterwards. A host that fills up keeps its
  booked promise. Re-reading it periodically or per heartbeat is tracked in
  komira-ai/jobscheduler#328.

## 4. Configuration (requirement 9, 10)

One flag: `--config <path>`. The file has a `[server]` table and `[[pool]]` tables. Every
table refuses unknown fields. Paths of secrets are paths; the file holds no secret, and
because every table denies unknown fields, a secret written inline has no key to go in.

### 4.1 A user's pool (about ten lines)

```toml
[[pool]]
name = "gpu"
class = "whole-node"
max_instances = 4
max_spend_per_hour = "12.00"      # required for every elastic pool, no default
max_spend_per_day  = "96.00"      # required too: the hard total bound (6.2 Caps)
idle = "10m"
provider = { kind = "lambdalabs", api_key_file = "/run/credentials/js/lambda-key",
             types = [{ name = "gpu_1x_a10", gpu = "nvidia-a10", gpus = 1 }] }
network  = { kind = "tailscale", oauth_client_file = "/run/credentials/js/ts-oauth",
             tag = "tag:js-agent" }
agent    = { x86_64 = { url = "https://pkgs.example.org/js-agent-x86_64", sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef" } }
```

That is the whole pool (twelve lines with the two spend keys; `agent` is needed once the pool is
`live`, and `plan` leaves it out). Not in it: the bootstrap, join tokens, Tailscale keys, the
firewall, the CA file. The service makes them. The agent download is the operator's pin, one
entry per CPU architecture the pool's offers have (7.3).
Two things the ten lines depend on, stated plainly:

- The `types` row names the gpu class by hand. Lambda's catalog gives the GPU as free text (3.5),
  so requirement 13's "derivable from a catalog" is met for cpu, memory, disk, gpu count and price
  and not for the class.
- Lambda's API requires exactly one ssh key name at launch, and the key is unused (requirement 8).
  An account with no key, or with several and no `ssh_key` line, makes the pool unavailable
  until a person acts. That is a hand-made input, against requirement 10. 5.3 and Open questions 3
  ask the maintainers to let the service create one key; until then the example above needs a key
  in the account, and the account's only key is used.

`max_spend_per_hour` is a rate. `max_spend_per_day` is the bound on total spend in a day
(6.2 Caps). Both are required in the first slice.

A new elastic pool is in `launch = "plan"` with no line saying so: it checks, shows its
plan, and launches nothing. Adding `launch = "live"` is the line that spends money, and it
is the operator's go for the first launch (requirement 15). It is a config edit and a restart, so it
is one reviewable diff. The example above therefore spends nothing as written. PROPOSAL
that `plan` is the default (14 Q14).

### 4.2 A full service file

```toml
[server]
listen         = "100.64.0.1:7300"                       # client, operator, status routes; private or tailnet address
agent_listen   = "100.64.0.1:7301"                       # agent routes only; required with an elastic pool (Q23)
advertise_url  = "https://js.example.ts.net:7301"        # what agents dial: the agent port
state_file     = "/var/lib/jobscheduler/state"
boot           = "resume"                                # or "initialise"
client_token_file = "/run/credentials/js/client-token"   # the existing client token (today --token-file), same file and check; no new key or field
tls_cert       = "/run/credentials/js/tls-cert"
tls_key        = "/run/credentials/js/tls-key"
# tick, default_ttl, max_ttl, timeouts: same names and defaults as today's flags

[[pool]]
name     = "gpu-static"
class    = "gpu-host"
provider = { kind = "static", hosts_file = "/etc/jobscheduler/hosts.toml" }

[[pool]]                       # the 4.1 pool, with every optional key written out
name   = "gpu"
class  = "whole-node"
exec   = "container"          # default; "sandbox-exec" for a macOS pool
max_instances = 4
max_spend_per_hour = "12.00"
max_spend_per_day  = "96.00"
idle = "10m"
boot_timeout = "15m"          # default
launch = "plan"               # default; "live" spends money
provider = { kind = "lambdalabs", api_key_file = "/run/credentials/js/lambda-key",
             regions = ["us-west-1", "us-east-1"], ssh_key = "my-key",   # regions, ssh_key optional
             types = [{ name = "gpu_1x_a10", gpu = "nvidia-a10", gpus = 1 },
                      { name = "gpu_2x_a10", gpu = "nvidia-a10", gpus = 2 }] }
network  = { kind = "tailscale", oauth_client_file = "/run/credentials/js/ts-oauth", tag = "tag:js-agent" }
agent    = { x86_64  = { url = "https://pkgs.example.org/js-agent-x86_64",  sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef" },
             aarch64 = { url = "https://pkgs.example.org/js-agent-aarch64", sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef" } }
```

A pool whose hosts already have a route to the scheduler omits `network` (mode `default`). The
network mode is the same set for every provider: the pool names it in `network`, and no provider
table has a network key. A Lambda pool in `default` mode works only if the operator's own image or
VPN setup gives the host a route to `advertise_url` (section 7); Lambda pools use `tailscale`.

A future AWS pool would put `launch_template` and `ami = "ami-0abc"` inside its provider
table; that is the only place an AMI appears. Not built.

### 4.3 Where each noun lives

| thing | lives in |
|---|---|
| image digest, command, cpu, memory, disk, gpu count and type, os, arch, required mode | the job (`ContainerSpec`, `Requirements`; 3.1) |
| provider kind, launch settings, host image if the kind has one, instance types, regions | the pool's `provider` table |
| execution mode the pool's hosts run | the pool's `exec` |
| bootstrap, join token, network key, firewall | generated by the service per launch |
| pool floor (registry shape) | derived from `types` and `exec` (4.5) |

### 4.4 What start-up refuses

`Config::from_cli` in `crates/js-lease-server/src/config.rs` already refuses a bad flag and
exits 2 with `StartupError { flag, reason }` (`src/error.rs`). The new file reuses that exit
path. `StartupError.flag` is a `&'static str`; a pool and a field are runtime text, so PR 5
adds a second constructor that renders `pool "gpu": provider.api_key_file: <reason>` and
keeps `flag()` for old callers. Refused at start, offline, with no network call, each
message naming pool and field:

| refusal | example message |
|---|---|
| file missing or unreadable | `--config /etc/js.toml: cannot read: No such file` |
| unknown table or field anywhere | `pool "gpu": provider.api_keyfile: unknown field; known: api_key_file, regions, types, ssh_key, api_url` |
| unknown `provider.kind` or `network.kind` | `pool "gpu": provider.kind: "lambda" is not a kind; known: lambdalabs, static` |
| credential file missing, not regular, not owned by the service user, or group/other readable | `pool "gpu": provider.api_key_file: mode 0644, must be 0600 or 0400` (the `read_token` rule in `config.rs`) |
| type name not 1 to 64 lower case letters, digits or `_`; gpu class not in the agent's spelling (`GpuClass::parse`); a type twice; two gpu classes in one pool | `pool "gpu": provider.types[1].name: ...` |
| elastic pool without `max_instances`, `max_spend_per_hour` and `max_spend_per_day`; a spend value not a decimal with at most two places, or zero; a day cap below the hour cap | `pool "gpu": max_spend_per_day: required` |
| `max_instances` of 256 or more (the join-token table holds 256 waiting tokens, `MAX_WAITING` in `join_tokens.rs`) | `pool "gpu": max_instances: 300 exceeds 255` |
| `launch = "live"` on a pool whose provider cannot be built | `pool "gpu": launch: ...` |
| two pools with one name; more than one registering pool; a class that books part of a host (first slice) | `pool "b": only one pool may register agents in this version` |
| `exec` not `container` or `sandbox-exec`; `sandbox-exec` with an elastic kind that cannot launch macOS | `pool "mac": exec: ...` |
| an elastic pool and no `server.agent_listen`; `agent_listen` equal to `listen` | `server.agent_listen: required with an elastic pool; must differ from server.listen` |
| `advertise_url` that is `http://` and not loopback, whenever any elastic pool exists (the agent refuses it, `crates/js-agent/src/cli.rs`; a pool that passed start and then failed every boot would bill 15 minutes per launch with only "booting, no hello" to show) | `server.advertise_url: http:// is refused by agents; use https (or loopback)` |
| with `--config`, any other flag (PR 10) | `--listen: not allowed with --config; set server.listen` |

What cannot be checked offline is not checked at start: a real instance type name, a key
that works, a tailnet that exists. Those are the background checks of section 10. A bad
instance type name is checkable offline only when the provider ships a static list; Lambda
does not, so it is a catalog check (10.1), not a start refusal.

### 4.5 The pool floor

The registry takes one `Shape` per pool and refuses a hello whose shape does not satisfy it
(`docs/design/registration.md`, Admission). An elastic pool has many sizes. The floor is
built offline from the config: cpu 1000 millis, memory 1 GiB, `gpus` = the least `gpus` over
`types`, `gpu_class` = the one class, and the required capabilities of 3.6 (`runtime` equal
to `exec`, `gpu-toolkit` if gpus > 0). Real capacity is whatever the agent reports; the
floor only keeps a wrong machine out. PROPOSAL (requirements do not set the floor).

## 5. Provider plumbing (requirement 12)

### 5.1 The registry of kinds

A table compiled into `js-lease-server`, file `src/kinds.rs`:

```rust
pub type Ctor = fn(&toml::Table, &BuildCtx<'_>) -> Result<Built, KindError>;
pub enum Built { Static(Box<dyn CapacitySource>), Elastic(Box<dyn ElasticProvider>) }
pub struct BuildCtx<'a> { pub pool: &'a str, pub scheduler: &'a SchedulerId,
    pub clock: Arc<dyn Clock>, pub files: &'a dyn SecretFiles }  // files: owner-only reads; tests inject
pub struct KindError { pub field: String, pub reason: String }
impl Kinds {
    pub fn standard() -> Self;   // "static", "lambdalabs"
    pub fn build(&self, kind: &str, table: &toml::Table, ctx: &BuildCtx<'_>) -> Result<Built, KindError>;
}
```

The core reads `kind` and hands the rest of the table to the constructor; it never looks
inside. Each constructor owns a `#[serde(deny_unknown_fields)]` struct. Later kinds (`aws`,
`gcp`, `azure`, `k8s`) are one line each in `standard()`. A network mode table does the
same for `default` and `tailscale`; it is read by the launch shell, never by a provider.

Constructors live in the provider crates:

- `js_pool_static::kind::build` reads `hosts_file` and returns `Built::Static(...)` over
  today's `StaticPoolSource` (`capacity.rs`) and today's file format
  (`crates/js-pool-static/example.toml`).
- `js_provider_lambdalabs::kind::build` returns `Built::Elastic`. The crate doc says
  "Nothing here has a built-in address"; the constructor now holds the default API address
  and `api_url` overrides it, which the HTTP tests need (`crates/js-provider-lambdalabs/tests/http`).

### 5.2 The elastic trait (crate `js-elastic`)

PR 1 moves the provider-neutral parts of `elastic.rs`, `own.rs` and `config.rs` out of the
Lambda crate. The Lambda `Elastic` trait is nearly general already: acquire is idempotent on
an acquire id, list returns what is ours, release reads back and answers `Gone` only on a
not-found by id, and `Unknown` is never `Gone`. What is Lambda-shaped is `Candidate {
instance_type, region }`, `GpuRequest` and `InstanceId`; those become `Offer` and `HostRef`.
The semantics of `Acquired`, `Released`, `Refusal` and `Held` stay word for word, so the
crate's tests (`tests/offline`, `tests/http`, `tests/first_launch`) move with them.

```rust
// crates/js-elastic/src/lib.rs
/// Never in a log, Debug, status page. No Serialize, Clone or Display; Debug prints "***".
/// (Zeroing on drop, RLIMIT_CORE and a panic hook are Later, section 12: no requirement asks for them.)
pub struct Secret(String);

pub struct Offer {
    pub id: String,                  // the provider's name for it; opaque to the core
    pub zone: Option<String>,        // one Offer per zone with capacity
    pub cpu_millis: u64, pub memory_bytes: u64, pub disk_bytes: u64,
    pub gpus: u32, pub gpu_class: Option<GpuClass>,
    pub os: String, pub arch: String, pub exec: ExecMode,   // ExecMode: Container | SandboxExec
    pub price_cents_per_hour: Option<u64>, pub price_unit: PriceUnit,
    pub available: bool,             // capacity now, per the catalog
}
impl Offer { pub fn fit(&self, demand: &Demand) -> Result<(), Dimension>; }  // names the failing dimension

/// Chosen before launch, derived from scheduler id and acquire id; same `js-<hash>-`
/// prefix as the Lambda ownership marks, so `Owner::claim` works unchanged. The acquire id is
/// 128 random bits taken from the scaler's input (the shell draws them; the pure step has no randomness)
/// when the Launching record is made, and written in the record, so a name is never reused
/// (not after a restart, not after a lost state file; see 6.3).
pub struct HostName(String);
pub struct HostRef(pub String);      // the provider's own id for a machine

pub struct AcquireRequest { pub acquire: AcquireId, pub pool: String, pub name: HostName,
    pub offer: Offer, pub labels: Vec<(String, String)>, pub payload: BootPayload }
/// The rendered boot input, opaque to the provider (Lambda: `user_data`). A `Secret`.
pub struct BootPayload(Secret);

pub enum Acquired { Accepted { held: Held, extras: Vec<Extra> }, Refused(Refusal), Unknown,
    Deferred { wait: Duration } }                                  // unchanged; Deferred = nothing sent
/// `Deferred` means no call went out: the provider sent nothing to the cloud, so nothing can
/// exist for the acquire id. The one case where an earlier launch of the same acquire id
/// may have landed is an unresolved call (an earlier call got no answer). A provider in that
/// case may answer `Deferred` for the retry, and then reports the acquire id as unresolved
/// (`PoolInput::unresolved`, 6.1) until a call for it is answered.
pub enum Released { Gone, Terminating(HostStatus), Refused(Claim), Unknown }   // unchanged

pub trait ElasticProvider: Send + Sync {
    fn catalog(&self) -> Result<Vec<Offer>, ProviderError>;   // also the credential check
    fn host_prep(&self, offer: &Offer) -> Vec<BootStep>;      // what this image lacks (7.3); data, from the offer only
    fn payload_limit(&self) -> usize;                         // bytes the cloud accepts
    fn acquire(&self, request: &AcquireRequest) -> Acquired;  // idempotent on request.acquire; delivers request.payload as is
    fn list(&self) -> Result<Vec<Held>, ProviderError>;       // an error is never an empty list
    fn release(&self, id: &HostRef) -> Released;              // terminates if ours, reads back
}
// No network type appears in this trait. The plan view on the status page is the service's (below).
```

`BootPlan` is data with secret slots (not a string), built by the service, never by a provider:
the agent artifact (url, sha256), the scheduler url, pool, the instance line (`<pool>:<name>`),
the provider's `host_prep` steps, the network mode's steps, and slots named `join_token` and
`network.*` for secrets. `BootSecrets` fills the slots. The service has one function
`render(&BootPlan, Fill) -> String` where `Fill` is `Mask` or `Real(&BootSecrets)`; the `Real`
render becomes the `BootPayload`. The service alone owns the plan view: the status page shows the
`Mask` render of the `BootPlan` the launch follows (secret slots print `***`), so it cannot show a
secret because it never holds one. The provider has no `describe`: it only sees the payload. A provider never sees a
`BootPlan`, a network mode or `BootSecrets`: its trait takes the payload and delivers it in
whatever field its cloud has (Lambda: `user_data`). It does not branch on the network mode. A
test renders both fills and asserts they differ only at the secret bytes (13.2).

Ownership marks (`own.rs`: tags `jobscheduler:scheduler/acquire/pool` and the `js-<hash>-`
name prefix, both needed to terminate, either counted toward the cap) stay inside the
Lambda crate.

The scaler, not the provider, picks the offer. Today the Lambda crate picks
(`select.rs::candidates`: cheapest, then fewest gpus, then name, then region). That moves to
`js-scaler::choose`, so the next kind does not copy it. The Lambda crate keeps the
catalog-to-offer conversion and the `OptionTable` filter.

`ElasticConfig`'s `enabled` and `Mode::DryRun/Live` stay as a second lock under the pool's
`launch` setting: `launch = "live"` builds the provider with `.enable().live()`, `plan`
builds it without. A bug in the scaler that ignores `launch` still cannot launch from a
plan-mode provider.

### 5.3 What the Lambda crate changes (requirement 8)

All in `crates/js-provider-lambdalabs`:

- `model::LaunchRequest` (today: region, instance_type, ssh_key, name, tags) gains
  `user_data: Option<Secret>` and `firewall_ruleset_ids: Vec<String>`.
- `wire::LaunchBody` (today: `region_name`, `instance_type_name`, `ssh_key_names: [_; 1]`,
  `name`, `tags`) gains `user_data` and `firewall_rulesets`. The API's field names are
  checked against Lambda's current reference in PR 3 (Open questions 2).
- `Debug`, `Display`, `FakeLambda`'s `Call` log and every error `cut()` quote must not
  print `user_data`. A test greps captured logs, as the existing test does for the API key.
- The service checks the render against `payload_limit()` and refuses `Shape` before the call.
  The limit is not verified (Open questions 2); the render is measured against it in a unit test.
  The Lambda crate returns `host_prep` (the stock-image package table of 7.3) and delivers the
  payload as `user_data`; it holds no cloud-init template and no network code.
- `LambdaApi` gains `list_firewall_rulesets` and `create_firewall_ruleset` (no inbound
  rules). `ensure_firewall` finds a ruleset named `js-<scheduler>-noinbound` or creates it,
  in the region, and every launch attaches it; it reads the ruleset again before every launch
  (one list call), and a ruleset that holds a rule by then refuses the launch. Rulesets can only be
  attached at launch (requirement 8), so a launch with no ruleset is refused `Shape`, not sent.
- The ssh key. The API requires exactly one name (`ssh_key_names: [&str; 1]`, `wire.rs`)
  and `ElasticConfig::new` requires one with no default. The key is unused, because the
  ruleset has no inbound rule. First slice, with no new requirement: the pool uses
  `provider.ssh_key = "<name>"` if set; else the account's only key; else the check
  `ssh_key` fails and the pool is unavailable with `provider.ssh_key: account has 0 keys;
  name one` (or `has 3 keys; name one`). On a fresh account that is a hand-made step in the
  Lambda console, which requirement 10 says there should not be. So the design asks the maintainers
  (Open questions 3) for the alternative: the service creates one key named
  `js-<scheduler>` the first time the check finds the account has none, from a throwaway
  public key whose private half is never stored, and uses it. It writes to the cloud
  account, which is why it needs a word. RECOMMENDATION: yes, the key is inert (no inbound
  rule exists) and it removes the one hand-made input.
- `HttpApi::connect` refuses `https://` without a CA bundle (`SetupError::CaBundleRequired`,
  `http.rs`: platform roots are not read). A ten-line pool has no bundle. Following #195 in
  `js-agent` the client uses bundled public roots when no bundle is given; a bundle
  replaces them. `ca_bundle` stays for the loopback TLS tests.
- `catalog()` returns `Offer`s: one per (type with a `types` row, region with capacity and,
  if `regions` is set, in that list). `disk_bytes` from `storage_gib`; price from
  `price_cents_per_hour`.
- `kind::build` as in 5.1. `ElasticConfig::max_machines` stays as the provider's backstop,
  set to `max_instances`.

### 5.4 The network layer (module `network` of `js-lease-server`)

One layer for the whole service. Providers do not implement it and do not see it. A mode
contributes boot steps and a per-launch secret (`prepare`, `describe`); `check` and `forget` are
service-side lifecycle hooks, not contributions. The trait and
the two modes live in `crates/js-lease-server/src/network/` (`mod.rs`, `default.rs`,
`tailscale.rs`). They split into crates only when a mode needs a dependency the server should not
carry or a second consumer exists.

```rust
pub trait NetworkMode: Send + Sync {
    // contributions to a BootPlan
    fn prepare(&self, launch: &LaunchCtx) -> Result<Contribution, NetworkError>; // per launch, never in plan mode
    fn describe(&self, launch: &LaunchCtx) -> Vec<NetworkStep>;  // the same steps with empty secret slots (plan mode)
    // service-side lifecycle hooks; not part of the plan, never passed to a provider
    fn check(&self) -> Result<(), NetworkError>;   // can this mode issue what a launch needs? creates nothing
    fn forget(&self, host: &HostName) -> Result<(), NetworkError>; // after release; best effort, bounded retries
}
pub struct LaunchCtx<'a> { pub pool: &'a str, pub host: &'a HostName, pub valid_for: Duration }
pub struct Contribution { pub steps: Vec<NetworkStep>, pub secrets: Vec<(SlotName, Secret)> } // the whole interface
pub enum NetworkStep { Run(Vec<String>),                       // argv, never a shell line; never a Secret
    SecretFile { path: String, slot: SlotName },               // written 0600
    Fetch { url: String, sha256: [u8; 32], dest: String } }
```

The launch path in the shell (`scaler/launch.rs`) is `mode.prepare` to `BootPlan` to `render` to
`provider.acquire(payload)`. The shell is the only place that holds both a mode and a provider;
neither knows the other. The renderer quotes every argv element, so no step can add shell syntax.
`default` returns an empty `Contribution`; its `check` confirms `advertise_url` parses and, for
`https`, that the `[server]` certificate files are readable. In plan mode the service calls
`describe` and never `prepare`, so a plan mints no key and calls no Tailscale endpoint (13.2).
`tailscale` is section 8.

## 6. Scaler and instance lifecycle (requirement 14)

### 6.1 Where it runs and what it reads

Crate `js-scaler` is pure: no clock reads, no I/O, no threads. It takes the previous `State`, one
`Input` and the time now, and returns the next `State` and the `Vec<Action>`. The shell lives in
`crates/js-lease-server/src/scaler/` and runs on the tick (`--tick`, `ticker.rs`, 5 s default): once per tick it snapshots the registry
(hosts and wait list under one lock), calls `step`, runs the actions on a `blocking()` task
(`util.rs`), and feeds the answers into the next step. The scaler ticks on a task of its own, so a
slow provider call does not hold back the table's lease expiry, and a tick that finds the scaler
still at the last one skips it. No lock is held across a provider call, nor the lock of the shell's
own memory (a tick works on a copy and swaps it in), so the status page is answered during one. A
panic in the scaler's tick ends its task: `/healthz` answers 503 and the supervisor of the process
must restart the server (the records are on disk before any call they describe, so a restart launches
nothing twice). A registry refusal to remove the host of a machine that is gone is kept and tried
again at every tick, and alerted. Lambda allows one launch per 12 s
(`ElasticConfig::launch_interval`); `Acquired::Deferred { wait }` is honoured by skipping
the pool until the injected clock passes it.

Demand. The registry keeps a wait list of whole-node requests that found no host
(`crates/js-registry/src/whole/queue.rs`: in memory, capped at 1024, an entry lives while
the consumer presents it again within `regain_grace`). It has no public read. PR 7 adds
`Registry::waiting() -> Vec<Waiting { id, demand: Demand, since, reserved: Option<HostId> }>`.
`reserved` is set when the request holds a mark on a host: `whole/reserve.rs` marks a free
host as assigned, or a busy host as reserved, for one waiting request, and
`whole/placement.rs` counts a busy or reserved host as capable. Such a request already has a
host coming and must not buy a second one (6.2 Planned).

Capability. `whole::placement::answer` returns `NoCapableHost` when no known host could ever
serve a request, even if a cloud could launch one (`crates/js-registry/src/whole/placement.rs`).
With an elastic pool that is wrong: the request must wait. PR 7 gives `Registry` an optional
`Launchable` (`Fn(&Demand) -> bool`, set by the shell from current offers and caps).
`capable` is true when it says yes. This is rule D35 in `docs/design/providers.md` ("could
launch"). The shell answers true if some available offer fits and the caps allow one more
machine once idle hosts count as free. A pool whose checks fail answers false, so a request
fails fast with the existing code instead of waiting on a dead cloud.

```rust
// crates/js-scaler/src/lib.rs
pub struct Input { pub scheduler: SchedulerId /* names the hosts it launches */,
    pub pools: Vec<PoolInput> /* the order requests fall through */, pub waiting: Vec<Waiting>,
    pub hosts: Vec<HostFacts> /* id, pool, state (Live, Lost, Awaiting), shape, mark, busy, lease_given_up */,
    pub registry_complete: bool /* hosts is the registry's whole, settled view */,
    pub acquired: Vec<AcquireResult> /* answers to acquires, with the attempt they answer */,
    pub released: Vec<ReleaseResult> /* answers to releases, read back */,
    pub not_found: Vec<String> /* machines a read by id found the provider no longer has */,
    pub entropy: Vec<[u8; 16]> /* random values for this step's acquire ids; never repeated */ }
pub struct PoolInput { pub settings: Settings, pub health: Health, pub offers: Vec<Offer>,
    pub held: Option<Vec<Held>> /* None if list failed */,
    pub unresolved: Vec<AcquireId> /* acquire ids the provider holds an unresolved call for */ }
pub struct State { pub records: Records /* the only part written to disk */, /* and what the last step found:
    blocked, plan, no_offer, refused, silent, silent_released, ... */ }
pub enum Action { Save(Box<Records>) /* the whole records, written first when a step changed them */,
    Mint { host: String, valid_for: Duration },
    Acquire(Box<AcquirePlan>) /* the shell renders the payload */, Drain(String), Undrain(String),
    ReleaseHost(String) /* registry mark, only if still quiet */, Revoke(String),
    Release { host: String, id: HostRef }, ForgetNetwork(String), RemoveHost(String) }
pub fn step(state: State, input: &Input, now: Millis) -> (State, Vec<Action>);
```

### 6.2 State machine of one instance

A record exists for each acquire, written to `<state file>.scaler` before the call it
describes (write-whole-file-and-rename, as `agent_state::replace` does for `join_tokens.rs`
and the bindings file). A pool starts in `Checking`: nothing launches before the first full
check pass succeeds (10.1).

The file is never taken to be empty by default. A pool with `launch = "live"` whose file is
missing does not start: a start with no records forgets the machines in flight and the day's
spend, and could launch twice. The operator who says this is the first start does it with the
flag that already says it for the lease file, `--boot initialise` (`boot = "initialise"`); every
later start is `--boot resume`, which also refuses a missing records file. A pool in `plan`
mode starts without the file.

```
Planned -> Launching -> Booting -> Registered -> Draining -> Releasing -> Gone
              |  \-> Refused (dropped at the end of the step)    \-> Releasing (boot_timeout: no hello, host-contract)
              \-> Unresolved (Unknown) -> Booting (list shows it) | Failed (list lacks it after hold)
Failed (from Unresolved) stays counted until two clean lists (below).
Registered/Draining -> Releasing -> Gone also when the provider says the host is gone (Lost).
A hello below the offer: Registered -> Draining -> Releasing (the cooldown starts at the hello; the
host is drained and released once it is quiet).
```

Rules, each tied to a requirement or to existing behaviour:

- Planned: the scaler found `waiting` demand not covered. A waiting request is covered when
  any of these holds: it has `reserved = Some(host)` (a host is set aside for it, free or
  busy); or a registered host that is free and fits is not set aside for another request; or a
  record in Planned, Launching, Unresolved or Booting whose offer fits has no request matched to it.
  Matching is greedy in queue order, one request to one host or record. With a whole-host
  class one uncovered request needs one machine: uncovered = waiting requests with no match.
  At most `max_step` (default 1) launches per tick. Registered capacity is tried first: that
  is the registry's own start path, which runs before a request reaches the wait list
  (requirement 14). PROPOSAL (Open questions 21): a reservation counts as covering however long
  its `drain_deadline` is. The cost of that choice is a request waiting on a long drain while a
  launch would have served it sooner; the benefit is that no paid machine is bought for a
  request that has a host. The reservation's own give-up (`reserve.rs`, step 4) drops the mark,
  and the request is uncovered again on the next tick.
- Draining hosts do not cover. Before any `Acquire`, if an uncovered request fits a Draining
  host that is not yet Releasing, the scaler emits `Undrain` for it instead (see Registered to
  Draining).
- Caps (14, 15): before Planned require all of: pool health is Available (not Checking);
  `launch = "live"`; live records plus `held` instances not in a record (orphans count) are
  below `max_instances`; the sum of stored prices of those machines plus the candidate's is
  at or below `max_spend_per_hour`; the day total (below) plus the candidate's minimum cost
  is at or below `max_spend_per_day`; the candidate has a price in the pool's unit. An offer
  without a price is not launchable when a cap is set, and a cap is always set. The price is
  stored in the record at launch, so a later catalog change cannot hide a held machine from
  the sum. A machine in Unresolved counts at its offer's price until a list settles it. When
  caps fail, the pool shows `blocked: max_instances`, `blocked: max_spend` or
  `blocked: max_spend_day` and the request keeps waiting. `held == None` (the list failed)
  blocks launches the same way.
  The day total (requirement 15, a hard bound on spend over time) is the sum, over every record,
  of its stored price times the part of its billed time that lies in the last 24 hours on the
  injected clock: the billed time runs from launch to Gone (or to now, if not Gone), never less
  than one minute, and for a boot that failed or was never seen, never less than `boot_timeout`.
  A record that ended more than a day ago costs nothing and is dropped; one that still runs
  costs the 24 hours it has run. A launch waiting for its time (`Deferred`) counts its least
  cost, and its retry is checked against every cap again. The billing granule is
  not verified (the Lambda crate and docs do not state it; Open questions 2): the design
  rounds each record up to whole minutes and counts failed boots in full, which can only
  overstate. The candidate's minimum cost is its price times `boot_timeout` (at least a
  minute): a launch that never boots costs that much. The one-minute floor on the billed time is
  for a record whose host said hello and for an orphan, which did not fail a boot. A record
  that is not yet Gone keeps accruing, so a stuck drain eats the day cap and stops launches
  instead of growing without bound. The day cap is a rate over a window, not a lifetime
  total; the lifetime bound is `max_instances` times the day cap times days run, and the
  operator's go is per `launch = "live"` edit.
- Choosing: among available offers that fit, lowest price, then fewest gpus,
  then id, then zone (the order `select.rs` uses today).
- Planned to Launching: the shell mints the join token for `(pool, name)` (7.2), asks the
  network for steps, renders the bootstrap, writes the Launching record, then calls
  `acquire`. The record holds the acquire id, name, offer id and zone, stored price, the
  token expiry and the time. It holds no secret. It is durable before the call, so a crash
  after the call and before the answer leaves a record to reconcile. The Lambda provider's
  guard (answered and unknown launches held in memory, `provider.rs::Launching`) covers
  the same race inside one process; the record covers a restart, which the crate doc says
  it cannot ("a restart, or a second process, sees only the list, which can lag a write").
- Recovery never mints twice. `JoinTokens::mint` replaces an earlier unused token for the
  same instance (`join_tokens.rs`, test `minting_replaces_drops_and_is_bounded`). A re-mint
  after a launch that may have gone out would lock out the booted host. So: a record found
  after a restart is adopted as is, and no token and no Tailscale key is minted for it. A
  new token is minted only for `Deferred` (nothing was sent) and for a new acquire id.
- Launching to Booting on `Accepted`: record the cloud id. Extras (`Accepted { extras }`)
  are already released by the provider and recorded as such.
- Launching to Refused on `Refused`: revoke the token, forget the network key; no machine
  exists. `Capacity` marks that offer unavailable for the step and tries the next
  offer in the same step; with no other offer that fits, the request waits and the offer is tried
  again on the next step. A refused record is billed nothing and is dropped at the end of the step
  that learns of the refusal, so a pool refused over and over does not grow its records. `Quota` and `Denied` mark the pool blocked with the reason.
- Launching to Unresolved on `Unknown`: never launch again with a new acquire id for this
  demand until `list` has settled it. Next ticks call `list`; a held instance with this
  acquire id moves the record to Booting. The Lambda crate says `list` can lag a write
  (`crates/js-provider-lambdalabs/src/provider.rs` doc), so a list that lacks the instance
  proves little. After `unknown_hold` with no instance listed the record becomes Failed and
  its token is revoked, but it is still counted: at its offer's price against
  `max_instances`, `max_spend_per_hour` and the day total, until two successful lists, at
  least `unknown_hold` apart, both show no instance with this acquire id or name. Default
  `unknown_hold` is 30 minutes, longer than `boot_timeout` (PROPOSAL; it was 10 minutes in
  `ElasticConfig`, and the setting stays configurable). If an instance with the record's
  name appears after Failed, it is adopted back into Booting (its token was revoked, so
  the scaler mints nothing and releases it: an adopted-after-Failed instance goes straight to
  Releasing) and counted throughout. This is the "never double-launch" rule, and a late
  list can no longer put two machines on one request without both being counted. Test in
  13.2.
- `Deferred { wait }`: nothing was sent. The record stays Planned (not yet Launching), the
  token is re-minted at the retry. A Planned record covers the request it was made for. When
  no waiting request is matched to it any more (the request went away, or a host covers it), the
  record is forgotten and its token revoked; in a pool that only plans it would otherwise stay
  counted for good.
  `Deferred` holds only while no call went out. If the provider holds an unresolved call for
  the record's acquire id (an earlier launch of it got no answer and may have landed; the
  Lambda provider's answer for an acquire with an open call is of this kind), the shell lists
  that id in `PoolInput::unresolved`, and the step keeps the Planned record even when no
  request waits for it: it stays counted, and its retry (the same acquire id) settles the
  call, after which the machine is Booting and is released at `boot_timeout` if nobody
  wanted it. The shell's duty is to keep that list true for as long as the provider holds the
  call; dropping the record while it does would leave the machine to turn up as an orphan,
  which is never released (6.4).
- Booting to Registered: the first hello whose `instance.instance_id` equals the name is
  admitted by the registry. The hello's shape is compared with the offer. A host that
  reports less than its offer (cpu, memory, gpus, gpu class) is admitted but condemned: the
  boot-failure cooldown below starts at once, and the host is drained and then released as soon as it is
  quiet (`drain` and `release_host` are each checked under the registry's lock, so no work is
  cut; a host that already has work is left until it is quiet; it is never taken back for a
  request). Otherwise the real shape is the capacity, and the pending request
  is placed by the registry's normal path on the consumer's next poll.
- Booting to Releasing on `boot_timeout` (default 15 min, counted from the launch, as the join
  token's life is): the agent never said hello (or was refused `host-contract`). A host the
  registry shows has said hello, so a silent one is not released by this. Only a complete
  registry view gives up on a machine: on an incomplete one its host may have said hello and be
  missing from the part that was read, so the machine is held and counted. The instance is released. Any boot failure blocks new
  launches in the pool for `boot_fail_cooldown` (default 10 min, a fixed value; PROPOSAL,
  14 Q14) and shows `blocked: boot_failed`. The billed minutes of the failed boot stay in the
  day total. Counting failures, doubling the cooldown and suppressing a single offer are
  Later (section 12); the caps bound the loss meanwhile.
- Registered to Draining: the host holds no lease and no runs for `idle`
  (`HostView.releasing` false and `lease` none, `registry.rs::hosts`) and carries no mark.
  The scaler asks `Registry::drain(host)` (6.3). `drain` fails with `Busy` if the host has
  any lease, run or open stream at that moment, checked under the registry's lock; a drain
  never cuts running work, and requirement 14's "no deadline" is met because there is nothing to
  wait for: the scaler only drains a host that is already quiet. A Draining host is never
  booked, by lease placement or by a direct run start. It stays Draining only until the
  scaler's next tick; there are two exits and only the scaler takes them. `Undrain` (above:
  an uncovered request fits this host, so reuse beats a launch) or `release_host` (below). No
  placement path clears the mark. A request that arrives meanwhile waits for the scaler's
  next tick (at most `tick`, 5 s by default).
- Draining to Releasing is two steps. First the registry call `Registry::release_host(host)`
  sets the mark `Releasing` only if the host is Draining and still has no lease, no run and
  no stream, in the same critical section that books hosts, and fails `Busy` otherwise (the
  scaler then undrains). After `Releasing` is set the registry books nothing on the host, so
  there is no work for a revoke to cut. Then, in this order: revoke the join token,
  then terminate, then forget the network device. Revoke first, so a host that outlives a
  failed terminate cannot re-register. `release` answers are read back: `Terminating` keeps the record in Releasing
  and the next tick reads again; `Unknown` retries every tick and is never read as gone;
  `Gone` (a 404 by id after a successful list, `provider.rs::trust_absent`) moves to Gone
  and calls `Registry::remove`. Release keeps retrying until a read by id says Gone.
- Lost: a registered host silent past grace is not released on silence alone (silence is
  Unknown, `registration.md` Lifecycle), and not while the registry still holds its leases.
  If the provider reports `terminated` or `preempted`, or `get_instance` answers not-found
  (the input names the machine), the scaler revokes, forgets the device and removes the host so
  its slot is free. A silent machine the scaler launched also costs money, so once the host books nothing
  (the lease grace has given up its leases, or it held none; the shell says so in the host facts,
  `lease_given_up`, and sets it for every silent host that has no bookings) and it has stayed
  silent for `unknown_hold`, it is released the way an idle one is: drained and marked for
  release, which the registry checks is quiet when it runs those calls, then revoked, released
  and read back to Gone once the registry shows the mark. A silent host the scaler had already
  marked `Releasing` needs no flag. The step names each machine it releases this way for the
  shell to log. A host that shows a lease, a run or a stream, or a `Held` or `Reserved` mark, is
  never released, whatever the flag says. Hosts of a static pool are never released by the
  scaler. Until then the machine counts toward the caps and the state shows the age of its
  silence.
- A machine whose host the registry no longer shows (it ran `forget_lost`) is released the same
  way, but only on a complete, settled registry view (the shell says so in the input; a view
  read while the registry is loading, or in part, is incomplete and holds everything) and when
  the machine has been missing on two steps at least `unknown_hold` apart, with no step between
  that showed it or had an incomplete view. Releasing on a view that merely lacks hosts would
  cut work after a registry restart.
- Orphans: an instance in `list` with our marks and no record (a lost state file). Policy
  is in 6.4.

### 6.3 Registry additions (crate `js-registry`)

- `Registry::waiting(&self) -> Vec<Waiting>` (reads `WaitList`).
- Drain and release marks. The registry already marks hosts: `HostMark::{Reserved, Held}` with a
  `drain_deadline` (`crates/js-registry/src/registry.rs`, `HostView`), used when a waiting
  whole-node request reserves a busy host. That is a different thing: it serves one named
  request and has a deadline. Scale-down needs a mark with no request and no deadline. The
  design adds variants `HostMark::Draining` and `HostMark::Releasing` and the calls
  `Registry::drain(&HostId) -> Result<(), Busy>`, `undrain`, and
  `release_host(&HostId) -> Result<(), Busy>`, built on the same "takes no start" path
  `Reserved` uses. Each call checks "no lease, no run, no stream" and sets the mark under the
  one lock that books hosts. The direct run start (`crates/js-lease-server/src/run_api.rs`)
  reads the same mark under the same lock; if it does not today, PR 7 changes it and a test
  proves it. A host with any mark is never chosen for idle drain, and a `Draining` or
  `Releasing` host is never reserved or booked, which `js-sim` tests as interleavings (13.2).
  The existing reservation deadline is untouched and is not the drain deadline of requirement 14.
- `Registry::remove(&HostId)`: only for a host in `Releasing` with no lease and no live
  stream; frees the `max_machines` slot. A grep of `crates/js-registry/src/registry.rs` and
  `crates/js-registry/src/registry/provider_side.rs` at the base commit finds no public
  release or remove; `docs/design/registration.md` mentions "operator removal" but no
  method exists. The registry does have `forget_lost` (`registry.rs`), which drops a host
  lost for longer than `grace + retention` and holding no lease. They interact like this:
  `forget_lost` is the registry's own path for a silent host and stays; `remove` is the
  scaler's call after a read-back `Gone`; `remove` on a host already forgotten is a no-op
  `Ok`; the scaler counts machines by its records and the provider's `list`, never by the
  registry, so a forgotten host that still exists in the cloud is still counted and still
  released (on a complete registry view, missing twice; 6.2 Lost). `forget_lost` drops a forgotten host's binding too (`registry.rs`: the binding
  file is rewritten from live records only), so nothing on the scheduler side remembers a
  forgotten host. That is safe here because every launch has a fresh `HostName` (the
  acquire id is the pool and 128 random bits from the input, unique per acquire, 5.2), so a name is never reused, even after
  a lost state file; and a forgotten host's token is already spent and its credential is
  revoked at release (7.2), so it cannot re-register under the old name. Only a hand-joined
  host that reuses an old name meets `host-bound`, which is the existing behaviour.
- `Launchable`, as in 6.1.
- `Agents` today requires `AgentTokens::open(--agent-token-file)` for any registering pool
  (`config.rs::hosts`: both agent flags come together). An elastic pool needs only one-time
  tokens, so the shared pool token file becomes optional and a pool without it admits only
  hosts whose one-time token verifies. A hand-joined pool still sets `join_token_file`.

### 6.4 Orphans, lost state, spend over time

- After a lost state file: the scaler adopts what `list` shows. Every instance with our
  marks and no record becomes an Orphan record, counts toward both caps at the catalog
  price, and shows in status. Launches are paused until one `list` has succeeded after
  start. This makes a lost file visible and cannot double-launch.
- Orphans are reported, not terminated, by default (as the crate's `sweep` is
  report-only). Auto-release would delete instances the scheduler has no record of, and a
  second scheduler on one account looks the same. Open questions 8. So the scaler never
  releases an orphan, including a machine a launch answer named and the provider could not
  confirm released, or one it learned is a duplicate: the shell's duty is to alert on every
  Orphan record in status, and the operator releases the machine at the provider. The same
  holds for a record whose pool is no longer in the config: the records keep it and it counts,
  but no pass reconciles or releases it, so the shell alerts on a record of an unknown pool and
  the operator releases it at the provider and removes the record. `release_orphans` stays
  Later (section 12).
- `max_spend_per_hour` caps the rate. `max_spend_per_day` is in the first slice (6.2 Caps)
  and bounds spend over time even when a stuck or Lost machine is counted forever. A Lost
  host that the provider still lists keeps accruing in the day total until released; status
  shows its age. `max_age` is Later. The day cap gates new launches only: machines already
  running when it is reached keep running (and accruing) until they are released.

### 6.5 What the single-pool slice costs

Requirement 14 says "match pending demand to elastic pools by offers". The first slice does this
for one pool. `js-scaler::step` is written per pool and takes a list of them, so the
algorithm is not the limit; the limit is the registry, which takes one `PoolConfig`. The
later change: `Registry` holds a map pool to config and floor, hello carries the pool (it
already does, `--pool`), and `Waiting` carries which pools could serve it. For part-host
classes: first-fit pack of pending demand onto one new host, and status shows `no_offer`
with the failing `Dimension` per waiting request. Both are one later PR each (section 12,
PR 11 and 12), after the live proof.

## 7. Lambda AI end to end

A Lambda host has no private path to a scheduler, so a Lambda AI pool uses
`network = { kind = "tailscale", ... }` (4.1). A Lambda pool in `default` mode works only if the
operator's own image or VPN setup gives the host a route to `advertise_url`; the stock image does
not, and the bootstrap of 7.3 adds none. Without a route the host never says hello and is
released at `boot_timeout` (F22).

### 7.1 The sequence

1. A consumer's whole-node request finds no free host and waits in the registry; `Launchable` is true,
   so it is not refused `no-capable-host`.
2. Next tick the scaler sees one uncovered request, takes offers from `catalog()`, picks the
   cheapest fit, passes the caps and writes the Launching record (6.2).
3. The shell mints the join token for `(pool, host name)` (7.2), gets the Tailscale key (8),
   renders the bootstrap (7.3), ensures the no-inbound ruleset exists in the region (5.3), and
   `acquire` launches: type, region, one ssh key name, ruleset, `user_data`, name, tags.
4. The record becomes Booting. The instance boots, cloud-init runs the bootstrap, the host joins
   the tailnet and the agent says hello to `advertise_url` with the token and its instance line.
5. The registry admits the hello (token unused, unexpired, instance equals the name, shape
   satisfies the floor including `runtime`, pool below `max_machines`), answers `Welcome` with
   the host credential (7.2) and the host registers with its real shape. The consumer's poll arrives and the registry places the lease.
6. After `idle` of quiet: Draining, then Releasing (the registry sets the mark only if the host is
   still quiet, 6.2), then revoke the token, terminate, read back, delete the tailnet device.
   Record: Gone.

### 7.2 The join token binding and the host credential (requirement 8)

Today `JoinTokens::mint` binds a token to an `InstanceIdentity { provider, instance_id }`
(`crates/js-lease-server/src/join_tokens.rs`), and the first hello presenting it must name
the same pair (`JoinRefusal::WrongInstance`). Lambda's instance id is unknown until the
launch answers, but the token must be in `user_data`, which is sent with the launch.

Decision (requirement 8 asks which): bind to a name chosen before launch, not on first use. The
name is `HostName`, which is also the Lambda instance `name` (the crate already sets it,
`provider.rs::launch_request`, `Owner::name`). The token's `instance_id` is that name and
`provider` is the pool name (a valid `ProviderId`, `crates/js-types/src/ids.rs`). The agent's
`--instance-file` line is `<pool>:<name>`, written by the bootstrap; the hello carries it
(`Hello.instance`, `crates/js-agent-wire/src/hello.rs`).

What the binding is worth, plainly. The module doc of `join_tokens.rs` says the instance a
hello names is the hello's own claim, and that the binding "guards against mix-ups (a token
delivered to the wrong machine, a host id reused by another instance), not against a holder
of the token, who can name the instance too". The same holds here. The name is derived from
the acquire id, shows in Lambda's instance list and on the status page, so a reader of
`user_data` knows it. The binding therefore prevents mix-ups between our own launches and
nothing else. It is chosen over first-use binding because it needs no change to
`join_tokens.rs` and keeps `WrongInstance` meaningful, not because it adds secrecy.

The real defences against a stolen token, in the order they act:

1. Expiry: the token is minted with ttl `boot_timeout` plus a margin (default 5 min, so 20 min), counted from the launch, under
   `MAX_TTL` (24 h). An unused token is dead after that.
2. The host credential (below), issued at the first hello. After it, the token is dead.
3. Revoke at release, before terminate (6.2), and at Failed.
4. The Tailscale ACL: the host can reach the agent port only (8.3). Binding the first hello to
   the peer's tailnet identity is not part of this design. It would need the service to ask
   `tailscaled` who the peer is, which is a service feature outside the mode layer (a mode is a
   boot step plus a secret) and outside requirement 5.

Single-use, stated precisely. Today the token is single-use for registration only: after the
first hello it cannot register a second host id (`join-token-used`), but the first hello makes
it the host's credential for up to `CREDENTIAL_TTL` (24 h) of silence (`join_tokens.rs`). Its
copy stays in `user_data` on Lambda's side for the instance's life. Requirement 8 calls both
secrets single-use and short-lived, so the first slice includes the fix, before any live
launch, not as an option:

- `Welcome` (`crates/js-agent-wire/src/hello.rs`) gains an optional field `host_credential`:
  256 random bits, minted by the registry at the first admitted hello for that host.
- The agent writes it to a 0600 file under its state root and uses it for every later
  heartbeat and call (`crates/js-agent`). Older agents ignore the field and keep using the
  token (the token path is unchanged for a hand-joined pool).
- The join token is marked retired at that hello: it no longer authenticates anything, so
  the `user_data` copy is useless. The credential is stored hashed in the same whole-file
  state as tokens (`agent_state::replace`), revoked by the same `revoke_for(pool, name)`, and
  carries the same `CREDENTIAL_TTL` silence bound.
- Failure case: the `Welcome` is lost on the wire after the registry retired the token. The
  host cannot re-register; the scaler sees Booting with no heartbeat, `boot_timeout` releases it.
  Tested in 13.3.
- The Welcome that can carry the credential is sent with `Cache-Control: no-store` and
  `Pragma: no-cache`.
- An agent that is given no `--token-file` uses the `host-credential` file of its `--state-root`
  when there is one it can read; with none it refuses to start, naming `--token-file`.
- The server flag `--require-host-credential` (off by default; turn it on once every agent is new)
  refuses the first hello of a one-time token that does not say the agent keeps a credential
  (`join-token-credential-required`, token left unused). Pool join tokens are unaffected. The
  flag may move to `[server]` of the configuration file.
- Not done: removing the token file from the host. The boot script ends at
  `systemctl enable --now js-agent.service`, when the agent has not yet read the file or said
  hello, so the `EXIT` trap cannot remove it without the agent losing its way in (a restart before
  the credential is saved needs the token). A safe point needs either a wait in the script for the
  credential file, which holds the boot for as long as the server is out of reach, or the agent
  removing the file after its credential is durable and a unit that does not name it. Follow-up.
- Not done: retiring the token at the credential's first use, so that a lost Welcome does not
  strand the host. A re-hello with the token would have to return the same credential, which the
  server holds only as a digest; keeping the secret in the file for that would change what the file
  holds. The lost-Welcome case stays as above.

In code: `Agents` gains `mint_for(pool, name, ttl)` and `revoke_for(pool, name)`, in-process
wrappers over `joins.mint` and `joins.revoke`; the HTTP routes stay for tooling. Docs in
`join_tokens.rs` and `join_api.rs` that say "the provider's instance id" are reworded to "an id
the minting side chose". `MintError::Full` (256 waiting, `MAX_WAITING`) blocks a launch and shows
in status; the loader rule on `max_instances` (4.4) keeps it from occurring.

### 7.3 The bootstrap (Lambda: stock image plus a built-in template)

Lambda gives no custom images (requirement 2). The service renders a cloud-init script from the
`BootPlan` (module `bootstrap` of `js-lease-server`: one renderer for every provider and mode).
What is specific to the stock Lambda image is data the Lambda crate returns from `host_prep`
(the package table of step 1); the script is delivered as `user_data`. It does, in order:

1. Host contract (requirement 3). Check `nvidia-smi` exits 0 and the container binary at the path
   the template names exists. For each missing piece, install it only from the template's
   pinned table of packages (url and sha256 per arch, compiled into the crate). If the
   table has no entry or a checksum fails, write a console-log line and do not start the
   agent: the host never says hello, `boot_timeout` releases it, and status shows "booting,
   no hello". The table is empty until the live run shows what the stock image lacks (Open
   questions 5). The agent then reports `runtime` and `gpu-toolkit` from what it actually
   finds (3.6), so a bootstrap that lied is caught at admission as `host-contract`.
2. Write `/etc/jobscheduler/token` (0600) and `/etc/jobscheduler/instance`
   (`<pool>:<name>`, not secret).
3. Run the network steps the pool's mode contributed (`tailscale`: binary fetched with checksum,
   `tailscale up`; `default`: nothing).
4. Fetch the agent for the offer's arch from `BootPlan.agent.url`, verify the sha256 before
   the agent ever starts, install to `/usr/local/bin/js-agent`. The pool's `agent` table pins
   one artifact (`url`, `sha256`) per CPU architecture; the launch takes the entry for the
   offer's architecture and never another's. An offer whose architecture has no entry is not
   launched: the pool's checks mark it unavailable naming the offer, the architecture and the
   field (`agent.<arch>`), and a launch that reaches the provider layer anyway is refused with
   a `Shape` refusal. Write the GPU toolkit hook (`/usr/local/lib/js-agent/gpu-toolkit-hook`,
   mode 0700), a program that runs `nvidia-container-cli info`, which fails without a working
   driver and toolkit (`--version` would prove nothing).
5. Make the agent's state root. `js-agent` with `--container-bin` requires `--state-root`
   on a different device from every mount root and the scratch root and refuses to start
   otherwise (`crates/js-agent/src/cli.rs`). Lambda instances have one root disk. PROPOSAL:
   a small ext4 image file attached with `losetup` and mounted as the state root, which has
   its own `st_dev`. A tmpfs is wrong: the agent says a taint on a tmpfs is lost at reboot.
   Needs one live test (Open questions 5).
6. Write and start `js-agent.service` with `--server`, `--token-file`, `--instance-file`,
   `--pool`, `--host-id` (the machine's `HostName`, which is how the scaler finds the host of
   a record), `--container-bin`, `--container-pull missing` (a launched machine is new and holds no image, so
   the agent's default `never`, which exists so that a job cannot make a host reach a registry of
   its choosing, would fail every job on it; the server refuses a run whose image is not pinned by
   a sha256 digest, so a pull fetches the content the submitter named, and the host reaches the
   registry only over the pool's network; a pool-level setting is Later), `--gpu-toolkit-hook` (on every
   offer; the agent runs the hook only when the host offers a GPU, and a pool with GPUs refuses
   a host without it as `host-contract` on `gpu-toolkit`), `--state-root`, `--scratch-root`, the
   `--watch-dir` list the agent's help suggests, and labels.
7. Cleanup, from a `trap ... EXIT` installed as the first line of the script, so a failure at any
   earlier step still runs it. It removes (with `rm`; on a journaling or copy-on-write
   filesystem that does not erase blocks, and nothing here claims it does) the secret files used
   and every cloud-init copy of the user data: the rendered script, `/var/lib/cloud/instance/`
   and `/var/lib/cloud/instances/<id>/`, `/var/lib/cloud/data/`, the datasource cache and
   `/var/log/cloud-init*.log`. The template's list comes from a `find / -newer` pass on a live
   stock-image run, recorded in the PR and re-checked on every stock-image change (Open questions
   5); until that run exists the design does not claim removal. After the Welcome credential
   (7.2) a surviving token copy is useless, so cleanup is defence in depth, not the guard.
8. Block metadata for jobs. A process that reaches the provider's metadata address can read the
   user data again, and the host runs untrusted GPU jobs in containers. The template drops traffic
   from the container networks (and the job user) to the metadata address before the agent starts,
   and the agent runs containers on a bridge, never the host network. The address and the way to
   drop it on the stock image are not verified (Open questions 5); the live checklist (13.4) tests
   that a container cannot read user data. No job runs before the first hello, and the token is
   retired at it, so the window between boot and first hello holds no job.

Hardening of the template, each a golden test of the rendered text with every secret
masked: `write_files` mode 0600 for every secret; no `set -x` and no `echo` of a secret;
every download checksummed before use; secret files removed after use by the EXIT trap (the
Tailscale key is removed with `rm` after `tailscale up`; no claim of erasure); the whole render
below the user_data limit.

The agent artifact. The operator pins it per CPU architecture in the pool's `agent` table
(`url` over https and `sha256`, 4.1), and the host fetches from that address; the service
serves nothing for it, so its API is its only surface and no route is unauthenticated for
the download. A checksum in `user_data` guards against a corrupt or swapped download from a
reachable server; it does not guard against a compromised service, which can mint tokens
anyway.

The launch call carries: region and instance type from the offer, the one ssh key name, the
one no-inbound ruleset id, `user_data` (the opaque payload from the service, a `Secret`), the `HostName` as
name, and our ownership tags (`own.rs`).

## 8. Tailscale network mode (requirement 7)

### 8.1 Config

`network = { kind = "tailscale", oauth_client_file = "<path>", tag = "tag:js-agent" }`. The
file holds `client_id` and `client_secret` in TOML, owner-only like every secret file
(PROPOSAL for the format). OAuth clients do not expire, so nothing rotates by hand; hand-made
auth keys expire after 90 days and are not used.

### 8.2 Per launch

`network::tailscale::prepare` (`crates/js-lease-server/src/network/tailscale.rs`):

1. Get an access token with the OAuth client (client-credentials grant). Cache it until a
   minute before it expires; the clock is injected.
2. Create one auth key: single-use (not reusable), ephemeral, pre-approved, tagged with the
   pool's tag, expiry 10 minutes (`LaunchCtx.valid_for`). One key per launch, never shared.
3. Return `Fetch` for the Tailscale static binary (pinned version and sha256 in a table
   compiled into the crate and updated by release; PROPOSAL), `SecretFile` for
   `/etc/jobscheduler/ts-authkey`, then `Run(["systemd-run", "--unit=tailscaled", "tailscaled", ...])`, a bounded wait for
   `tailscaled`'s socket (`Run(["sh", "-c", <constant loop>, "sh", <socket path>])`: at most 30
   checks, one second apart, then a `logger` line and a non-zero exit, which stops the boot), and
   `Run(["tailscale", "up", "--auth-key=file:/etc/jobscheduler/ts-authkey", "--hostname=<name>",
   "--accept-dns=true"])`. DNS is accepted because `advertise_url` is a MagicDNS name and the
   server's certificate is for that name, so the host must resolve it. Then `Run(["rm", "-f", "/etc/jobscheduler/ts-authkey"])`. The EXIT trap (7.3) removes the file
   too, if `tailscale up` fails.

The endpoints and scopes (`auth_keys` to create keys, `devices:core` to delete a device) are
checked against Tailscale's API reference in PR 2; the design depends on them existing, as
requirement 7 states.

### 8.3 Access control and cleanup

The operator's tailnet policy puts the tag on a rule that allows `tag:js-agent` to reach the
scheduler's node on the agent port (`server.agent_listen`) and nothing else. That is not our code; the docs ship the
snippet. The service does not change the policy. Requirement 7 relies on that rule to keep a
compromised GPU host to the agent port, so the design does not leave it unchecked:

- The docs say the OAuth client must be created with only the scopes `auth_keys` (write) and
  `devices:core` (write), limited to the one tag; a client allowed to mint keys for other tags
  would let a compromised service (or a bug) put a host on a wider ACL. The service cannot see
  a client's tag limits, so this is documentation plus the live check below.
- The live checklist (13.4) has two items: from a launched host, connect to another tailnet
  node, and to the scheduler's `listen` port (client, operator, status); both must fail, and the
  agent port must succeed.
- PROPOSAL (Open questions 22): if the OAuth client was also given the read scope for the policy
  file, `check()` reads the policy and shows `network: warning: no rule limits tag:js-agent to the
  agent port` when it cannot find one. A warning, never a failure, since parsing a policy
  correctly is the operator's tool's job.
- The client, operator and status APIs get nothing new in the service. They are served on
  `server.listen` and the network decides who reaches them (9). A launched host carries
  `tag:js-agent`, and one port cannot tell agent routes from operator routes. So the split is in
  the first slice (PR 4): `server.agent_listen` is required with an elastic pool and serves only
  the agent routes (hello, heartbeat); the agent routes are not served on
  `listen`, and the other routes are not served on `agent_listen`. The policy snippet gives the tag
  the agent port only. Without the split the network could not separate a launched host from the
  operator routes, which is why it is not optional. Open risk: the split holds only while the operator's
  policy gives the tag the agent port alone; the service cannot verify the policy (the warning
  above is best effort), so 13.4 checks it live.

After release, `forget(host)` looks up the device by hostname and deletes it. The device is
ephemeral and drops when offline anyway; the delete frees the name. A failed delete is logged and
shown and does not block the release. It is sent once, after the machine is confirmed gone;
the keys are single-use, ephemeral and tagged, so a device whose delete failed drops when it
goes offline.

### 8.5 Live check

`check()` exchanges the OAuth client for a token. A refused client is `network: credentials
failed`. It does not create a key (a check should not spend or litter). Whether a mint works
is proved by the first launch; if the mint fails then, the launch is not made and the pool
shows `network: key mint failed: <status>`. A key that is 10 minutes valid against a slow
boot is Open questions 11.

### 8.6 The scheduler's side

The service does not join the tailnet. The deployment does (the host's own `tailscaled`, or
a sidecar), and `advertise_url` names it. Nothing in `js-lease-server`'s core modules,
`js-scaler`, `js-elastic`, `js-registry` or any provider crate names `network::`; only `kinds.rs`
and the launch shell (`scaler/`, through the trait) do. A CI check greps for the path and fails
if a file other than those and `network/` names it
(PR 10). Nothing depends on our hosts either (requirement 5): no address, name or path of ours
appears in a default.

## 9. Security (requirements 8, 11, 15)

- No secret in an image. The service image holds the server binary, the agent binaries and docs.
  Every secret is a file named in the config (client token, TLS key, Lambda key, OAuth client),
  mounted by systemd credentials, a Kubernetes Secret or a cloud secret store. The start-up check
  is the existing owner-only rule (`config.rs::read_private`: 0600 or 0400, owned by the service
  user); systemd `LoadCredential` meets it when `User=` is the service user.
- Secrets the service makes: the per-launch join token and Tailscale key, made as late as
  possible, valid minutes (token `boot_timeout` + margin, key 10 minutes). 7.2 gives the token's
  limits.
- Why user data (requirement 8): a later SSH push needs inbound 22 for the instance's whole life. The
  cost is that secrets sit in the launch request. They cross TLS to Lambda and are never logged.
  The bootstrap removes the on-host copies it can list (7.3), blocks container access to the
  metadata address, and the token is dead after the first hello (the Welcome credential, 7.2). `providers.md` ("No secret is baked in or passed in
  the launch request (K6)") and `registration.md` ("never in user data") say the opposite; PR 10
  amends both and names the requirement.
- Whether the ruleset really means no inbound is a human check before the first live launch:
  Lambda's account-wide rules may still allow 22. The docs do not say "no inbound" until tested
  (Open question 10).
- In process: `Secret` has no `Serialize`, `Clone` or `Display`, and its `Debug` prints `***`; a
  panicking check is contained to its pool. Zeroing on drop, `RLIMIT_CORE` 0 and a panic hook that
  strips payloads are Later (section 12), not asked for by a requirement. The status page never holds a secret (the plan view is the `Mask` render of the `BootPlan`), and a
  test greps the serialised status for known test secrets.
- Spend is bounded five ways: `max_instances`, `max_spend_per_hour` on stored prices,
  `max_spend_per_day` on billed time including failed boots (6.2), the provider's `max_machines`
  and launch interval, and `launch = "plan"` with the provider's second lock. A scaler bug that ignores one meets the next. No live launch without the operator's go.
- The agent pins the scheduler (`--ca-cert` or bundled roots). It refuses non-loopback `http://`
  ("the join token never crosses a network in clear", `cli.rs`), also over a tailnet, so start-up
  refuses a non-loopback `http://` `advertise_url` for every elastic pool (4.4) and a tailnet
  scheduler serves TLS on its tailnet name with an operator certificate.
- Client and operator authentication is built around the service, not in it. The scheduler has no
  public listener; the status, client and operator APIs are reachable only on the private or
  tailnet address, and tailnet ACLs or a proxy in front decide who gets there. This design adds no
  client key and no status token. The client token that exists on main is kept as it is. Agent
  registration auth (one-time join token, host credential at first hello, revoke on terminate) is
  machine registration, not client auth, and stays (7.2).

## 10. Status and validation (requirement 9)

### 10.1 Live checks

Each elastic pool has a check loop, one task per pool on the server's runtime, on an
injected `Ticker` (default 60 s; 10 s while unavailable, doubling to 5 min). A pool begins
`Checking` and launches nothing until one full pass succeeds. Each run:

| check | how | failure shows as |
|---|---|---|
| credentials | `provider.catalog()` (a refused key is `invalid-api-key`/`forbidden`, matched on the API's `code`, `error.rs`) | `credentials: failed (<code>)` |
| catalog | offers returned; each `types` row matches a catalog entry and the gpu count agrees | `catalog: gpu_9x_x is not offered` |
| price | each offer has a price in the pool's unit | `catalog: gpu_1x_a10 has no price` |
| ssh key | the named key exists, or the account has exactly one (or, if Q3 is accepted, the service creates `js-<scheduler>`) | `provider.ssh_key: account has 0 keys; name one` |
| network | `network.check()` | `network: failed (<reason>)` |
| firewall (Lambda) | list rulesets; the no-inbound one exists or can be created | `firewall: <reason>` |

A pool is Available when all pass. An unavailable pool launches nothing, answers
`Launchable = false`, keeps its running machines (they still serve and are still released),
and shows its reason; it is retried on the backoff. A check never exits the process and
never blocks a tick: it runs on `blocking()` with the clients' own time limits (the Lambda
client bounds every call, `http.rs`), and a panic is caught per pool.

### 10.2 The status API

`GET /v1/pools` and `GET /status`, an HTML rendering of the same data. They are served only on
`server.listen` (4.2), never on the agent port, and who may reach them is the network's job
(tailnet ACL, or a proxy in front). This design adds no status token and no client key: the
config has no `status_token_file`. The client token that exists on main stays as it is, with no new
field. A launched host must not reach these routes: the ACL gives it the agent port only (8.3). `/healthz` is open, as today (`api.rs`).

```jsonc
{ "pools": [ { "name": "gpu", "kind": "lambdalabs", "network": "tailscale", "exec": "container",
    "launch": "plan", "state": "available",      // checking | available | unavailable
    "credentials": { "ok": true, "checked_at_ms": 0 },
    "network_check": { "ok": false, "reason": "oauth: invalid_client", "checked_at_ms": 0 },
    "instances": { "n": 1, "max": 4, "spend_cents_per_hour": 129, "max_spend_cents_per_hour": 1200 },
    "blocked": null,       // max_instances | max_spend | max_spend_day | boot_failed | unavailable
    "no_offer": null,      // or { "request": "...", "dimension": "gpus" }
    "last_launch": { "at_ms": 0, "outcome": "accepted", "host": "js-ab12-cd34" },
    "machines": [ { "host": "js-ab12-cd34", "state": "registered", "offer": "gpu_1x_a10",
                    "zone": "us-west-1", "price_cents_per_hour": 129, "idle_since_ms": null } ],
    "orphans": [],
    "next_launch": { "offer": "gpu_1x_a10", "zone": "us-west-1", "name": "js-...",
                     "plan": "<Mask render of the BootPlan>",   // secret slots (join_token, network.*) print ***
                     "secret_slots": ["join_token", "network.ts_authkey"] } } ] }
```

`next_launch` is the plan for the head of the waiting queue, or for a one-request demand
when the queue is empty. The service builds it from the same `BootPlan` the live launch renders
(network steps from `NetworkMode::describe`, host-prep from the provider), so the page cannot
show a plan the code would not follow. The provider supplies only the host-prep data. A wildcard
`listen` or `agent_listen` shows as a top-level `server: warning: binds all interfaces` (3.4). The
existing `GET /v1/hosts` (`api.rs`, source `static` or `registered`) is unchanged and gains `pool`
and `launch_name`.

## 11. Failure table

| # | failure | defence | section |
|---|---|---|---|
| F1 | crash after the call, before the answer | Launching record written first; adopt on restart; no re-mint | 6.2 |
| F2 | `acquire` answers `Unknown` | Unresolved; no new acquire id until `list` settles; `unknown_hold` | 6.2 |
| F3 | `list` fails | `held == None` blocks launches; never read as empty | 6.2 |
| F4 | lost state file | adopt orphans, count them, pause until a `list` succeeds | 6.4 |
| F5 | host never says hello | `boot_timeout`, release, `boot_fail_cooldown` | 6.2 |
| F6 | host reports less than its offer, or lacks runtime/toolkit | a short hello: cooldown, drained, released once quiet; a missing runtime or toolkit: refused `host-contract` | 3.6, 6.2 |
| F7 | terminate fails or is slow | revoke first; retry until read-back `Gone`; `Unknown` is not `Gone` | 6.2 |
| F8 | catalog price moves | stored price in the record, summed for the caps | 6.2 |
| F9 | stuck drain or Lost host accrues forever | `max_spend_per_day` on billed time stops launches; status shows age | 6.2, 6.4 |
| F10 | credentials revoked mid-run | pool unavailable; running hosts still served and released; healthz 200 | 10.1 |
| F11 | a check panics or hangs | contained per pool; client time limits | 10.1 |
| F12 | secret leaks to log, Debug or status | `Secret` type, masked render, grep tests | 4.2, 5.2, 9 |
| F12b | a job on the host reads user data from the metadata address | bootstrap blocks container access; token retired at first hello; live check | 7.2, 7.3, 13.4 |
| F13 | user_data larger than the API accepts | `Shape` refusal before the call | 5.3 |
| F14 | Tailscale key expires before boot | boot fails, `boot_timeout`, `boot_fail_cooldown` | 8.5 |
| F15 | token table full (256) | loader rule on `max_instances` | 4.4, 7.2 |
| F16 | a second scheduler on the same cloud account | ownership marks include the scheduler id; foreign instances are not counted or touched | 5.2 |
| F17 | work starts on a host between the quiet check and terminate | `drain` and `release_host` check "no lease, run or stream" and set the mark under the booking lock; direct run start reads it; `js-sim` interleavings | 6.2, 6.3, 13.2 |
| F18 | a burst of demand | `max_step` per tick, launch interval, caps, concurrency test | 6.2, 13.2 |
| F19 | `list` lags past `unknown_hold`; a late instance and a new launch for one request | Failed-unsettled record counted until two clean lists; late instance released | 6.2 |
| F20 | a request that holds a reserved host also buys a machine | `Waiting.reserved`; a reserved request is covered | 6.1, 6.2 |
| F21 | a launched host reaches client, operator or status routes | no public listener; agent routes on their own required port (Q23); the network ACL gives a launched host that port only; no new in-service token. Open risk: the service cannot verify the operator's policy (live check 13.4) | 3.4, 8.3, 9 |
| F22 | a `default`-mode pool whose hosts have no route to `advertise_url` | host never says hello; `boot_timeout`, release, cooldown; status shows "booting, no hello" | 6.2, 7 (intro) |

## 12. What changes in each crate and the PR sequence

Each PR is green alone, and `launch = "plan"` is the only reachable mode until PR 8.

| # | PR | crates | content |
|---|---|---|---|
| 1 | elastic types | new `js-elastic`; `js-types`; `js-provider-lambdalabs` | trait (`host_prep`, `payload_limit`, `acquire(payload)`), `Offer` (`fit` refuses os/arch mismatch), `Demand`, `Secret`, `BootPayload`, `HostName`, `BootPlan`, `Acquired`/`Released`/`Held` moved; `ContainerSpec`, `Requirements` (os, arch from the request); Lambda implements `ElasticProvider`; the existing provider tests move and stay |
| 2 | network layer, renderer | `js-lease-server` (modules `network`, `bootstrap`) | `NetworkMode` trait, `default`, `tailscale` (OAuth, key mint, device delete), `render` with `Mask`/`Real`, template golden tests, fake HTTP server |
| 3 | Lambda launch inputs | `js-provider-lambdalabs` | `user_data` as the opaque payload, firewall ids, `ensure_firewall`, ssh key rule, bundled roots, `catalog()` offers, `host_prep`, `payload_limit`, `kind::build`, fake updated, log-grep test |
| 4 | config file | `js-lease-server`, `js-pool-static` | `--config`, `[server]`, `[[pool]]`, `kinds.rs`, static `kind::build`, offline validation, `StartupError` with pool and field; `listen` and required `agent_listen` (route split), wildcard-bind warning; flags still work when `--config` is absent; one registering pool |
| 5 | checks and status | `js-lease-server` | per-pool check loop, `Checking`, `/v1/pools`, `/status` (private address only, no status token), masked plan |
| 6 | host contract | `js-caps`, `js-agent`, `js-agent-wire`, `js-registry` | `runtime`, `gpu-toolkit`, `exec` capabilities, `host-contract` and `execution-mismatch` refusals, pool `exec` |
| 7 | registry | `js-registry`, `js-lease-server` | `waiting` (with `reserved`), `HostMark::Draining`/`Releasing`, `drain`/`undrain`/`release_host`, `remove`, direct run start reads the marks, `Launchable`, optional shared token file, `mint_for`/`revoke_for` |
| 7b | host credential | `js-agent-wire`, `js-agent`, `js-lease-server` | `Welcome.host_credential`, agent state file, token retired at first hello (7.2); before any live launch |
| 8 | scaler | new `js-scaler`, `js-lease-server` | pure `step`, records file, shell on the tick, caps (hour and day), `boot_fail_cooldown`; `launch = "live"` honoured |
| 9 | Lambda end to end | `js-lease-server` (tests), docs | e2e with `FakeLambda`, fake Tailscale and a hello through the real registry |
| 10 | one config file; docs | `js-lease-server`, docs | server flags into `[server]`; flags refused beside `--config`; amend `providers.md` K6, `registration.md`, `configuration.md`; the path check of 8.6 (no provider crate names a network type); fix the stale `--ca-cert` comment in `js-agent/src/cli.rs` |
| 11 | disk and exec on jobs | `js-caps`, `js-types`, `js-agent`, `js-registry`, `js-scaler`, `js-lease-server` | `disk-gib` and `exec` carried by the lease request (a 400 for a bad value), reported by the agent (`disk-gib` from the `--scratch-root` volume), compared by `Shape::satisfies`, `js_caps::Request` and `Offer::fit`; needed before any macOS pool and before disk-sized launches |
| 12 | more pools | `js-registry`, `js-scaler` | several registering pools; part-host classes; first-fit packing |

PR 7b is required before the go (7.2). Later, each behind its own need and not built first:
price-drift marking; per-offer suppression and doubling backoff; counting boot failures;
`zeroize`, `RLIMIT_CORE` and a panic hook; a separate `js-network` crate; orphan release
(`release_orphans`); `max_age`; the `exec` kind; a `script` network mode (needs limits and a
review story for operator output); a public scheduler listener (it needs a full in-service
client-auth story: client keys, rotation, rate limits, audit).

Then, not a PR: the operator's go (requirement 15). The operator adds `launch = "live"` with
`max_instances = 1` and a small `max_spend_per_hour`; one GPU lease is requested; the
live checklist in 13.4 is watched.

Smallest path to GPU jobs on Lambda: PRs 1 to 9. PRs 2, 4, 5 and 6 can proceed in
parallel with 3. PR 7 can start once the `Demand` type of PR 1 exists. The consumer runs on flags
until PR 10 and its unit files move in the same change.

Not changed: `js-provider` (the lease-level trait is not the elastic trait).

## 13. Tests that prove it

All are behaviour tests. None asserts elapsed time: time is `ManualClock`
(`crates/js-provider-lambdalabs/src/clock.rs`), the server's `SharedClock` and the steppable
`Ticker` (`ticker.rs`). Fakes: `FakeLambda` (records calls, can answer `Unknown` after a launch
lands), a new `FakeTailscale` (an HTTP fake like `tests/http`), and a fake agent that posts a hello.

### 13.1 Config (PR 4) and plumbing (PRs 1 to 3)

- Each refusal in 4.4 has its own test asserting pool and field in the message. An unknown
  provider field, a world-readable `api_key_file` and an inline secret are refused. Catches a
  typo read as a default and a check that names nothing.
- The 4.1 example, read out of the docs file, with fake secret files, builds a pool in `plan`.
  Catches the example drifting from the code.
- A log sink as the existing API-key test installs: launch with a known marker as join token
  and as Tailscale key; the marker is in no log, error, `Debug` of `AcquireRequest`, or serialised
  status. `render(Mask)` and `render(Real)` of one `BootPlan` differ only at the secret bytes.
- Plan mode: no `prepare`, no mint, no Tailscale call, no `acquire`. A dry run costs and creates
  nothing. A plan-mode provider refuses a direct `acquire` (either lock failing alone is caught).
- The Lambda launch body has `user_data` and exactly one ruleset id with no inbound rule; a render
  over the size limit is refused `Shape`. Template golden tests cover the 7.3 hardening list.
- Moved Lambda tests pass unchanged: idempotent acquire, `Unknown` is not `Gone`, `max_machines`,
  half-marked instances block launches.
- One layer: a fake provider records the `AcquireRequest` it receives; the same launch under
  `default` and under `tailscale` gives calls that differ only in the payload bytes, and the 8.6
  path check finds no network type in any provider crate. `network.kind = "none"` or `"script"` is
  refused with `known: default, tailscale`. A config with `status_token_file` is refused as an
  unknown field.
- `advertise_url` `http://js.example:7300` with an elastic pool: refused at start naming
  `server.advertise_url`, for modes `default` and `tailscale`. An elastic pool with no
  `agent_listen`, or `agent_listen` equal to `listen`, is refused naming `server.agent_listen`.
  `listen = "0.0.0.0:7300"` is accepted and shows the `server: warning: binds all interfaces` status
  line (default mode on a VPC stays supported).
- Route split (PR 4), against a real server with both ports bound: `hello` and heartbeat
  answer on `agent_listen` and get 404 on `listen`; `GET /v1/pools`,
  `/status`, the client routes and the operator routes answer on `listen` and get 404 on
  `agent_listen`. Catches a route left on the wrong port, which would let a tag that may reach
  only the agent port reach operator routes. With one port in the test the same check cannot fail,
  so the test fixture binds two.
- A pool with no `max_spend_per_day` is refused; a render of the template with a forced failure
  at step 3 still runs the EXIT trap (the golden test asserts the trap is the script's first
  command and that it removes every path in the cleanup list).
- `Offer::fit` refuses an offer whose os or arch differs from the request's.
- Tailscale: the key request is ephemeral, not reusable, pre-approved, tagged, ten minutes; the
  OAuth token is reused until near expiry and refetched after the injected clock passes it.
- An agent reporting no `runtime` is refused `host-contract`; a `sandbox-exec` pool refuses a
  `container` hello with `execution-mismatch`.

### 13.2 Scaler (PR 8), on the pure `step`

- One waiting request: one `Acquire`; the same input plus the record: still one. 100 waiting
  requests with `max_instances = 3`: never more than 3 records and acquires over any ticks.
- Reserved demand: one busy host and one waiting request holding a reservation on it
  (`reserved = Some(host)`): zero `Acquire`; after the reservation is given up: one. Two waiting
  requests and one reserved host: one. A free host assigned to a request covers it. Catches buying
  a machine for a request that has one.
- Restart between record and call: a `list` showing the instance gives no second `Acquire` and no
  second token. A `list` lacking it gives none before `unknown_hold`; after, Failed, still counted
  against the caps until two clean lists `unknown_hold` apart. An instance appearing after Failed
  is adopted, released and never uncounted. `Unknown` then a list showing it: Booting, same cloud id.
- Caps: `max_instances` with an Unresolved record and an orphan counted; `held == None` blocks;
  `max_spend_per_hour` just under sum-plus-candidate blocks; the stored price is summed, not the
  current catalog price; an unpriced offer is never chosen. `max_spend_per_day`: a failed boot counts
  for `boot_timeout`; a record that never reaches Gone keeps accruing and blocks the next launch;
  a record leaves the window when it ended more than 24 hours ago, and one that still runs counts
  the part of its run inside the window. A machine launched 30 hours ago and still running costs
  its last 24 hours, not its 30.
- Choice: cheaper first, then fewer gpus, id, zone. A hello below its offer starts
  `boot_fail_cooldown` and the host is drained and then released once it is quiet (a host with work is
  left until it is). No fitting offer: no launch, `Launchable` false, `no_offer` names the dimension.
- Idle and drain: a quiet host is drained; a request arriving meanwhile is not booked on it, and the
  next tick emits `Undrain` (not `Acquire`). A host with a running job is never released;
  `drain` and `release_host` fail `Busy` with a lease, run or stream. `js-sim` interleavings:
  (a) lease placement vs `drain`; (b) lease placement vs `release_host`; (c) a direct run start vs
  `release_host`; (d) a run started by a lease booked before the mark. In every order either the work
  runs and the host is kept, or the host is released and the work is refused.
- Release: `Revoke` precedes `Release` and follows `ReleaseHost`; `Released::Unknown` retries every
  tick and never removes; `Gone` removes and revokes once. Booting past `boot_timeout` is released and
  starts `boot_fail_cooldown`; on an incomplete registry view it is held. A Lost host the provider
  calls `terminated` is removed. A silent host that books nothing is drained and released after
  `unknown_hold`; one that shows work is never released. Failing health
  or `Checking`: no launch, running hosts still released.

### 13.3 End to end (PR 9), in process, with fakes

- From a config file: a request waits; a tick launches; the fake agent reads the recorded
  `user_data`, extracts token and instance line, says hello; the lease is granted. The clock moves
  past `idle`; the fake shows the token revoked (a hello with it gets 401) before terminate, then
  terminate, then the device deleted.
- Binding: right token with another instance gets `join-token-instance`; the right name succeeds
  once; a second host id gets `join-token-used`. A hello naming the right instance with a copy of the
  token (the holder case) succeeds before the first hello: the test asserts the binding is a mix-up
  guard and does not claim more. After the first hello the token alone gets 401 and the host credential
  works. Welcome lost on the wire: the host is released at `boot_timeout`, no second host registers.
- A refused OAuth client: the service starts, the pool is unavailable with the reason, `/healthz`
  is 200, a static pool still serves. Restart with a Launching record and `Unknown`: no second machine.
- `default` mode (F22; Lambda needs `tailscale` unless the operator provides reachability). The
  rendered plan for a `default` pool has no network steps and no network secret slot. (a) The fake
  agent is not started (no route to `advertise_url`): the host is Booting with "no hello", released at
  `boot_timeout` with the token revoked, `boot_fail_cooldown` blocks the next launch, status shows
  "booting, no hello". (b) The fake agent is started (the operator's own route): hello, lease granted,
  same path as the `tailscale` run without any Tailscale call. Catches a `default` pool that
  hangs or bills past `boot_timeout`, and a `default` render that still contains a Tailscale step.
- Status at each step carries state and the masked plan and none of the test secrets.

### 13.4 Live (not in CI)

After the operator's go: one launch, `max_instances = 1`. Before it, a person checks that the instance
accepts no inbound connection, including port 22 under Lambda's account-wide rules. A person also
records the cloud-init file list from the stock image (7.3 step 7). The checklist then asserts, from
a launched host, that a connection to another tailnet node and to the scheduler's `listen` port
(client, operator, status routes) both fail while the agent port succeeds (8.3), which is the only
test that the operator's policy and the route split work together; from inside a job container, that the metadata address cannot be read (7.3 step 8);
that no cloud-init path in the cleanup list still holds the token; and from status and Lambda's list: our tags and name; a hello arrived; the lease ran; the
instance terminated and read back `Gone`; the device gone from the tailnet; the token revoked.
This proves the open items on the stock image, the state root, the API field names and the
user_data limit.

## 14. Open questions

Each has a recommendation. None is a requirement yet. Items marked PROPOSAL in the text are here.

1. Server flags into `[server]` (PR 10). Requirement 9 says configuration is a file plus starting the
   service; flags and file coexist until PR 10 because the consumer runs on flags. Recommendation: move
   them, refuse other flags beside `--config`. Every unit that passes flags changes once.
2. Lambda API names for `user_data` and firewall rulesets, and the user_data size limit. The crate
   has fixtures only for launch, instances and instance types (`tests/fixtures`). Recommendation:
   PR 3 adds fixtures from the live API reference; the live run is the real check.
3. ssh key (5.3, 4.1). Requirement 8 needs exactly one key name and requirement 10 wants no hand-made input.
   Without a requirement the pool is unavailable on an account with no key until a person adds one.
   Question: may the service create one key named `js-<scheduler>` from a throwaway public key
   (private half never stored), when the account has none? It writes to the account. "Never create"
   conflicts with requirement 10. Recommendation: yes, create it; the key is
   inert because the attached ruleset has no inbound rule.
4. Join token after first hello (7.2). In the first slice (PR 7b). Open point: the
   `Welcome` wire change is additive, but is a host credential acceptable as a second
   secret type next to the token? Recommendation: yes.
5. The stock Lambda image: driver, runtime, toolkit, and a state root on its own device (loop
   image). Not verified. Recommendation: the install table starts empty, the live run fills it, and
   the bootstrap fails visibly rather than guessing.
6. Where the host gets the agent binary. Decided: from the operator's pin per architecture
   (`agent` in the pool, 7.3), checksummed; the service serves no download.
7. Provider hierarchy (`providers.md`). Recommendation: one provider per pool, pools chosen by
   label; add pool priority only if a consumer needs fall-through.
8. Orphans: report-only, adopted and counted. Recommendation: a `release_orphans = true` pool
   setting only after the live run is clean; auto-delete by default needs a maintainers' decision.
9. Stuck-drain cost: `max_spend_per_day` is a required key (6.2). Remaining question: is a day
   window the right hard bound, or do the maintainers want a lifetime total per pool? Recommendation: keep
   the day window, add a lifetime figure only if asked. A tainted host is not cut either; it is shown
   for a person.
10. Lambda's account-wide firewall rules and inbound 22: a human check before the first live
    launch (9, 13.4).
11. The 10-minute Tailscale key against boot time. Recommendation: keep 10 minutes, measure live.
12. Status and client access (10.2): the status and client APIs are served only on the private or
    tailnet address, and access is the network's job. No status token, no new client key; the
    existing client token stays. Recommendation: accept.
13. Plain `http://` over a tailnet: the agent refuses non-loopback `http://` (`cli.rs`), so start-up
    refuses it for every elastic pool (4.4). The cost: a tailnet scheduler needs a TLS certificate for
    its tailnet name from the operator. Recommendation: accept.
14. Defaults no requirement sets: `launch = "plan"` default with the provider's second lock; `max_step`
    1; `boot_fail_cooldown` 10 min fixed; `unknown_hold` 30 min; the pool floor; one gpu class per
    pool; gpu class from `types` rows.
    Recommendation: accept, each a setting.
15. `max_spend_per_hour` as USD. Prices are `price_cents_per_hour`; `hosts.md` says there is "no
    dollar model". Recommendation: the cap is in the kind's `price_unit`; an unpriced kind gets no
    elastic pool until it declares a unit.
16. `require_digest` on elastic pools (3.1). Recommendation: yes; a tag can change under a fleet.
17. Amend `providers.md` K6 and `registration.md` "never in user data" for Lambda. The AWS path
    (signed instance identity) is unchanged.
18. Several elastic pools and part-host classes (6.5). Recommendation: after the live proof.
19. Modes beyond `default` and `tailscale` are Later (section 12). Recommendation: none until a pool
    needs one.
20. Deviations from requirements (1.1): one registering pool instead of several, gpu class from a
    `types` row (disk and `exec` on the job were deviations too, and PR 11 has landed them).
    Recommendation: accept for the first slice; PR 12 lifts the first.
21. A reservation counts as covering however long its `drain_deadline` is (6.2). Alternative: launch
    when the reservation's deadline is further away than `boot_timeout`. Recommendation: covering
    always in the first slice; revisit with live data, since the alternative can buy a second machine.
22. A policy-read warning in `check()` (8.3). Not in the first slice. Recommendation: the warning later.
23. Two ports (3.4, 8.3, 4.4). The agent routes and the operator routes share one port today, so a
    network ACL cannot give a launched host the first and not the second. Decision for the first
    slice: `server.agent_listen` is required with an elastic pool and serves only the agent routes;
    `listen` serves the rest. No wildcard-bind refusal: a wildcard is normal on a VPC or in a
    container and an offline check cannot tell whether it is publicly reachable, so the server warns
    and shows it in status. Open risk: separation depends on the operator's policy giving the tag the
    agent port only. Recommendation: accept.

## 15. Not verified

Read from code and docs only; nothing was built or run. Not checked against a live system:
Lambda's launch field names, user_data limit and ruleset behaviour; Tailscale's OAuth and
auth-key endpoints and scopes; the contents of the stock Lambda image; whether `losetup`
works there. The wording of refusals in 4.4 is illustrative. The line numbers of `registry.rs`
and `join_tokens.rs` were read at 73132d5f7 and will drift.

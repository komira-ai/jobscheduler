# Configuration example

Companion to [docs/design.md](../design.md), section 5.7. The rules that govern this file are in 5.7; section numbers refer to design.md.

## The server's file today

The server (`js-lease-server`) reads one file, `--config <path>`, in place of its flags: a `[server]` table and `[[pool]]` tables, each
refusing a field it does not know, with the secrets as paths of owner-only files. The format and every refusal are in
[pools-providers-network.md](pools-providers-network.md), section 4; the crate docs list the keys. The flags still work when `--config`
is absent. With it, **any other flag is refused**, naming the first one on the command line and the key that takes its place, with exit
status 2 and before the file is read: `--listen: not allowed with --config; set server.listen`.

| flag | in the file |
|---|---|
| `--listen`, `--state-file`, `--boot`, `--default-ttl`, `--max-ttl`, `--tick`, `--tls-cert`, `--tls-key` | `server.<name>`, the flag's name with `_` for `-` |
| `--max-connections`, `--handshake-timeout`, `--header-timeout`, `--read-timeout`, `--write-timeout`, `--max-requests`, `--drain-timeout` | `server.<name>`, same defaults |
| `--agent-hello-burst`, `--agent-hello-interval`, `--require-host-credential`, `--placement`, `--estimate-feed`, `--cpu-slack` | `server.agent_hello_burst`, `server.agent_hello_interval`, `server.require_host_credential`, `server.placement`, `server.estimate_feed`, `server.cpu_slack`, same defaults (`placement` is `"registry"` unless the file says `"js-sched"`; `"js-sched"` is refused, as the flag is, for a joined pool whose capabilities name no `arch`, and not for an elastic pool, whose floor names none: each of its machines is placed by the floor's numbers and the capabilities its agent reported at hello, `arch` among them; `estimate_feed` is `false`, and `true` is refused beside `usage_off = true`, as the flags conflict; `cpu_slack` is a number from `1.0` to `2.0`, js-sched's default `1.1` when absent, and is refused unless `placement` is `"js-sched"`); refused unless a pool registers agents |
| `--usage-db`, `--usage-off`, `--profile-store-bytes`, `--history-retention-days` | `server.usage_db`, `server.usage_off`, `server.profile_store_bytes`, `server.history_retention_days`, same defaults and rules (`usage_db` is refused beside `usage_off = true`, as the flags conflict; the budget is at least 1; the retention is a whole number of days, `90` by default, at least 1) |
| `--job-retention` | `server.job_retention`, same default (`"7d"`) and rules: how long an ended job's record and result (exit code, reason, log tails, timing, usage) are kept after the job ends, as a whole count of days or a duration such as `36h` or `90m`, at least one minute; zero, a negative or unparsable value, or less than a minute is refused at start naming the flag or the key. It is read once at start and is the window the job store (`js-jobs`) applies when a job ends, so a changed window affects only the jobs that end after the restart. The job store of `--job-listen` is opened with it; without `--job-listen` the setting is checked and kept, and nothing reads it. Allocation history is not part of it |
| `--job-feed-retention` | `server.job_feed_retention`, same default (`100000`) and rules: how many of the newest changes of the job store's change feed each tick keeps, a whole number of at least 1 (a count of feed changes, never a time); zero or a non-number is refused at start naming the flag or the key. Older rows are trimmed by the job pass of the tick, and a reader whose cursor is behind them is refused with a stale-cursor error and starts again from the tables. It is read once at start and applies to the job store of `--job-listen`; without `--job-listen` the setting is checked and kept, and nothing reads it |
| `--job-allocation-retention` | `server.job_allocation_retention`, same default (`14d`) and rules as `--job-retention`: how long a freed allocation's row is kept after its free, a whole count of days or a duration of at least one minute. The tick deletes freed rows past it, at most 500 a tick; booked and releasing rows, and the usage history of an allocation, are never deleted. It is read once at start and applies to the job store of `--job-listen`; without `--job-listen` the setting is checked and kept, and nothing reads it |
| `--job-listen` | `server.job_listen`, off unless given: the address the job service is served on (see "The job service's port" below); off the loopback it needs `server.tls_cert` and `server.tls_key`, the same pair as `server.listen`; refused unless a pool registers agents |
| `--metrics-listen` | `server.metrics_listen`, off unless given: the address the operator's metrics are served on (see "The metrics port" below); a private address only, plain HTTP, no token; an address that is not a private one is refused at start naming the flag or the key; needs no agent registration |
| `--max-job-wall` | `server.max_job_wall`, `"30d"` unless given (the job service's default, the longest `server.max_lease_wall` accepts; it does not follow `server.max_lease_wall`): the most a submitted job's wall limit may be, as a whole count of days or a duration such as `12h` or `90m`, at least one second; zero, a negative or unparsable value, or less than a second is refused at start naming the flag or the key. A job asking for more is refused `wall-time-above-ceiling`. It is read once at start; without `--job-listen` it is checked and kept, and nothing reads it |
| `--store`, `--allow-sqlite-store` | `server.store` and `server.allow_sqlite_store` (`false` unless given; `true` is refused without `server.store`, as the flag is without `--store`): the store the server's state is to be kept in, and the gate of its SQLite backend; see "The store" below |
| `--store-unreachable-after` | `server.store_unreachable_after`, `"60s"` unless given: how long the store paths of the tick (the lease file's write, the job pass) may fail tick after tick before `/healthz` answers 503 `store unreachable` and the alert `store:unreachable` holds; the first tick whose store work succeeds clears both; zero or an unparsable value is refused at start naming the flag or the key |
| none (the file only) | `server.alert_webhook_url`, `server.alert_webhook_token_file` and `server.alert_repeat_after`: every alert of the scaler, and the store's `store:unreachable`, is also POSTed as `{source, key, text}` JSON to the URL (`https`, or `http` to a loopback address) when it begins to hold, again every `alert_repeat_after` (default 1h, plus up to one tick and the receiver's latency) while it holds, and again if it clears and returns; delivery is off the tick path; the token file is owner-only and holds the bearer token the receiver wants; both other keys are refused without the URL |
| `--token-file` | `server.client_token_file` |
| `--client-token NAME=PATH` or `NAME:ROLE=PATH` (repeatable) | a `[[client]]` table with `name`, `token_file` and, optionally, `role` (`"client"`, the default, or `"operator"`): one more client of the job service (see "The job service's port" below); refused without `--job-listen` (`server.job_listen`) |
| `--pool-config` | a `[[pool]]` with `provider = { kind = "static", hosts_file = ... }` |
| `--agent-pool-config` | a `[[pool]]` with `kind = "joined"` for machines started by hand (the pool's shape and timing are its keys), or with an elastic provider for machines the server launches |
| `--agent-token-file` | `join_token_file` of that pool |

### The job service's port

`--job-listen <addr>` (`server.job_listen`) serves the job service (`jobscheduler.v1.JobService`) on a port of its own. It is off
by default: without it the server binds no other port, opens no job store and behaves as before. On, one listener speaks HTTP/1.1
and HTTP/2 on that port: a request whose content type is `application/grpc` goes to the gRPC service, every other request to the
HTTP/JSON gateway ([http-gateway.md](../http-gateway.md)). Both are behind one check of the bearer token of a client; a request
without one is answered `UNAUTHENTICATED` (gRPC) or 401 with an empty body (HTTP). One port for both protocols keeps one address, one
TLS setup and one connection cap for the service, whichever client calls it.

The clients are `default`, whose token is the one of `--token-file`, and one per `--client-token NAME[:ROLE]=PATH` or `[[client]]`
table:

```toml
[[client]]
name       = "ci"                             # 1 to 40 bytes of letters, digits, '.', '_' and '-'; unique; not "default"
token_file = "/run/credentials/js/ci-token"   # owner-only (0600 or 0400), one token of at least 32 bytes, as client_token_file
role       = "client"                         # optional: "client" (the default) or "operator"; anything else is refused
```

A start is refused, naming the entry and never the token, for two clients of one name, a client named `default`, a name that is not
one, a short token, a token file others can read, the same token in two entries, and a client token that the join token file of the
pool also holds. The server keeps each named client's token only as its SHA-256 digest, compares a presented token with every
client's, records the client's name on each job it submits (`Job.client`, and the `client` filter of `ListJobs`), and logs the
client, its role, the target and the outcome on each call's line. A `client` reads, lists and cancels only its own jobs and makes no
fleet call; an `operator`, and `default` always, may call everything on every job. An entry with no role is a `client`, so an entry
whose holder calls the fleet calls or cancels other clients' jobs needs `role = "operator"` (the role model is in
[http-gateway.md](../http-gateway.md), "Clients, roles and token reload"). Every tick reads each named client's token file again:
to rotate or revoke a token, write the new token into its file, and the old one is refused from the request after the next tick;
a file that cannot be read or holds no valid token keeps every token in force, with a warning. Adding or removing a client, or
changing its role, takes a restart (a restart ends no job); `default`'s token is read at start only, with the lease routes'.

- **TLS.** The rule of `--listen`: an address off the loopback is refused at start without `--tls-cert` and `--tls-key` (the same two
  flags, not new ones), with a message naming `--job-listen` and saying that a bearer token never crosses a network in clear. With
  them the port speaks TLS over the same certificate and key, reloaded with the others on SIGHUP, and offers HTTP/2 (`h2`) and
  HTTP/1.1.
- **Limits.** The port has the connection cap and timeouts of the other listeners, and stops with them. An address in use is refused
  at start naming the flag.
- **Agents.** The flag needs `--agent-pool-config` (the key, a pool that registers agents): the job store is held by the agent side,
  and jobs run on hosts that register.
- **The job store.** Turning the port on opens the job store: today a SQLite file beside the lease file, `<state file>.jobs`, owned
  by the server's user and owner-only (mode 0600), with SQLite's own files beside it. It is the single-node development backend: the
  file moves into the shared store when the server's state does. A file that cannot be made owner-only or opened as a state store
  is refused at start naming `--job-listen`. With the store open, the heartbeat of an agent that reports the `per-job` capability is
  also served from it: its job listing is committed and it is told the starts and stops of its jobs. Without `--job-listen` such a
  heartbeat gets the lease and run reply alone, as before.
- **The job ceiling.** `--max-job-wall` (`server.max_job_wall`, 30 days unless given) is the most a submitted job's wall limit may
  be; every job must carry a wall limit, and one above the ceiling is refused `wall-time-above-ceiling`.
- **Not shared with `--listen`.** The lease routes stay on `--listen` and the job routes are not served there: the gateway's
  `/v1/hosts` and `/v1/pools` are other routes than the lease server's of the same names, so the two cannot share a port while the
  lease API exists.

A pool of hand-joined agents is one `[[pool]]` of kind `joined`. It launches nothing, so it has no `provider`, `network`, `launch`,
`max_instances`, `idle`, `boot_timeout` or spend key, and each of those is refused by name. It registers agents;
`server.agent_listen` is required as for an elastic pool. A file may declare any number of joined and elastic pools, each with its own
`join_token_file`, but the server starts only on a file with at most one pool that registers agents: with more it refuses naming the
second (`several elastic pools are parsed; running them needs the ordered walk of design 5.6`, or `several pools register agents; ...`
when they are not all elastic). At most one pool of the agentless `static` provider kind is served; that kind is deprecated, and a later
version removes it.

```toml
[[pool]]
kind  = "joined"
name  = "nodes"                       # the `pool` of a hello, and the `provider` of a one-time token
class = "whole-node"
exec  = "container"                   # optional; absent or "none": no host contract
shape = { cpu_millis = 16000, memory_bytes = 68719476736 }   # optional keys: gpus, gpu_class
capabilities = { os = "linux", arch = "x86_64" }               # optional; as the agent pool file's [capabilities]
max_machines    = 6                   # required; from 1 to 262144, as in the agent pool file
memory_schedulable_percent = 90          # optional; from 1 to 100, default 100: the share of the shape's memory the pool books
join_token_file = "/run/credentials/js/join-token"           # optional shared pool token
heartbeat_interval = "10s"            # optional; as are grace ("1m") and lost_retention ("24h")
```

A node joins with a one-time token (the mint refuses any other `provider` than the pool's name with a 400): `POST /v1/agent/join-tokens` with the client token and `{"provider": "nodes", "instance_id": "node-1"}`
mints it, and the node's `--instance-file` holds `nodes:node-1`. The first hello swaps the token for a host credential. What the old
`--agent-pool-config` file had and this kind lacks: the whole-node keys (`max_whole_node`, `drain_after`, ...).

**Memory headroom.** One key, `memory_schedulable_percent`, a whole number from 1 to 100 (default 100), sets how much of the memory an
operator configured the server books; there is no other memory knob. It is written in three places, each in its own format and read the
same way (0, 101, a negative number or a word is refused at the check that reads it, naming the key):

| where | what it cuts |
|---|---|
| a `[[pool]]` of the `--config` file | the hosts of a static pool (the pool's key replaces the hosts file's, once, never on top of it; it may be larger as well as smaller than the file's own value), and the floor memory (1 GiB) of an elastic pool |
| the hosts file of a static pool (top level, before the first `[[host]]`; `--pool-config` or a `hosts_file`) | the `memory_bytes` of every host the file lists |
| the agent pool file (`--agent-pool-config`; top level, before `[shape]`) | the `[shape]` `memory_bytes` the pool books for each host |

The server books `floor(memory x percent / 100)` bytes, computed once where the booked shape is built, and every booking path sees that
number: a lease, a whole-node lease, a run's memory limit, a reservation and the host listing. It applies to the memory the operator
configured and never to a figure an agent reports about itself: a host that claims more than the pool lists is still booked at the cut. A
host's hello is held to the whole listed memory, not the cut (a host must really have the memory; the pool books less of it). `GET
/v1/hosts` shows, per host, `memory: {configured_bytes, schedulable_percent, bookable_bytes}` (`shape.memory_bytes` is the bookable
figure), and `GET /v1/pools` and `/status` show the same three for an elastic pool's floor.

Under `--placement js-sched` the percent of an agent pool (the agent pool file, or a `[[pool]]` whose hosts register) is the placement
engine's rule instead: the registry's book takes each host's whole `[shape]` memory, and the engine books no more than
`floor(memory x percent / 100)` of it. What fits the pool is still judged at the cut, so a request above it is refused `no-fit`, and
`GET /v1/hosts` shows the same as under `registry`: `shape.memory_bytes` is the cut, beside `memory`. A static pool is not affected.

### The store

`--store <path>` (`server.store`) is the `store` setting of the design ([jobscheduler-design.md](jobscheduler-design.md), section 4.1:
"The Postgres connection, as a path to a file that holds it. SQLite is accepted only for tests and single-node development."). It is
optional for now. With it the host bindings and the join tokens' digests are kept in the store (see "What it keeps" below); the lease
file, the job store and the usage file are kept as without it.

- **The file.** One line: `postgres://...` or `postgresql://...` for Postgres, or `sqlite:<absolute path>` for SQLite. Blank lines and
  the whitespace around the line are ignored; anything else (two lines, another scheme, a relative path) is refused. The file must be
  owner-only, as `--token-file`'s is, and its content is never printed: a refusal names the file and says what is wrong, a URL that
  does not parse is refused as one, and a Postgres server's refusal is reported by its SQLSTATE alone. The URL's `sslmode` (`disable`
  to `verify-full`) and `sslrootcert` choose its TLS. A Postgres store must hold the state schema already; the server creates none, and
  a database without it is refused when the lock is taken.
- **The dev flag.** `--allow-sqlite-store` (`server.allow_sqlite_store = true`) accepts a `sqlite:` store. Without it such a store is
  refused at start with exit status 2: `--store: <file> names a sqlite: store, which is accepted only with --allow-sqlite-store: SQLite
  is for tests and single-node development`. With a Postgres store the flag is accepted and does nothing, so one file serves both kinds
  of environment. Like every flag, it is refused beside `--config`.
- **The lock.** The server takes the store's writer lock before anything else (before it reads or locks the lease file and before it
  binds a listener) and holds it for its life: a session-level advisory lock on a connection of its own on Postgres, the lock file
  beside the store's file on SQLite. It tries once and does not wait.
- **A refused start.** A second server on the same store exits 2 with one line, `--store: another process holds the store's writer
  lock`, having bound nothing and opened neither the store nor the lease file. A lock that cannot be tried (the server is down, the
  login is refused) is `--store: the store's writer lock could not be taken: ...`, also exit 2.
- **A lost lock.** Every tick asks the lock's session first. When the session is gone (the database restarted, an administrator ended
  it, the connection broke), the lock is gone with it: the tick does nothing more, the server stops serving and ticking, writes
  nothing more (there is no final tick), logs `the store's writer lock was lost, so the server stopped: ...` and exits 1. Taking the
  lock also takes the store's next writer generation, and every commit checks it: once another process takes the lock, no commit of
  this one succeeds, between ticks too.
- **What it keeps.** The host bindings (with each host's taint and cordon) and the digests of the join tokens and host credentials:
  `hosts/binding/<host id>` and `hosts/join/<digest>`, each a JSON object with a `version`, whose unknown fields are kept and written
  back; and the scaler's records and the hosts the registry refused to remove: `acquires/scaler/records` (the records file's own
  format, written whole) and `acquires/scaler/unremoved` (`{"version":1,"hosts":[...]}`). They are read once at start, after the lock;
  `<state file>.agents`, `.joins`, `.scaler` and `.scaler.unremoved` are then not written. Each change is committed before the server
  holds it: a hello that uses a token commits the use and the host's binding in one transaction, and a refused hello gives both back in
  one more; a taint, a cordon or its lift, a mint and a revoke each commit first; the scaler commits its records before the first
  provider call of a step, and a commit that fails stops the step as a failed file write does. A failed commit changes nothing and is
  answered as the failed file write of the same change is, or with 503 `state-write-failed` where no file write is checked (a hello
  with a pool token, a heartbeat's taint). A heartbeat that brings no new taint, a known host's hello and the host book make no store
  call (a known host's hello commits only when its stored binding still holds a cordon that lapsed since the last tick); a host
  credential's renewed time is committed by the tick. A live pool that is not at its first start does not start without the records
  key, as it does not without the file. The lease file, the job store and the usage database stay where they are.
- **The first start.** Right after the lock, a start over a store with no `meta/server-state-imported` marker and nothing under
  `hosts/` or `acquires/` imports the four files once: it reads them with the readers a server without the setting uses (a missing
  file is empty; a file of a bad version, a damaged file or one others can use is refused at start, naming it, and nothing is
  written) and writes their bindings, join-token rows, records text and hosts with the marker in one transaction. The marker is written
  even when there are no files. A start that finds the marker reads none of the files and names each one still there once at info
  level. A store that holds server state without the marker gets the marker only. The server never deletes or changes the files: they
  hold what it held at the cutover and go stale from its first write after it, so a rollback to a build without the setting finds
  them as they were then; an operator removes them. The lease file is not imported.

```toml
[server]
store              = "/run/credentials/js/store"   # owner-only; one line: postgres://... (or sqlite:/var/lib/js/state.db)
allow_sqlite_store = false                         # true only for tests and single-node development
```

### The metrics port

`--metrics-listen <addr>` (`server.metrics_listen`) serves `GET /metrics` on a port of its own, in the Prometheus text format
(version 0.0.4, content type `text/plain; version=0.0.4; charset=utf-8`). It is off by default: without it the server binds no other
port and counts nothing. Any other path, on that port, is answered as an unknown route, and the lease port does not serve `/metrics`.

| family | kind | labels | what |
|---|---|---|---|
| `js_jobs` | gauge | `state` | the jobs in the job tables, by state (`queued`, `placed`, `starting`, `running`, `succeeded`, `failed`, `cancelled`, `lost`, `failed-to-start`); only with the job service on |
| `js_queue_oldest_age_seconds` | gauge | | how long the oldest queued job has waited since its submit, 0 with none queued; only with the job service on |
| `js_job_placements_total` | counter | | the queued jobs placed on a host |
| `js_queue_wait_seconds` | histogram | | each placed job's wait from its submit to its placement |
| `js_job_start_failures_total` | counter | `code` | the jobs that failed to start, by code (`no-capacity`, `start-timeout`, `exclusive-not-ready`, `agent-refused`) |
| `js_jobs_lost_total` | counter | | the jobs lost: their host read gone by the job pass, or their container missing from the first listing after a new hello |
| `js_store_write_failures_total` | counter | | the passes over the job tables the server makes itself (the job pass, a placement, a heartbeat's job listing) that failed, so their write did not commit; a client's submit or cancel that fails is answered to the client |
| `js_job_pass_duration_seconds` | histogram | | how long each job pass of the tick took |
| `js_acquires_total` | counter | `pool` | the launches the scaler ran, whatever their answer (a launch refused before the provider is called, such as one for an architecture with no pinned agent, included) |
| `js_acquire_failovers_total` | counter | `pool` | those refused for capacity or quota, after which the scaler tries another offer |
| `js_host_cpu_booked_millicores`, `js_host_cpu_free_millicores` | gauge | `host` | each host's CPU booked (its leases and its jobs' live allocations; one that books the host whole counts as its shape) and free (its shape less that, and never below 0) |
| `js_host_memory_booked_bytes`, `js_host_memory_free_bytes` | gauge | `host` | the same of memory, of the bookable memory (`memory_schedulable_percent`) |
| `js_host_whole_mark` | gauge | `host`, `state` | 1 for each host set aside whole, by its mark (`reserved`, `held`, `draining`, `releasing`) |

The counters count from the start of the process; a start failure or a loss is counted as its commit lands, so a pass that fails later
still counts what it ended. The gauges are read at each scrape from the state `GET /v1/hosts` and the job service read, holding no lock
of the server's while the text is written; a scrape whose read of the job tables fails is answered 503. The parts are read one after
another, not in one transaction, so a job placed between two reads can show as queued while its booking already counts. The text
names no job, lease, run or token: its labels are a state, a failure code, a host's id, a pool's name and a mark.

- **Address.** The route takes no token, so it is served only on a private address (the rule of `/status`: loopback, RFC 1918, the
  shared range a tailnet uses, link-local and IPv6 unique-local; a wildcard is accepted), and on each request only to a peer whose
  address is private too; any other peer gets the answer of an unknown route. An address that is not a private one is refused at
  start. The port needs no TLS and speaks plain HTTP whatever the other ports speak: bind it to a private or tailnet address.
- **Limits.** The port has the connection cap and timeouts of the other listeners, and stops with them. An address in use is refused
  at start naming the flag or the key.

### Machine providers, platforms and pool settings

A pool may name container platforms in priority order instead of naming its provider (design `jobscheduler-design.md`, section 4.2).
`[[machine_provider]]` tables declare where machines come from: `kind = "static"` is the hosts that run `js-agent` and register
themselves (no key beside `name` and `kind`; it is not the agentless `static` provider kind of a pool's `provider`), and any other kind is
a provider kind compiled in, whose own keys follow (`lambdalabs`: `api_key_file` and the rest, as under `provider`). `ec2` and `gce` are
built, with the keys of `js-provider-ec2` and `js-provider-gce`. `[[platform]]` tables declare platforms: only `kind = "default"` is built (`kubernetes`, `cloud_run` and `ecs` are
refused as not built), with `machine_providers` listing declared machine providers first to last. Names follow the grammar of pool
names and are unique among their kind of table; every declared machine provider must be named by a platform and every platform by a
pool.

```toml
[[machine_provider]]
name = "rack"
kind = "static"

[[machine_provider]]
name = "lambda"
kind = "lambdalabs"
api_key_file = "/run/credentials/js/lambda-key"

[[platform]]
name = "ours"
kind = "default"
machine_providers = ["rack", "lambda"]  # first to last

[[pool]]
name = "gpu"
class = "whole-node"
platforms = ["ours"]                    # first to last; refused beside provider
max_machines = 4                        # max_instances is the same cap by its older name; giving both is refused
max_spend_per_hour = "12.00"
max_spend_per_day = "96.00"
max_queue_wait = "30m"                  # default 1h
start_timeout = "15m"                   # default 15m
path_grace = "15s"                      # default 15s

[pool.provider_limits.lambda]
allowed_instance_types = ["gpu_1x_a10"] # names, none empty, none twice
max_price_cents_per_hour = 200          # 1 or more
max_machines = 2                        # 1 or more, at most the pool's
max_spot_machines = 1                   # at most this table's max_machines
quota_backoff = "1h"                    # default 1h; a positive duration, zero is refused
acquire_retry = "60s"                   # default 60s; a positive duration, zero is refused
catalog_refresh = "1h"                  # default 1h; a positive duration, zero is refused
offer_cooldown = "1h"                   # default 1h; a positive duration, zero is refused
```

A pool written with `provider = { kind = "lambdalabs", ... }` is read as exactly these tables: a machine provider made from its provider
table, the `static` machine provider, and a `default` platform naming the two, `static` first. The loader's names start with `+`, which no
name in the file may hold. This version keeps one fixed order of hosts, registered hosts first and then the elastic provider, so a
platform that lists a launching machine provider before `static` is refused (`machine_providers: static must come first in this version
(the ordered walk of design 5.6 is not built yet)`), and a pool's platforms must reach exactly one machine provider that launches.

**Parsed and checked; what acts on them.** `max_queue_wait`, `start_timeout`, `path_grace` (each a positive duration) and the
`provider_limits` tables are read and checked at start. Admission of a lease request (design 5.6, step 2) reads the limits: an offer
whose instance type is not allowed, whose price is above the maximum, or that has no price under a maximum does not admit a request, and
a provider whose `max_machines` (else the pool's) is zero admits none. `path_grace` is how long a waiting whole-node request of the pool
keeps its place once no host could serve it, after which it is refused `no-capable-host` (it is the registry's `regain_grace` for the
pool). The job pass of the tick (design 6.1, step 1) ends a job of the server's job tables that waits past `max_queue_wait` (counted
from when it was queued) as `no-capacity`, and a placed or starting one that has not started within `start_timeout` (counted from its
placement) as `start-timeout`; each deadline is held to at least the server's boot plus 120 s, so an outage fails no job at the first
tick. A joined pool has neither key and takes the defaults. Nothing acts on the limits yet: the pick of an offer when a pool acquires a
machine (design 5.6, pass 2) will read them. `catalog_refresh` is how long the provider's cached catalog is used before the tick fetches
it again; admission reads the cache and never calls the provider, a fetch that fails keeps the last catalog and is tried again at the
next tick, and a refusal to launch for capacity or quota makes a fetch due at once. `offer_cooldown` is how long an offer (an instance
type in a zone) that was cooled down is passed over by the pick of an offer, while admission still counts it; it is read and checked,
and nothing enters a cool-down yet (the check of a host's hello against its offer will). `quota_backoff` is how long, after the provider refuses a launch for quota, pass 2 passes over that
instance type in that region (in that zone, for providers whose zones are regions); the provider as a whole is not braked.
`acquire_retry` is how long a request waits when every offer was refused or passed over: it stays waiting and pass 2 asks again after
this long, or at once after a completed catalog refresh. Zero for any of the four is refused, naming the key: a
zero refresh would ask the cloud at every tick, a zero cool-down is none, a zero back-off passes over nothing, and a zero retry would
ask at every tick. A limit
names a launching machine provider of the pool's platforms (a `static` one launches nothing and is refused), and an unknown key in it is
refused.

## Live logs: `[logs]`

The table of [live-logs.md](live-logs.md), section 6, which gives each key's type, default and bound. It is in the `--config` file only,
beside `[server]`, and has no flags: a server run from flags has no sink. An unknown key is refused; `sink = "none"` (the default, also
when the table is absent) with any other key present is refused, naming the key. The keys are files, never inline secrets, and each
file is owner-only like every other key file (mode 0600 or 0400, owned by the service user), holding exactly
`{"access_key_id": ..., "secret_access_key": ...}`: an id of 1 to 128 letters and digits and a secret of 1 to 1,024 printable ASCII
bytes. The server reads and checks the table and both key files at start, offline, refusing a bad value naming its key and never
repeating it, and names the sink in its start log line (`logs=object_store` or `logs=none`, never a key). With `object_store` the
server mints each uploading host's credential with the mint key (or derives it from the signing key), delivers and renews it in heartbeat replies, and puts each job's sink
in its start (live-logs.md, sections 3.1 and 4). The read key serves `GetLogs` with a stream (live-logs.md, section 8; `docs/http-gateway.md`,
"Reading logs"): the server lists and reads the job's objects with it, under the prefix of the host the job ran on, and passes the bytes
through, and `max_follow` bounds how many of those calls with `follow` wait at once (one more is refused `too-many-follows`).

The rules the server checks at start: `url` is `s3://BUCKET` or `s3://BUCKET/PREFIX`, with the prefix of the object layout (live-logs.md,
section 2: no leading or trailing `/`, segments of `[A-Za-z0-9._-]`, none empty, `.` or `..`, at most 476 bytes, so that a host's
prefix fits under it); `region` is 1 to 64
bytes of `[a-z0-9-]`; `endpoint` and `sts_endpoint` are `https` URLs, or `http` to a loopback address, with no path, query, fragment or
user; `credentials` is `"sts"` (the default) or `"presigned_prefix"` (anything else is refused, naming the key), and a key of the
other kind is refused, naming the key: `sts_endpoint`, `role_arn` and `mint_key_file` belong to `sts`, `signing_key_file` (required)
to `presigned_prefix`, a store with no STS whose host credentials are prefix grants (live-logs.md, section 4.1); `role_arn` begins with `arn:` and is at most 2,048 bytes of
printable ASCII; `credential_ttl` is from 15 minutes to 12 hours; `max_follow` is from 1 to 4,096. `credential_ttl` must also be at most
the role's maximum session duration, which is the operator's to set in the store and which the server cannot check at start: STS
refuses a longer session, and the refusal shows as a mint failure.

```toml
[logs]
sink            = "object_store"                        # "object_store" | "none"; default "none"
url             = "s3://bucket/prefix"                  # required with object_store; the prefix is optional
region          = "us-east-1"                           # required with object_store
endpoint        = "https://objects.example"             # required: the store's URL; the server names no cloud's host by default
path_style      = false                                 # optional; true for stores that need /bucket/key URLs
read_key_file   = "/run/credentials/js/logs-read-key"   # required: list and get on the prefix; JSON {access_key_id, secret_access_key}
credentials     = "sts"                                 # how a host's credential is minted: "sts" or "presigned_prefix"
sts_endpoint    = "https://objects.example"             # optional; default endpoint
role_arn        = "arn:aws:iam::<account-id>:role/js-logs"   # required with sts; its maximum session duration at least credential_ttl
mint_key_file   = "/run/credentials/js/logs-mint-key"   # required with sts: the key that calls AssumeRole
# signing_key_file = "/run/credentials/js/logs-signing-key"  # required with presigned_prefix, instead of the three keys above
credential_ttl  = "1h"                                  # 15m to 12h
max_follow      = 256                                   # most GetLogs calls waiting at once
```

The agent's one flag for it is `--log-file-cap` (default 64 MiB, from 1 MiB to 1 GiB): the local file of each stream of a `crun` job
whose start carries a log sink, and for a native job with a sink the most bytes of each of its log files that are uploaded.

The rest of this page is the example of the scheduler's own configuration of section 5.7, which the server does not read yet.

```toml
[[shape]]                                   # what one host offers; pools and providers refer to it by name
name = "mac-studio-gpu"                     # GPUs exist only on the Mac Studio machines of the GPU pool (D32)
gpu_class = "apple-m"
[shape.resources]
cpu_millis = 24000
memory_bytes = 206158430208
gpus = 1                                    # the machine's one GPU, leased with the whole host
[shape.capabilities]
os = "macos"
arch = "aarch64"

[[shape]]                                   # a shape with no GPU
name = "linux-x86-16"
[shape.resources]
cpu_millis = 16000
memory_bytes = 68719476736
gpus = 0
[shape.capabilities]
os = "linux"
arch = "x86_64"

[[provider]]                                # the DefaultProvider: hosts that register themselves (5.8); it creates and destroys nothing
name = "registered"
kind = "default"
deny_hosts_file = "<path-to-deny-file>"     # host ids refused and evicted (5.8)

[[provider]]                                # launches and terminates instances
name = "cloud"
kind = "aws"
enabled = false                             # a provider that bills an account starts disabled (12.4)
role_arn = "arn:aws:iam::<account-id>:role/<role-name>"
region = "<region>"
launch_template = "<launch-template-name>"  # the image, the agent bootstrap and the security group live here
subnets = ["<subnet-a>", "<subnet-b>"]
[provider.instance_types]                   # shape name to instance type
linux-x86-16 = "<instance-type>"           # a cloud provider offers no GPU shape (D32); launch options, price and VM facts: hosts.md

[[pool]]                                    # providers (in order) + shape + max_machines + labels; there is no minimum (D38)
name = "gpu"                                # the GPU machines run js-agent and register (D32, D36); no elastic provider has a GPU shape
providers = ["registered"]                  # one entry: nothing to fall through to
shape = "mac-studio-gpu"
class = "whole-node"                        # a dedicated whole-node pool; the merged name gpu-host is read as this (5.17). Every lease is a whole host
max_whole_node_per_consumer = 2             # default 1; max_whole_node defaults to max_machines here, and max_draining to 1 (whole-node.md)
max_machines = 8                            # never exceeded, in-flight and tainted hosts counted (5.11)
regain_grace = "5m"                         # a queued request waits this long after its last capable host left (D41); default 5m
join_token_file = "<path-to-token-file>"    # one file per pool that admits registering hosts; may hold several tokens, so rotation overlaps
labels = { tier = "gpu" }

[[pool]]
name = "cpu-burst"
providers = ["registered", "cloud"]         # the hierarchy: registered hosts first, an aws machine only when none can serve now (5.8)
shape = "linux-x86-16"
class = "action"
max_machines = 24                           # the pool's total over every provider, registered hosts included (5.11)
provider_max_machines = { cloud = 4 }       # at most 4 of the 24 are aws machines; the provider never exceeds it
join_token_file = "<path-to-token-file>"    # the registered hosts of this pool (a machine aws launches presents its instance identity instead)
max_step = 2                                # most machines acquired in one tick; default 2
idle_timeout = "10m"                        # a host idle this long is released (5.10); the one idle delay
cooldown = "2m"                             # no re-acquire after a refusal, a boot failure or a release
boot_timeout = "15m"
max_taints_in_a_row = 3                     # replacement stops, with an alert, after this many (5.12)
allow_host_jobs = false                     # default; true lets a consumer that also allows it run host jobs (5.12)
labels = { tier = "cpu", site = "cloud" }
```

A consumer picks pools in its own configuration, not the scheduler's. For kbf, a sketch of the section that maps its action classes
to selectors (the file is kbf's; the scheduler never reads it):

```toml
[[scheduler.class]]
kbf_class = "whole_machine"                 # kbf's name; leases a whole gpu host from any pool labelled tier=gpu (the GPU pool, D32)
class = "whole-node"                        # the generic class (5.17); the merged name gpu-host is read as this
labels = { tier = "gpu" }

[scheduler]
max_leased_machines = 16                    # kbf's own cap on machine leases held at once; it queues the rest (5.11)
allow_host_jobs = false                     # default; a host job also needs the pool's and the agent's consent (5.12)
```

Capabilities, VMs and the whole-node class (5.13, 5.14, 5.17) add this, all optional. The schema declares namespaced keys beyond the built-in
ones; a pool may state the class its hosts serve, the time it waits for a whole-node host to become ready, and a base image its elastic
hosts are launched with (so the scaler can count a constrained request as demand); the agent's VM settings are flags (R4), not file keys:

```toml
[[capability]]                              # beyond the built-in arch, os, os_version, xcode, sim.*, base_images, vms
key = "acme.metal"
kind = "count"                              # text | version | set_text | set_version | count

[[pool]]
name = "mac-vms"
shape = "mac-arm64"
providers = ["registered"]
class = "vm"                                # runtime_tag vm (6.2)
max_machines = 4
prepare_timeout = "10m"                     # a whole-node lease gives up when its host is not ready this soon (5.17)
max_whole_node = 2                          # this mixed pool offers whole-node leases only because this is set: most hosts reserved or held at once
max_draining = 1                            # most hosts reserved and not yet held; default 1
max_whole_node_per_consumer = 1             # default 1
keep_for_ordinary = 1                       # hosts kept unmarked for ordinary work; default 1 in a mixed pool; max_whole_node + this <= max_machines
drain_deadline = "4h"                       # when a reserved host is expected to be drained at the latest, shown only; default the request's wall limit; nothing happens at it (whole-node.md, section 4)
base_images = ["macos15-xcode16-a"]         # what an elastic host of this pool is launched with; the scaler counts a guest constraint as demand only if one of these can meet it
[pool.capabilities]
os = "macos"
arch = "aarch64"
```

```sh
# the agent's own flags (5.14): the VM backend is off unless --vms is above zero
js-agent --vms 2 --vm-driver <absolute-path> --vm-store <dir> --vm-network none \
  --vm-boot-timeout 5m \
  --cap acme.metal=1 --facts-interval 5m
```

A pool lists providers in order, and an entry may be a tier of equal providers (several clouds, hosts.md section 8): `providers =
["registered", ["cloud-a", "cloud-b"], "cloud"]`; a flat list means what it did.

A pool specifies `max_machines` and no minimum: `min_machines` and `min` are refused with a message saying so (5.7, D38). `provider = "x"` is
shorthand for a one-entry `providers`, except that an elastic `x` gets the `default` provider in front of it (5.8).

**Test provider.** The operator-listed pool exists for tests and conformance and for the single-host stepping stone. A file that declares
it is refused unless the scheduler runs with `--allow-test-providers`; no production example uses it. Its shape is:

```toml
[[provider]]                                # TESTS ONLY: hosts the operator lists (the merged --pool-config hosts)
name = "listed"
kind = "static"

[[host]]                                    # the merged fields (id, gpu_class, resources, capabilities) plus the provider
provider = "listed"
id = "test-host-01"
shape = "linux-x86-16"

[[pool]]
name = "test-pool"
providers = ["listed"]                      # max_machines defaults to the hosts listed
shape = "linux-x86-16"
class = "action"
```

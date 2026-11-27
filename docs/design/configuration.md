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
| `--agent-client-ca` | `server.agent_client_ca`: the CA file of the hosts' certificates (needs `server.tls_cert` and `server.tls_key`, refused at start naming the key otherwise; read again with the TLS pair on SIGHUP). A host certificate (exactly one DNS SAN, the host id; end entity; `clientAuth` only; URIs and CN ignored) is the credential of its host on the agent routes; join tokens and the mint route stay for the scaler path; see the flag table in the server's crate documentation |
| `--listen`, `--state-file` (the base path of the server's files: `.jobs`, `.agents`, `.joins`, `.scaler`; there is no lease file), `--boot` (`initialise` or `resume`; it governs the first start and whether the records files are required), `--tick`, `--tls-cert`, `--tls-key` | `server.<name>`, the flag's name with `_` for `-` |
| `--default-ttl`, `--max-ttl` | removed: a lease has no time to live (it ends when released or at its maximum wall time). Either flag, and the keys `server.default_ttl` and `server.max_ttl`, are refused at start naming the flag or the key |
| `--max-connections`, `--handshake-timeout`, `--header-timeout`, `--read-timeout`, `--write-timeout`, `--max-requests`, `--drain-timeout`, `--drain-hold` | `server.<name>`, same defaults |
| `--agent-hello-burst`, `--agent-hello-interval`, `--require-host-credential`, `--placement`, `--estimate-feed`, `--cpu-slack`, `--pack-limit`, `--score-balance`, `--score-slack-tolerance-ppm`, `--key-share-percent`, `--placement-curves` | `server.agent_hello_burst`, `server.agent_hello_interval`, `server.require_host_credential`, `server.placement`, `server.estimate_feed`, `server.cpu_slack`, `server.pack_limit`, `server.score_balance`, `server.score_slack_tolerance_ppm`, `server.key_share_percent`, `server.placement_curves`, same defaults (`placement` is deprecated and does nothing: js-sched is the only placement, `"js-sched"` is accepted with a warning and `"registry"` is refused at start; a joined pool whose capabilities name no `arch` is refused, as with the flag, and an elastic pool, whose floor names none, is not: each of its machines is placed by the floor's numbers and the capabilities its agent reported at hello, `arch` among them; `estimate_feed` is `false`, and `true` is refused beside `usage_off = true`, as the flags conflict; `cpu_slack` is a number from `1.0` to `2.0`, js-sched's default `1.1` when absent; `pack_limit` is a number from `0.0` to `1.0`, with at most three decimals, js-sched's default `0.5` when absent: a host is packed (the fullest that fits is chosen) while its larger share of CPU or memory, with the new lease booked, stays within it, and past it the least full host is chosen; `score_balance` (0 to 4), `score_slack_tolerance_ppm` (0 to 1000000) and `key_share_percent` (0 to 100) are whole numbers, each js-sched's default when absent (1, 10000 and 25) and refused outside its range, never clamped: the score's balance of the two resources, the overshoot of a host's capacity the score ignores, and the share of a host's CPU one action key (for jobs, one job) may take before the score prefers another host; they rank the hosts a job fits on and admit nothing ([placement-by-load.md](placement-by-load.md)); `placement_curves` is `"off"`, `"shadow"` or `"on"`, `"off"` when absent, any other value refused as `placement_curves must be off, shadow or on; got "<value>"`: `"shadow"` learns a CPU and a memory profile for each job class from the recorded series and logs one line per run, and one for each job the curve rule `"on"` would place on another host or on none, and placement is the same in `"off"` and `"shadow"`; `"on"` learns and logs as `"shadow"` does and books a class that passes its four checks by its CPU curve ([placement-profiles-live.md](placement-profiles-live.md)), on every host unless `curve_hosts` names some; the two keys below have the flags of the next row); refused unless a pool registers agents |
| `--curve-hosts`, `--curve-max-wait-seconds` | `server.curve_hosts` and `server.curve_max_wait_seconds`, read in every mode and booking only under `server.placement_curves = "on"` (`--placement-curves on`; under `"shadow"`, `curve_hosts` names the hosts its log line judges by curve, for that line alone); the flags and the keys are read and refused by the same code, the refusal naming the flag or the key. `curve_hosts` is an array of host ids (1 to 63 characters of `a-z`, `0-9`, `.`, `_` and `-`): curves are booked only for jobs placed on the hosts it names, and every job on any other host is booked flat; absent, curves are booked on every host. On the command line the list is one value of host ids separated by commas (`--curve-hosts mac-1,mac-2`), or the flag given again for each host; without the flag `on` books by curve on every host, as without the key. An empty array, an empty name, a name twice, a value that is not an array of strings and a name that is not a host id are refused at start naming the key or the flag and quoting the name, such as `--curve-hosts: host id "Mac-1" must be 1 to 63 characters of [a-z0-9._-]`. No list of hosts is known at start (a pool names how many machines may join, not which), so a host id that never registers is not refused: it books no curve. `curve_max_wait_seconds` is a whole number from `0` to `300`, `30` when absent: how long a job of a curve class waits with no host fitting its curve before it is tested with its flat number (at `0` a curve never delays a job); another value is refused as `server.curve_max_wait_seconds: must be from 0 to 300` (`--curve-max-wait-seconds: must be from 0 to 300` for the flag, and a flag value that is not a whole number is refused by the command line). Both are read once at start, and a change is a restart |
| `--usage-db`, `--usage-off`, `--profile-store-bytes`, `--history-retention-days` | `server.usage_db`, `server.usage_off`, `server.profile_store_bytes`, `server.history_retention_days`, same defaults and rules (with `--store` the usage rows are in the store and `usage_db` is not used; `usage_db` is refused beside `usage_off = true`, as the flags conflict; the budget is at least 1; the retention is a whole number of days, `90` by default, at least 1) |
| `--job-retention` | `server.job_retention`, same default (`"7d"`) and rules: how long an ended job's record and result (exit code, reason, log tails, timing, usage) are kept after the job ends, as a whole count of days or a duration such as `36h` or `90m`, at least one minute; zero, a negative or unparsable value, or less than a minute is refused at start naming the flag or the key. It is read once at start and is the window the job store (`js-jobs`) applies when a job ends, so a changed window affects only the jobs that end after the restart. The job store of `--job-listen` is opened with it; without `--job-listen` the setting is checked and kept, and nothing reads it. Allocation history is not part of it |
| `--job-feed-retention` | `server.job_feed_retention`, same default (`100000`) and rules: how many of the newest changes of the job store's change feed each tick keeps, a whole number of at least 1 (a count of feed changes, never a time); zero or a non-number is refused at start naming the flag or the key. Older rows are trimmed by the job pass of the tick, and a reader whose cursor is behind them is refused with a stale-cursor error and starts again from the tables. It is read once at start and applies to the job store of `--job-listen`; without `--job-listen` the setting is checked and kept, and nothing reads it |
| `--job-allocation-retention` | `server.job_allocation_retention`, same default (`14d`) and rules as `--job-retention`: how long a freed allocation's row is kept after its free, a whole count of days or a duration of at least one minute. The tick deletes freed rows past it, at most 500 a tick; booked and releasing rows, and the usage history of an allocation, are never deleted. It is read once at start and applies to the job store of `--job-listen`; without `--job-listen` the setting is checked and kept, and nothing reads it |
| `--job-listen` | `server.job_listen`, off unless given: the address the job service is served on (see "The job service's port" below); off the loopback it needs `server.tls_cert` and `server.tls_key`, the same pair as `server.listen`; refused unless a pool registers agents |
| `--metrics-listen` | `server.metrics_listen`, off unless given: the address the operator's metrics are served on (see "The metrics port" below); a private address only, plain HTTP, no token; an address that is not a private one is refused at start naming the flag or the key; needs no agent registration |
| `--ready-listen` | `server.ready_listen`, off unless given: the address the readiness route is served on (see "The readiness port" below); a loopback address only, plain HTTP, no token; an address that is not a loopback one is refused at start naming the flag or the key; with a store setting it also makes the server wait for a held writer lock instead of refusing to start |
| `--backfill-bounds-enforced`, `--backfill-margin` | `server.backfill_bounds_enforced` (a boolean, `true` when absent), `server.backfill_margin` (a duration, `"1m"` when absent; zero or an unparsable value is refused naming the key): whether the placement engine trusts a job's wall limit as its end, and the margin it keeps. The job service ends every job at its wall limit, so the limit is trusted unless the operator says `false`. Trusted, a host a whole-node request has set aside takes a job that ends, with the margin to spare, before the host's tail; and a job whose end the engine does not know (an allocation taken up after a restart, whose wall limit it was not told) counts, when a host is picked to drain, as ending at its start plus the ceiling, `server.max_job_wall`, the one wall limit the job service holds every job to (`server.max_lease_wall` bounds no job, so it is not the ceiling). With `false` nothing backfills and such a host ranks after every host whose tail is known. Either way a job past its end is read as ending the margin from now. Refused unless a pool registers agents |
| `--max-job-wall` | `server.max_job_wall`, `"30d"` unless given (the job service's default, the longest `server.max_lease_wall` accepts; it does not follow `server.max_lease_wall`): the most a submitted job's wall limit may be, as a whole count of days or a duration such as `12h` or `90m`, at least one second; zero, a negative or unparsable value, or less than a second is refused at start naming the flag or the key. A job asking for more is refused `wall-time-above-ceiling`. It is read once at start; without `--job-listen` it is checked and kept, and nothing reads it |
| `--store`, `--allow-sqlite-store` | `server.store` and `server.allow_sqlite_store` (`false` unless given; `true` is refused without `server.store`, as the flag is without `--store`): the store the server's state is to be kept in, and the gate of its SQLite backend; see "The store" below |
| `--store-unreachable-after` | `server.store_unreachable_after`, `"60s"` unless given: how long the store paths of the tick (the job pass) may fail tick after tick before `/healthz` answers 503 `store unreachable` and the alert `store:unreachable` holds; the first tick whose store work succeeds clears both; zero or an unparsable value is refused at start naming the flag or the key |
| none (the file only) | `server.dispatch_lease_secs` (`10`, a whole number of seconds from 1 to 3600) and `server.dispatch_recheck_ms` (`500`, a whole number of milliseconds from 1 to 4999, below the 5 s hold of a dispatch poll): the dispatch poll's timings ([multi-instance.md](multi-instance.md), section 0.2). A server's claim of a dispatch message holds it for `dispatch_lease_secs` on the store's clock, for the agent to act and acknowledge; a message not acknowledged by then is handed out again by the next poll of the host, on any server. A held poll reads its host's queue again every `dispatch_recheck_ms` whatever woke it, which is how it finds a message another server wrote; a placement on the same server wakes it at once. A value outside the range, or not a whole number, is refused at start naming the key. Read once at start; a server started on flags has the defaults |
| `--cold-cpu-millis`, `--cold-memory-bytes` | `server.cold_cpu_millis` and `server.cold_memory_bytes`: what a queued job that names no CPU or memory books, for each dimension it leaves at zero, while neither its class, its fallback class nor its client has a run ([sizing.md](sizing.md), section 7.7). Whole numbers: the CPU in millicores from `250`, the memory in bytes from `1073741824` (1 GiB); a value below that, negative or not an integer is refused at start naming the key. Each left out is its default, `22000` (22 cores) and `4294967296` (4 GiB), whatever the hosts' size; a CPU above a quarter of the most cores a registered host has books that quarter (the cold CPU is held at a quarter of the largest host), and a memory above the most a registered host has books that most. The flags take the same values, and either one without `--agent-pool-config` is refused |
| `--cold-memory-bytes-on <OS=BYTES>` | `server.cold_memory_bytes_on`, a table such as `{ macos = 4294967296 }`: the memory a queued job whose `os` constraint is that operating system (the name as the job gives it) books in place of the cold memory while neither its class, its fallback class nor its client has a run ([sizing.md](sizing.md), section 7.7). The bytes are whole numbers from `1073741824` (1 GiB); a value below that, negative or not an integer, and an empty name, are refused at start naming the key. A job of any other `os`, or of none, books `server.cold_memory_bytes`. A memory above the most a registered host has books that most. The flag is repeatable, each operating system once (one given twice is refused at start), and is refused without `--agent-pool-config` |
| `--cold-cpu-millis-on <OS=MILLIS>` | `server.cold_cpu_millis_on`, a table such as `{ macos = 1000 }`: the CPU a queued job whose `os` constraint is that operating system (the name as the job gives it) books in place of the cold CPU while neither its class, its fallback class nor its client has a run with a CPU sample ([sizing.md](sizing.md), section 7.7). The millicores are whole numbers from `250` to `4294967295`; a value outside them, negative or not an integer, and an empty name, are refused at start naming the key. A job of any other `os`, or of none, books `server.cold_cpu_millis`. A CPU above a quarter of the most cores a registered host has books that quarter. The flag is repeatable, each operating system once (one given twice is refused at start), and is refused without `--agent-pool-config` |
| none (the file only) | `server.alert_webhook_url`, `server.alert_webhook_token_file` and `server.alert_repeat_after`: every alert of the scaler, and the store's `store:unreachable`, is also POSTed as `{source, key, text}` JSON to the URL (`https`, or `http` to a loopback address) when it begins to hold, again every `alert_repeat_after` (default 1h, plus up to one tick and the receiver's latency) while it holds, and again if it clears and returns; delivery is off the tick path; the token file is owner-only and holds the bearer token the receiver wants; both other keys are refused without the URL |
| `--token-file` | `server.client_token_file` |
| `--client-token NAME=PATH` or `NAME:ROLE=PATH` (repeatable) | a `[[client]]` table with `name`, `token_file` and, optionally, `role` (`"client"`, the default, or `"operator"`): one more client of the job service (see "The job service's port" below); refused without `--job-listen` (`server.job_listen`) |
| `--pool-config` | removed: refused at start with the migration of [Migrating an agentless pool](#migrating-an-agentless-pool); a `[[pool]]` with `provider = { kind = "static", hosts_file = ... }` is refused the same way |
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
- **The job store.** Turning the port on opens the job store: with `--store`, rows of that store (the first start over it begins with no jobs unless `js-lease-server job-store copy` copied a `.jobs` file of an earlier start into it; the server itself leaves that file unread, and `job-store export` writes the store's job and usage rows back to files: `docs/runbooks/state-database.md`, section 6); without it, a SQLite file named after the state file, `<state file>.jobs`, owned
  by the server's user and owner-only (mode 0600), with SQLite's own files beside it, the single-node development backend. A file that cannot be made owner-only or opened as a state store
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
`join_token_file`. The server starts on several joined pools, whose hosts share its one registry (each host in the pool its hello names,
with that pool's shared tokens only), and on an elastic pool alone: beside another registering pool an elastic one is refused naming the
second (`several elastic pools are parsed; this version runs one, as the server keeps one registry of agents`, or `an elastic pool is
served alone; this version runs several pools only when every one is joined`). A joined pool after the first must not have the first's
name, must have its `memory_schedulable_percent` and must not be of class `whole-node`; the flags say the same with a repeated
`--agent-pool-config` (`crates/js-lease-server/src/lib.rs`).

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
prepare_timeout = "20m"               # optional; class whole-node only (refused on any other); default 10m: how long a whole-node lease may take to become ready
```

A node joins with a one-time token (the mint refuses any other `provider` than the pool's name with a 400): `POST /v1/agent/join-tokens` with the client token and `{"provider": "nodes", "instance_id": "node-1"}`
mints it, and the node's `--instance-file` holds `nodes:node-1`. The first hello swaps the token for a host credential. What the old
`--agent-pool-config` file had and this kind lacks: the whole-node keys other than `prepare_timeout` (`max_whole_node`, `drain_after`, ...).

**Memory headroom.** One key, `memory_schedulable_percent`, a whole number from 1 to 100 (default 100), sets how much of the memory an
operator configured the server books; there is no other memory knob. It is written in two places, each in its own format and read the
same way (0, 101, a negative number or a word is refused at the check that reads it, naming the key):

| where | what it cuts |
|---|---|
| a `[[pool]]` of the `--config` file | the memory of each host of a joined pool, and the floor memory (1 GiB) of an elastic pool |
| the agent pool file (`--agent-pool-config`; top level, before `[shape]`) | the memory of each host the pool books |

**A host's capacity is what it reports.** A registered host is booked at the CPU, memory, GPUs and disk its hello reports, less or more
than the pool's `[shape]`: a host that reports less (a node that lost a DIMM, say) joins and is booked at what it has, and a host that
reports more is booked at what it has too. The `[shape]` numbers are no floor and no ceiling; they are booked only for a host that has
not said hello since a restart, until it does, and they size an elastic pool's launches and the cold and pack defaults. Only the host's
capabilities are held to the pool's at its hello (`[capabilities]`, and the class of GPUs both name; refused `below-floor`). In a pool that
declares `exec`, a host's GPUs are booked only when it reports a working `gpu-toolkit`; a pool whose `[shape]` has GPUs refuses a GPU
host without one (`host-contract`).

The server books `floor(memory x percent / 100)` bytes of that memory, computed once where the booked shape is built, and every booking
path sees that number: a lease, a whole-node lease, a run's memory limit, a reservation and the host listing. The percent is never applied
twice and a hello is not held to any memory. `GET /v1/hosts` shows, per host, `memory: {configured_bytes, schedulable_percent,
bookable_bytes}`, where `configured_bytes` is the memory the host reports (the `[shape]`'s for a host that has not said hello since a
restart) and `shape.memory_bytes` is the bookable figure; `GET /v1/pools` and `/status` show the same three for an elastic pool's floor.

The percent of an agent pool (the agent pool file, or a `[[pool]]` whose hosts register) is the placement
engine's rule instead: the registry's book takes each host's whole booked memory, and the engine books no more than
`floor(memory x percent / 100)` of it. What fits the pool is still judged at the cut, so a request above it is refused `no-fit`, and
`GET /v1/hosts` shows `shape.memory_bytes` as the cut, beside `memory`.

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
- **No lock on Postgres.** Any number of servers open one Postgres store and serve at once: nothing is taken at start, nothing is checked
  in a commit, and no server is the leader. Every decision (placing a job, booking a host, a periodic duty, a host's heartbeat writes) is a
  transaction whose preconditions hold for however many servers make it. A server opens the store before it reads any file of the state
  file's family and before it binds a listener, except the readiness listener of `--ready-listen`. A login that is refused or a database that
  cannot be opened is `--store: the store could not be opened: ...`, exit 2.
- **The lock of a SQLite store.** A SQLite file is written by one process: the server takes the lock file beside it before anything else and
  holds it for its life. It tries once and does not wait, unless `--ready-listen` is given: then it logs `waiting for the store's writer lock` once,
  tries again every second while another process holds the lock, and takes it before it reads anything from the store or binds any other
  listener (see "The readiness port"). Without `--ready-listen`, a second server on the same file exits 2 with one line, `--store: another
  process holds the store's writer lock`, having bound nothing and opened neither the store nor any file of the state file's family.
- **What it keeps.** The host bindings (with each host's taint and cordon; with the job service on, the job store's `cordon/<host>` rows
  are the cordons' record, which every server places by) and the digests of the join tokens and host credentials:
  `hosts/binding/<host id>` and `hosts/join/<digest>`, each a JSON object with a `version`, whose unknown fields are kept and written
  back; and the scaler's records and the hosts the registry refused to remove: `acquires/scaler/records` (the records file's own
  format, written whole) and `acquires/scaler/unremoved` (`{"version":1,"hosts":[...]}`). They are read at start, after the lock;
  `<state file>.agents`, `.joins`, `.scaler` and `.scaler.unremoved` are then not written. Over a store another server shares (an
  update that runs two for a while), two rows are also read by key later: a host's binding at the hello of a host this server's
  registry does not know, so it keeps the taint or cordon another server stored after this one started, and the row of a one-time
  token or host credential this server does not hold, at its first use, so a host another server admitted meanwhile is not refused
  401 here (a read that fails answers 503 `state-write-failed`). The tick deletes the binding only of a host this server's registry
  held and then forgot, never of one it never held, which another server may be serving; so a binding
  of a host that no server holds again stays in the store, as inert as before (a binding is enforced only
  for a host a registry holds). Each change is committed before the server
  holds it: a hello that uses a token commits the use and the host's binding in one transaction, and a refused hello gives both back in
  one more; a taint, a cordon or its lift, a mint and a revoke each commit first; the scaler commits its records before the first
  provider call of a step, and a commit that fails stops the step as a failed file write does. A failed commit changes nothing and is
  answered as the failed file write of the same change is, or with 503 `state-write-failed` where no file write is checked (a hello
  with a pool token, a heartbeat's taint). A heartbeat that brings no new taint, a known host's hello and the host book make no store
  call (a known host's hello commits only when its stored binding still holds a cordon that lapsed since the last tick); a host
  credential's renewed time is committed by the tick. A live pool that is not at its first start does not start without the records
  key, as it does not without the file. The job store and the usage database stay where they are.
- **The first start.** Right after the lock, a start over a store with no `meta/server-state-imported` marker and nothing under
  `hosts/` or `acquires/` imports the four files once: it reads them with the readers a server without the setting uses (a missing
  file is empty; a file of a bad version, a damaged file or one others can use is refused at start, naming it, and nothing is
  written) and writes their bindings, join-token rows, records text and hosts with the marker in one transaction. The marker is written
  even when there are no files. A start that finds the marker reads none of the files and names each one still there once at info
  level. A store that holds server state without the marker gets the marker only. The server never deletes or changes the files: they
  hold what it held at the cutover and go stale from its first write after it, so a rollback to a build without the setting finds
  them as they were then; an operator removes them. The lease file is not imported, and the server never opens it: a lease file left by an earlier release is ignored and stays as it was, so a rollback to that release reads it as it was left.

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
| `js_callback_pending` | gauge | | the completion callbacks of ended jobs not yet delivered, abandoned or expired, claimed or not, read from the store's due entries; one past its deadline counts until a try finds it expired; only with the job service on |
| `js_job_placements_total` | counter | | the queued jobs placed on a host |
| `js_queue_wait_seconds` | histogram | | each placed job's wait from its submit to its placement |
| `js_job_start_failures_total` | counter | `code` | the jobs that failed to start, by code (`no-capacity`, `start-timeout`, `exclusive-not-ready`, `agent-refused`, `input-unavailable`); a start failure that a job's retry policy follows with another attempt is a retry, not counted here |
| `js_jobs_lost_total` | counter | | the jobs lost: their host read gone by the job pass, or their container missing from the first listing after a new hello; a loss that a job's retry policy follows with another attempt is a retry, not counted here |
| `js_job_retries_total` | counter | `outcome` | the attempts of jobs that ended in a way that retries and were followed by another attempt, by how they ended: `start-failed` (a start failure of the host's: a start timeout, an input the host could not read, a refusal with a code of the host) or `host-lost` (`docs/design/job-retry-and-completion-callback.md`, section 3.3) |
| `js_job_attempts_exhausted_total` | counter | | the jobs that ended in a way that retries with no attempt left under their retry policy; each is also counted as a start failure or a loss |
| `js_store_write_failures_total` | counter | | the passes over the job tables the server makes itself (the job pass, a placement, a heartbeat's job listing) that failed, so their write did not commit; a client's submit or cancel that fails is answered to the client |
| `js_job_pass_duration_seconds` | histogram | | how long each job pass of the tick took |
| `js_job_tables_total` | counter | `from` | the passes over the queued jobs, by where each took the job tables (the live allocations, the queued jobs, the marks and the cordons) from: `store` (read, as a write came since the last read and the store's feed could not tell what it changed), `kept` (no write since), `submitted` (no write since but the commits of this server's submits, whose jobs were added to the tables kept), `changed` (a write came since, and the tables kept were brought up to date from the store's feed with the rows that changed, reading only the marks or the cordons again when one of theirs changed) |
| `js_submit_phase_seconds` | histogram | `phase` | how long each phase of each `SubmitJob` call took, by phase: `validation` (the checks of the job, no I/O), `get` (the read of its id in the job tables, for a job with an id, its wait for a place on the blocking pool included), `admission` (for a job not yet held), `insert-checkout` (of the insert of the job as queued, the time spent getting connections of the store's pool: an idle one, a new one dialled, or the wait for one to come back; 0 on a SQLite store), `insert-transaction` (the rest of the insert: its reads, its transaction and its commit) and `note-inserted` (the placer told of the recorded job; it never waits for a pass that holds the placer's tables). A call is timed in each phase it reaches, whatever the phase's answer (a refused job in `validation` alone, a retry of a held job in all but `admission`); the read of a callback's status after the insert, the wait for a place on the blocking pool before it, and the answer are in none. Buckets from 100 us to 1 s |
| `js_callback_deliveries_total` | counter | `result` | the tries of jobs' completion callbacks (docs/design/job-retry-and-completion-callback.md) whose answer this server recorded, by result: `delivered` (a 2xx), `abandoned` (a 410), `failed` (any other status, a timeout, a refused connection, a redirect, a name that did not resolve) or `refused-address` (the address rule refused an address of the receiver, and nothing was sent); a try whose claim lapsed before its answer, or whose answer the store did not take, is not counted |
| `js_placement_refusals_total` | counter | `host`, `reason` | the refusals of a host to a queued job by the placement, by the predicate that refused: `cpu` (the CPU slack: what the host is charged plus the job's cores is above its cores times the slack), `predicted-memory` (the load predicate's memory side), `booked-memory` (the flat memory guard), `cpu-curve` (the curve hosts' slot-by-slot CPU check, under `placement_curves = "on"`) or `cold-cap` (the cold leases admitted this window reach the cap). A job is counted once for each host that refused it in each pass; a host refused for none of these (a lease cap, a job larger than the host) is not counted. The same refusals are logged: one info line per host per pass with the count of each reason and the figures of the latest, and one debug line for each, naming the job |
| `js_curve_wait_expiries_total` | counter | | the queued jobs of a curve class that waited past `server.curve_max_wait_seconds` with no host fitting their curve, and were tested with their flat number from then on; written at 0 in every mode. A rise means curves book more than the hosts can give |
| `js_agent_start_refusals_total` | counter | `host`, `code` | the job starts a host's agent refused, by the refusal code the agent listed (`no-capacity`, `input-not-allowed`, ...), the part of its detail before the first `:` (a `provider-refused` detail carries the provider's reason after it): the code itself when it is 1 to 64 characters of `a-z`, `0-9` and `-`, `other` otherwise, and `none` when the agent listed no code. A refusal is counted once, when the requeue or the end it brings is committed, whether the job then goes back to the queue (`host-held`, `no-capacity`, `bundle-pending`) or fails to start |
| `js_agent_bodies_too_large_total` | counter | `route` | the agents' requests refused 413 `body-too-large`, their body over the 65,536 bytes the server reads, by route: `hello` or `heartbeat`. The agent ends at that answer, so each is an agent that stopped; an agent of this release holds what a heartbeat lists to that body (`docs/design/agent-wire.md`, section 1) |
| `js_acquires_total` | counter | `pool` | the launches the scaler ran, whatever their answer (a launch refused before the provider is called, such as one for an architecture with no pinned agent, included) |
| `js_acquire_failovers_total` | counter | `pool` | those refused for capacity or quota, after which the scaler tries another offer |
| `js_host_cpu_booked_millicores`, `js_host_cpu_free_millicores` | gauge | `host` | each host's CPU booked (its leases and its jobs' live allocations; one that books the host whole counts as its shape) and free (its shape less that, and never below 0) |
| `js_host_memory_booked_bytes`, `js_host_memory_free_bytes` | gauge | `host` | the same of memory, of the bookable memory (`memory_schedulable_percent`) |
| `js_curve_classes` | gauge | `state` | the job classes with a profile, `curve` (passing all four checks, booked by their CPU curve) and `flat` (failing a check, booked their flat number); written only under `placement_curves = "on"` |
| `js_host_cpu_booked_peak_millicores` | gauge | `host` | the fullest slot of each curve host's booked CPU curve at the last placement pass; written only under `placement_curves = "on"`; `js_host_cpu_booked_millicores` stays the sum of the flat bookings |
| `js_host_whole_mark` | gauge | `host`, `state` | 1 for each host set aside whole, by its mark (`reserved`, `held`, `draining`, `releasing`) |

The counters count from the start of the process; a start failure or a loss is counted as its commit lands, so a pass that fails later
still counts what it ended. The gauges are read at each scrape from the state `GET /v1/hosts` and the job service read, holding no lock
of the server's while the text is written; a scrape whose read of the job tables fails is answered 503. The parts are read one after
another, not in one transaction, so a job placed between two reads can show as queued while its booking already counts. The text
names no job, lease, run, token or callback: its labels are a state, a failure code, a host's id, a pool's name, a mark, a callback's
result and a phase of a submit.

- **Address.** The route takes no token, so it is served only on a private address (the rule of `/status`: loopback, RFC 1918, the
  shared range a tailnet uses, link-local and IPv6 unique-local; a wildcard is accepted), and on each request only to a peer whose
  address is private too; any other peer gets the answer of an unknown route. An address that is not a private one is refused at
  start. The port needs no TLS and speaks plain HTTP whatever the other ports speak: bind it to a private or tailnet address.
- **Limits.** The port has the connection cap and timeouts of the other listeners, and stops with them. An address in use is refused
  at start naming the flag or the key.

### The readiness port

`--ready-listen <addr>` (`server.ready_listen`) serves `GET /readyz` on a port of its own, plain HTTP, with no token. It is off by
default: without it the server binds no such port, tries a SQLite store's file lock once and refuses a start whose lock another process
holds (a Postgres store has none). Any other path, on that port, than `/readyz` and `/readyz/sizes` (below) is a 404.

| phase | answer |
|---|---|
| another process holds the SQLite store's file lock and the server retries | 503 `waiting for the store's writer lock` |
| the store is open (a server without a store setting, or over Postgres, has no lock to take) and the listeners are not all serving yet | 503 `starting` |
| every listener is bound and serving | 200 `ready` |
| from the SIGTERM or ctrl-c that starts the stop until the process exits | 503 `draining` |

`GET /readyz/sizes` on the same port answers whether the job sizes are taught: 503 `teaching` until the server has read the ended jobs
its start rebuilds the sizes from (after it answers `/readyz` 200), then 200 `taught`; 200 `nothing to teach` for a server with no job
tables, and 200 `not taught: the live allocations were not read`, `not taught: the ended jobs were not read` or `not taught: the server
stopped before the ended jobs were read` when the read failed or was stopped. A deploy can wait for its 200 before it replaces the server.

- **Address.** A loopback address only, since the route takes no token; any other address is refused at start naming the flag or the
  key. An address in use is refused at start naming the flag or the key.
- **Order.** The port is bound before a SQLite store's file lock is tried, so a supervisor sees the phases from outside; the program
  prints `ready on <addr>` as soon as it is bound, and `listening on <addr>` when the start is done. The port is the last listener
  closed at a stop and answers through the whole drain of the others.
- **A signal while waiting.** SIGTERM or ctrl-c while the server waits for the lock or starts ends the process with status 0, at
  once, without a drain and with the lock not taken. A server without `--ready-listen` does the same with a signal that arrives
  before its start is done.
- **`/healthz`** is another question, served on `--listen`: whether the tick loop and the store paths work.

### Machine providers, platforms and pool settings

A pool may name container platforms in priority order instead of naming its provider (design `jobscheduler-design.md`, section 4.2).
`[[machine_provider]]` tables declare where machines come from: `kind = "static"` is the hosts that run `js-agent` and register
themselves (no key beside `name` and `kind`; a pool's `provider` of kind `static` is refused), and any other kind is
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
capacity_requeues = 8                   # default 8; 0 ends a job at the first refusal for capacity
capacity_refusal_bar = "60s"            # default 60s; how long a host that refused for capacity is kept off the job
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
size_tolerance_percent = 10             # default 10; an integer from 0 to 50
```

A pool written with `provider = { kind = "lambdalabs", ... }` is read as exactly these tables: a machine provider made from its provider
table, the `static` machine provider, and a `default` platform naming the two, `static` first. The loader's names start with `+`, which no
name in the file may hold. The job pass places a job on a free host of the first machine provider in the order the pool lists its
platforms and each platform its `machine_providers` (any order, a launching provider before `static` included), and on a host of a later
provider only when no host of an earlier one can take it now. A pool's platforms must reach at least one machine provider that launches,
and may reach several: pass 2 asks them for a machine in the same order (design 5.6, step 5), each inside its own `provider_limits`
entry and share, a `capacity` refusal falling through to the provider's other offers and then to the next provider, and a request that
every provider refused waits the soonest `acquire_retry` among them. Two machine providers of one kind (one provider id) in one pool
are refused (`its provider is "lambdalabs", as is that of an earlier machine provider of the pool; a pool reaches each provider once`),
and every provider must launch hosts of the pool's `exec` mode. The pool's floor is the fewest GPUs of a class every provider's hosts
share (none when they share no class). Its health is its first launching provider's checks, the scaler reads that provider's notices,
and a later provider's offers are its cached catalog.

**Parsed and checked; what acts on them.** `max_queue_wait`, `start_timeout`, `path_grace` (each a positive duration) and the
`provider_limits` tables are read and checked at start. Admission of a lease request (design 5.6, step 2) reads the limits: an offer
whose instance type is not allowed, whose price is above the maximum, or that has no price under a maximum does not admit a request, and
a provider whose `max_machines` (else the pool's) is zero admits none. `path_grace` is how long a waiting whole-node request of the pool
keeps its place once no host could serve it, after which it is refused `no-capable-host` (it is the registry's `regain_grace` for the
pool). The job pass of the tick (design 6.1, step 1) ends a job of the server's job tables that waits past `max_queue_wait` (counted
from when it was queued) as `no-capacity`, and a placed or starting one that has not started within `start_timeout` (counted from its
placement) as `start-timeout`; each deadline is held to at least the server's boot plus 120 s, so an outage fails no job at the first
tick. `capacity_requeues` (0 to 1000, default 8) is how many times a start a host refused for capacity (`no-capacity`) sends a job back to
the queue; the refusal after that ends the job as failed to start, with a detail naming the hosts that refused it, and 0 ends it at the
first. `capacity_refusal_bar` (a positive duration, default 60s) is how long the refusing host is kept off that job. A joined pool has none of
these keys and takes the defaults. Nothing acts on the limits yet: the pick of an offer when a pool acquires a
machine (design 5.6, pass 2) will read them. `catalog_refresh` is how long the provider's cached catalog is used before the tick fetches
it again; admission reads the cache and never calls the provider, a fetch that fails keeps the last catalog and is tried again at the
next tick, and a refusal to launch for capacity or quota makes a fetch due at once. `offer_cooldown` is how long an offer (an instance
type in a zone) that was cooled down is passed over by the pick of an offer, while admission still counts it. The check of a launched
machine's hello against the offer recorded for its acquire (design 5.6.1, step 6) cools an offer down: a hello whose accelerator model
(by canonical name after alias resolution), accelerator count (at least the offer's), architecture, CPU, memory or disk does not match
releases the machine, cools the offer down for `offer_cooldown` keyed by (provider, instance type, zone), and counts a failed boot
toward the pool's failure brake. `size_tolerance_percent` is how far below the offer's CPU, memory and disk the hello may measure and
still match, because an agent measures less than the nominal size; it is an integer from 0 to 50, 10 when absent, and any other value
(above 50, negative, or not an integer) is refused at start naming the key. `quota_backoff` is how long, after the provider refuses a launch for quota, pass 2 passes over that
instance type in that region (in that zone, for providers whose zones are regions); the provider as a whole is not braked.
`acquire_retry` is how long a request waits when every offer was refused or passed over: it stays waiting and pass 2 asks again after
this long, or at once after a completed catalog refresh. Zero for any of the four is refused, naming the key: a
zero refresh would ask the cloud at every tick, a zero cool-down is none, a zero back-off passes over nothing, and a zero retry would
ask at every tick. A limit
names a launching machine provider of the pool's platforms (a `static` one launches nothing and is refused), and an unknown key in it is
refused.

## Migrating an agentless pool

The agentless pool, in which the server booked hosts from a list in a file and nothing ran on them, was removed. The server refuses it at
start with exit status 2, and the message names the migration.

### The old files

The old file was a `[[pool]]` that named a hosts file:

```toml
[[pool]]
name     = "gpu"
class    = "gpu-host"
provider = { kind = "static", hosts_file = "/etc/jobscheduler/hosts.toml" }
```

and the hosts file listed each host:

```toml
[[host]]
id = "gpu-01"
gpu_class = "nvidia-a100"

[host.resources]
cpu_millis = 64000
memory_bytes = 549755813888
gpus = 8
```

The replacement is one pool of `kind = "joined"`: the pool of hosts that run `js-agent` and register. It lists no hosts; a host is in the
pool when its agent registers, up to `max_machines`, and every host books the pool's one shape. Install and start `js-agent` on each host that
the hosts file listed, naming the pool `gpu` and the server's agent address, then move the server to one of the two routes below, remove
the old pool and the hosts file, and restart the server. The two routes are not mixed: the `--config` file's pool is written in the file's
`[[pool]]` format, and the `--agent-pool-config` file has a format of its own.

### Route 1: a pool in the `--config` file

A registering pool needs `agent_listen` in `[server]`, which the agentless pool did not (`listen` stays as it was). The pool is a `[[pool]]`
with `kind = "joined"`; its shared token, if any, is `join_token_file` of the pool.

```toml
[server]
# the keys the file already has, and the address the agents register on:
agent_listen = "127.0.0.1:7443"
```

```toml
[[pool]]
kind         = "joined"
name         = "gpu"
class        = "whole-node"
max_machines = 2
shape        = { cpu_millis = 64000, memory_bytes = 549755813888, gpus = 8, gpu_class = "nvidia-a100" }
capabilities = { os = "linux", arch = "x86_64" }          # optional
join_token_file = "/run/credentials/js/join-token"        # optional shared pool token
```

### Route 2: the flags `--agent-pool-config` and `--agent-token-file`

The agent pool file is one pool at its root, with the shape and the capabilities as tables of their own; it has no `[[pool]]` and no
`kind`. The shared token is not in this file: it is the file that `--agent-token-file` names (optional: without it the pool admits only
hosts whose one-time join token verifies). The pool file is what turns agent registration on.

```toml
name         = "gpu"
class        = "gpu-host"
max_machines = 2

[shape]
cpu_millis = 64000
memory_bytes = 549755813888
gpus = 8
gpu_class = "nvidia-a100"

[capabilities]
os = "linux"
arch = "x86_64"
```

The server is then started with `--agent-pool-config <that file> --agent-token-file <the token file>` beside its other flags; the agents register on
the server's own `--listen` address.

The flag `--pool-config` is refused the same way, with the same migration in its message, and beside `--config` it is refused as every flag is.
The message never echoes the path. A server given neither `--agent-pool-config` nor a registering `[[pool]]` is refused at start
naming `--agent-pool-config` (or `pool`, for the file): every server serves the hosts that run `js-agent` and register. The provider kind
`static` of a `[[pool]]` is not the `static` machine provider of a `[[machine_provider]]`, which names the hosts that register and is
read as before.

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

**Test provider.** The operator-listed pool was a design for tests and conformance and the single-host stepping stone; it is not built (the
agentless pool was removed, and a file that declares a provider of kind `static` is refused). Its design shape was:

```toml
[[provider]]                                # TESTS ONLY: hosts the operator lists (not built; the agentless pool was removed)
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

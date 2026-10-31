//! A lease server: a [`js_lease::LeaseTable`] behind an authenticated HTTP JSON API, so
//! a CI job can hold a whole GPU host for as long as it renews.
//!
//! The library is the program: [`run`] takes the arguments and the streams to report
//! to, and `main` calls it. The pieces:
//!
//! - [`Config`] and [`Cli`]: the flags or the configuration file, and the checks that
//!   refuse a start (a token file anyone can read, a listen address off the loopback
//!   without TLS).
//! - [`Kinds`]: the provider kinds a `[[pool]]` of the configuration file may name,
//!   compiled in (`static` and `lambdalabs`).
//! - [`CapacitySource`]: where the hosts come from. [`StaticPoolSource`] reads the
//!   operator's pool file. With agent registration on (see "Agents" below), the static
//!   pool and the hosts agents register are two sources of the same trait, layered
//!   behind one provider.
//! - [`network`]: how a machine the service launches reaches it (the `default` and
//!   `tailscale` modes), and [`bootstrap`]: the script such a machine boots with, rendered
//!   from a plan with its secrets masked or filled. A provider receives the rendered
//!   payload and no network type. The scaler's launch step ([`Launcher`]) is the one place
//!   that holds both a mode and a provider.
//! - [`Scaler`]: the shell that runs [`js_scaler::step`] on the server's tick for an
//!   elastic pool: it reads the registry and the provider, runs the step's actions in
//!   order, and keeps the pool's records in `<state file>.scaler` ([`RecordsFile`]), owner
//!   only and written whole before the call a record describes, and the hosts the registry
//!   refused to remove in `<state file>.scaler.unremoved` ([`UnremovedFile`]), written the
//!   same way and before the registry is asked to remove a host, so that a restart tries
//!   them again.
//! - [`start`] and [`Running`]: the server, with time and ticks injected ([`SharedClock`],
//!   [`Ticker`]) so a test steps both.
//!
//! # Flags
//!
//! Configuration is these flags, or one file (`--config`, below), never both: every flag
//! below has a key in the file, and a flag given beside `--config` is refused. The one secret,
//! the bearer token, is read from a file that only its owner can use.
//!
//! | flag | meaning |
//! |---|---|
//! | `--listen <addr>` | default `127.0.0.1:7300`; off the loopback it needs both TLS flags |
//! | `--state-file <path>` | the lease file |
//! | `--boot initialise\|resume` | required: `initialise` may create the file, `resume` needs it |
//! | `--pool-config <path>` | the static pool file ([`js_pool_static`]; its top-level `memory_schedulable_percent`, 1 to 100 and default 100, is the share of each host's `memory_bytes` that is booked, rounded down); optional when agents register hosts, and one of the two sources is required (a start with neither is refused naming this flag) |
//! | `--agent-token-file <path>` | the pool's shared join tokens, one per line, each as strong as `--token-file`'s, in an owner-only file; read again at every tick. Optional: without it the pool admits only hosts whose one-time join token verifies. Needs `--agent-pool-config` |
//! | `--agent-pool-config <path>` | the pool agents join, as TOML: `name`, `class`, `max_machines`, `[shape]` (`cpu_millis`, `memory_bytes`, `gpus?`, `gpu_class?`, `disk_bytes?`: the scratch disk the pool books for every host and the floor a hello must meet; zero or absent books none; required above zero when `[capabilities]` names `scratch-quota`, or the file is refused; a pool that books disk and does not name `scratch-quota` refuses every agent that does not report disk, which no current agent does; a pool that names `scratch-quota` refuses a host whose `scratch-quota` is not `project`, whose `scratch-default-gib` is absent, zero or above the wire maximum, or whose default is more than `disk_bytes`, with `shape-not-accepted`), `[capabilities]`, `heartbeat_interval`, `grace`, `lost_retention`, `memory_schedulable_percent` (1 to 100, default 100: the share of `[shape]` `memory_bytes` the pool books, rounded down; a hello is still held to the whole shape; write it before `[shape]`), `exec` (`container` or `sandbox-exec`: the hosts must report a `runtime` of that mode, and `gpu-toolkit` when the shape has GPUs, or are refused `host-contract` or `execution-mismatch`; absent, nothing is required). Turns agent registration on |
//! | `--agent-hello-burst`, `--agent-hello-interval` | the hellos one join token may say in a burst (`60`) and how often one more is allowed after (`1s`) |
//! | `--require-host-credential` | off by default: turn it on once every agent is new. The first hello of a one-time join token must then say the agent keeps a host credential, or it is refused `join-token-credential-required` (403) and the token stays unused; so the token of every launched host is dead after its first hello. Pool join tokens are not affected. Needs `--agent-pool-config` |
//! | `--token-file <path>` | required: a regular file owned by the server's user, mode 0600 or 0400, holding one token of printable ASCII of at least 32 bytes ([`MIN_TOKEN_BYTES`]; trailing whitespace is dropped). A shorter token is refused at start |
//! | `--default-ttl`, `--max-ttl` | `600s` and `3600s` unless given; `90`, `90s`, `250ms`, `15m`, `2h` |
//! | `--tick` | how often the table is ticked and, with agent registration on, the agent registry is advanced (reservation drain and prepare deadlines), `5s` unless given |
//! | `--tls-cert`, `--tls-key` | PEM files; both or neither. The key file must be owner-only like the token file. Both are read at start only: renew a certificate by restarting the server |
//! | `--max-connections` | connections open at once, `256` unless given; at the cap the server stops accepting |
//! | `--handshake-timeout` | a TLS client's time to finish its handshake, `10s` |
//! | `--header-timeout` | a client's time to send a request head, from the connection's start and after each response (so also how long a keep-alive connection may idle), `10s` |
//! | `--read-timeout` | how long a read from a client may make no progress before the connection is closed (a client that sends a head and stops in the body); keep it above `--header-timeout`, `30s` |
//! | `--write-timeout` | how long a write to a client may make no progress before the connection is closed, `10s` |
//! | `--max-requests` | requests served on one connection before its last answer closes it, `100` |
//! | `--drain-timeout` | how long a stop waits for requests in flight before it cuts the rest, `10s` |
//! | `--usage-db <path>` | the file the per-minute usage of nodes and the learned cost curves are kept in; `usage.db` beside `--state-file` unless given. Needs agent registration to have anything to keep. Not with `--usage-off` |
//! | `--usage-off` | turns usage recording and its read routes off (for an emergency: the per-minute series is meant to be always collected) |
//! | `--profile-store-bytes <n>` | the most bytes the learned cost curves may take in the usage file, `268435456`; `0` is refused |
//!
//! A start that is refused prints the flag at fault and exits with status 2. The
//! hello limit flags and `--require-host-credential` need `--agent-pool-config`, and are
//! refused without it.
//!
//! # The configuration file
//!
//! `--config <path>` reads a `[server]` table and `[[pool]]` tables instead of the flags
//! above, and no flag may be given beside it: the first one on the command line is refused,
//! such as `--listen: not allowed with --config; set server.listen`, before the file is read
//! and with exit status 2. `--token-file` is `server.client_token_file`, `--pool-config` and
//! `--agent-pool-config` are `[[pool]]` tables, `--agent-token-file` is a pool's
//! `join_token_file`, and every other flag is the `[server]` key of its name with `_` for `-`. Every table refuses a field it does not know, and the file holds no secret: a
//! secret is the path of a file that only its owner can use. A refusal names where it is,
//! `server.listen: ...` or `pool "gpu": provider.api_key_file: ...`, and never repeats a
//! value of the file; a syntax error says the line. Nothing is checked over the network
//! at start.
//!
//! ```toml
//! [server]
//! listen            = "100.64.0.1:7300"   # default 127.0.0.1:7300; off the loopback it needs tls_cert and tls_key
//! agent_listen      = "100.64.0.1:7301"   # agent routes only; required with an elastic pool
//! advertise_url     = "https://js.example.ts.net:7301"   # what agents dial; http:// off the loopback is refused with an elastic pool; required when a pool is live
//! scheduler_id      = "js"                               # default "js": marks what this scheduler launches; give each scheduler on one cloud account its own
//! state_file        = "/var/lib/jobscheduler/state"      # required
//! boot              = "resume"                           # required: "initialise" or "resume"
//! client_token_file = "/run/credentials/js/client-token" # required: the file --token-file names
//! tls_cert          = "/run/credentials/js/tls-cert"
//! tls_key           = "/run/credentials/js/tls-key"
//! # tick, default_ttl, max_ttl, handshake_timeout, header_timeout, read_timeout,
//! # write_timeout, drain_timeout, max_connections, max_requests: the flags' names and defaults
//! # agent_hello_burst (60), agent_hello_interval ("1s"), require_host_credential (false):
//! # the flags' names and defaults; refused unless a pool registers agents (an elastic pool)
//! # max_lease_wall: the most a lease's max_wall_seconds may ask for; none by default, and
//! # required by a pool with accelerators (a floor of one GPU or more); a lease asking for
//! # more is refused 422 `wall-time-above-ceiling`; config file only, there is no flag
//! # usage_db (usage.db beside state_file), usage_off (false), profile_store_bytes (268435456):
//! # the flags' names, defaults and rules; usage_db is refused beside usage_off = true
//! # alert_webhook_url, alert_webhook_token_file, alert_repeat_after (1h): every alert of the
//! # scaler is also POSTed as {source, key, text} JSON
//! # to the https URL (http only to a loopback address) when it begins to hold, every
//! # alert_repeat_after while it holds, and again if it clears and returns; delivery is off the
//! # tick path. The token file (owner-only) holds the bearer token the receiver wants. The token
//! # file and alert_repeat_after are refused without the URL.
//!
//! [[pool]]
//! name     = "gpu-static"
//! class    = "gpu-host"
//! provider = { kind = "static", hosts_file = "/etc/jobscheduler/hosts.toml" }
//! ```
//!
//! A pool whose provider launches machines (an elastic pool) also has `max_instances`
//! (1 to 255), `max_spend_per_hour` and `max_spend_per_day` (strings: a decimal with at
//! most two places, above zero, the day cap not below the hour cap), and optionally `idle`,
//! `max_age`, `min_host_lifetime`, `boot_timeout` (`15m`), `launch`, `exec`, `join_token_file`, `agent` and `network`.
//! Every pool, static or elastic, may set `memory_schedulable_percent`, a whole number from
//! 1 to 100 (default 100): the share of the pool's configured memory that is booked, rounded
//! down. A static pool books that share of each host's `memory_bytes` (the pool's key
//! replaces the one in its hosts file); an elastic pool books that share of its floor's
//! memory, 1 GiB. An agent's own claim never raises it, and a hello must still meet the
//! whole floor. `0`, above `100`, a negative number or a word is refused naming the pool and
//! the field.
//! An elastic pool may also set `live_until`, a TOML date-time with an offset such as
//! `2027-01-01T12:00:00Z`: from that instant the pool is in `plan` (the scaler's step ends the
//! go, and a file read after it starts the pool in `plan` with a warning), so a forgotten
//! `launch = "live"` stops spending by itself. Beside the pools' own caps, three totals count
//! the machines of every pool: `max_spend_per_day_total` (the last 24 hours),
//! `max_spend_per_month_total` (the last 30 days, rolling) and `max_spend_since_go_total`
//! (since each pool's go, its first launch since it was last in `plan`; the budget begins
//! again at the next go). They are strings like the pool caps, may be set in `[server]` (all pools) and
//! in a `[budget.<kind>]` table (the pools of that provider kind), and a launch must hold
//! both; a month total below the day total is refused, and a `[budget]` table of a kind the
//! server does not build is refused. A total only stops launches (`blocked` shows
//! `max_spend_total` in `GET /v1/pools`): it never drains or releases a machine, so the
//! machines admitted before it was reached run on, and can take the window past the figure. The
//! overrun is bounded by `max_spend_per_hour` for the cloud lifetime of a launch only when the
//! pool has both `max_age` and `server.max_lease_wall` (a busy host with no `max_age` is never
//! retired) and the releases succeed (a machine the provider will not release, or that the
//! cloud does not end, runs on: an alert says so, no cap does); the file warns when a total is
//! set and a live pool lacks either key. The totals are sums over the pools of their scope:
//! the server's since-go total adds the go of each pool that has one, so it is not a stable
//! figure when pools come and go. A pool taken out of the file for good keeps its go, and its
//! settled spend stays in that sum, until the pool is put back in `plan` for one step (the
//! step that sees a pool in plan ends its go), after which it can be taken out. `GET
//! /v1/pools` shows `launch` as the mode the pool is in now (`plan` once `live_until` has
//! passed) and `live_until_ms`.
//!
//! Rolling back to a server older than the totals: it reads the records file, but its next
//! save drops `go` and `pool_kinds` and it forgets records after 24 hours. After rolling forward
//! again the budget since the go starts afresh at the pool's next launch (the old go and what
//! was settled into it are forgotten), and the month window holds only the days since the
//! rollback, for up to 30 days. For that period tighten the totals, or hold the pools in
//! `plan`. A retry of a launch the provider told to wait (a `Planned` record), made when the
//! pool has no go, begins none: that launch is outside any go, and is bounded by the day and
//! month totals only.
//! `launch` is `plan` unless the file says `live`: a pool in `plan` launches nothing, and
//! one that is `live` launches machines for requests that wait, within `max_instances`,
//! `max_spend_per_hour` and `max_spend_per_day`, and releases them when they have been
//! idle for `idle` (ten minutes unless the file says) and have lived `min_host_lifetime`
//! (none unless the file says). A host is also drained and released at `max_age` from its
//! launch whether or not it is idle, without cutting its work: the drain only stops new
//! leases, and a lease ends within `server.max_lease_wall`. A pool with accelerators (its
//! provider's floor has one GPU or more) must set `idle`, `max_age` and `min_host_lifetime`,
//! and the file `server.max_lease_wall`; `max_age` must be longer than `boot_timeout` and
//! `min_host_lifetime` no longer than `max_age`. Each launch carries `max_age`
//! plus `max_lease_wall` plus the prepare timeout, `unknown_hold` and 10 minutes as the
//! lifetime the cloud is to enforce (a provider whose cloud cannot, Lambda, ignores it: there
//! only `max_age` with a live server bounds the cost). `max_age` and `max_lease_wall` are at
//! most 30 days. A `live` pool also needs `agent =
//! { x86_64 = { url = "https://...", sha256 = "<64 hex digits>" } }`, the build of the agent
//! the launched machine downloads and checks, one for each CPU architecture its offers
//! have (an offer of another is never launched with a build of this one), and `server.advertise_url`; with a Tailscale
//! network it needs `binary = { url = ..., sha256 = ... }` for the Tailscale build too
//! (`api_url` there names another Tailscale API, for a test). After a boot that fails
//! the pool launches nothing for ten minutes.
//! `exec` is `container` unless the file says `sandbox-exec`, or `none` for a pool whose
//! hosts predate the host contract: the hosts must report a `runtime` of that mode, and
//! `gpu-toolkit` when the pool has GPUs. `network` is `{ kind = "default" }`, which is
//! what a pool without it gets, or `{ kind = "tailscale", oauth_client_file = ..., tag
//! = ... }`. At most one elastic pool and one static pool are served in this version.
//!
//! A pool of machines that run `js-agent` and are started by hand has `kind = "joined"`
//! and no `provider`. Its keys are `name`, `class`, `shape` (`{ cpu_millis, memory_bytes }`
//! and optionally `gpus` and `gpu_class`), `max_machines` (from 1 to 262144; required, as
//! in the agent pool file), and optionally `capabilities` (a table of text values, as the
//! agent pool file's `[capabilities]`: a lease request is matched against them, and a
//! host is admitted only if its hello reports each with the same value; a pool without
//! them fits no request that requires a capability), `exec` (as above, but absent is
//! `none`), `heartbeat_interval`, `grace`, `lost_retention` and `join_token_file` (the
//! shared pool token). It launches nothing: `launch`, `network`, `provider`,
//! `max_instances`, `max_spend_per_hour`, `max_spend_per_day`, `idle`, `max_age`, `min_host_lifetime` and `boot_timeout`
//! are each refused naming the key. It is the pool that registers agents, so it excludes
//! an elastic pool, and `server.agent_listen` is required. One-time tokens for its nodes
//! come from `POST /v1/agent/join-tokens` with `provider` the pool's name and
//! `instance_id` the node's; a mint naming another provider is refused 400. An embedder
//! builds the pool from its own settings with [`JoinedPool`] and [`JoinedPool::validate`].
//!
//! # API
//!
//! Every route but `/healthz` needs `Authorization: Bearer <token>` (the scheme in any
//! letter case); a missing or wrong
//! token is a 401 with an empty body. Bodies are JSON, at most 64 KiB. Errors are
//! `{"error": "<code>"}` (a 400 adds `"reason"`, which says what kind of fault it was and
//! where the parser stopped, and never repeats a value of the request).
//!
//! # Who may do what
//!
//! The server is single-tenant: there is one token, and so one trust domain. Whoever
//! holds the token can list every lease, and renew or release any of them, whichever
//! client acquired it. A lease has no owner the server checks. Share the token only with
//! parties that trust each other with every host of the pool. Per-tenant leases (each
//! tenant with a token of its own that sees and changes only its own leases; the design
//! document lists the shared token as a risk) are a follow-up and not built here. The run
//! routes extend that model to code execution: isolation between tenants is lease-id
//! secrecy under the one shared client token, and lease ids are sequential, so whoever
//! holds the token can queue work on, read and stop the runs of any lease.
//!
//! | request | answers |
//! |---|---|
//! | `POST /v1/leases` | 201 `{id, host, expires_at_ms}`; 202 `{id, state: "pending"}` when the provider has not confirmed the start (a `whole-node` lease is always 202: it is listed `pending` until its host's agent reports it ready, then `held` with its expiry and end counted from that report, and is dropped if that takes longer than the pool's `prepare_timeout`, 600 s unless the pool file sets it: at that deadline the table asks the registry first, which ends a lease whose agent stayed silent as a failed prepare (counted against the request, the host excluded for `reserve_backoff`), and expires it itself only if the registry still has no answer at the tick after; a ready report within one tick of the deadline can lose to it, and the host is then restored); 409 for `no-capacity`, `no-fit`, `unsupported-class`, `lease-ended`, `no-capable-host`, `exclusive-not-ready`; 422 for a start-contract refusal (`wall-time-not-positive`, `wall-time-above-ceiling` (the `max_wall_seconds` is above `server.max_lease_wall`, when the file sets one), `reserved-label`, `invalid-label`, `unknown-class`, `below-class-minimum`); the body's `resources` take an optional `disk_bytes`, the scratch disk the lease books (above zero and at most 2^46 - 1, else 400 naming `disk_bytes`; left out or `null` books none). A 400 that names an unknown field means an older server (there is no capability endpoint), so send `disk_bytes` only to a server that has it. 409 `no-fit` is terminal (the request can never fit; read the error code), while `no-capacity` is the one to retry. Today only agent pools and static host files can book disk: elastic and joined pools have no `disk_bytes` key, so a request for disk there is refused `no-fit` and never waits |
//! | `POST /v1/leases/{id}/renew` | 200 `{expires_at_ms, host_reserved?}`, where `host_reserved: true` says the host the lease holds is set aside for a waiting whole-node request (the renewal is not refused for it; the field is left out when false); 404 `unknown-lease`; 409 `not-held` or `expired` |
//! | `DELETE /v1/leases/{id}` | 200 `{state: "freed"}`; 202 `{state: "releasing"}`; 404 |
//! | `GET /v1/leases` | 200 `{leases: [{id, state, host?, acquired_at_ms, expires_at_ms, ends_at_ms, labels}]}` |
//! | `GET /v1/hosts` | 200 `{hosts: [{id, source, shape {cpu_millis, memory_bytes, gpus, gpu_class?, disk_bytes?}, memory {configured_bytes, schedulable_percent, bookable_bytes}, capabilities, labels, lease?, state?, lost?, phase?, agent_version?, releasing?, taint?}]}`; `source` is `static` or `registered`, and `state` (`live`, `lost`, `awaiting`), `lost`, `phase`, `agent_version`, `releasing` (no lease, runs still to finish) and `taint` (`{reason, run_id?, check?}`, once the host reported one), `mark` (`reserved` for a waiting request, `held` while a whole-node lease is committed, `draining` and `releasing` while the host is being scaled away), `reserved_for` (the request id) and `drain_deadline_ms` (when a reservation draining a busy host is given up) are those of a registered host; so is `cordon {expires_at_ms}`, present while the host is cordoned (see "Cordon" below: a cordon left by a dead hook lapses within its last `lease_secs`, at most 3600 s after its last extension, or the host's own `:uncordon` lifts it) |
//! | `GET /healthz` | 200 `ok`, no token; 503 once the tick loop has stopped |
//! | `GET /v1/pools` | 200 `{pools: [{name, kind, network, exec, launch, memory {configured_bytes, schedulable_percent, bookable_bytes}, state, reasons, credentials, network_check, instances, blocked, no_offer, last_launch, machines {host, state, offer, zone, price_cents_per_hour, provider_id}, orphans, last_tick_ms, scaler, next_launch}], warnings}`; `state` is `checking` (no pass yet), `available` or `unavailable`; `provider_checks` is the provider's own checks of the last pass, each `{name, ok, reason, note, checked_at_ms}`; `next_launch` holds the launch plan with every secret printed as `***`; no token, on `server.listen` only, and not served (404) when that is not a private address, nor to a peer that is not a private address (an IPv4-mapped IPv6 peer counts as its IPv4 form) |
//! | `GET /status` | 200 the same data as a page; same rules |
//!
//! An unknown route is a 404, a known route with another method a 405, a body that is
//! not the JSON the route takes a 400, a body over the cap a 413, and a lease file that
//! cannot be written a 500 (`state-write-failed`; the cause is in the log).
//!
//! A 500 `state-write-failed` says the lease file could not be written, and what the
//! request left behind depends on the route:
//!
//! - `POST /v1/leases`: the file could not be written before the provider was called,
//!   so the provider was not called and no host was started. The lease is not granted
//!   and the client has no id. The record stays pending in memory, is listed until the
//!   next tick, and that tick finds nothing on the provider and drops it; it is never
//!   started later. The client may retry the acquire after this 500.
//! - `POST /v1/leases/{id}/renew`: the new expiry took effect in memory only. The lease
//!   is held and the client may renew again; a restart before the next successful
//!   write loses the extension.
//! - `DELETE /v1/leases/{id}`: the lease is marked releasing in memory and the provider
//!   was not asked to stop it yet. The next tick stops it. Treat the lease as released.
//!
//! An acquire is not safe to retry blindly. If the connection drops or times out after
//! the request was sent and before the answer arrived, the client cannot tell whether
//! the lease was granted: the server may hold a lease, and a host, that the client has
//! no id for. A client must not repeat `POST /v1/leases` after a lost answer without
//! first listing (`GET /v1/leases`) and finding its lease by the labels it sent, or the
//! retry can take a second host; the first one lapses only when its ttl does. An
//! idempotency key, so that a repeated acquire returns the lease it made, is a
//! follow-up and not built here. Renew and release name their lease and are safe to
//! repeat.
//!
//! A whole-node request whose tries on hosts all failed is answered `exclusive-not-ready`
//! once, and the server then forgets the request. A client that lost that 409 and
//! retries is placed afresh, on a fresh host choice, as a new request; it does not get
//! the refusal again. `no-capable-host` says no host of the pool could ever serve the
//! request, as the pool stands.
//!
//! The table sits behind one async mutex, taken for the table call alone and never held
//! across an await. A table call is synchronous: it writes and syncs the lease file and
//! calls the provider on a runtime worker thread, with the lock held, so every other
//! request and the tick wait for it. That is cheap for the static pool; a provider that
//! can take long (one that calls over a network) must bound its own calls, see
//! [`CapacitySource`].
//!
//! # Agents
//!
//! With `--agent-pool-config` (and, for hosts that join with a token shared by the pool,
//! `--agent-token-file`) a machine can register itself as
//! a host of the pool the pool file describes, beside the static pool (if any). The
//! machine's agent calls two routes with a join token, on the same listener, body cap,
//! timeouts and connection cap as everything else. Without the pool file the routes do not
//! exist and the server builds exactly what it always has.
//!
//! | request | answers |
//! |---|---|
//! | `POST /v1/agent/hello` | 200 the welcome (protocol, epoch, heartbeat interval, grace, pool, session secret); 400 `invalid-hello`, a wire code, or `unsupported-protocol` with the majors served; 403 `unknown-pool`, `host-bound`, `host-listed`, `shape-not-accepted`, `host-contract` and `execution-mismatch` (with `"field"`, the capability at fault; only a pool that declares an execution mode sends them), `pool-full`; 409 `host-live`; 413 `body-too-large`; 429 `rate-limited` with `Retry-After`; 503 `session-unavailable` |
//! | `POST /v1/agent/heartbeat` | 200 `{commands, result_acks, taint_ack}` (see "Runs" and "Taint"); 400 `invalid-heartbeat`; 403 `host-bound`, `session-mismatch`; 409 `stale-epoch`, `unknown-host` (the agent says hello again); 413 |
//! | `POST /v1/hosts/{id}:cordon` | with a host credential, for that host only: 200 the host as `GET /v1/hosts/{id}` reads it, plus `already_cordoned`; body optional, `{"lease_secs": n}` (default 300, cut to 3600); 400 `invalid-cordon`; 403 `host-credential-required`, or the code of a credential that is not the host's (`join-token-not-admitted`, `join-token-expired`); 409 `unknown-host`; 413; 503 `cordon-unsaved` |
//! | `POST /v1/hosts/{id}:uncordon` | as the cordon, with no body: 200 the host, plus `lifted`; 200 also when it was not cordoned |
//! | `GET /v1/hosts/{id}` | as the cordon: 200 `{id, state, epoch, session_started_ms?, leases, lease?, releasing, cordon? {expires_at_ms, expires_in_ms}, taint?, agent_version?, runtime?}` |
//!
//! **Cordon.** A host takes itself out of placement with `POST /v1/hosts/{id}:cordon`, so
//! that an update of its agent can wait for the lease it holds to end without a new one
//! arriving. The routes serve the host of the credential and no other: the credential is
//! the host credential (or the one-time token) that registered the host, and the shared
//! pool token names no host and is refused. A cordon is a lease the requester sets and
//! renews: a repeat extends it from the time of the repeat and never shortens it, the
//! server cuts a request to an hour, and a cordon that is not renewed lifts by itself. A
//! cordoned host takes no new lease; the lease it holds, and its runs, go on, and it
//! reads `leases: 0` and `releasing: false` when they have ended. `already_cordoned` says
//! whether the host was cordoned before the call, so that a requester lifts only a cordon
//! it placed (the server does not tell who placed one, and an uncordon lifts any cordon, so a
//! requester uncordons only after a cordon whose reply said `already_cordoned: false`). There is no
//! total cap on a cordon that is renewed; a requester that dies leaves it to lapse within its last
//! `lease_secs`, at most an hour. A cordon is written to the agent
//! state file before it is acknowledged and survives a restart; an uncordon is written the
//! same way. The routes are served by the agent routes' listener (`agent_listen`, when
//! there is one).
//!
//! A leasing client asks for a lease as ever and is given a static host first, a
//! registered one when the static pool has none; it cannot tell which it got except by
//! `GET /v1/hosts`. A registered host takes a lease only while it is live (heard within
//! the grace period) and serving. A host that goes silent keeps its lease and reads
//! `Unknown` until it is heard again or the lease ends by its ttl or a release.
//!
//! **Tokens.** The join tokens and the client token are different secrets: each is a 401
//! with an empty body on the other's routes, and a join-token file that holds the client
//! token is refused. A file may hold several tokens, so that a new one can be added
//! before an old one is removed; the file is read again at every tick, and a read that
//! fails (a half-saved edit, a mode that is no longer private) keeps the tokens in force,
//! is logged and is counted in [`Running::agent_errors`]. A host id is bound to the
//! credential (the SHA-256 of the token) of the first hello that admitted it, so
//! removing a token also removes the ability of the hosts bound to it to heartbeat or say
//! hello again: that is revocation. Moving such a host to a new token means the registry
//! forgetting it first (it is forgotten after it has been lost for `lost_retention`, if
//! no lease holds it), and a host that holds a lease is never forgotten.
//!
//! **What a join token can and cannot do.** Every host of a pool shares the join tokens.
//! A session secret issued in the welcome and carried in every heartbeat keeps one
//! token holder from keeping another's host alive, and a binding to the first credential
//! (and to the instance, when the hello names one) keeps it from taking over a live host.
//! A holder of a token can still say hello for an id that is not instance-bound once the
//! host is lost, and can fill `max_machines` with made-up ids; hellos are limited per
//! token (`--agent-hello-burst`, `--agent-hello-interval`) to slow that, not to stop it.
//! Per-host credentials close it and are not built. The limit is one bucket for each join
//! token, shared by every agent of the pool that uses it, so a holder of the token can
//! spend the bucket and make the pool's other agents get 429 `rate-limited` on their
//! hellos until it refills: that is the per-host credential gap (design D18) again, not
//! something the limit prevents. A refusal is logged once per token per interval, and
//! the bucket of a token that has left the token file is dropped at the next tick that
//! reads the file. The registered hosts of a pool are
//! as trusted as the holders of its join tokens: a lease is a whole machine that the
//! pool's holders run.
//!
//! **Taint.** The first taint a heartbeat reports is kept for the host (`taint` in
//! `GET /v1/hosts`) and acknowledged with `taint_ack: true`, but only once it is in the
//! state file, so an agent that stops reporting it has not left the server able to lose it
//! in a restart; when the file cannot be written the host is tainted at once and the reply
//! says `taint_ack: false`, so the agent reports again. A start the agent refuses with the
//! detail `host-tainted` taints the host the same way, since an agent that cannot write
//! its own marker (a full disk) may never report. A tainted host is sent no start, takes
//! no new run (409 `host-tainted`), takes no new lease (a request only it could serve is
//! `no-capacity`; a lease already on it is untouched until it ends), still counts toward
//! `max_machines`, is never forgotten for being silent, and keeps its
//! taint across its own hello and a server restart (the `.agents` file holds it, see
//! "Restart"). The runs it already holds finish: their stops are sent, their results are
//! acknowledged, and the budget returns when the host stops listing them. Nothing clears a
//! taint: an operator stops the server and removes the `taint_` keys of the host's entry
//! in the `.agents` file.
//!
//! **Not built.** Clearing a taint, the agent's side of the runs below, several pools, a
//! deny list of host ids.
//!
//! # Runs
//!
//! With agents on, a client that holds a lease on a registered host can queue container
//! runs, and native runs on a macOS host, on it. This API is interim: it is removed when the run service replaces it.
//!
//! | request | answers |
//! |---|---|
//! | `POST /v1/leases/{id}/runs` | body: `image` or `native` (both is 422 `invalid-image`, neither is 400): `native` is `{program, args?, network?}`, a program the agent runs on the host with no container (an absolute path, or `@bundle/<entrypoint>` with `bundle`), `network` one of `off` (the default), `loopback` or `on`, and nothing else is read from it; `bundle?` (`name[:tag]@sha256:<64 lower case hex>`, only beside `native`: the toolchain the job runs from); `args?` (of a container run), `limits {cpu_millis, memory_bytes, disk_bytes?, pids?, wall_ms?}` (`disk_bytes`: the most scratch disk the run may use, above zero and below 2^46), `gpu_indices?`, `mounts?`, `files?` (`[{name, contents}]`: secret text files the agent writes before the container starts and mounts read-only at `/run/js-files/<name>`, mode 0444; at most 8, names `[A-Za-z0-9._-]` and unique, 16 KiB each and 64 KiB in all, no NUL; contained runs only; the list, a run's entry and the logs never show the contents), `isolation?`, `labels?`, `profile?` (a string of 1 to 128 bytes of `0-9`, `a-z`, `.`, `_` and `-`, the first a digit or a letter: the name of the profile a clean run's cost curve is learned under, the client's own key for the work the run does). 202 `{run_id, state: "queued"}`; 404 `unknown-lease`; 409 `not-held`, `host-lost`, `host-tainted`, `not-ready` (a whole-node lease whose agent has not reported `exclusive_ready`), `no-capacity`, `host-not-registered` (a lease on a static host), `files-unsupported` (the body has `files` and the host's agent did not report the `run-files` capability, so it would ignore them), `disk-unsupported` (the body has `limits.disk_bytes` and the host's agent did not report the `scratch-quota` capability, so it would run the job unbounded), `native-unsupported` (the body has `native` and the host's agent did not report `exec` as `sandbox-exec`), `bundle-unsupported` (the body has `bundle` and the host's agent reported no `bundle-root`, so it holds no bundle store); 422 `invalid-bundle` (a malformed bundle, or one beside no `native`), `bundle-digest-required` (a bundle without a digest), `invalid-program`, `limits-exceed-shape`, `invalid-image`, `image-digest-required` (the image has no `@sha256:<64 lower case hex>` digest; `name:tag@sha256:...` is accepted), `invalid-mount`, `invalid-job`, `invalid-profile` (a `profile` that is not that grammar), `wall-time-not-positive`, `host-jobs-not-allowed`; 400 `invalid-file` (a file is badly named, repeated, too large or holds a NUL, or there are too many) and 400 for a body that is not this |
//! | `GET /v1/leases/{id}/runs` | 200 `{runs: [{run_id, state, outcome?, exit_code?, detail?}]}`, oldest first; `state` is `queued`, `starting`, `running`, `exited` (the container has exited and its result is held, a stop asked for or not), `stopping`, `ended` or `never-started` |
//! | `GET /v1/leases/{id}/runs/{run_id}` | 200 one entry of the list, with `log_tail {stdout, stderr, truncated}` when the host read the end of the job's output (a contained run that exited; at most 8 KiB of each stream; the list leaves it out); the text is the job's own, returned as written to whoever can read the run, so a task must not print secrets; it is untrusted bytes (it may hold ANSI escapes, U+202E or NUL), so a consumer that prints it to a terminal must strip control characters; 404 `unknown-lease` or `unknown-run`; 410 `run-forgotten` |
//! | `DELETE /v1/leases/{id}/runs/{run_id}` | 202 `{run_id, state}`; 404 `unknown-lease` or `unknown-run`; 410 `run-forgotten` |
//!
//! - **Delivery.** A run is carried to the host on its next heartbeat reply as a start, and
//!   again on every reply until the host lists the run in its heartbeat. A stop is a
//!   retained intent sent on every reply until the host no longer lists the run. A run the
//!   host lists that the server holds no live record of is answered with a cancel and
//!   never adopted. At most 1024 commands go in a reply, stops first, then cancels, then
//!   starts.
//! - **Results** ride in the heartbeat and each gets an acknowledgement: `ACCEPTED`,
//!   `DUPLICATE`, `STALE` (the lease has moved on, or the server has no record of the
//!   run: a result of a run the server forgot at a restart is likewise `STALE` and is
//!   dropped), `UNKNOWN_LEASE`, or `MISMATCH` (a run of another host, or named under
//!   another lease). An accepted result is shown by `GET`. At most 256 finished runs
//!   are kept per host (`MAX_FINISHED_RUNS`): beyond that the oldest finished run, and
//!   with it its result, is evicted and reads `unknown-run`.
//! - **Bounds.** The sum of the limits of a host's live runs fits the shape the lease
//!   booked, and at most 256 runs are live at once; a run that does not fit is refused
//!   `no-capacity`, not queued. A run is bounded by the lease: the server keeps the instant
//!   the lease ended when the run was queued, and every start is made from the time left
//!   at the moment it is sent. Its wall limit is the one asked for (that, when none is
//!   asked), cut to the time left then, so the wall cap is recomputed on every send, and
//!   the agent may start it for no longer than that either. A renewal does not stretch a
//!   run already queued, so a client that renews cannot run a job past the lease time it
//!   queued it under. Once that time is up no start is sent: a run the host has not listed
//!   gets a stop (the agent then retires its id) and, when the host still does not list it,
//!   reads `never-started` and gives its budget back, whether or not the lease lives on.
//!   Host isolation is refused until the scheduler records the taint such a job leaves.
//! - **Reuse.** A run's limits return to the lease only when the host no longer lists the
//!   run, which it does after cleanup. When a lease ends (a release or its expiry) with
//!   live runs, the booking is free at once but the host takes no new lease, and shows
//!   `releasing` in `GET /v1/hosts`, until a heartbeat lists none; each live run is sent a
//!   stop meanwhile.
//! - **Run ids** are minted by the server from the lease table's term and a counter, so
//!   one is never reused, a restart included.
//! - **Restart.** Runs are in memory only. A restart forgets them; a host that lists one
//!   in its hello or heartbeat is told to cancel it, and takes no new lease until it
//!   lists none. A held run with no server record is cancelled even when its lease still
//!   holds the host, and the agent kills a cancelled run without a result: its work is
//!   lost and nothing tells the lease holder, except that `GET` and `DELETE` of its run id
//!   (minted in an earlier boot, see the term in the id) answer 410 `run-forgotten`,
//!   where an id the lease never had is 404 `unknown-run`. Persisting runs is later work,
//!   as are queueing and placement.
//! - **A host that was lost** before it listed a queued run never starts it: the run
//!   reads `never-started` once the host is heard again.
//!
//! A host out of the serving phase is sent no start, and takes no new run (`no-capacity`).
//!
//! # Limits
//!
//! Before any token is checked a client can hold only so much. At most
//! `--max-connections` connections are open (the permit is taken before a connection is
//! accepted and kept until it closes), a TLS handshake that is not finished in
//! `--handshake-timeout` is dropped, and a connection whose request head has not arrived
//! in `--header-timeout` is closed; that clock starts again after each response, so it
//! also closes a keep-alive connection that idles. A read from the client that makes no
//! progress for `--read-timeout` closes the connection too. Writes are bounded as well: a write to the client that makes no progress for
//! `--write-timeout` closes the connection (a client that sends requests and never reads
//! the answers is cut and its slot is free again), and the answer to a connection's
//! `--max-requests`th request says `Connection: close`, with later requests pipelined
//! behind it unanswered. The server reads at most 64 KiB of a request head, so an
//! unauthenticated client cannot make it buffer more before the token is checked (a
//! larger head is a 431). A client that sends its head and then stalls on the
//! body does so with a valid token, since the token is checked on the head; it is cut
//! by `--read-timeout`. Those two timeouts limit stalls, not total time. A failed accept (descriptors used
//! up, say) is logged and retried after 100 ms.
//!
//! # Restart
//!
//! The lease file is the record of the leases, and for the static pool it is also the
//! record of the pool's bookings: the pool is built again from `--pool-config` at every
//! start and holds its bookings in memory only. So before the table looks at a held
//! lease, the server reads the hosts the file names ([`js_lease::read_bookings`]) and
//! asks the [`CapacitySource`] to seat each lease on its host again, in lease id order.
//! A lease the source seats is observed present, stays held on the same host, and the
//! host is not leased to anyone else; it can be renewed and released as before.
//!
//! A booking the source cannot seat is logged at warn level with the lease, the host,
//! the state and the reason, and the table then finds the provider ignorant of it and
//! drops it: a host the pool file no longer lists, or a host the file gives to two
//! leases (the lower id keeps it). Edit the pool file with that in mind: removing a
//! host from it ends the leases on it at the next start. A source that keeps its own
//! state across restarts (a provider that is a service, or hosts that agents report) is
//! free to ignore the bookings it is given.
//!
//! The registered hosts are a source of that second kind in part. The registry holds
//! nothing across a restart, so each booking of the lease file that names a host the
//! static pool does not list is seated on the registry as an *awaiting* host: its lease
//! is held and observes `Unknown` (never `Absent`, so the table keeps it), the host takes
//! no new lease, and it counts toward `max_machines`, until its agent says hello again.
//! That hello is admitted as any other and the lease then observes `Present`. An awaiting
//! host whose lease has ended and whose agent has not returned is forgotten, binding
//! included, once the grace period and `lost_retention` have passed since the restart
//! that seated it; the next tick writes the file of bindings without it. An agent
//! whose heartbeat reaches a restarted server is answered 409 `unknown-host` and says
//! hello.
//!
//! What would otherwise let any join-token holder take such a lease with the first hello
//! after the restart is the binding of the host id to the credential and instance it was
//! admitted with. The server keeps the bindings in a file of its own beside the lease
//! file, `<state file>.agents` (TOML, mode 0600, replaced whole, keys it does not know
//! ignored), written when a hello admits a host and again at a tick when it differs, and
//! reads it at start. It holds SHA-256 digests of join tokens, not tokens, and the taint of
//! each host that reported one: such a host is put back at start as an awaiting host (and
//! counts toward `max_machines`) whether or not it holds a lease. The file is
//! not part of the lease file, so the lease file's format and the static-only start are
//! unchanged. A file that is present and unreadable, or not private, is a refused start
//! naming `--state-file`. A missing file means no bindings: a lease on a registered host
//! then restores without one, and the first hello binds the id. If the file cannot be
//! written (the failure is logged and counted) a lease granted meanwhile survives a
//! restart without its binding until a later tick writes it.
//!
//! # Usage
//!
//! With agents on (and no `--usage-off`) a heartbeat may carry the node's load. After the
//! registry accepted the beat, the server checks the load, turns it into plain values and
//! gives it to a [`UsageSink`]; the sink owns the arithmetic and the storage. A load that
//! is bad costs nothing but its own data: the beat's results, held list and taint are
//! handled as ever and the reply is unchanged (the load is counted in
//! [`Running::usage_counters`]). A beat the registry refuses (unknown host, stale epoch,
//! wrong session) records nothing. Runs that ended cleanly (an `ACCEPTED` result with the
//! outcome `succeeded` or `failed`) are handed to the sink with their series once the
//! host has sent two more beats, so the lease's last partial minute is not lost, if a
//! [`KeySource`] names the work they did; by default that is the `profile` name the client
//! supplied when it queued the run, so a run queued without one is not learned from and
//! `runs_without_key` counts it. The seam types and
//! [`NoUsage`] are public so that a test or an embedding program can supply its own
//! ([`Config::with_usage_sink`], [`Config::with_key_source`]). Without a sink of its own
//! the server records into the SQLite file `--usage-db` names (beside the lease file by
//! default): node minutes are written by the flush, and the profiles learned from clean
//! runs are kept under `--profile-store-bytes`; a file that cannot be opened refuses the
//! start. `--usage-off` leaves the usage side out altogether.
//!
//! Every tick acts by the wall clock of the server: the sink is flushed every five
//! minutes and once more at shutdown, and pruned at the first tick and once a day. A
//! flush logs one `node minute` line for each node minute it closed and a warning for
//! each node and minute that had readings dropped.
//!
//! | request | answers |
//! |---|---|
//! | `GET /v1/usage/nodes/{id}?from=<minute>&to=<minute>` | 200 `{host, from, to, rows: [{minute, cpu_millis, ws_bytes, leases}], next}`; minutes are wall minutes since the Unix epoch, default the last 60 minutes up to now, at most 1,440 rows, and `next` is the minute to ask from for the rest (else `null`); a node with no rows is 200 with none, so an unknown node and a quiet one look alike; 400 for an id that is not exactly an id (capitals, escapes, slashes, dot-only segments, over 63 bytes), a query that is not `from` and `to` once each in digits, or `to` before `from`; 503 `usage-unavailable` |
//! | `GET /v1/usage/keys/{key}` | 200 `{key, runs, curve, last_used_day, stale}`, `curve` being `null` until there are enough runs, else `[{minute, cores_milli, ws_bytes}]` (`cores_milli` is `null` when the fit did not learn cores: too few runs outside node pressure, a memory-only curve); 404 `unknown-key`; 400 for a key that is not 1 to 128 characters of `[0-9a-z._-]` starting with a letter or digit; 503 `usage-unavailable` |
//!
//! Both are behind the client token (a join token is a 401), exist only when usage is on,
//! and never echo the request in an answer or a log line.
//!
//! # Ticks
//!
//! Only a tick expires a lease, retries a stop or resolves a pending start. The server
//! ticks the table (then advances the agent registry, when agent registration is on) at every tick of its [`Ticker`] (a timer of `--tick` in production),
//! logs what a tick changed, and counts the ticks that could not write the lease file
//! ([`Running::save_errors`]); `/healthz` answers 503 once the loop has stopped. On
//! SIGTERM or ctrl-c the server stops accepting, gives the requests
//! in flight `--drain-timeout` to finish (a connection still open then is cut), ticks
//! once more and exits 0. If that last tick cannot write the lease file the file is
//! behind the table, so the server logs the cause and exits 1 instead. A second signal
//! while it drains is not needed: the drain is bounded.

mod adapt;
mod agent_api;
mod agent_pool;
mod agent_state;
mod agent_tokens;
mod agents;
mod alerts;
mod api;
pub mod bootstrap;
mod capacity;
mod checks;
mod config;
mod error;
mod file;
mod file_checks;
mod flags;
mod hosts;
mod io_timeout;
mod join_api;
mod join_tokens;
mod kinds;
mod layered;
mod listener;
pub mod network;
mod pool;
mod rate;
mod run_api;
mod scaler;
mod sched;
mod server;
mod status;
mod ticker;
mod tls;
mod usage;
mod usage_api;
mod usage_hub;
mod usage_js;
mod usage_map;
mod usage_setup;
mod util;
mod wire;

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use js_lease::SystemClock;
use tokio::runtime::Builder;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

pub use adapt::SharedClock;
pub use api::BODY_LIMIT;
pub use capacity::{CapacitySource, Seated, StaticPoolSource, Unseated};
pub use config::{
    BootMode, Cli, Config, Limits, MIN_TOKEN_BYTES, TlsFiles, Token, is_private_address,
    parse_duration,
};
pub use error::StartupError;
pub use file::{ElasticPlan, JoinedPool, Launch, NetworkPlan};
pub use kinds::{
    BuildCtx, Built, Ctor, ElasticBuilt, Floor, KindError, Kinds, OwnerOnlyFiles, SecretFiles,
};
#[doc(hidden)]
pub use layered::LayeredProvider;
pub use pool::LaunchInputs;
pub use scaler::{
    Call, Effects, KEY_VALID_FOR, Launcher, REGISTRY_FLOOR, REGISTRY_QUIET, RETRY_AFTER,
    RecordsFile, Scaler, UnremovedFile,
};
pub use server::{Running, start, start_with_checks};
pub use ticker::{ChannelTicker, IntervalTicker, Tick, TickHandle, Ticker, channel_ticker};
pub use usage::{
    BadKey, ClockIn, DropReason, Fit, HostLimits, KeyProfile, KeySource, LoadIn, MAX_KEY_LEN,
    MinuteIn, NoKeys, NoUsage, NodeRow, Observation, Observed, ProfileCurves, ProfileKey, RunEnd,
    RunMinute, RunSeries, SinkCounters, UsageFault, UsageSink,
};
pub use usage_hub::UsageCounters;

use error::Failure;

/// The program: runs with the process's arguments and standard streams.
pub fn main() -> ExitCode {
    run(
        std::env::args_os(),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    )
}

/// Runs the server as the command line `args` (the program name first) says, until
/// SIGTERM or ctrl-c. Usage text and the line naming the listen address go to `out`;
/// a refusal or failure goes to `err`.
///
/// Exit status: 0 after an orderly stop (and for `--help`), 2 for a usage error or a
/// start that was refused (the message names the flag), 1 for any other failure.
pub fn run(
    args: impl IntoIterator<Item = OsString>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let parsed = Cli::try_parse_from(args);
    let cli = match parsed {
        Ok(cli) => cli,
        Err(usage) => return usage_exit(&usage, out, err),
    };
    let result = Config::from_cli(cli)
        .map_err(Failure::from)
        .and_then(|config| serve(config, out));
    exit_code(result, err)
}

/// Prints a usage error where clap would and picks the exit status clap picks.
fn usage_exit(usage: &clap::Error, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let _ = match usage.use_stderr() {
        true => write!(err, "{usage}"),
        false => write!(out, "{usage}"),
    };
    ExitCode::from(u8::try_from(usage.exit_code()).unwrap_or(2))
}

/// Maps a result to the exit status, printing a failure to `err`.
fn exit_code(result: Result<(), Failure>, err: &mut dyn Write) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            let _ = writeln!(err, "js-lease-server: {failure}");
            failure.code()
        }
    }
}

/// Serves `config` on a runtime of its own until a termination signal.
fn serve(config: Config, out: &mut dyn Write) -> Result<(), Failure> {
    Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(Failure::Runtime)
        .and_then(|runtime| runtime.block_on(serve_until_signal(config, out)))
}

async fn serve_until_signal(config: Config, out: &mut dyn Write) -> Result<(), Failure> {
    // The subscriber is global; a second server in one process keeps the first's.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .try_init();
    let ticker = IntervalTicker::new(config.tick_period());
    let running = start(config, SharedClock::new(SystemClock), ticker).await?;
    let _ = writeln!(out, "listening on {}", running.addr());
    let _ = out.flush();
    termination().await.map_err(Failure::Runtime)?;
    running.shutdown().await.map_err(Failure::Runtime)
}

/// Completes at SIGTERM or SIGINT.
async fn termination() -> std::io::Result<()> {
    let (sender, mut received) = mpsc::channel(2);
    for kind in [SignalKind::terminate(), SignalKind::interrupt()] {
        let mut stream = signal(kind)?;
        let sender = sender.clone();
        tokio::spawn(async move {
            stream.recv().await;
            let _ = sender.send(()).await;
        });
    }
    received.recv().await;
    Ok(())
}

//! A lease server: a [`js_lease::LeaseTable`] behind an authenticated HTTP JSON API, so
//! a CI job can hold a whole GPU host for as long as it renews.
//!
//! The library is the program: [`run`] takes the arguments and the streams to report
//! to, and `main` calls it. The pieces:
//!
//! - [`Config`] and [`Cli`]: the flags, and the checks that refuse a start (a token file
//!   anyone can read, a listen address off the loopback without TLS).
//! - [`CapacitySource`]: where the hosts come from. [`StaticPoolSource`] reads the
//!   operator's pool file. With agent registration on (see "Agents" below), the static
//!   pool and the hosts agents register are two sources of the same trait, layered
//!   behind one provider.
//! - [`start`] and [`Running`]: the server, with time and ticks injected ([`SharedClock`],
//!   [`Ticker`]) so a test steps both.
//!
//! # Flags
//!
//! Configuration is flags only; the one secret, the bearer token, is read from a file
//! that only its owner can use.
//!
//! | flag | meaning |
//! |---|---|
//! | `--listen <addr>` | default `127.0.0.1:7300`; off the loopback it needs both TLS flags |
//! | `--state-file <path>` | the lease file |
//! | `--boot initialise\|resume` | required: `initialise` may create the file, `resume` needs it |
//! | `--pool-config <path>` | the static pool file ([`js_pool_static`]); optional when agents register hosts, and one of the two sources is required (a start with neither is refused naming this flag) |
//! | `--agent-token-file <path>` | turns agent registration on: join tokens, one per line, each as strong as `--token-file`'s, in an owner-only file; read again at every tick. Needs `--agent-pool-config` |
//! | `--agent-pool-config <path>` | the pool agents join, as TOML: `name`, `class`, `max_machines`, `[shape]`, `[capabilities]`, `heartbeat_interval`, `grace`, `lost_retention`. Needs `--agent-token-file` |
//! | `--agent-hello-burst`, `--agent-hello-interval` | the hellos one join token may say in a burst (`60`) and how often one more is allowed after (`1s`) |
//! | `--token-file <path>` | required: a regular file owned by the server's user, mode 0600 or 0400, holding one token of printable ASCII of at least 32 bytes ([`MIN_TOKEN_BYTES`]; trailing whitespace is dropped). A shorter token is refused at start |
//! | `--default-ttl`, `--max-ttl` | `600s` and `3600s` unless given; `90`, `90s`, `15m`, `2h` |
//! | `--tick` | how often the table is ticked and, with agent registration on, the agent registry is advanced (reservation drain and prepare deadlines), `5s` unless given |
//! | `--tls-cert`, `--tls-key` | PEM files; both or neither. The key file must be owner-only like the token file. Both are read at start only: renew a certificate by restarting the server |
//! | `--max-connections` | connections open at once, `256` unless given; at the cap the server stops accepting |
//! | `--handshake-timeout` | a TLS client's time to finish its handshake, `10s` |
//! | `--header-timeout` | a client's time to send a request head, from the connection's start and after each response (so also how long a keep-alive connection may idle), `10s` |
//! | `--read-timeout` | how long a read from a client may make no progress before the connection is closed (a client that sends a head and stops in the body); keep it above `--header-timeout`, `30s` |
//! | `--write-timeout` | how long a write to a client may make no progress before the connection is closed, `10s` |
//! | `--max-requests` | requests served on one connection before its last answer closes it, `100` |
//! | `--drain-timeout` | how long a stop waits for requests in flight before it cuts the rest, `10s` |
//!
//! A start that is refused prints the flag at fault and exits with status 2.
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
//! | `POST /v1/leases` | 201 `{id, host, expires_at_ms}`; 202 `{id, state: "pending"}` when the provider has not confirmed the start (a `whole-node` lease is always 202: it is listed `pending` until its host's agent reports it ready, then `held` with its expiry and end counted from that report, and is dropped if that takes longer than the pool's `prepare_timeout`, 600 s unless the pool file sets it: at that deadline the table asks the registry first, which ends a lease whose agent stayed silent as a failed prepare (counted against the request, the host excluded for `reserve_backoff`), and expires it itself only if the registry still has no answer at the tick after; a ready report within one tick of the deadline can lose to it, and the host is then restored); 409 for `no-capacity`, `no-fit`, `unsupported-class`, `lease-ended`, `no-capable-host`, `exclusive-not-ready`; 422 for a start-contract refusal (`wall-time-not-positive`, `reserved-label`, `invalid-label`, `unknown-class`, `below-class-minimum`) |
//! | `POST /v1/leases/{id}/renew` | 200 `{expires_at_ms, host_reserved?}`, where `host_reserved: true` says the host the lease holds is set aside for a waiting whole-node request (the renewal is not refused for it; the field is left out when false); 404 `unknown-lease`; 409 `not-held` or `expired` |
//! | `DELETE /v1/leases/{id}` | 200 `{state: "freed"}`; 202 `{state: "releasing"}`; 404 |
//! | `GET /v1/leases` | 200 `{leases: [{id, state, host?, acquired_at_ms, expires_at_ms, ends_at_ms, labels}]}` |
//! | `GET /v1/hosts` | 200 `{hosts: [{id, source, shape, capabilities, labels, lease?, state?, lost?, phase?, agent_version?, releasing?, taint?}]}`; `source` is `static` or `registered`, and `state` (`live`, `lost`, `awaiting`), `lost`, `phase`, `agent_version`, `releasing` (no lease, runs still to finish) and `taint` (`{reason, run_id?, check?}`, once the host reported one), `mark` (`reserved` for a waiting request, `held` while a whole-node lease is committed), `reserved_for` (the request id) and `drain_deadline_ms` (when a reservation draining a busy host is given up) are those of a registered host |
//! | `GET /healthz` | 200 `ok`, no token; 503 once the tick loop has stopped |
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
//! With `--agent-token-file` and `--agent-pool-config` a machine can register itself as
//! a host of the pool the pool file describes, beside the static pool (if any). The
//! machine's agent calls two routes with a join token, on the same listener, body cap,
//! timeouts and connection cap as everything else. Without those flags the routes do not
//! exist and the server builds exactly what it always has.
//!
//! | request | answers |
//! |---|---|
//! | `POST /v1/agent/hello` | 200 the welcome (protocol, epoch, heartbeat interval, grace, pool, session secret); 400 `invalid-hello`, a wire code, or `unsupported-protocol` with the majors served; 403 `unknown-pool`, `host-bound`, `host-listed`, `shape-not-accepted`, `pool-full`; 409 `host-live`; 413 `body-too-large`; 429 `rate-limited` with `Retry-After`; 503 `session-unavailable` |
//! | `POST /v1/agent/heartbeat` | 200 `{commands, result_acks, taint_ack}` (see "Runs" and "Taint"); 400 `invalid-heartbeat`; 403 `host-bound`, `session-mismatch`; 409 `stale-epoch`, `unknown-host` (the agent says hello again); 413 |
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
//! runs on it. This API is interim: it is removed when the run service replaces it.
//!
//! | request | answers |
//! |---|---|
//! | `POST /v1/leases/{id}/runs` | body: `image`, `args?`, `limits {cpu_millis, memory_bytes, pids?, wall_ms?}`, `gpu_indices?`, `mounts?`, `isolation?`, `labels?`. 202 `{run_id, state: "queued"}`; 404 `unknown-lease`; 409 `not-held`, `host-lost`, `host-tainted`, `not-ready` (a whole-node lease whose agent has not reported `exclusive_ready`), `no-capacity`, `host-not-registered` (a lease on a static host); 422 `limits-exceed-shape`, `invalid-image`, `invalid-mount`, `invalid-job`, `wall-time-not-positive`, `host-jobs-not-allowed`; 400 for a body that is not this |
//! | `GET /v1/leases/{id}/runs` | 200 `{runs: [{run_id, state, outcome?, exit_code?, detail?}]}`, oldest first; `state` is `queued`, `starting`, `running`, `exited` (the container has exited and its result is held, a stop asked for or not), `stopping`, `ended` or `never-started` |
//! | `GET /v1/leases/{id}/runs/{run_id}` | 200 one entry of the list; 404 `unknown-lease` or `unknown-run`; 410 `run-forgotten` |
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
mod api;
mod capacity;
mod config;
mod error;
mod hosts;
mod io_timeout;
mod layered;
mod listener;
mod rate;
mod run_api;
mod server;
mod ticker;
mod tls;
mod util;
mod wire;

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use clap::Parser;
use js_lease::SystemClock;
use tokio::runtime::Builder;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

pub use adapt::SharedClock;
pub use api::BODY_LIMIT;
pub use capacity::{CapacitySource, Seated, StaticPoolSource, Unseated};
pub use config::{BootMode, Cli, Config, Limits, MIN_TOKEN_BYTES, TlsFiles, Token, parse_duration};
pub use error::StartupError;
#[doc(hidden)]
pub use layered::LayeredProvider;
pub use server::{Running, start};
pub use ticker::{ChannelTicker, IntervalTicker, Tick, TickHandle, Ticker, channel_ticker};

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

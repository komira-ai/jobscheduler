# js-server as N instances on one Postgres

Status: design. A living document: each pull request that builds a part of it turns that part's tag from **Designed** to **Built**
and names the code. **Built** names code on `main` with a path and line; **Designed** means nothing on `main` implements it yet, and
says which pull request of section 9 does. Line numbers are those of `main` as last merged into this document's branch; a later
change moves them, and the pull request that moves a decision's code updates its row.

The server is `js-server`, the binary of the `js-lease-server` crate. Its store is the `js-state` `Store` (`--store`, Postgres);
its job tables are `js-jobs`, its placement `js-sched-placer`, its host sessions `js-registry`, its usage `js-usage`. A SQLite
store is one process's (its file lock, `js-state/src/writer.rs`) and is not what this document is about.

The same rule governs the hostkeeper coordinator's design, `docs/design/coordinator-multi-instance.md` in its repository. Section 13
of that document lists the decisions already taken for js-server, and section 6 of this one carries them over.

## 0. The shape

js-server instances are stateless, the ordinary shape of a service in front of one database: what one instance knows about a host
or a job, every instance reads in Postgres, and any instance serves any request of any agent or client. Sections 1 to 12 audit the
code against this shape and list what is left to build; section 9 marks each gap kept, shrunk, deleted or new (G15 to G18) by it. Section 0.1 splits js-server into a stateless
JobScheduler, to which the shape below applies, and a JobManager that places and runs the periodic work under a lease.

- **Any instance serves any agent.** The agent opens a connection per request (`js-client/src/client.rs:282`) and the edge sends
  it to any instance. That instance authenticates the request by the host's certificate (G3) and reads what it needs of the host
  from rows. No instance owns a host's session, so a request that lands on another instance than the last one is served as it is:
  there is no hello to repeat, and no `409 unknown-host` or `403 session-mismatch` for a host the database knows. A beat of an
  epoch the row has passed is still `409 stale-epoch`. An agent says hello when it starts, as today.
- **A host is rows.**
  - `node/<host>` (G15): the epoch of its last hello (and the digest of its session secret until G3), its pool, claim, shape,
    capabilities and labels, its phase and last load reading, whether the first listing since the hello is applied, whether its
    last listing was clean, the epoch and sequence of its last applied beat, and `heard_at`, the database's `now()` at that beat.
    These are what the placement engine's hosts and a beat's reply read today from the registry.
  - `hostbook/<host>`: its booking (Built). `cordon/<host>`: its cordon (Built, #932). `hosts/binding/<host>`: its taint (G4).
  - `taint/<host>`, `mark/<job>`, `hostmark/<host>`: a host job's taint, a whole-host job's drain, a scale-down mark.
  - The registry becomes a cache of these rows, refreshed from them, that never decides.
- **The heartbeat is a row `UPDATE`.** A beat updates `node/<host>` at the version read, guarded by its (epoch, sequence): the
  sequence is raised by the agent for every attempt and reset by a hello, and an older beat changes nothing and is answered from
  the current rows. It sets `heard_at = now()`, and every write of its job listing that follows is conditional on the version it
  wrote (#883's `Checked` store; today each listing step, each end of an unlisted job and each whole-host move is a transaction of
  its own: `per_job.rs`, `job_loss.rs`, `job_whole.rs`).
  - The sequence is an optional field the agent adds (G15). An agent that sends none is serialized by the row but not ordered by
    age: a beat it gave up at its 10 s limit can still commit after a newer one, and its older listing would free a container the
    host still holds (`per_job.rs`: releasing, ended, not listed). For such an agent a confirmed container is freed only when two
    applied beats in a row omit it (a count in `node/`).
  - The reply (starts, stops, acknowledgements) is a function of the rows, so every instance gives the same one. A reply lost on
    the way is given again by the next beat, on whichever instance takes it: a `starting` whole-host job's start rides on every
    beat until the host lists it (`per_job.rs` `starts_for`).
- **The queue is claimed and the host booked conditionally.** A placement claims its job with `SELECT ... FOR UPDATE SKIP LOCKED`
  and books the host by the conditional write of `hostbook/<host>` (Built, #879). The same transaction checks the host's rows: the
  cordon (Built), the taint, the ports and the marks (G7). The manager places over every live host (a `node/` row heard within
  grace), not over the hosts whose requests it happens to serve. Since section 0.1 the placer is the JobManager holding the lease,
  and its placement transaction also checks the lease.
- **LISTEN/NOTIFY wakes; rows decide** (G16; whom it wakes is in section 0.2). After a commit that gives a host work (a placement, a cancel), the committing instance sends a
  best-effort `NOTIFY`: on the manager channel for a submit, a free in a listing, a cancel and a lease release (it wakes the lease holder only; a standby acts on a release), and on the host channel with the host id for a placement (it wakes the scheduler holding that host's beat) (section 0.2, G16). A held beat wakes at once instead of at its poll. A lost notification costs one poll (the held beat's
  5 s, the tick). When the `LISTEN` connection is made again, every held beat is woken once, since it may have missed notices.
- **Deadlines are the database's `now()`** (G2). Every time that one instance writes and another compares (`heard_at`, queue and
  start deadlines, retention, a cordon's end, a duty's due time) is the database's clock.
- **Periodic work is the manager's** (section 0.1). Settling silent hosts, expiry, retention and the feed trim run on the manager that
  holds the lease, and their transactions check it (`trim_feed` and `reap_expired` take no transaction: they remove only what is past its time and are safe twice). The due-time claim of #881 (Built) is deleted with G17: the lease already serializes the duties, and G9 and G11 take no claim either. Settlement judges a host's silence by `heard_at` (G6).

What the shape removes:

- a host's session as one instance's possession, and the hello and `409`/`403` exchange when a request lands on another instance;
- the per-instance "first listing" and "listing clean" flags, which become `node/` columns: the hello sets "first", and the beat
  that applies the first listing clears it in its own write (the lose-unlisted and prepare-restart rules still need it);
- the save of bindings from memory, and the local `gone` map;
- the connection-ownership rule of komira-ai/jobscheduler#883. Its mechanism, every listing write conditional on the host's row,
  stays as the beat's guard.

**Its cost.** A heartbeat becomes one short write per host per beat interval (5 s by default, `js-agent-wire/src/lib.rs:225`),
where today a beat that changes nothing writes nothing. That is about the rate the project owner accepted for the coordinator
(komira-ai/hostkeeper's design, section 19, decision 4). Through the `Store`, each beat is also a revision and a feed row, taken
under the commit head behind every claimed placement (3.1). It moves the head that every pass's table cache is judged by
(`js-jobs/src/store/written.rs`, #880), so a pass is skipped as unchanged only between beats. The other standard form is a plain
`node` table on the store's own connections (`js-state/src/postgres/extension.rs`, no head, no feed), with
`UPDATE ... WHERE epoch = $1 AND seq < $2 RETURNING` as the guard. It gives up the same-transaction `Checked` guard of the listing.
G15 chooses between the two by measuring both at the deployment's host count.

This amends [jobscheduler-design.md](jobscheduler-design.md) section 10.1: "live sessions, session secrets, heartbeat times" are
rows now (the secret as a digest until G3). The whole-host `Committed`, `Ready` and `Restoring` states stay out of the database: a
ready or refused report rides on every beat until the job is `starting` (`job_whole.rs`), and `Restoring` is the stored phase.

### 0.1 Two roles: the JobScheduler and the JobManager

Decided by the project owner (2026-11-19): js-server has two roles.

- **The JobScheduler** is the job API and the agent API: submit, get, cancel and list, and the agent's hello and heartbeat. It is
  stateless over Postgres, and any number of copies run at once. Every rule of section 0 above is the scheduler's. It places no
  job. A submit commits the queued job and wakes the manager; a beat commits its listing (frees included) and wakes it too.
- **The JobManager** is the placer and the periodic work. It takes queued jobs (the claim with `SKIP LOCKED`), books the host
  (`hostbook/<host>`), and writes the start that the next beat's reply carries. It runs the duties: expiry, retention, settling
  silent hosts, the feed trim, the usage prune, the scaler and its catalog refresh, and `keep_taints` (the taint is G7's row written in the placement transaction; the binding save goes with G4).

Where the rest of today's tick goes (`js-lease-server/src/api.rs:187`, `agents.rs:865`): the JobScheduler keeps the token reload and the limiter, `mirror_cordons`, `save_bindings`, `joins.flush`, the usage tick (a beat's minutes) and the log-grants tick (grants ride the hello and the beat reply). `registry.advance` (marks and prepare deadlines of the hosts a copy serves) is the scheduler's; G15 and G7 make what it decides rows. A manager serves no job or agent route; it keeps the readiness route (`--ready-listen`): `starting`, then `ready` once its row is written (standby or active) and its loop runs, then `draining`. A start-first update waits for it, and a manager whose loop stops frees the lease and exits.
- **One binary, a role flag.** `--role scheduler|manager|both` (config key `server.role`, default `both`; like every setting, a flag or a key of the config file, never both). `both` is today's process and a manager like any other: it takes the lease (the lease row and its `Txn::check` work on SQLite too), so two `both` processes in an update's overlap place one at a time. A single-node install is unchanged but for the two rows. Where the roles split, a submit answers the job `queued` and no longer places in the call (`js-lease-server/src/job_api/submit.rs:283`); `both` places in the call only while it holds the lease. A deployment runs `scheduler` copies behind the edge, and
  `manager` copies with no public listener of their own. One binary keeps one release, one image, one config parser and one
  schema lock-step. The manager needs the scheduler's job tables and host rows anyway, and two binaries would split that code for
  no gain.

**Which managers may place: the manager rows and the lease.**

- `manager/<id>` is one row per manager process: its id (random at start), its release, its state (`standby`, `active` or
  `draining`), the lease generation it last held, and `seen_at` from the database's `now()`. `standby` is a manager that does not hold the lease. The status route and
  `js manager list` show these rows. A manager that has exited leaves its row `draining`, and its `seen_at` is written at each renewal (the holder) and each lease poll (a
  standby) with a time to live of a day (`put_ttl`), so the row of an exited or crashed manager goes by itself and no prune runs.
- `manager/lease` is one row: the holder's id, a generation, and `expires_at` from the database's `now()` plus
  `manager_lease_secs` (a config key, default 10). The holder is the one active manager.
  - **Take:** a standby takes the lease when it is free or has expired. In one transaction it writes the lease at the version read (generation raised by one) and its own row `active` at the version read, so a drain that landed first conflicts it and a `draining` manager never takes. The fence below reads the lease alone; a manager's row follows it and is for the operator, so one row decides who places.
  - **Renew:** the holder renews every third of the lease, at the version read, keeping the generation.
- **Fencing.** Every claim and placement transaction of a manager reads `manager/lease` and checks it in the same transaction
  (`Txn::check`, `js-state/src/types.rs:153`, at the version read). It places only if the row names this manager, at the
  generation it holds, with `expires_at` after `now()`. A manager whose lease was freed or taken over commits not one placement after that commit. On Postgres a decision holds the commit head from before its read of `manager/lease` to its commit (3.1, `hold_head`), so a drain, a takeover or a renewal cannot commit between the read and the commit: it commits first and the decision reads it, or the placement commits first, under the lease it still held. Where the store does not hold the head (SQLite, the default `claim_decide`) the `Cond::Version` check is what fails: the transaction conflicts and writes nothing, and a renewal that lands in between (it bumps the version) makes it run again once. The version check is the fence; `expires_at` is liveness only (a manager starts no placement on a lease it reads expired), since a stale `now` cannot be trusted at commit. The duties check it the same way: the manager's store is the job store behind a wrapper that adds `check(manager/lease, Cond::Version(v))` to every `txn`, `apply_batch` and `claim_decide` it passes (and reads the row in a decision), `v` being the version of the manager's last take or renewal. Nothing in `place_claimed`, the job pass or the scaler's rows changes its signature. `trim_feed` and `reap_expired` take no transaction, so they cannot carry a check: they delete only what is past its time and are safe twice, and are not fenced. The scaler's provider calls are outside the database (G9).
- **Drain.** SIGTERM, or the operator's `js manager drain <id>` (a job-API route of the operator role, `POST /v1/managers/{id}/drain`),
  writes the manager's row `draining`. If it holds the lease, the same transaction frees the lease (no holder, the generation raised
  by one). The fence then holds at once, even before the manager has read its row. The manager stops taking work, lets its
  in-flight transactions end (a placement already past its lease read commits first, as above; any later one fails the check), and exits 0. A process its supervisor starts again is a new manager with a new id, a standby, so a drain never hands the lease back to the drained process. A drain sends a `NOTIFY` so that a standby takes the
  lease at once instead of at its poll.
- **On an update the deploy drains the old manager, and a new start never does.** The new release's manager starts as a standby,
  and the start-first update, once the new one's readiness answers `ready`, sends the old one SIGTERM. That SIGTERM frees the lease, and the new manager takes it within one
  notification. A start that took the lease from a live holder would let a broken release take placement from a working one, and a
  manager restarted by hand would steal it. So the hand-over is the old manager's drain, and the new one only waits for a free or
  expired lease.
- **This amends the rule of section 1 for the placer only.** The owner's earlier decision of no leader and no lease stands for the
  JobScheduler. The manager's lease is a row like every other decision: taken, renewed and checked by conditional writes, judged
  on the database's clock, and released by a transaction. No instance's memory decides who places.

### 0.2 One active manager, not N

Recommended: **one active manager, holding the lease, with one standby or more.** The choices compare as follows.

| | One active (the lease) | N active (`SKIP LOCKED`, no lease) |
|---|---|---|
| Placement throughput | serial, one transaction at a time | also serial: every claimed placement holds the commit head from its start (3.1), so N managers queue on it and add contention, not throughput |
| Queue order and fairness | exact: one engine walks the queue | two managers walk the queue at once, so a lower-priority job can place while the other holds the one ahead of it (a held job is skipped), and each walk sees the other's bookings a transaction late |
| Whole-host drains, the scaler, the duties | one decider: no two hosts drained for one job (G8 goes), one scaler loop (G9 shrinks), duties need no claim | each needs its own N-safe rule (G8, all of G9, the duty claims) |
| A crash | placement and the duties pause until the lease expires (`manager_lease_secs`, 10 s by default, which is 100 times the start target for a job submitted in that window) | none |
| An update, a drain | none: the drain frees the lease and the standby takes it within one notification | none |
| Code | a lease row, its renewal, the fence check | already there (#879), plus the N-safe parts of G8 and G9 |

**Placement latency** (inferred from the round trips, not measured; G17 measures it):

1. A submit commits the job and sends `NOTIFY`.
2. The manager wakes on its `LISTEN` connection.
3. It runs one placement transaction: the lease check, the claim, the booking and the start, in a few round trips on the socket.
4. It sends `NOTIFY` for the host, and the scheduler holding the host's beat answers it with the start.

Inferred: the transaction is the lease read, the job and `hostbook` reads, the host's allocations and the commit, a few round trips, and each wake-up adds one. The pass reads every live `node/` row before it places (G7), which grows with the fleet and is the larger risk to 100 ms than the lease check. A burst of submits places one after another, each waiting for those ahead (the same with N active). It should meet the 100 ms start target for a host that holds a
beat; any other host gets the start at its next beat, within its 5 s interval. N active managers have the same steady latency. One
active costs up to the lease time only after a crash; a drain or an update costs nothing.

**LISTEN/NOTIFY wakes the active manager** (G16): a submit, a free in a beat's listing, a cancel and a lease release notify the
manager channel, and only the lease holder acts on them. A standby listens only for a lease release. A placement notifies the
host's channel, and that wakes the scheduler holding the host's beat. Rows still decide: a lost notification costs one poll.

## 1. The rule

Direction from the project owner (2026-11-15, binding): **Postgres is the single source of truth, and any number of js-server
instances over one store just work.** The JobScheduler has no leader, no lease and no election; the placer and the periodic work have one lease row (section 0.1, the owner's amendment of 2026-11-19). Every decision (placement, admission,
a host's session, a taint or cordon, a periodic duty, a scaler step) is made inside a Postgres transaction whose correctness does not
depend on which instance runs it or how many run it at once, except that placement, the periodic duties and the scaler run in the one JobManager that holds the lease. The owner's decision of 2026-11-19 (section 0.1) amends this for the
placer: one JobManager at a time holds a lease row, and every placement checks that row in its own transaction. The rule stands for
the JobScheduler, and the lease is itself a row that is taken, renewed and checked by conditional writes on the database's clock.

1. **Postgres decides; memory only caches.** An instance's memory holds caches that can be rebuilt from the rows. A cache may
   choose what to try (which job to place first, which host to offer), but the decision is taken against the rows, in the
   transaction that writes it. A cache that decides is a defect.
2. **Three tools, and nothing else.** Every coordination is one of:
   - a **row lock** taken inside the deciding transaction (a `Txn` locks every key it names, in key order, and holds the lock to
     its commit: `js-state/src/postgres/tx.rs:1-25`; `Store::claim_decide` takes the commit head first and then locks the claimed
     row `FOR UPDATE SKIP LOCKED`, deciding under both: `js-state/src/postgres/claim.rs:98`, `session.rs:155-157`);
   - a **conditional write**: `Cond::Version` (a compare-and-set on the version read) or `Cond::Absent` (a unique key); the
     `Conflict` says who lost;
   - a **claim by advancing a due time** for periodic work (`JobStore::claim_duty`, `js-jobs/src/store/duties.rs:46`): the
     instance whose advance of the duty's row commits runs it for the period.
   Except the manager's lease (section 0.1), no step elects, leases, counts instances or relies on being the only process. An instance that dies at any point leaves
   no lock or lease that another must clear by a timeout, except a crashed manager's lease, which expires: its transaction committed or it did not. (A host that falls silent
   is settled after a timeout, as today, but that is the host's silence judged on the store's clock, not an instance's hold.)
3. **Time is the database's.** A time that one instance writes and another compares (a deadline, an expiry, a due time, a
   last-heard time) is read from the store's clock (`Store::now`, the database's `now()`), never from the process clock. The
   process clock times only what stays in the process (a tick interval, a backoff, a rate limit).
4. **Fail closed.** An instance that cannot use the database decides nothing: no placement, no admission, no settlement, no
   duty, no scaler step. A request that needs a decision is answered with a server error the agent retries with its backoff
   (`503` or `500` `state-write-failed`; `js-agent/src/session.rs:18-42` makes `401`, every `4xx` but `408`, `409` and `429`, and every `403` but `session-mismatch` and `pool-full` fatal, and retries `5xx`), never `401`,
   and hosts keep running what they run.
5. **Formats change additively (B8).** New key prefixes and new optional fields only.
6. **No runtime machinery for a config mistake (B20).** Every number is a config key with a default; a deploy's mistake is
   checked once at deploy.

The deployment runs one instance in steady state and already updates it **start-first** (its stack, outside this repository):
the new instance serves beside the old one while the old one drains, on every update. So the gaps of section 9 that an overlap
reaches are live hazards now, not after section 9; section 12 lists them, says how an update reaches and drains the instances, and
gives the order in which they are closed. Correctness is argued and tested for any N. The host-native update hook of
`docs/lease-server-update.md` is a different install, of one server, stop-first.

## 2. What is already Built

The merged pull requests komira-ai/jobscheduler#874, #877, #878, #879, #880, #881, #923, #932 and #935 put the store, the job tables,
placement, the periodic duties and the cordon under the rule:

- **No writer lock and no fence** (#878). A Postgres store takes no lock at start and checks nothing in a commit;
  `writer_generation` is unread (`docs/design/configuration.md`, "No lock on Postgres"; `docs/runbooks/state-database.md`).
- **The job tables and usage rows live in the store** (#874), so every instance reads the same jobs.
- **A placement claims its job and books its host in one transaction** (#877, #879).
- **A pass re-reads the tables after any instance's commit** (#880), judged by the store's head revision.
- **The job pass and the feed trim are duties claimed per period** (#881).
- **A start that loses a race at the open retries** (#923).
- **A second server keeps the first's bindings, taints and tokens, and the drain moves agents at once** (#935): the bindings save
  deletes only what this server's registry held (`js-lease-server/src/agent_state.rs:358`, `:369`), a hello of a host the registry does not
  know reads its binding row (`js-lease-server/src/agents.rs:567`), a token not held is read by key
  (`js-lease-server/src/join_tokens_stored.rs:19`), and on SIGTERM the agent listener closes and nothing more is placed
  (`js-lease-server/src/drain.rs:28`).
- **A cordon is a row every placement checks** (#932): `cordon/<host>`, decided at the version read and checked absent by every
  placement transaction, whichever server took the cordon (`js-jobs/src/store/cordons.rs:107`, `js-jobs/src/store/whole.rs:190-196`,
  `:290-291`).

The tables of section 3 list each decision with its code.

## 3. The audit: every decision of js-server against the rule

Each row is a decision two instances could make together. "How it holds" names the tool of rule 2 or the reason it fails;
"Tag" is Built, Designed (with the gap of section 9 that builds it) or Per instance (state that is the process's own by design
and decides nothing shared). Paths are under `crates/`.

### 3.1 The store and the start

| Decision | Where | How it holds | Tag |
|---|---|---|---|
| A transaction's preconditions hold at its commit | `js-state/src/postgres/tx.rs:1-25`, `:269`, `:420` | every key it names is locked in key order (a missing key reserved by a placeholder row), and `Cond` is judged on the locked rows | Built |
| Revisions are minted in commit order, with no gap in the change feed | `js-state/src/postgres/tx.rs:191` (a tail), `:156` (`hold_head`) | `LOCK TABLE head` in a plain transaction's tail, so bodies run at once and tails queue; a claimed decision holds it from its start | Built |
| A claimed row is decided by one transaction | `js-state/src/postgres/claim.rs:98`, `js-state/src/postgres/session.rs:155-157` | the commit head taken first (`hold_head`), then the claimed row `FOR UPDATE SKIP LOCKED` (a held row is skipped, never waited for); the decision's reads are what its commit finds. This serializes the decisions of every instance on the head; their bodies are short (one job, one host's rows) | Built |
| A schema migration is applied once by racing installers | `js-state/src/postgres/schema.rs:80` | `pg_advisory_xact_lock` inside the applying transaction, version re-read under it; the server itself only judges the schema | Built |
| The job store's schema row is written once | `js-jobs/src/store.rs:227-235` | `Cond::Absent`; a conflict re-reads the row | Built |
| The index backfill and the boot raise of deadlines at open | `js-lease-server/src/job_store.rs:195` (`booted_until_won`), `js-jobs/src/store/ending.rs:275`, `:325` | each write at `Cond::Version`; a beaten open retries (#923); a raise is a max, so a second start only delays deadlines | Built |
| The first start imports the files beside the state file once | `js-lease-server/src/store/import.rs:99-100` | marker and rows `Cond::Absent` in one transaction, so one import wins; the loser's `Conflict` refuses its start (exit 2) instead of reading the marker | Built (correctness); Designed (G11: the loser starts) |
| Row expiry (TTL) | `js-state/src/postgres/tx.rs:146`, `:289` | judged on the database clock inside each transaction | Built |
| Feed trim | `js-lease-server/src/job_pass.rs:242`, `js-lease-server/src/job_store/feed_trim.rs:44` | a duty claimed per period; a trim by absolute revision is idempotent | Built |
| An unreachable database | `js-lease-server/src/job_pass.rs:190`, `:225`, `js-lease-server/src/store_health.rs` | every store call fails and nothing is decided; a hello's binding read or write or a taint's write answers `503`, a listing's commit `500` `state-write-failed` (`js-lease-server/src/agent_api.rs:324`, `:352`, `:659-662`), both retried by the agent | Built |
| `Store::now` when the database cannot be read | `js-state/src/postgres/mod.rs:920-931` | fails with the read's error (`Store::now` returns `Result<StoreTime, StoreError>`, `js-state/src/store.rs:121`); the `js-jobs` writes that stamp rows from it (`js-jobs/src/store.rs:342`, `:486`, `ending.rs:62`, `:139`) fail with it and write nothing | Built (#938) |
| Readiness, drain and shutdown | `js-lease-server/src/ready.rs:26`, `js-lease-server/src/server.rs:309` | one instance's own phases (#876); nothing waits on another. At SIGTERM the agent listener closes, held beats are answered at once and no job is placed (#935, `js-lease-server/src/drain.rs:28`, `server.rs:840`), so agents move at once; the route is served on a loopback address only, so the edge cannot see a drain (section 12) | Per instance; Built (agents leave at once); Designed (G13: the edge sees it) |
| The host-native update hook's "is a server running" probe | `js-state/src/writer.rs:128`, `js-state/src/postgres/serving.rs:21`, `js-lease-server/src/update_hook/step/mod.rs` | reads `pg_stat_activity` by the server's application name; it serves the one-server install of `docs/lease-server-update.md`, not an N-instance deploy (which is the orchestrator's start-first update) | Per instance |

### 3.2 Hosts: identity, admission, taint, cordon

| Decision | Where | How it holds | Tag |
|---|---|---|---|
| A host proves who it is | `js-lease-server/src/host_cert.rs:86` (`host_of`), `js-lease-server/src/client_verifier.rs:29`, `js-lease-server/src/agent_api/host_cert.rs:34` (`admit`), `js-lease-server/src/agent_api.rs:179` (`host_cert::admit`), `:271` (`host_cert::check` in the hello), `:407` (in the heartbeat) | the host certificate hostkeeper issues to an enrolled host, verified against `--agent-client-ca`; the host id is its one DNS SAN (#922). A function of the certificate and the CA, so every instance answers alike | Built |
| A host without a certificate joins by a join token or a pool token and is given a host credential | `js-lease-server/src/join_tokens.rs:353` (read at start), `:421` (`identify`, memory), `js-lease-server/src/join_tokens_stored.rs:19` (`identify_stored`, a row by key), `js-lease-server/src/agent_api/unknown.rs:34` (`identify_single`), `join_tokens.rs:516` (`admit_writing`), `:433-470` (mint; its cap `:450`), `:652` (revoke), `:691` (renewal), `:627` (give-back), `js-lease-server/src/host_keys.rs:45-55`, `js-lease-server/src/agent_tokens.rs:50` | the token map is read at start and decides from memory, and a token not held is read by its row at its first use (#935), so a token minted or a credential issued on A is known to B; every use, mint, revoke, renewal and give-back is still committed at `Cond::Any` (the comment there still says "the server is the store's one writer"): a revoke or use on A is not seen by B once B holds the token, and two instances can admit two hosts with one token | Shrunk by #935 (no `401` for a token another instance minted); Designed (G3: removed) |
| A host id is bound to the credential and provider instance of its first hello (`HostBound`) | `js-registry/src/registry.rs:460-467` | judged on the local registry and the bindings read at start; the binding written at `Cond::Any`. After G3 the certificate names the host and there is nothing to bind | Designed (G4 until G3, which deletes it) |
| A refused hello undoes its binding | `js-lease-server/src/agent_state.rs:601-617` (`change`), `js-lease-server/src/agent_api.rs:375-389` (`give_back`) | the undo puts back what this instance's copy holds, which #935 refreshes from the row at the hello of a host the registry does not know (`BindingStore::stored`, `agent_state.rs:501`); it is written at `Cond::Any` | Designed (G4) |
| Hellos of one host are serialized | `js-lease-server/src/agent_api.rs:282` (`host_writes`; `restore_stored` runs inside it, `:288`) | a process mutex; correct across instances only once G3 removes the token writes and G4 makes the binding writes conditional; then it is an economy | Per instance (after G3, G4) |
| The bindings are saved | `js-lease-server/src/agents.rs:913` (`save_bindings`), `js-lease-server/src/agent_state.rs:358-379` (`writes_to`, `gone`) | puts every binding of this registry that differs from the copy, at `Cond::Any`, and deletes only a binding this registry held and let go (#935: `Held::held`); a binding it never held, which another instance serves, is left alone. The put still overwrites a taint the other instance stored in a binding it also holds | Shrunk by #935; Designed (G4: the save goes) |
| A taint a heartbeat reports is stored | `js-lease-server/src/agent_state.rs:647` (`commit_taint`, the `Cond::Version` put at `:668`) | `Cond::Version` of the row read, after checking the stored taint is the expected one. #940 adds the clear: a heartbeat that reports a taint cleared on a host whose registry holds none ends the stored taint when it is the cleared one (or a refused start's `host-tainted`), by the same conditional write (`agent_state.rs:686` `end_taint`) | Built |
| Every instance sees a host's taint | `js-lease-server/src/agents.rs:303` (`restore_job_taints` at start; defined `js-lease-server/src/agents_taints.rs:157`), `:567` (`restore_stored`, at a hello), `js-lease-server/src/sched/jobs_pass.rs:69` (the placer's own registry), `:163` (the hosts filter) | the registry's taint decides placement and is loaded at start and, since #935, at the hello of a host the registry does not know (its binding row is read first), so a host that comes to B from A keeps A's taint; a host B already holds does not see a taint A stores later | Shrunk by #935; Designed (G4, G7) |
| A cordon and its lift | `js-lease-server/src/agents.rs:639` (`keep`), `js-lease-server/src/agents_cordon.rs:146` (`cordon`), `:212` (`mirror_cordons`), `js-jobs/src/store/cordons.rs:107` (`cordon_host`), `js-jobs/src/store/whole.rs:191`, `:291` | with the job service on, the cordon is the row `cordon/<host>` (put with a TTL), decided in the store at the version read (`Cond::Absent` or `Cond::Version`; a lost race is `CordonConflict`, answered `503 cordon-unsaved`), and every placement transaction checks it absent, so a cordon placed on any instance keeps every instance's placement off the host and it lapses by the store's clock; each tick puts the rows on the registry (`mirror_cordons`), so a lift made on B reaches A's listing, scaler and probes at A's next tick. What stays per instance: the binding's cordon end is still rebuilt from the local registry and put at `Cond::Any` (`keep_cordon`), a host held by another instance answers `404` (`held_cordon` reads the local registry), and without the job service the registry decides in memory | Built for placement; Designed (G4) for the binding and the `404` |
| An operator clears a host's taint (`js-agent clear-taint` of #939 is the agent's own local clear, not this route) | `js-lease-server/src/agents_taints.rs:86` | the write is conditional (`commit_taint`), but the host must be in this registry: a host held elsewhere answers `404` (fails closed) | Designed (G4) |
| Host certificate admission (`--agent-client-ca`) | `js-lease-server/src/host_cert.rs:86`, `js-lease-server/src/client_verifier.rs:29` | a function of the certificate and the configured CA | Per instance |
| The hello rate limit | `js-lease-server/src/agents.rs:72` | a limiter of this process | Per instance |

### 3.3 The session and the heartbeat

| Decision | Where | How it holds | Tag |
|---|---|---|---|
| Which connection of a host commits its job listing | `js-lease-server/src/agent_api.rs:570-627` (`first_listing` :570, `lose_unlisted` :608, `job_whole::serve` :624, `per_job::serve` :627, over the plain job store) | nothing orders two connections: an old connection on A can commit a listing over B's newer one, freeing an allocation the host holds or losing a job it lists | Designed (G15, which takes #883's `Checked` store) |
| A host's first listing after a hello loses the jobs it does not list | `js-lease-server/src/agents.rs:474` (`first_listing`, memory), `js-lease-server/src/agent_api.rs:570`, `:608`, `js-lease-server/src/job_loss.rs:96-107` | each end is `Cond::Version`, but "first" is this instance's flag; safe once only the newest connection writes. A whole-host job moved to `starting` on a ready report (`js-lease-server/src/job_whole.rs:160`) whose reply, carrying the start, is lost is ended `Lost` by the next first listing without having run; this happens on a one-instance restart too | Designed (G15: no hello on a change of instance, and the start is sent again by the next beat) |
| A host's session, epoch, secret, last heard | `js-registry/src/registry.rs:474-525`, `js-registry/src/registry/beat.rs:131` | held by the instance that took the hello; a beat that reaches another instance is refused (`409 unknown-host` or `403 session-mismatch`) and the agent says hello there. Section 0 makes the session the row `node/<host>`, checked on any instance | Designed (G15) |
| `HostLive` (a second agent process on a live host) | `js-registry/src/registry.rs:468-473` | judged on the local registry: a different agent process that says hello to B while the host is live on A is admitted | Designed (G5) |
| The pool's machine cap (`PoolFull`) | `js-registry/src/registry.rs:506-515` | counts this registry's hosts: N instances admit N times the cap | Designed (G5) |
| Whole-host job states (placed, starting, running, prepare deadline) | `js-jobs/src/store/whole.rs:354`, `:359`, `:396`, `js-lease-server/src/job_whole.rs:143` | rows written at `Cond::Version` | Built |
| The whole-host start's taint and the clock it judges deadlines by | `js-lease-server/src/agent_api.rs:545` (`own.host.taint`), `js-lease-server/src/job_whole.rs:143` (`serve`) | the registry's taint; the clock is the store's, read only for a step that stamps a time (#938) | Built (the clock); Designed (G4, G7: the taint) |
| A job's start command, repeated by two instances | `js-lease-server/src/per_job.rs:166`, `js-lease-server/src/job_start.rs:120` | a pure function of the rows; the placement that creates it is claimed | Built |
| Listing-clean (a host whose last listing was not clean takes no job) | `js-lease-server/src/agents.rs:503` (`note_listing`, memory), `:515` (`listing_clean`), `js-lease-server/src/sched/jobs_pass.rs:163` | another instance's placement does not know | Designed (G15) |
| Held beats and their wakes | `js-lease-server/src/held_beat.rs:36` | the process's own wakes; a placement by another instance goes out on the host's next beat, at most its hold later; a draining instance ends every hold at once and begins none (#935, `js-lease-server/src/agent_api/held.rs:95-113`) | Per instance; Designed (G16: woken across instances) |
| Log upload credentials per host | `js-lease-server/src/logs/mod.rs:155-307` | stateless provider credentials; several valid at once; a flip costs a mint | Per instance |

### 3.4 Placement and the queue

| Decision | Where | How it holds | Tag |
|---|---|---|---|
| A queued job is placed by exactly one instance | `js-jobs/src/store/whole.rs:128-132`, `:286` | `claim_decide` on `job/<id>` (`SKIP LOCKED`), the job written at `Cond::Version` | Built |
| A host is never booked past its room | `js-jobs/src/store/whole.rs:176-209` | the host's live allocations summed in the claimed transaction, `HostFull` when they leave no room, `hostbook/<host>` written at `Cond::Version` | Built |
| One host job (`isolation=host`) per host | `js-jobs/src/store/whole.rs:289`, `js-jobs/src/store/taints.rs:36` | `taint/<host>` put at `Cond::Absent` in the placing transaction | Built |
| No job on a cordoned host | `js-jobs/src/store/whole.rs:190-196` (`place_claimed`), `:290-291` (every placement), `js-jobs/src/store/cordons.rs:168` (`cordons`), `js-sched-placer/src/jobs.rs:708-713` | the row `cordon/<host>`, decided at the version read (`cordons.rs`); every placement transaction checks it absent (`HostCordoned` in `place_claimed`, a conflicting commit in `place` and `place_with`), whichever instance took the cordon; each pass also drops the hosts whose cordon the store holds from the open hosts | Built |
| No contained job beside a host job, no job on a tainted host | `js-sched-placer/src/jobs.rs:931-941` (`open_to_jobs`, registry view), `js-lease-server/src/sched/jobs_pass.rs:163` | the engine's memory only; the placing transaction reads neither the host's taint nor `taint/<host>` (it puts the row for a host job only), and `taint/<host>` is later deleted at `Cond::Any` (`js-jobs/src/store/taints.rs:72`) | Designed (G7) |
| A host port is held by one job per host | `js-sched-placer/src/jobs.rs:472` | the engine's memory only; the placing transaction does not compare ports | Designed (G7) |
| A host the scaler drains or releases takes no new job | `js-registry/src/registry/marks.rs:1-25` ("the marks are held in memory only") | a mark in one instance's registry; another instance's placement does not see it | Designed (G7, G9) |
| One mark row per whole-host job | `js-jobs/src/store/marks.rs:81-94` | `mark/<job>` at `Cond::Absent` or `Cond::Version`, with the job checked at its version | Built |
| Which host a whole-host job drains | `js-sched-placer/src/whole_jobs.rs:204-210`, `js-sched-placer/src/jobs.rs:741` | each instance chooses over its own hosts and deletes a mark whose host it never heard after its own start plus grace: two instances drain two hosts in turn (the rows stay consistent; the work is wasted) | Designed (G17: one manager decides; G8 deleted) |
| Why a job waits (queue status note) | `js-sched-placer/src/jobs.rs:779`, `js-jobs/src/store/status.rs:38` | conditional, but its text depends on the instance's view, so two instances rewrite it in turn and each rewrite moves the head | Designed (G17: one manager writes it) |
| The tables a pass decides from | `js-sched-placer/src/jobs_seen.rs:192` | a cache judged by the store's head (#880); every placement is re-decided in its transaction | Built |
| Which hosts an instance places on | `js-lease-server/src/sched/jobs_pass.rs:154` (`registry.hosts()`; nothing at all once draining, `:147`) | the hosts whose sessions it holds; section 0 has every instance place over every live `node/` row | Designed (G7) |
| Which manager places | the lease row `manager/lease` and `manager/<id>` (section 0.1) | one lease holder, checked in every claim and placement transaction at the version read; a drain frees it in the same transaction | Designed (G17) |
| Queue order | `js-jobs/src/store/queue.rs:20` | a total order of row columns, the same on every instance | Built |
| Job sizing | `js-sched-placer/src/sizes.rs:564` | from the shared tables; the booking is checked in the transaction | Per instance |

### 3.5 The job API

| Decision | Where | How it holds | Tag |
|---|---|---|---|
| A client-chosen job id is recorded once | `js-jobs/src/store.rs:376-387` | `Cond::Absent`; a conflict answers from the stored job | Built |
| A minted job id is unique | `js-jobs/src/store.rs:402-411` | random ids, `Cond::Absent`, retried on a collision | Built |
| A cancel racing a placement on another instance | `js-jobs/src/store.rs:554-573` | `Cond::Version`; the loser writes nothing | Built |
| Job reads, results, logs | `js-lease-server/src/job_api/read.rs:220`, `js-lease-server/src/job_api/logs.rs:239-249` | read from the rows; a follow polls the row, no local wake decides | Built |
| A submit is refused `no-capable-host` | `js-lease-server/src/admit.rs:94-108`, `js-lease-server/src/job_api/submit.rs:264` | judged over this instance's registry: a host held by another instance is invisible, so a job it could run is refused for good. Since #940 a tainted host counts as capable (`js-sched/src/admission_ever.rs`, `HostView.tainted` is gone), so a taint alone refuses nothing; placement still skips it | Designed (G5) |
| `ListHosts`, `GET /v1/hosts`, a job's `host_silent_since_ms` | `js-lease-server/src/hosts.rs:283`, `js-lease-server/src/job_api/read.rs:165` | this instance's registry: the answer depends on which instance is asked | Designed (G5) |
| Cordon, uncordon, clear taint through the API | see 3.2; `js-lease-server/src/agents_cordon.rs:60-63`, `:80`, `:112` | the cordon and its lift are decided on the store row `cordon/<host>` at the version read (`jobs.cordon_host`, `jobs.uncordon_host`), so a cordon holds on every instance; but the host must be in this instance's registry (`held_cordon` answers `NotHeld`, `404`, for a host held elsewhere), and clear taint is unchanged | Designed (G4) |
| Brake reset (`POST /v1/pools/{pool}/brake/reset`) | `js-lease-server/src/brake_api.rs:48`, `js-lease-server/src/scaler/mod.rs:199` | recorded in this instance's memory | Designed (G9) |
| Client tokens, rate and concurrency limits, the catalog cache, validation | `js-lease-server/src/job_api/auth.rs:34`, `js-lease-server/src/rate.rs`, `js-lease-server/src/catalogs.rs:57` | configuration, limiters and caches that only delay | Per instance |

### 3.6 Periodic work and the scaler

| Decision | Where | How it holds | Tag |
|---|---|---|---|
| The job pass (queue expiry, prepare expiry, retention, loss) runs once per period | `js-lease-server/src/job_pass.rs:186-187`, `js-jobs/src/store/duties.rs:42-58` | a due-time claim; each end in the pass is `Cond::Version` (`js-jobs/src/store/ending.rs:89-97`) | Built |
| The time the pass and the duty claims compare | `js-lease-server/src/job_pass.rs:197` (`jobs.now()`), `js-jobs/src/store/duties.rs:46-52` | the store's clock, read once per pass and inside `claim_duty`; `job_pass.rs:176` (`began`) is the pass's own duration | Built (#938) |
| Times stored in host rows (join-token and credential expiry, which G3 removes; the binding's cordon end) | `js-lease-server/src/join_tokens.rs:440-441`, `:529-544`, `:557`, `js-lease-server/src/agents_cordon.rs:132` | the token times are stamped and judged with the process clock (G3); the binding's cordon end is now stored on the store's clock (#938, `agent_state.rs` `Skew`); with the job service the cordon itself is now a store row `cordon/<host>` whose time to live is the store's (`js-jobs/src/store/cordons.rs:123-128`), so only the binding's copy of the end (`agents_cordon.rs:127`) and the registry's cordon use the process clock | Designed (G2; G3 for the token times) |
| A silent host's jobs are lost (settlement) | `js-lease-server/src/job_pass.rs:130` (`gone`), `:186` (while `gone` is not empty the claim is bypassed and the pass runs regardless), `js-lease-server/src/job_alloc.rs:140` | "absent" is this instance's registry forgetting the host; a host connected to another instance is never heard here, so this instance can lose its live work | Designed (G6) |
| `taint/<host>` rows are dropped once kept | `js-jobs/src/store/taints.rs:72` | `Cond::Any`: a taint row rewritten between the scan and the delete is lost | Designed (G4) |
| The scaler runs a step | `js-lease-server/src/server.rs:658` (`ScalerLoop::spawn`), `:689` (`scaler.tick()`), `js-lease-server/src/scaler/mod.rs:189` | every instance with an elastic pool runs its own loop, with no claim | Designed (G17: the scaler runs in the active manager only; G9) |
| The scaler's records and unremoved hosts | `js-lease-server/src/scaler/kept.rs:51` | read at start, written whole at `Cond::Any`: two scalers overwrite each other | Designed (G9) |
| The scaler's caps and budgets | `js-scaler/src/budget.rs:88`, `js-scaler/src/caps.rs:97` | judged over this instance's records: N instances spend up to N times a cap | Designed (G17: one manager judges them; G9 for the fence) |
| The scaler's view of hosts and its drain and release | `js-lease-server/src/scaler/live.rs:98`, `:222`, `:271-302` | this registry's hosts; an acquire id drawn per process | Designed (G9) |
| Alerts (webhook, log lines) | `js-lease-server/src/alerts.rs:38` | each instance's own; a duplicate notice, not a decision | Per instance |
| Metrics, TLS and token-file reload | `js-lease-server/src/metrics.rs`, `js-lease-server/src/agents.rs:865-904` (`refresh`: token and client-token reload, `mirror_cordons`, `save_bindings`; the TLS reload is in `js-lease-server/src/tls.rs`, not in `agents.rs`) | each instance's own | Per instance |

### 3.7 Usage

| Decision | Where | How it holds | Tag |
|---|---|---|---|
| A host-hour pack of minute rows | `js-usage/src/store.rs:101-106` | `Cond::Version` or `Cond::Absent`; a conflict keeps the minutes buffered | Built |
| The same host-minute flushed by two instances (a host that moved within the minute) | `js-usage/src/store.rs:108` | the later flush replaces the minute instead of adding to it | Designed (G11) |
| An ended allocation's history record | `js-usage/src/history.rs:348` | `Cond::Absent`, in the commit that frees the allocation | Built |
| The usage prune | `js-usage/src/hub.rs:448-457` (`due`) | every instance prunes each day; each delete is `Cond::Version`, so duplicate work only | Designed (G11) |
| Profile curves' eviction | `js-usage/src/profile_store.rs:220` | victims deleted at `Cond::Any`; dormant (the server feeds it no results) | Designed (G11) |
| The minute buffer, counters, the usage mutex | `js-usage/src/hub.rs:488-490`, `js-lease-server/src/usage_js.rs:124`, `:165` | the process's own; losing one unflushed buffer when an instance dies is accepted (section 6) | Per instance |

## 4. Time

Rule 3 is Built for the job store (#938), and not yet for the marks, presence, join tokens or usage minutes. The job rows are stamped with the store's clock (`js-jobs/src/store.rs:342`, `ending.rs:139`), but the pass,
the heartbeat and the host rows compare with the process clock (section 3.6), and `Store::now` answers the epoch when the database
cannot be read (section 3.1). G2 makes one clock:

- `Store::now` is fallible (Built, #938: no `try_now` was added; Postgres returns the read's error, SQLite and the fake answer their clocks). Every caller that stamps or compares a shared time takes it from `try_now` and fails the call, writing nothing,
  when it fails. That is rule 4 for time.
- The job pass (`job_pass.rs:197`, `jobs.now()`), prepare expiry, the duty claims (`claim_duty(duty, period)` reads the clock itself, `js-jobs/src/store/duties.rs:46-52`), the whole-host start (`job_whole::serve`, only for a step that stamps a time) and the
  binding's cordon end (`agent_state.rs` `Skew`, the store's clock less the server's, read at start, at each save and before a cordon's commit) take the store's time (Built, #938). (Join-token and credential expiry,
  `join_tokens.rs:440-557`, go with G3.) The whole-host mark's `claim_by` (`js-sched-placer/src/jobs.rs:741`, still the process clock) is replaced by a stored deadline on this clock, by G17 (G8 is deleted).
- The process clock stays for intervals that are the process's own: ticks, backoffs, the hello limiter, the held beat.

One more round trip per pass or request, and none per heartbeat: its `heard_at` is `now()` inside the beat's own write (section 0, G15).

**Load.** Two costs grow with the fleet, not with the instances. Every claimed decision (a placement) holds the commit head from
its start (3.1), so the placements are serialized there, and since section 0.1 they are the one manager's; each holds it for one job's rows, and more schedulers add none. The heartbeat is a row write per host per beat interval (section 0, its cost): at the
default 5 s, 100 hosts are 20 writes a second. Neither is measured here; G15's pull request measures the beat's write at the
deployment's host count, and `js-state/tests/postgres_head_queue.rs` exercises writers queued on the head.

## 5. Hosts across instances

Section 0 is the design: a host is its rows, and any instance serves its requests. This section says what each decision about a
host reads.

- **The session** (G15): `node/<host>`, written by the hello at the version it read (a first hello at `Cond::Absent`) and by every
  beat, as section 0 says. A beat on any instance is checked against the row (its epoch, and the session digest until G3) and
  applied by the guarded `UPDATE`; its listing's writes are conditional on the version that `UPDATE` wrote, through #883's
  `Checked` store.
- **Identity** (G3, G4): a host is its certificate. `hosts/binding/<host>` is read by key in the hello's transaction and written at
  the version read. js-server keeps no per-host secret in a row beyond the digest of the session secret until G3 (after it the certificate proves the host and a beat's `session` is not checked; an old agent process is refused by its epoch, `409 stale-epoch`, which stays): the session string a beat carries is checked against the row like
  its epoch, and a log upload credential is minted per instance from the host id (section 3.3).
- **Admission** (`no-capable-host`, G5): the live `node/` rows (heard within grace by the database's time). The refusal is never
  taken on less than the rows.
- **Host listings and `host_silent_since_ms`** (G5): the `node/` rows.
- **`HostLive`** (G5, G15): a hello of a host whose row was heard within grace is refused as today, judged on the row, so every
  instance answers alike; the refusal read from the local record (`js-registry/src/registry.rs:468-473`) goes. A hello of the same
  agent process (its welcome lost) takes the host over at once: G15 adds an optional hello field naming the agent process, which no
  wire carries today. A new agent process, as after a restart, waits out the grace as now.
- **`PoolFull`** (G5): the hello of a host new to the pool locks the pool's row `pool/<pool>` (read and written at `Cond::Version`
  in the hello transaction, so two such hellos of one pool are serialized) and counts the pool's `node/` rows that are not forgotten, as the registry counts its records today (a lost host inside retention counts). A known host is
  admitted as today.
- **Settlement** (G6): the claimed job pass reads the `node/` rows. A host read absent by the rule the registry forgets one by today (`heard_at` older than grace plus retention, no taint, no cordon, no work the placer holds on it: `js-registry/src/registry/record.rs:149`, `beat.rs` `forgotten`; silence alone is never absence, `job_pass.rs:117`) by
  the database's time is lost: each of its live allocations is ended in a transaction that also checks `node/<host>` at the version
  read, so a beat that lands meanwhile makes the settlement a conflict and nothing is lost. The registry's `gone` map no longer
  decides, and the job pass no longer bypasses its claim for it (`job_pass.rs:186`). A host is settled the same way whichever
  instance served it last, or whether that instance is still running. G6's tests: a host whose last instance stops is settled by
  another after grace plus retention on an injected store clock, and a beat landing between the read and the end makes the
  settlement a conflict that loses nothing.

## 6. Decisions already taken for js-server

Carried from komira-ai/hostkeeper's design, section 13:

| Decision | Here | Tag |
|---|---|---|
| A host enrolled in hostkeeper holds one per-host certificate the coordinator controls; no join tokens, no token passing, no per-service certificates, no revocation machinery (a compromised host is unplugged) | the certificate is the identity of every agent request (#922); G3 deletes the token and host-credential paths instead of making them N-safe | Built (the certificate); Designed (G3, the removal) |
| No agent stickiness beyond TCP keep-alive plus a server-side connection max age | an agent keeps no connection: it opens one per request (`js-client/src/client.rs:282`). A job client may keep one, so the job listener closes a connection older than `client_connection_max_age` (a config key with a default) at the end of a request | Built (agents: no connection is kept); not needed for job clients, since any instance serves them (G10 deleted) |
| The job-store cutover is a short pause with a dry-run copy, and a Postgres-to-SQLite export for rollback | #874 shipped the first start over Postgres with an empty job store ("drain the farm") and no copy or export tool | Designed (G12) |
| A hello with the same credential and agent instance takes the host over at once | in section 0 no instance owns the host, so the hello is judged on the row on any instance; the same agent process is recognised by an optional hello field naming it, which G15 adds (none on the wire today); a new agent process waits out the grace, as today | Designed (G15) |
| Losing one unflushed usage buffer when an instance dies is accepted | the minute buffer flushes every few minutes (`js-usage/src/hub.rs:448`) | Built |

## 7. Compatibility (B8)

Every change of section 9 is additive but G3's; G17 adds the role flag, so its default `both` keeps every existing deploy as it is. New key prefixes: `node/` (G15; beside, and not under, the usage rows' `usage/node/`),
`pool/` (G5), `hostmark/` (G7); new optional fields in the `node/` value (G15) and a stored deadline in the `mark/<job>` value (G17); the rows `manager/<id>` and `manager/lease` (G17);
a usage row per instance beside a host's hour pack (G11; a release before reads the pack alone and misses only the overlap's minutes, as today); new scaler rows under `acquires/scaler/` for a launch and for a pool's brake (G9). No
existing key is renamed, re-meant or written in a new shape. G15 adds two optional wire fields, the beat's sequence within its
session and, in the hello, the agent process; an older agent does not send them and an older server ignores them; they are named as
a change of the wire in that pull request. `NOTIFY` (G16) carries no stored format. G17 changes the public interface and names it in its pull request: the flag `--role` and key `server.role` (default `both`), the key `manager_lease_secs`, and the scheduler role's no longer placing; G18 adds `js manager drain|list`, `POST /v1/managers/{id}/drain` and `GET /v1/managers`. All are additive: an agent or client that never calls them is unchanged. The first update that carries G17 overlaps a release that places without checking the lease, as today (one overlap, no worse than now). Its test: the release before serves a store holding `manager/` rows, and the new release places over a store the old one placed in. The binding and scaler-record rows keep their values; what changes is
that they are written at the version read, which a reader does not see. An older release ignores the new prefixes. G3 is the one change
that removes: by the owner's ruling it deletes the join-token mint and revoke, `--agent-token-file`, the host credential a welcome
carries and the agent's token and credential flags, and refuses an agent request without a certificate. It names that change of
the public interface in its pull request, lands only when no deployed agent still joins by token (so none is refused), and stops
reading and writing `hosts/join/` without deleting any of its rows, so a release before it still finds them (its test: the release
before serves a store the new release ran on).

Mixed releases: the update is start-first (section 12), so the release that closes a gap overlaps, once, with the release before
it, which still makes the write the gap is about. That overlap is no worse than today's; the next update overlaps two fixed
releases. A later change to a row this document adds is additive in the same way.
Each gap's pull request that adds a row names its test that the previous release's reader serves a store holding it (as
`js-state/tests/postgres_schema.rs::migrating_to_n_plus_1_leaves_the_serving_n_child_working` does for the schema), and says so
where it adds none.

## 8. Tests

Each gap's race is tested in Rust against real Postgres (the `state-postgres` CI job and the crate's test support), with two or
three store handles standing for two or three instances, made to meet at the step that decides by a barrier or a channel, never by
a sleep or a timing assertion (T10). Every test has a failure case: the same race with the condition or the lock removed loses a
row, places twice or decides from memory, and the test fails on it (T15). The tests already on `main` that run two instances over
one store are `js-lease-server/src/sched/jobs_two_servers_tests.rs::two_servers_place_each_job_once_and_never_book_a_host_past_its_cores`,
`js-lease-server/src/agent_api_store_tests.rs::two_servers_do_not_overwrite_each_others_taint`,
`js-lease-server/src/job_pass_tests.rs::a_pass_another_server_claimed_is_left_to_it_until_the_period_is_out`,
`js-jobs/src/suite/duties.rs::a_duty_is_claimed_by_one_server_for_each_period`,
`js-jobs/src/suite/idempotency.rs::minted_ids_do_not_repeat_across_instances` and
`js-state/tests/claim_decide.rs::writers_racing_for_one_row_place_it_once`.

## 9. The gaps, one pull request each

Each lands after this document, is one subject, changes neither the replica count nor the update order (G17 adds the manager service and the drain step of section 12), and turns its rows of section 3 to Built with paths.
They land in the order of section 12's "Order of safety" (the gaps an update's overlap reaches today first), then in the table's order.

| # | Under sections 0 and 0.1 | Pull request | Closes (section 3) | After |
|---|---|---|---|---|
| G2 | Kept: section 0's deadlines by `now()` | **One clock** (Built by #938; no `try_now`: `Store::now` itself is fallible): the store's time for every shared deadline, expiry and due time. Left to other gaps: the whole-host mark's `claim_by` (G17), join-token times (G3), presence (G5), usage minutes (G11) | `Store::now` epoch; the pass's clock; host-row times; the whole-host start's clock | none |
| G3 | Kept: the certificate is how any instance knows the host | **Host identity is the host certificate**: delete the one-time join tokens (their mint, revoke and use-once paths, the cap of waiting tokens, the give-back of a refused hello), the pool tokens of `--agent-token-file`, and the host credentials a welcome issues and the tick renews; an agent request without a host certificate is refused; the agent's token and credential flags go. Nothing replaces them: the certificate names the host (#922), and no per-host secret is kept in a row (section 5). The scaler's mint and revoke of a launched machine's token (`js-lease-server/src/scaler/live.rs:209`, `:259`) go too, while the scaler is off; G9 launches machines with certificates. It lands once every agent presents a certificate: Linux hosts do (#922), the Macs do not yet, so until then the token hazards of section 12 stay live for the Macs. komira-ai/jobscheduler#935 (Built) reads a token's row by key at its first use (`js-lease-server/src/join_tokens_stored.rs:19`, `js-lease-server/src/agent_api/unknown.rs:34`) meanwhile; that goes with the tokens | 3.2: a host without a certificate; 3.6: the token times | none |
| G15 | New: section 0's heartbeat | **The heartbeat is a row `UPDATE` on any instance**: the session in `node/<host>` (section 0's fields), written by the hello at the version read and by every beat; a beat guarded by its (epoch, sequence), its listing's writes conditional on the version it wrote, through #883's `Checked` store; two optional, additive wire fields, the beat's sequence and, in the hello, the agent process (so the same process takes the host over at once), the agent sending them first; for an agent that sends no sequence, a confirmed container freed only after two applied beats in a row omit it; the first-listing and listing-clean flags as `node/` columns; the reply built from the rows; no `409 unknown-host` or `403 session-mismatch` for a host the row knows; the registry refreshed from the rows. It measures the beat's write through the `Store` against a plain `node` table and takes the cheaper one that keeps the guard (section 0, its cost). komira-ai/jobscheduler#883 is closed in favour of this pull request, which takes its `Checked` store and its tests; `docs/design/jobscheduler-design.md` section 10.1 amended | 3.3: which connection commits a listing, first listing, the session, listing-clean; the lost whole-host start (was G14) | G2, G3 |
| G4 | Shrunk: no bindings save is left to make conditional; each change of a binding is its own conditional write where it is decided | **Host bindings are rows, and after G3 only the taint**: until G3, `HostBound` is judged on the binding row read in the hello's transaction (`Cond::Absent` for a first hello, `Cond::Version` otherwise) and a refused hello is judged before its binding is written, so there is nothing to undo; after G3 the certificate names the host, so `HostBound`, the undo, the binding's credential and its copy of the cordon end (`cordon/<host>` is the cordon) go, and the row keeps the taint; the registry holds no bindings to save, so `save_bindings` goes; cordon, uncordon and clear taint take the host from the rows, so a host served by another instance is not `404` (`js-lease-server/src/agents_cordon.rs:60-63`, `agents_taints.rs:86`); `taint/<host>` dropped at the version scanned. Built by komira-ai/jobscheduler#935: the save deletes only a binding this registry held and let go (`js-lease-server/src/agent_state.rs:358`), and the hello of a host the registry does not know reads its binding row first (`js-lease-server/src/agents.rs:567`) | 3.2: host bound, undo, saved, taint seen, cordon's `404`, clear taint; 3.5: cordon and clear taint through the API; `taint/` drop | G3 |
| G5 | Shrunk: presence is the beat's own `UPDATE` (G15), so no presence write or `presence_refresh` of its own; what is left is the readers | **Decisions read the `node/` rows**: admission, host listings, `host_silent_since_ms`, `HostLive` (judged on the row; the same agent process by G15's hello field) and `PoolFull` (under `pool/<pool>`, counting the rows not forgotten) read the `node/` rows (section 5) | 3.3: `HostLive`, `PoolFull`; 3.5: admission, listings | G15 |
| G17 | New: section 0.1 | **The JobScheduler and JobManager roles**: `--role` with `scheduler`, `manager` or `both` (`server.role`, default `both`); the manager rows `manager/<id>` and the lease `manager/lease` (take a free or expired lease at the version read with the generation raised, renew at a third of `manager_lease_secs`, by the database's clock); every claim, placement and duty transaction checks the lease in its own transaction; SIGTERM writes the row `draining` and frees the lease in one transaction and sends `NOTIFY` (G16 builds the wake; until it the standby takes a freed lease at its poll); the scheduler role places nothing (the beat's pass `js-lease-server/src/agent_api.rs:641` and its `keep_taints` at `:644`, the submit's place-now `js-lease-server/src/job_api/submit.rs:301` (called at `:283`) and the tick's `place_jobs` at `js-lease-server/src/api.rs:202` move to the manager; the other parts of the tick are assigned in section 0.1), and the duties and the scaler run in the manager. It measures the placement latency of section 0.2. Until G16, a submit's place-now and a beat's free reach the manager only at its poll: `both` wakes its manager in process, a lone `manager` waits for the tick. Tests, against Postgres with two managers (and the SQLite store for the version-check path): a manager whose lease is freed between its read and its commit places nothing; a standby takes an expired lease on an injected clock and the old holder's next placement conflicts; a standby takes a freed lease at its next poll (on the drain's notification once G16 is Built); two managers never both commit a placement in one generation; one `both` behaves as today, and N `both` copies place from one | 3.4: which host drains, queue notes (G8 deleted); 3.6: the mark's `claim_by`; 3.6: the scaler runs per instance; section 12: placement in the overlap | G2, G15 |
| G6 | Kept, simpler, and the manager's: silence is `heard_at`, so the local `gone` map and the claim bypass go; the active manager runs it under the lease | **Settlement by rows**: the claimed job pass loses a host's allocations by `node/<host>`'s `heard_at` and version, never by the local `gone` map; the claim bypass goes | 3.6: settlement | G15, G17 |
| G7 | Kept, in the manager's placement transaction, over every live host | **Placement re-checks the host in its transaction, over every live host**: the pass builds the engine's hosts (`host_up`, `heard` with the load, `OpenHost`) from the live `node/` rows it reads before each pass, not from the hosts this instance serves, and the pass's skip rule (`jobs_seen.rs`, engine change counts) counts a changed row, not a local call; `place_claimed` reads the host's binding (taint; the cordon is Built, see below), `taint/<host>`, the scale-down mark and the ports of the host's live allocations in the claimed transaction and refuses the host; the scale-down mark becomes a row `hostmark/<host>`, written at `Cond::Version` by whoever marks, the registry mark its cache | 3.4: contained beside host job, taint, ports, scale-down mark, which hosts an instance places on (the cordon check is Built: `js-jobs/src/store/whole.rs:190-196`, `:290-291`) | G4, G15, G17 |
| G16 | New: section 0's wake, aimed by section 0.2 | **LISTEN/NOTIFY wakes the active manager and held beats**: a submit, a listing's free, a cancel and a lease release `NOTIFY` the manager channel after their commit; a placement `NOTIFY`s the host's id on the host channel; the lease holder places, a standby acts only on a release, and the scheduler holding the host's beat answers it; each process `LISTEN`s on one connection of its own and wakes every waiter once when that connection is made again; the poll stays the guarantee (`js-state/src/postgres/mod.rs:25`). Tests: a submit on one scheduler is placed by the manager before its tick; a placement ends a beat held on another scheduler before its hold; with notifications off, both happen at the next poll | 3.3: held beats (a placement elsewhere waits for the next beat) | G15, G17 |
| G18 | New: section 0.1's operator drain | **The operator drains and lists managers**: `js manager drain <id>` and `js manager list`, over `POST /v1/managers/{id}/drain` and `GET /v1/managers` of the operator role; the drain is the same transaction as SIGTERM's (the row `draining`, the lease freed if held). A change of the public interface, named as such in that pull request. Tests: a drain through the route fences a manager that has not yet read its row; the list shows each manager's state and generation | section 0.1 | G17 |
| G9 | Shrunk: it runs in the one active manager, so it needs no claim per step and no cross-instance lock on its caps; left are its rows and the fence | **The scaler under the rule**: the scaler runs in the active manager only, so one step per pool needs no claim of its own; its records and unremoved set written at the row's version, a conflict re-reading and re-deciding the step; each launch a row at `Cond::Absent` before the provider call, its acquire id derived from that row so a repeated call is the same machine; caps and budgets judged in the transaction that writes the launch row, which checks the lease (no lock of its own: one manager decides); the lease checked again before each provider call (this narrows the window and does not close it: a manager paused after the check can still call; the launch row at `Cond::Absent`, whose write checks the lease, and the acquire id derived from it make that call the one the next manager repeats); the brake a row, its reset a conditional write; a release written to `hostmark/<host>` before the provider release; a machine it launches joins with a host certificate, since G3 removed the launch token, so the scaler stays off until a launched machine can be enrolled, as it is off on the deployment today | 3.6: scaler step, records, caps, host view; 3.5: brake reset | G2, G3, G7, G17 |
| G11 | Kept, and more needed (the prune runs in the manager, so after G17): with any instance taking any beat, a host's minute is split over instances as a rule, not only at an update | **Usage rows and the import**: the import's loser reads the marker and starts; a host-minute flushed by two instances keeps both: each instance's minutes are kept in a row of its own beside the hour pack (a new key under the host's hour, named by the instance's start), and a reader sums the hour pack and those rows, so a flush retried after a lost reply replaces its own row and never counts twice; the usage prune run by the active manager; profile eviction at the scanned version; the stale "one writer" doc in `js-usage/src/store.rs` | 3.1: import; 3.7: minute, prune, profiles | G17 |
| G12 | Kept: the cutover is not about instances | **Job-store cutover tools** (section 6): a dry-run copy of `<state file>.jobs` into the store with counts and a checksum per prefix, and an export of the store's job and usage rows to a SQLite file, with the round trip as its test. Moot for a deployment whose job store is already in Postgres with nothing to bring back | section 6 | none |
| G13 | Kept: section 12 | **Readiness the edge can reach** (section 12): `--ready-listen` accepts a private address as the agent and job listeners do (`js-lease-server/src/config.rs:417`, `js-lease-server/src/config/listen.rs:55`, `:79`), so the edge checks `/readyz` and sends no new connection to an instance from its first `503 draining` (Built by komira-ai/jobscheduler#935: agents move at SIGTERM because the agent listener closes, held beats are answered at once, and a draining instance places nothing, `js-lease-server/src/server.rs:840`, `js-lease-server/src/drain.rs:28`; job clients still reach the old instance through the hold); the stop grace above `--drain-hold` plus `--drain-timeout` is checked once at deploy (B20), outside this repository | 3.1: readiness; section 12 | none |
| G1 | Shrunk into G15: no connection owns a host, so #883's rule that the newest hello owns it and its `503` on a racing hello go; its mechanism, every listing write conditional on `node/<host>`, is G15's guard | ~~Finish komira-ai/jobscheduler#883~~: its `Checked` store and its two-server test (`two_servers_serve_one_store_place_each_job_once_and_keep_to_the_hosts_cores`) move into G15, and #883 is closed in favour of G15 | (now G15) | none |
| G8 | Deleted: one active manager walks the queue and chooses the host a whole-host job drains, so two hosts are never drained for one job and the queue note is written by one engine; the mark's deadline on the database's clock is G2's | ~~Whole-host drain and queue notes without churn~~ | 3.4 (now G17, G2) | none |
| G10 | Deleted: an agent keeps no connection, and any instance serves any job client, so a connection age only spreads load (B20: no machinery without a need); a draining instance closes its connections (section 12) | ~~Job-client connection max age~~ | section 6 | none |
| G14 | Deleted: a start is a function of the rows, so the next beat on any instance carries it again (G15), and with no hello on a change of instance the first-listing rule, a stored `node/` flag cleared in the beat's own writes, runs only after an agent restart, whose listing is the agent's whole account (`per_job.rs` `starts_for` sends a `starting` whole-host job's start on every beat until the host lists it, so a lost reply is sent again) | ~~A start whose reply was lost is sent again~~ | 3.3 (now G15) | none |
With G2 to G7, G9, G11, G13 and G15 to G18 Built, any number of instances is safe in steady state and through an update's overlap from this repository's
side; the edge's check and the stop grace are the deployment's.

## 10. Supersession (B22)

- This text replaces the earlier draft of this file (komira-ai/jobscheduler#873, never merged), which designed presence rows,
  a `hostbook` gate and a placement lock order of its own before #877 to #881 built placement and the duties differently. What
  it found that still holds is in section 3 and section 9.
- [deployment.md](deployment.md), "(b) later" and "Single active writer, not leader election": a second JobScheduler replica needs no leader,
  no election and no fence; the JobManager has one lease holder (section 0.1), fenced by the conditional write on the lease row, which is the fence that file's 'if (b) comes' sentence names. That file gets a pointer here, and its 'Single active writer, not leader election' text is amended in G17's pull request.
- [jobscheduler-design.md](jobscheduler-design.md) section 10.1, "a heartbeat is not a write", is amended by G15 in its pull request.
- [configuration.md](configuration.md), "What it keeps": G3 removes the tokens and host credentials from that text and G4 makes
  "read once at start" untrue for the bindings; each changes it.
- [../lease-server-update.md](../lease-server-update.md) describes the host-native, one-server, stop-first update; the deployment's
  start-first update is section 12 here.

## 11. Decisions made in this document

- **Settlement needs `heard_at`.** Settling by `node/<host>`'s version alone would leave a host whose instance died unsettled
  forever, since no live instance ever heard it. So every beat sets `heard_at = now()` in its own UPDATE (G15), at the 5 s rate the owner accepted for the coordinator.
- **`PoolFull` takes a row lock per pool, and only for a host new to the pool.** That serializes exactly the hellos that can
  overrun the cap, with no counter to keep in step.
- **The update hook stays a one-server tool.** It manages the host-native install of `docs/lease-server-update.md`; an
  N-instance deploy is the orchestrator's start-first update, not the hook's.
- **No separate model checker.** Every gap is one row's race, and its test runs that race against real Postgres with barriers.
  The protocol-level model of the coordinator (its section 14) covers the gate and the decisions across services, which js-server
  does not have.
- **G3 removes the tokens instead of making them N-safe.** The owner ruled that an enrolled host's certificate is its one
  identity, with no join tokens and no revocation machinery, so the token rows get no conditional writes and no cap lock.
- **One binary with a role flag, not two binaries** (section 0.1). The default `both` keeps a single-node install as it is, and
  the manager reads the same job tables and host rows the scheduler writes.
- **One active manager under a lease, not N** (section 0.2). Placement is already serial on the commit head, so N active managers
  buy no throughput. They cost an N-safe rule for the whole-host drain, the scaler and the duties. One costs a pause of at most the
  lease time after a crash, and nothing on an update, where the deploy's SIGTERM hands the lease over.
- **The deploy drains the old manager; a new start never does.** That way a broken release cannot take placement from a working
  one, and a manager restarted by hand cannot steal the lease.
- **The live hazards go first.** Section 12's list is what an update does to the deployment today; G15, G4, G5, G17, G7 and G11 close
  it, and G3 closes the token hazards once the Macs hold certificates. komira-ai/jobscheduler#935 (Built) landed parts of G3, G4
  and G13; each row says which part.
- **The standard shape (section 0) replaces sessions held by one instance.** The project owner found the earlier text too
  complicated for something fairly standard. With the heartbeat a row `UPDATE` on any instance, #883's connection ownership (G1),
  the connection max age (G10), the lost-start rule (G14) and, with one manager, the whole-host drain rule (G8) go; presence (G5),
  the bindings (G4) and the scaler (G9) shrink. Section 9 says which, and why. G13 changes which address a flag accepts; it is
  not a runtime gate.
- **G12 stays Designed** although #874 shipped without it, because the owner's decision asks for it. A deployment that has no
  SQLite job store to move needs only the export.

## 12. Updating js-server live

The project owner's requirement: a control-plane service is updated live, with no moment at which no instance serves and no run
stopped by the update.

**How agents and clients reach an instance.** Agents (the agent listener) and job clients (the job listener; the build frontend
among them) connect through the edge proxy in front of the service, outside this repository. It passes TCP through, so TLS and a
host's client certificate reach the server unchanged, and it sends each new connection to a task it believes up. Today it believes
a task up while a TCP connect succeeds: `/readyz` (`--ready-listen`) is served on a loopback address only
(`js-lease-server/src/config.rs:100-105`, enforced by `js-lease-server/src/config/listen.rs:55`), so the edge cannot see a drain.
An agent opens a new connection for every request, with a 10 s limit (`js-client/src/client.rs:278-282`), so it is never held to
one instance; a job client may keep its connection.

**The update is start-first today.** The deployment's stack (outside this repository; stated by its owner, not checked from here)
updates the service start-first, in updates and in rollbacks: the new task starts and serves beside the old one, and the old one is
then stopped and drains, within the stop grace. Two instances overlap
on every update now, whatever section 9 has built.

**What the old instance does on SIGTERM** (Built):

1. `/readyz` answers `503 draining` at once. The agent listener closes (a request in flight is answered, a held beat at once), no job is placed, and the instance keeps serving the job service and ticking for `--drain-hold` (15 s;
   `js-lease-server/src/server.rs:840`, `js-lease-server/src/drain.rs:28`, #876, #935).
2. It stops accepting connections. A request in flight is answered and its connection closed (`Connection: close` on HTTP/1.1,
   `GOAWAY` on HTTP/2), an idle connection is closed, and what is still open after `--drain-timeout` (10 s) is cut
   (`server.rs:858-882`, `js-lease-server/src/listener.rs:286-290`, `:368-392`, `:422-426`).
3. It ticks once more (`server.rs:882`) and exits. The final tick is the whole tick (`js-lease-server/src/api.rs:187`): the job pass
   (claimed), the registry's advance, the refresh (token reload, the cordon mirror, the bindings save, which since #935 deletes only a binding this registry held and let go, the credential flush, the
   usage tick, the log grants) and a placement pass that places nothing, because the instance is draining (#935, `js-lease-server/src/sched/jobs_pass.rs:147`); then the usage side's last flush (`server.rs:713-718`).
   The scaler does not run in it. Two hazards below come from this tick.

An agent whose connection was closed or refused retries with its backoff. Today the instance it reaches holds no session for it, so a
beat is answered `409 unknown-host` or `403 session-mismatch` and the agent says hello there (`js-agent/src/session.rs:32-45`); after G15 it serves the beat from the `node/` row and no hello is repeated.
Its jobs run on throughout: an agent keeps its jobs without a server for up to `--silence-stop` with no accepted reply (300 s by
default, `js-agent/src/cli.rs:311-316`), far more than the drain hold and drain timeout (25 s), and on the server's side silence
ends nothing before grace plus retention ([absence.md](absence.md)).

**Updating the two roles** (G17; until then one process runs both, as above). The JobScheduler service updates start-first as
above: every copy is stateless, and an old and a new copy serve side by side. The JobManager service updates start-first too:

1. The new release's manager starts as a standby and waits for a free or expired lease.
2. Once the new manager's readiness answers `ready`, the deploy sends the old one SIGTERM (with more than one old manager, one at a
   time; an old standby may take the freed lease for a moment, and its own drain hands it on, each hand-over fenced). In one transaction it writes its row `draining` and frees the lease, so the fence holds
   from that commit, and it sends `NOTIFY`.
3. The new manager takes the lease on that notification (at its poll until G16) and places from its next transaction. The old one lets its in-flight
   transactions end (a placement among them conflicts and writes nothing) and exits.

From the first update that carries G17, no moment has two managers placing, and none has no manager able to place for longer than one notification (one poll before G16). The update that first carries G17 overlaps a release that places without the lease: the fence binds only a release that checks it. A manager that crashes
mid-update instead of draining leaves the lease to expire (`manager_lease_secs`), and that pause is the only one. An operator can
drain a manager by hand the same way (`js manager drain`, G18).

**Hazards of the overlap that are live now:**

| During the overlap | Effect | Closed by |
|---|---|---|
| A submit reaches the new instance before the hosts have said hello to it | refused `no-capable-host` (`422`) for good, though the hosts serve on the old instance | G5 |
| Either instance's tick saves a binding that differs from the copy it last wrote (a lapsed or mirrored cordon end, a forgotten host) | the whole binding row is put or deleted at `Cond::Any` from that instance's copy, so a taint the other instance stored in it is lost, or a host it holds loses its binding | G4 (the delete of a binding never held, and the taint read at a hello: Built by komira-ai/jobscheduler#935; the put at `Cond::Any` remains) |
| A host job placed by one instance is a taint in that instance's memory, and the host's beats move to the other | the other may place a contained job beside the host job | G17 (only the lease holder places), G4, G7 |
| An agent's taint clear reaches the instance that holds no taint for the host (a beat after the update's start) | before #940 it was acknowledged and changed nothing, and the stored taint stayed until a later start put it back on a clean host | closed: komira-ai/jobscheduler#940 (Built, `js-lease-server/src/agent_state.rs:686`); G4 for the rest |
| Both instances run placement, in the beat's pass, the submit's place-now and the tick, each over the hosts it serves | each placement is claimed and booked safely (Built), but two engines walk the queue, drain hosts for whole-host jobs and write queue notes from different views | G17 (one fenced manager) |
| A host has a connection on each instance for a moment (a beat in flight to the old one when the agent reconnects to the new one) | the old connection can commit a listing over the new one's | G15 |
| A host that joins by token (the Macs) presents a host credential or one-time token the other instance issued after it started | the other reads its tokens at start, and a token it does not hold is answered `401` (the agent exits; `401` is fatal) until #935 below; one token can also admit two hosts | G3 (removes the tokens; live for the Macs until they hold certificates). komira-ai/jobscheduler#935 (Built) reads an unknown token's row by key, which ends the `401` until G3 |
| An agent that said hello to the new instance reaches the old one again (each request opens a new connection, so any may; since komira-ai/jobscheduler#935, only until the old one's SIGTERM) | its beat is `403 session-mismatch`, and its hello there is `409 host-live` while the old record is within its grace, so the agent backs off | G15 (no session to mismatch; the same agent is applied at once) |
| Both instances admit hosts new to a pool | the pool's cap counts per instance | G5 |
| `ListHosts` and `GET /v1/hosts` on either instance | each lists only the hosts whose session it holds | G5 |
| A whole-host job's start rides on a ready report's reply that is lost (a request cut by the drain, a killed process) | the next first listing ends it `Lost` without its having run | G15 (was G14) |
| A host's minute of usage is recorded by both instances (it moved during the minute, and the old instance flushes at exit) | the later flush replaces the minute, so up to a minute of the host's usage rows is lost | G11 |
| New connections reach the old instance through its drain hold, and are refused by it once it closes its listener, until the edge notices | an agent retries; a job client's request fails and is retried by the client | G13 (komira-ai/jobscheduler#935, Built, closes the agent listener at SIGTERM, so agents move at once, answers held beats at once and places nothing while draining; the edge's sight of `/readyz` remains), and a stop grace above `--drain-hold` plus `--drain-timeout` plus the final tick, checked once at deploy |

Not live in an update: settlement by the local registry (G6) needs a host unheard for grace plus retention, far longer than an
overlap; the clock (G2) is one clock while both tasks run on the one node; the scaler (G9) is off on the deployment; the import and prune parts of G11 are economy; G10 is deleted.

**Order of safety.** Each step is relied on only once the one before is Built:

1. Every write conditional and every duty claimed (section 2, Built): two instances never place one job twice, book a host past
   its room or run a duty twice.
2. The live hazards closed: G15, G4, G5, G17, G7 and G11's minute fold, with G2 and G3 before G15; G3's removal of the tokens once the
   Macs hold certificates. komira-ai/jobscheduler#935 (Built) landed parts of G3, G4 and G13 ahead of them.
3. The edge sees the drain (G13) and the stop grace covers it, so no connection is refused during an update.
4. The rest of section 9 (G6, G9, G16, G18, the rest of G11, G12), which matters for N instances in steady state or for a long overlap, not for one update.

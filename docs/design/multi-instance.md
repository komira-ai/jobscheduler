# js-server as N instances on one Postgres

Status: design. A living document: each pull request that builds a part of it turns that part's tag from **Designed** to **Built**
and names the code. **Built** names code on `main` with a path and line; **Designed** means nothing on `main` implements it yet, and
says which pull request of section 9 does. Line numbers are those of `main` when this document was written; a later change moves
them, and the pull request that moves a decision's code updates its row.

The server is `js-server`, the binary of the `js-lease-server` crate. Its store is the `js-state` `Store` (`--store`, Postgres);
its job tables are `js-jobs`, its placement `js-sched-placer`, its host sessions `js-registry`, its usage `js-usage`. A SQLite
store is one process's (its file lock, `js-state/src/writer.rs`) and is not what this document is about.

The same rule governs hostkeeper's coordinator, in komira-ai/hostkeeper `docs/design/coordinator-multi-instance.md`. Section 13
of that document lists the decisions already taken for js-server, and section 6 of this one carries them over.

## 1. The rule

Direction from the project owner (2026-11-15, binding): **Postgres is the single source of truth, and any number of js-server
instances over one store just work.** There is no leader, no leader lease and no election. Every decision (placement, admission,
a host's session, a taint or cordon, a periodic duty, a scaler step) is made inside a Postgres transaction whose correctness does not
depend on which instance runs it or how many run it at once.

1. **Postgres decides; memory only caches.** An instance's memory holds caches that can be rebuilt from the rows. A cache may
   choose what to try (which job to place first, which host to offer), but the decision is taken against the rows, in the
   transaction that writes it. A cache that decides is a defect.
2. **Three tools, and nothing else.** Every coordination is one of:
   - a **row lock** taken inside the deciding transaction (a `Txn` locks every key it names, in key order, and holds the lock to
     its commit: `js-state/src/postgres/tx.rs:1-25`; `Store::claim_decide` locks a claimed row `FOR UPDATE SKIP LOCKED` and
     decides under it: `js-state/src/postgres/claim.rs:98`);
   - a **conditional write**: `Cond::Version` (a compare-and-set on the version read) or `Cond::Absent` (a unique key); the
     `Conflict` says who lost;
   - a **claim by advancing a due time** for periodic work (`JobStore::claim_duty`, `js-jobs/src/store/duties.rs:42`): the
     instance whose advance of the duty's row commits runs it for the period.
   No step elects, leases, counts instances or relies on being the only process. An instance that dies at any point leaves
   nothing that another must clear by a timeout: its transaction committed or it did not.
3. **Time is the database's.** A time that one instance writes and another compares (a deadline, an expiry, a due time, a
   last-heard time) is read from the store's clock (`Store::now`, the database's `now()`), never from the process clock. The
   process clock times only what stays in the process (a tick interval, a backoff, a rate limit).
4. **Fail closed.** An instance that cannot use the database decides nothing: no placement, no admission, no settlement, no
   duty, no scaler step. A request that needs a decision is answered with a server error the agent retries with its backoff
   (`503` or `500` `state-write-failed`; `js-agent/src/session.rs:18-30` makes only `401` and named `4xx` fatal), never `401`,
   and hosts keep running what they run.
5. **Formats change additively (B8).** New key prefixes and new optional fields only.
6. **No runtime machinery for a config mistake (B20).** Every number is a config key with a default; a deploy's mistake is
   checked once at deploy.

The deploy shape does not change in this repository: one replica, stop-first (`docs/lease-server-update.md`), until every gap of
section 9 has landed. Correctness is argued and tested for any N. Moving the deploy to a start-first update (two instances while
an update overlaps) is an infrastructure change outside this repository, made after section 9 is Built.

## 2. What is already Built

The merged pull requests komira-ai/jobscheduler#874, #877, #878, #879, #880, #881 and #923 put the store, the job tables,
placement and the periodic duties under the rule:

- **No writer lock and no fence** (#878). A Postgres store takes no lock at start and checks nothing in a commit;
  `writer_generation` is unread (`docs/design/configuration.md`, "No lock on Postgres"; `docs/runbooks/state-database.md`).
- **The job tables and usage rows live in the store** (#874), so every instance reads the same jobs.
- **A placement claims its job and books its host in one transaction** (#877, #879).
- **A pass re-reads the tables after any instance's commit** (#880), judged by the store's head revision.
- **The job pass and the feed trim are duties claimed per period** (#881).
- **A start that loses a race at the open retries** (#923).

The tables of section 3 list each decision with its code.

## 3. The audit: every decision of js-server against the rule

Each row is a decision two instances could make together. "How it holds" names the tool of rule 2 or the reason it fails;
"Tag" is Built, Designed (with the gap of section 9 that builds it) or Per instance (state that is the process's own by design
and decides nothing shared). Paths are under `crates/`.

### 3.1 The store and the start

| Decision | Where | How it holds | Tag |
|---|---|---|---|
| A transaction's preconditions hold at its commit | `js-state/src/postgres/tx.rs:1-25`, `:269`, `:420` | every key it names is locked in key order (a missing key reserved by a placeholder row), and `Cond` is judged on the locked rows | Built |
| Revisions are minted in commit order, with no gap in the change feed | `js-state/src/postgres/tx.rs:156`, `:191` | `LOCK TABLE head` in the transaction's tail only, so bodies run at once and tails queue | Built |
| A claimed row is decided by one transaction | `js-state/src/postgres/claim.rs:98` | `FOR UPDATE SKIP LOCKED`; a held row is skipped, never waited for | Built |
| A schema migration is applied once by racing installers | `js-state/src/postgres/schema.rs:80` | `pg_advisory_xact_lock` inside the applying transaction, version re-read under it; the server itself only judges the schema | Built |
| The job store's schema row is written once | `js-jobs/src/store.rs:220` | `Cond::Absent`; a conflict re-reads the row | Built |
| The index backfill and the boot raise of deadlines at open | `js-lease-server/src/job_store.rs:195` (`booted_until_won`), `js-jobs/src/store/ending.rs:275`, `:325` | each write at `Cond::Version`; a beaten open retries (#923); a raise is a max, so a second start only delays deadlines | Built |
| The first start imports the files beside the state file once | `js-lease-server/src/store/import.rs:89-90` | marker and rows `Cond::Absent` in one transaction, so one import wins; the loser's `Conflict` refuses its start (exit 2) instead of reading the marker | Built (correctness); Designed (G11: the loser starts) |
| Row expiry (TTL) | `js-state/src/postgres/tx.rs:146`, `:289` | judged on the database clock inside each transaction | Built |
| Feed trim | `js-lease-server/src/job_store/feed_trim.rs:23`, `js-lease-server/src/job_pass.rs:227` | a duty claimed per period; a trim by absolute revision is idempotent | Built |
| An unreachable database | `js-lease-server/src/job_pass.rs:183`, `:210`, `js-lease-server/src/store_health.rs:250` | every store call fails and nothing is decided; a hello's or taint's write answers `503`, a listing's commit `500` `state-write-failed` (`js-lease-server/src/agent_api.rs:338`, `:645-649`), both retried by the agent | Built |
| `Store::now` when the database cannot be read | `js-state/src/postgres/mod.rs:920-930` | answers `StoreTime::ZERO` ("the store trait cannot report a failure here"); the `js-jobs` writes that stamp rows from it (`js-jobs/src/store.rs:327`, `:470`, `ending.rs:62`) stamp the epoch until their own write fails | Designed (G2) |
| Readiness, drain and shutdown | `js-lease-server/src/ready.rs:26`, `js-lease-server/src/server.rs:309` | one instance's own phases (#876); nothing waits on another | Per instance |
| The host-native update hook's "is a server running" probe | `js-state/src/writer.rs:128`, `js-lease-server/src/update_hook/step/mod.rs` | reads `pg_stat_activity` by the server's application name; it serves the one-server install of `docs/lease-server-update.md`, not an N-instance deploy (which is the orchestrator's start-first update) | Per instance |

### 3.2 Hosts: identity, admission, taint, cordon

| Decision | Where | How it holds | Tag |
|---|---|---|---|
| A join token or host credential is recognised | `js-lease-server/src/join_tokens.rs:351` (read once at start), `:431` (`identify` reads the in-memory map) | memory decides: a token minted or revoked on A is unknown to B until B restarts | Designed (G3) |
| A one-time join token is used once | `js-lease-server/src/join_tokens.rs:526` (`admit_writing`), `js-lease-server/src/host_keys.rs:45-55` | checked in memory and committed at `Cond::Any` (the doc comment there still says "the server is the store's one writer"): two instances can each admit a host with one token | Designed (G3) |
| A join token is minted, replaced, capped | `js-lease-server/src/join_tokens.rs:450-470` | the local map decides what to replace and the cap of waiting tokens; writes at `Cond::Any` | Designed (G3) |
| Revoking an instance's tokens | `js-lease-server/src/join_tokens.rs:662` | acts on the local map only; a token minted on B is not revoked by A, and B keeps accepting one A deleted | Designed (G3) |
| A host credential's renewed time | `js-lease-server/src/join_tokens.rs:724` (`flush`, by the tick) | the whole renewed row from the local copy at `Cond::Any`: can bring back a token another instance revoked | Designed (G3) |
| A refused hello gives back its token use and binding | `js-lease-server/src/join_tokens.rs:637`, `js-lease-server/src/agent_state.rs:411-420` | the undo is built from this instance's copy and written at `Cond::Any` | Designed (G3, G4) |
| A host id is bound to the credential and agent instance of its first hello (`HostBound`) | `js-registry/src/registry.rs:456-463`, `js-lease-server/src/agent_state.rs:413` | judged on the local registry and the bindings read at start; the binding written at `Cond::Any` | Designed (G4) |
| Hellos of one host are serialized | `js-lease-server/src/agent_api.rs:277` (`host_writes`) | a process mutex; correct across instances only once G3 and G4 make the writes conditional, after which it is an economy | Per instance (after G3, G4) |
| The bindings are saved | `js-lease-server/src/agents.rs:883` (`save_bindings`), `js-lease-server/src/agent_state.rs:286-297` (`writes_to`) | makes the store equal this instance's registry: puts every differing binding and **deletes every binding this registry no longer holds**, at `Cond::Any`; B's tick can delete a binding live on A | Designed (G4) |
| A taint a heartbeat reports is stored | `js-lease-server/src/agent_state.rs:478` (`commit_taint`) | `Cond::Version` of the row read, after checking the stored taint is the expected one | Built |
| Every instance sees a host's taint | `js-lease-server/src/agents.rs:296` (`restore_job_taints`, at start), `js-lease-server/src/sched/jobs_pass.rs:69` | the registry's taint decides placement and is loaded at start only; a host that says hello to B after A stored its taint is clean in B | Designed (G4, G7) |
| A cordon and its lift | `js-lease-server/src/agents.rs:600` (`keep`), `:670` (`cordon`) | the whole binding rebuilt from the local registry and put at `Cond::Any`; a host held by another instance answers `404`, and other instances' placement never sees the cordon | Designed (G4, G7) |
| An operator clears a host's taint | `js-lease-server/src/agents_taints.rs:86` | the write is conditional (`commit_taint`), but the host must be in this registry: a host held elsewhere answers `404` (fails closed) | Designed (G4) |
| Host certificate admission (`--agent-client-ca`) | `js-lease-server/src/host_cert.rs:58`, `js-lease-server/src/client_verifier.rs:27` | a function of the certificate and the configured CA | Per instance |
| Shared pool tokens of `--agent-token-file`, the hello rate limit | `js-lease-server/src/agent_tokens.rs:50`, `js-lease-server/src/agents.rs:69` | configuration and a limiter of this process | Per instance |

### 3.3 The session and the heartbeat

| Decision | Where | How it holds | Tag |
|---|---|---|---|
| Which connection of a host commits its job listing | `js-lease-server/src/agent_api.rs:580-614` (`per_job::serve`, `job_whole::serve`, `lose_unlisted` over the plain job store) | nothing orders two connections: an old connection on A can commit a listing over B's newer one, freeing an allocation the host holds or losing a job it lists | Designed (G1: komira-ai/jobscheduler#883) |
| A host's first listing after a hello loses the jobs it does not list | `js-lease-server/src/agents.rs:460` (`listed_sessions`, memory), `js-lease-server/src/agent_api.rs:594`, `js-lease-server/src/job_loss.rs:96-107` | each end is `Cond::Version`, but "first" is this instance's flag; safe once only the newest connection writes | Designed (G1) |
| A host's session, epoch, secret, last heard | `js-registry/src/registry.rs:470-521`, `js-registry/src/registry/beat.rs:123` | the instance holding the connection's cache; a beat that reaches another instance is refused (`409 unknown-host` or `403 session-mismatch`) and the agent says hello there | Per instance |
| `HostLive` (a second agent process on a live host) | `js-registry/src/registry.rs:464-469` | judged on the local registry: a different agent process that says hello to B while the host is live on A is admitted | Designed (G5) |
| The pool's machine cap (`PoolFull`) | `js-registry/src/registry.rs:502-511` | counts this registry's hosts: N instances admit N times the cap | Designed (G5) |
| Whole-host job states (placed, starting, running, prepare deadline) | `js-jobs/src/store/whole.rs:276`, `js-lease-server/src/job_whole.rs:138` | rows written at `Cond::Version` | Built |
| The whole-host start's taint and the clock it judges deadlines by | `js-lease-server/src/agent_api.rs:531` (`own.host.taint`), `:608` (`placing.clock.now()`) | the registry's taint and the process clock | Designed (G2, G4) |
| A job's start command, repeated by two instances | `js-lease-server/src/per_job.rs:165`, `js-lease-server/src/job_start.rs:120` | a pure function of the rows; the placement that creates it is claimed | Built |
| Listing-clean (a host whose last listing was not clean takes no job) | `js-lease-server/src/agents.rs:489` (`unclean`, memory), `js-lease-server/src/sched/jobs_pass.rs:158` | another instance's placement does not know | Designed (G5, G7) |
| Held beats and their wakes | `js-lease-server/src/held_beat.rs:36` | the process's own wakes; a placement by another instance goes out on the host's next beat | Per instance |
| Log upload credentials per host | `js-lease-server/src/logs/mod.rs:155-307` | stateless provider credentials; several valid at once; a flip costs a mint | Per instance |

### 3.4 Placement and the queue

| Decision | Where | How it holds | Tag |
|---|---|---|---|
| A queued job is placed by exactly one instance | `js-jobs/src/store/whole.rs:125-129`, `:276` | `claim_decide` on `job/<id>` (`SKIP LOCKED`), the job written at `Cond::Version` | Built |
| A host is never booked past its room | `js-jobs/src/store/whole.rs:174-199` | the host's live allocations summed in the claimed transaction, `HostFull` when they leave no room, `hostbook/<host>` written at `Cond::Version` | Built |
| One host job (`isolation=host`) per host | `js-jobs/src/store/whole.rs:279`, `js-jobs/src/store/taints.rs:36` | `taint/<host>` put at `Cond::Absent` in the placing transaction | Built |
| No contained job beside a host job, no job on a tainted or cordoned host | `js-sched-placer/src/jobs.rs:917-927` (`open_to_jobs`, registry view), `js-lease-server/src/sched/jobs_pass.rs:69` | the engine's memory only; the placing transaction reads neither the host's taint, its cordon nor `taint/<host>`, and `taint/<host>` is later deleted at `Cond::Any` (`js-jobs/src/store/taints.rs:72`) | Designed (G7) |
| A host port is held by one job per host | `js-sched-placer/src/jobs.rs:469` | the engine's memory only; the placing transaction does not compare ports | Designed (G7) |
| A host the scaler drains or releases takes no new job | `js-registry/src/registry/marks.rs:1-25` ("the marks are held in memory only") | a mark in one instance's registry; another instance's placement does not see it | Designed (G7, G9) |
| One mark row per whole-host job | `js-jobs/src/store/marks.rs:81-94` | `mark/<job>` at `Cond::Absent` or `Cond::Version`, with the job checked at its version | Built |
| Which host a whole-host job drains | `js-sched-placer/src/whole_jobs.rs:204-210`, `js-sched-placer/src/jobs.rs:727` | each instance chooses over its own hosts and deletes a mark whose host it never heard after its own start plus grace: two instances drain two hosts in turn (the rows stay consistent; the work is wasted) | Designed (G8) |
| Why a job waits (queue status note) | `js-sched-placer/src/jobs.rs:765`, `js-jobs/src/store/status.rs:38` | conditional, but its text depends on the instance's view, so two instances rewrite it in turn and each rewrite moves the head | Designed (G8) |
| The tables a pass decides from | `js-sched-placer/src/jobs_seen.rs:175` | a cache judged by the store's head (#880); every placement is re-decided in its transaction | Built |
| Which hosts an instance places on | `js-lease-server/src/sched/jobs_pass.rs:149` | the hosts whose connection it holds; each host is held by one instance and every instance reads the whole queue | Per instance |
| Queue order | `js-jobs/src/store/queue.rs:20` | a total order of row columns, the same on every instance | Built |
| Job sizing | `js-sched-placer/src/sizes.rs:541` | from the shared tables; the booking is checked in the transaction | Per instance |

### 3.5 The job API

| Decision | Where | How it holds | Tag |
|---|---|---|---|
| A client-chosen job id is recorded once | `js-jobs/src/store.rs:361-372` | `Cond::Absent`; a conflict answers from the stored job | Built |
| A minted job id is unique | `js-jobs/src/store.rs:387-396` | random ids, `Cond::Absent`, retried on a collision | Built |
| A cancel racing a placement on another instance | `js-jobs/src/store.rs:538-557` | `Cond::Version`; the loser writes nothing | Built |
| Job reads, results, logs | `js-lease-server/src/job_api/read.rs:220`, `js-lease-server/src/job_api/logs.rs:239-249` | read from the rows; a follow polls the row, no local wake decides | Built |
| A submit is refused `no-capable-host` | `js-lease-server/src/admit.rs:93-107`, `js-lease-server/src/job_api/submit.rs:264` | judged over this instance's registry: a host held by another instance is invisible, so a job it could run is refused for good | Designed (G5) |
| `ListHosts`, `GET /v1/hosts`, a job's `host_silent_since_ms` | `js-lease-server/src/hosts.rs:277`, `js-lease-server/src/job_api/read.rs:165` | this instance's registry: the answer depends on which instance is asked | Designed (G5) |
| Cordon, uncordon, clear taint through the API | see 3.2 | | Designed (G4) |
| Brake reset (`POST /v1/pools/{pool}/brake/reset`) | `js-lease-server/src/brake_api.rs:48`, `js-lease-server/src/scaler/mod.rs:199` | recorded in this instance's memory | Designed (G9) |
| Client tokens, rate and concurrency limits, the catalog cache, validation | `js-lease-server/src/job_api/auth.rs:34`, `js-lease-server/src/rate.rs`, `js-lease-server/src/catalogs.rs:57` | configuration, limiters and caches that only delay | Per instance |

### 3.6 Periodic work and the scaler

| Decision | Where | How it holds | Tag |
|---|---|---|---|
| The job pass (queue expiry, prepare expiry, retention, loss) runs once per period | `js-lease-server/src/job_pass.rs:179-180`, `js-jobs/src/store/duties.rs:42-58` | a due-time claim; each end in the pass is `Cond::Version` (`js-jobs/src/store/ending.rs:89-97`) | Built |
| The time the pass and the duty claims compare | `js-lease-server/src/job_pass.rs:168-169` | the process clock, against deadlines stamped with the store's clock: a skewed instance ends jobs early or late, or claims a period early | Designed (G2) |
| Times stored in host rows (join-token expiry, credential expiry, cordon end) | `js-lease-server/src/join_tokens.rs:450-455`, `:534-539` | stamped and judged with the process clock | Designed (G2) |
| A silent host's jobs are lost (settlement) | `js-lease-server/src/job_pass.rs:126` (`gone`), `:179` (the claim is skipped while `gone` is not empty), `js-lease-server/src/job_alloc.rs:140` | "absent" is this instance's registry forgetting the host; a host connected to another instance is never heard here, so this instance can lose its live work | Designed (G6) |
| `taint/<host>` rows are dropped once kept | `js-jobs/src/store/taints.rs:72` | `Cond::Any`: a taint row rewritten between the scan and the delete is lost | Designed (G4) |
| The scaler runs a step | `js-lease-server/src/server.rs:458`, `js-lease-server/src/scaler/mod.rs:189` | every instance with an elastic pool runs its own loop, with no claim | Designed (G9) |
| The scaler's records and unremoved hosts | `js-lease-server/src/scaler/kept.rs:51` | read at start, written whole at `Cond::Any`: two scalers overwrite each other | Designed (G9) |
| The scaler's caps and budgets | `js-scaler/src/budget.rs:88`, `js-scaler/src/caps.rs:97` | judged over this instance's records: N instances spend up to N times a cap | Designed (G9) |
| The scaler's view of hosts and its drain and release | `js-lease-server/src/scaler/live.rs:98`, `:222`, `:271-302` | this registry's hosts; an acquire id drawn per process | Designed (G9) |
| Alerts (webhook, log lines) | `js-lease-server/src/alerts.rs:38` | each instance's own; a duplicate notice, not a decision | Per instance |
| Metrics, TLS and token-file reload | `js-lease-server/src/metrics.rs`, `js-lease-server/src/agents.rs:838-870` | each instance's own | Per instance |

### 3.7 Usage

| Decision | Where | How it holds | Tag |
|---|---|---|---|
| A host-hour pack of minute rows | `js-usage/src/store.rs:101-106` | `Cond::Version` or `Cond::Absent`; a conflict keeps the minutes buffered | Built |
| The same host-minute flushed by two instances (a host that moved within the minute) | `js-usage/src/store.rs:108` | the later flush replaces the minute instead of adding to it | Designed (G11) |
| An ended allocation's history record | `js-usage/src/history.rs:348` | `Cond::Absent`, in the commit that frees the allocation | Built |
| The usage prune | `js-usage/src/hub.rs:448` | every instance prunes each day; each delete is `Cond::Version`, so duplicate work only | Designed (G11) |
| Profile curves' eviction | `js-usage/src/profile_store.rs:220` | victims deleted at `Cond::Any`; dormant (the server feeds it no results) | Designed (G11) |
| The minute buffer, counters, the usage mutex | `js-lease-server/src/usage_js.rs:193` | the process's own; losing one unflushed buffer when an instance dies is accepted (section 6) | Per instance |

## 4. Time

Rule 3 is not Built. The job rows are stamped with the store's clock (`js-jobs/src/store.rs:327`, `ending.rs:139`), but the pass,
the heartbeat and the host rows compare with the process clock (section 3.6), and `Store::now` answers the epoch when the database
cannot be read (section 3.1). G2 makes one clock:

- `Store::now` becomes fallible on Postgres (an added `try_now`, returning the store's error; SQLite and the fake answer their
  clocks as now). Every caller that stamps or compares a shared time takes it from `try_now` and fails the call, writing nothing,
  when it fails. That is rule 4 for time.
- The job pass, prepare expiry, the duty claims (`job_pass.rs:168-169`), the whole-host start (`agent_api.rs:608`), join-token and
  credential expiry and the cordon end (`join_tokens.rs:450-539`, `agents.rs:670`), and the whole-host mark's times
  (`js-sched-placer/src/jobs.rs:727`) take the store's time, once per pass or request.
- The process clock stays for intervals that are the process's own: ticks, backoffs, the hello limiter, the held beat.

One more round trip per pass or request, and none per heartbeat that decides nothing.

## 5. Hosts across instances

One host's connection is held by one instance at a time: an agent keeps one connection, and a beat that reaches another instance
is refused and the agent says hello there. The design keeps that (a session is the holder's cache), and puts in rows what the
other instances must know about the host:

- **The connection** (G1, komira-ai/jobscheduler#883): `node/<host>`, written by the hello at the version it read. Its version is
  the connection's id. Every write of a heartbeat's job listing goes through a store that also checks that row is still at that
  version (`js-state` `Checked`), so a replaced connection commits nothing and its beat is answered `403 session-mismatch`.
- **Presence** (G5): the same row gains optional fields, written by the holder: the host's pool, agent instance, its claim and
  shape as the hello reported them, whether its last listing was clean, and `heard_at`, the store's time of the last applied beat,
  refreshed at most every `presence_refresh` (a config key, default 5 s, below the grace). Each write goes through the same
  `Checked` store, so a superseded holder writes nothing. About one small write per host per 5 s, the rate the project owner
  accepted for the coordinator (komira-ai/hostkeeper's design, section 19, decision 4). This amends
  [jobscheduler-design.md](jobscheduler-design.md) section 10.1 ("a heartbeat is not a write"): a beat is still not a write;
  a coarse presence refresh is.
- **Identity** (G3, G4): `hosts/binding/<host>` and `hosts/join/<digest>` are read by key where a decision needs them, and every
  write is at the version read (`Cond::Version`, `Cond::Absent` for a new row). The registry and the token map stay as caches.

What reads presence:

- **Admission** (`no-capable-host`): the union of the live presence rows (heard within grace by the store's time) and this
  registry. A row read is a necessary condition, so a cache of the rows that is a little old is enough; the refusal is never
  taken on less than the rows.
- **Host listings and `host_silent_since_ms`**: the presence rows, with this registry decorating the hosts it holds.
- **`HostLive`**: at a hello whose agent instance differs from the row's, a row heard within grace refuses it as today. The same
  agent instance takes the host over at once, which is what #883 already does and what section 6 carries.
- **`PoolFull`**: the hello of a host new to the pool locks the pool's row `pool/<pool>` (a key read and written at
  `Cond::Version` in the hello transaction, so two such hellos of one pool are serialized) and counts the pool's live presence
  rows. A known host is admitted as today.
- **Settlement** (G6): the claimed job pass reads the presence rows. A host whose `heard_at` is older than grace plus retention by
  the store's time is lost: each of its live allocations is ended in a transaction that also checks `node/<host>` at the version
  read, so a hello that lands meanwhile makes the settlement a conflict and nothing is lost. The registry's `gone` map no longer
  decides, and the job pass no longer skips its claim for it (`job_pass.rs:179`). A host whose instance died is settled by
  whichever instance claims the pass, which is what an instance-local judgement cannot do.

## 6. Decisions already taken for js-server

Carried from komira-ai/hostkeeper's design, section 13:

| Decision | Here | Tag |
|---|---|---|
| No agent stickiness beyond TCP keep-alive plus a server-side connection max age | the agent listener closes a connection older than `agent_connection_max_age` (a config key with a default) at the end of a request, so agents spread over instances after a deploy | Designed (G10) |
| The job-store cutover is a short pause with a dry-run copy, and a Postgres-to-SQLite export for rollback | #874 shipped the first start over Postgres with an empty job store ("drain the farm") and no copy or export tool | Designed (G12) |
| A hello with the same credential and agent instance takes the host over at once | #883's connection row: the newest hello wins | Designed (G1) |
| Losing one unflushed usage buffer when an instance dies is accepted | the minute buffer flushes every few minutes (`js-usage/src/hub.rs:448`) | Built |

## 7. Compatibility (B8)

Every change of section 9 is additive. New key prefixes: `node/` (G1), `pool/` (G5); new optional fields in the `node/` value
(G5); new duty rows under `periodic/` (G9, G11); new scaler rows under `acquires/scaler/` for a launch (G9). No existing key is
renamed, re-meant or written in a new shape. The binding, join-token and scaler-record rows keep their values; what changes is
that they are written at the version read, which a reader does not see. An older release ignores the new prefixes.

Mixed releases: the deploy stays stop-first until section 9 is Built, so a release before a gap never runs beside the release that
closes it. After that, every release follows this rule, and a later change to a row this document adds is additive in the same way.
Each gap's pull request names its test that runs the previous release's store reader on the new rows (the existing
`exported_files` and skew tests are the pattern) where it adds a row, and says so where it adds none.

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

## 9. The gaps, one pull request each, in landing order

Each lands after this document, is one subject, keeps the deploy at one replica stop-first, and turns its rows of section 3 to
Built with paths.

| # | Pull request | Closes (section 3) | After |
|---|---|---|---|
| G1 | **Finish komira-ai/jobscheduler#883** (open, conflicts with `main` in `agent_api.rs`): the connection row `node/<host>` and the `Checked` store; merge `main` into its branch, keep its tests (`two_servers_serve_one_store_place_each_job_once_and_keep_to_the_hosts_cores` among them) | which connection commits a listing; first-listing loss; same agent takes over at once | none |
| G2 | **One clock**: `try_now`, the store's time for every shared deadline, expiry and due time (section 4) | `Store::now` epoch; the pass's clock; host-row times; the whole-host start's clock | none |
| G3 | **Join tokens and host credentials are rows**: `identify`, `registered_as` and `serves` read the token's row by key; a use is `Cond::Version` on the token row and `Cond::Absent` on a new credential row, in the hello's transaction; mint at `Cond::Absent`, replacement and purge at the scanned version; revoke by a scan of `hosts/join/`; renewal at the row's version, a conflict left to the next tick; the give-back at the version the admit wrote, skipped on conflict; the stale "one writer" comment in `host_keys.rs` goes | 3.2: recognised, used once, minted, revoked, renewed, given back | none |
| G4 | **Host bindings are rows**: `HostBound` judged on the binding row read in the hello's transaction (`Cond::Absent` for a first hello, `Cond::Version` otherwise), the registry record's taint and cordon taken from that row; `save_bindings` writes only what this instance changed, at the version read, and deletes nothing it did not explicitly forget; cordon, uncordon and clear taint read, change and write the row at its version and take the host from the row (a host held elsewhere is not `404`); `taint/<host>` dropped at the version scanned | 3.2: host bound, saved, taint seen, cordon, clear taint; `taint/` drop | G3 |
| G5 | **Presence in the connection row** (section 5): pool, agent instance, claim and shape, listing-clean, `heard_at`; `presence_refresh`; admission, host listings, `host_silent_since_ms`, `HostLive` and `PoolFull` read it; `docs/design/jobscheduler-design.md` section 10.1 amended | 3.3: `HostLive`, `PoolFull`, listing-clean; 3.5: admission, listings | G1, G2 |
| G6 | **Settlement by rows**: the claimed job pass loses a host's allocations by its presence row and `node/<host>` version, never by the local `gone` map; the claim bypass goes | 3.6: settlement | G5 |
| G7 | **Placement re-checks the host in its transaction**: `place_claimed` reads the host's binding (taint, cordon end by the store's time), `taint/<host>`, the scale-down mark and the ports of the host's live allocations in the claimed transaction and refuses the host; the scale-down mark becomes a row `hostmark/<host>`, written at `Cond::Version` by whoever marks, the registry mark its cache | 3.4: contained beside host job, taint, cordon, ports, scale-down mark | G4, G5 |
| G8 | **Whole-host drain and queue notes without churn**: a whole-host mark held for a host live on another instance is kept until its stored deadline by the store's time (no `claim_by` from the process's start); the queue note's text is the same whatever the instance sees | 3.4: which host drains; queue notes | G2 |
| G9 | **The scaler under the rule**: one step per pool and period by a duty claim (`periodic/scaler/<pool>`); its records and unremoved set written at the row's version, a conflict re-reading and re-deciding the step; each launch a row at `Cond::Absent` before the provider call, its acquire id derived from that row so a repeated call is the same machine; caps and budgets judged in the transaction that writes the launch row, under the pool's scaler row lock; the brake a row, its reset a conditional write; a release written to `hostmark/<host>` before the provider release | 3.6: scaler step, records, caps, host view; 3.5: brake reset | G2, G7 |
| G10 | **Agent connection max age** (section 6) | section 6 | none |
| G11 | **Economy**: the import's loser reads the marker and starts; a host-minute stored twice is added to, not replaced (CPU summed, peaks by max); the usage prune claimed by `periodic/usage-prune`; profile eviction at the scanned version; the stale "one writer" doc in `js-usage/src/store.rs` | 3.1: import; 3.7: minute, prune, profiles | G2 |
| G12 | **Job-store cutover tools** (section 6): a dry-run copy of `<state file>.jobs` into the store with counts and a checksum per prefix, and an export of the store's job and usage rows to a SQLite file, with the round trip as its test. Moot for a deployment whose job store is already in Postgres with nothing to bring back | section 6 | none |

After G1 to G11, a start-first update with two overlapping instances is safe from this repository's side; the deploy change is
outside it.

## 10. Supersession (B22)

- This text replaces the earlier draft of this file (komira-ai/jobscheduler#873, never merged), which designed presence rows,
  a `hostbook` gate and a placement lock order of its own before #877 to #881 built placement and the duties differently. What
  it found that still holds is in section 3 and section 9.
- [deployment.md](deployment.md), "(b) later" and "Single active writer, not leader election": a second replica needs no leader,
  no election and no fence; this document is the design. That file gets a pointer here.
- [jobscheduler-design.md](jobscheduler-design.md) section 10.1, "a heartbeat is not a write", is amended by G5 in its pull request.
- [configuration.md](configuration.md), "What it keeps": "read once at start" for the bindings and tokens becomes untrue with G3 and
  G4, which change that text.

## 11. Decisions made in this document

- **Settlement needs a presence row.** Settling by `node/<host>`'s version alone would leave a host whose instance died unsettled
  forever, since no live instance ever heard it. So `heard_at` is written, at the 5 s rate the owner accepted for the coordinator.
- **`PoolFull` takes a row lock per pool, and only for a host new to the pool.** That serializes exactly the hellos that can
  overrun the cap, with no counter to keep in step.
- **The update hook stays a one-server tool.** It manages the host-native install of `docs/lease-server-update.md`; an
  N-instance deploy is the orchestrator's start-first update, not the hook's.
- **No separate model checker.** Every gap is one row's race, and its test runs that race against real Postgres with barriers.
  The protocol-level model of the coordinator (its section 14) covers the gate and the decisions across services, which js-server
  does not have.
- **G12 stays Designed** although #874 shipped without it, because the owner's decision asks for it. A deployment that has no
  SQLite job store to move needs only the export.

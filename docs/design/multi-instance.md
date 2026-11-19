# js-server as N instances on one Postgres

Status: design. A living document: each pull request that builds a part of it turns that part's tag from **Designed** to **Built**
and names the code. **Built** names code on `main` with a path and line; **Designed** means nothing on `main` implements it yet, and
says which pull request of section 9 does. Line numbers are those of `main` as last merged into this document's branch; a later
change moves them, and the pull request that moves a decision's code updates its row.

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
     its commit: `js-state/src/postgres/tx.rs:1-25`; `Store::claim_decide` takes the commit head first and then locks the claimed
     row `FOR UPDATE SKIP LOCKED`, deciding under both: `js-state/src/postgres/claim.rs:98`, `session.rs:155-157`);
   - a **conditional write**: `Cond::Version` (a compare-and-set on the version read) or `Cond::Absent` (a unique key); the
     `Conflict` says who lost;
   - a **claim by advancing a due time** for periodic work (`JobStore::claim_duty`, `js-jobs/src/store/duties.rs:42`): the
     instance whose advance of the duty's row commits runs it for the period.
   No step elects, leases, counts instances or relies on being the only process. An instance that dies at any point leaves
   no lock or lease that another must clear by a timeout: its transaction committed or it did not. (A host that falls silent
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

The merged pull requests komira-ai/jobscheduler#874, #877, #878, #879, #880, #881, #923 and #932 put the store, the job tables,
placement, the periodic duties and the cordon under the rule:

- **No writer lock and no fence** (#878). A Postgres store takes no lock at start and checks nothing in a commit;
  `writer_generation` is unread (`docs/design/configuration.md`, "No lock on Postgres"; `docs/runbooks/state-database.md`).
- **The job tables and usage rows live in the store** (#874), so every instance reads the same jobs.
- **A placement claims its job and books its host in one transaction** (#877, #879).
- **A pass re-reads the tables after any instance's commit** (#880), judged by the store's head revision.
- **The job pass and the feed trim are duties claimed per period** (#881).
- **A start that loses a race at the open retries** (#923).
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
| The first start imports the files beside the state file once | `js-lease-server/src/store/import.rs:89-90` | marker and rows `Cond::Absent` in one transaction, so one import wins; the loser's `Conflict` refuses its start (exit 2) instead of reading the marker | Built (correctness); Designed (G11: the loser starts) |
| Row expiry (TTL) | `js-state/src/postgres/tx.rs:146`, `:289` | judged on the database clock inside each transaction | Built |
| Feed trim | `js-lease-server/src/job_pass.rs:227`, `js-lease-server/src/job_store/feed_trim.rs:44` | a duty claimed per period; a trim by absolute revision is idempotent | Built |
| An unreachable database | `js-lease-server/src/job_pass.rs:183`, `:210`, `js-lease-server/src/store_health.rs` | every store call fails and nothing is decided; a hello's or taint's write answers `503`, a listing's commit `500` `state-write-failed` (`js-lease-server/src/agent_api.rs:338`, `:645-649`), both retried by the agent | Built |
| `Store::now` when the database cannot be read | `js-state/src/postgres/mod.rs:920-930` | answers `StoreTime::ZERO` ("the store trait cannot report a failure here"); the `js-jobs` writes that stamp rows from it (`js-jobs/src/store.rs:330`, `:474`, `ending.rs:62`) stamp the epoch until their own write fails | Designed (G2) |
| Readiness, drain and shutdown | `js-lease-server/src/ready.rs:26`, `js-lease-server/src/server.rs:309` | one instance's own phases (#876); nothing waits on another. The route is served on a loopback address only, so the edge cannot see a drain (section 12) | Per instance; Designed (G13: the edge sees it) |
| The host-native update hook's "is a server running" probe | `js-state/src/writer.rs:128`, `js-state/src/postgres/serving.rs:21`, `js-lease-server/src/update_hook/step/mod.rs` | reads `pg_stat_activity` by the server's application name; it serves the one-server install of `docs/lease-server-update.md`, not an N-instance deploy (which is the orchestrator's start-first update) | Per instance |

### 3.2 Hosts: identity, admission, taint, cordon

| Decision | Where | How it holds | Tag |
|---|---|---|---|
| A host proves who it is | `js-lease-server/src/host_cert.rs:86` (`host_of`), `js-lease-server/src/client_verifier.rs:29`, `js-lease-server/src/agent_api/host_cert.rs:34` (`admit`), `js-lease-server/src/agent_api.rs:177`, `:264` | the host certificate hostkeeper issues to an enrolled host, verified against `--agent-client-ca`; the host id is its one DNS SAN (#922). A function of the certificate and the CA, so every instance answers alike | Built |
| A host without a certificate joins by a join token or a pool token and is given a host credential | `js-lease-server/src/join_tokens.rs:351` (read once at start), `:431` (`identify`), `:526` (`admit_writing`), `:450-470` (mint and its cap), `:662` (revoke), `:724` (renewal), `:637` (give-back), `js-lease-server/src/host_keys.rs:45-55`, `js-lease-server/src/agent_tokens.rs:50` | the token map is read at start and decides from memory, and every use, mint, revoke, renewal and give-back is committed at `Cond::Any` (the comment there still says "the server is the store's one writer"): a token minted or revoked on A is unknown to B, and two instances can admit two hosts with one token | Designed (G3: removed) |
| A host id is bound to the agent instance of its first hello (`HostBound`) | `js-registry/src/registry.rs:460-467` | judged on the local registry and the bindings read at start; the binding written at `Cond::Any` | Designed (G4) |
| A refused hello undoes its binding | `js-lease-server/src/agent_state.rs:411-420` | the undo is built from this instance's copy and written at `Cond::Any` | Designed (G4) |
| Hellos of one host are serialized | `js-lease-server/src/agent_api.rs:277` (`host_writes`) | a process mutex; correct across instances only once G3 removes the token writes and G4 makes the binding writes conditional; then it is an economy | Per instance (after G3, G4) |
| The bindings are saved | `js-lease-server/src/agents.rs:837` (`save_bindings`), `js-lease-server/src/agent_state.rs:286-297` (`writes_to`) | makes the store equal this instance's registry: puts every differing binding and **deletes every binding this registry no longer holds**, at `Cond::Any`; B's tick can delete a binding live on A | Designed (G4) |
| A taint a heartbeat reports is stored | `js-lease-server/src/agent_state.rs:478` (`commit_taint`) | `Cond::Version` of the row read, after checking the stored taint is the expected one | Built |
| Every instance sees a host's taint | `js-lease-server/src/agents.rs:297` (`restore_job_taints`, at start), `js-lease-server/src/sched/jobs_pass.rs:69` (the placer's own registry), `:158` (the hosts filter) | the registry's taint decides placement and is loaded at start only; a host that says hello to B after A stored its taint is clean in B | Designed (G4, G7) |
| A cordon and its lift | `js-lease-server/src/agents.rs:606` (`keep`), `js-lease-server/src/agents_cordon.rs:141` (`cordon`), `:212` (`mirror_cordons`), `js-jobs/src/store/cordons.rs:107` (`cordon_host`), `js-jobs/src/store/whole.rs:191`, `:291` | with the job service on, the cordon is the row `cordon/<host>` (put with a TTL), decided in the store at the version read (`Cond::Absent` or `Cond::Version`; a lost race is `CordonConflict`, answered `503 cordon-unsaved`), and every placement transaction checks it absent, so a cordon placed on any instance keeps every instance's placement off the host and it lapses by the store's clock; each tick puts the rows on the registry (`mirror_cordons`), so a lift made on B reaches A's listing, scaler and probes at A's next tick. What stays per instance: the binding's cordon end is still rebuilt from the local registry and put at `Cond::Any` (`keep_cordon`), a host held by another instance answers `404` (`held_cordon` reads the local registry), and without the job service the registry decides in memory | Built for placement; Designed (G4) for the binding and the `404` |
| An operator clears a host's taint | `js-lease-server/src/agents_taints.rs:86` | the write is conditional (`commit_taint`), but the host must be in this registry: a host held elsewhere answers `404` (fails closed) | Designed (G4) |
| Host certificate admission (`--agent-client-ca`) | `js-lease-server/src/host_cert.rs:86`, `js-lease-server/src/client_verifier.rs:29` | a function of the certificate and the configured CA | Per instance |
| The hello rate limit | `js-lease-server/src/agents.rs:69` | a limiter of this process | Per instance |

### 3.3 The session and the heartbeat

| Decision | Where | How it holds | Tag |
|---|---|---|---|
| Which connection of a host commits its job listing | `js-lease-server/src/agent_api.rs:580-614` (`per_job::serve`, `job_whole::serve`, `lose_unlisted` over the plain job store) | nothing orders two connections: an old connection on A can commit a listing over B's newer one, freeing an allocation the host holds or losing a job it lists | Designed (G1: komira-ai/jobscheduler#883) |
| A host's first listing after a hello loses the jobs it does not list | `js-lease-server/src/agents.rs:466` (`first_listing`, memory), `js-lease-server/src/agent_api.rs:594`, `js-lease-server/src/job_loss.rs:96-107` | each end is `Cond::Version`, but "first" is this instance's flag; safe once only the newest connection writes | Designed (G1) |
| A host's session, epoch, secret, last heard | `js-registry/src/registry.rs:474-525`, `js-registry/src/registry/beat.rs:123` | the instance holding the connection's cache; a beat that reaches another instance is refused (`409 unknown-host` or `403 session-mismatch`) and the agent says hello there | Per instance |
| `HostLive` (a second agent process on a live host) | `js-registry/src/registry.rs:468-473` | judged on the local registry: a different agent process that says hello to B while the host is live on A is admitted | Designed (G5) |
| The pool's machine cap (`PoolFull`) | `js-registry/src/registry.rs:506-515` | counts this registry's hosts: N instances admit N times the cap | Designed (G5) |
| Whole-host job states (placed, starting, running, prepare deadline) | `js-jobs/src/store/whole.rs:354`, `:359`, `:396`, `js-lease-server/src/job_whole.rs:138` | rows written at `Cond::Version` | Built |
| The whole-host start's taint and the clock it judges deadlines by | `js-lease-server/src/agent_api.rs:531` (`own.host.taint`), `:608` (`placing.clock.now()`) | the registry's taint and the process clock | Designed (G2, G4) |
| A job's start command, repeated by two instances | `js-lease-server/src/per_job.rs:165`, `js-lease-server/src/job_start.rs:120` | a pure function of the rows; the placement that creates it is claimed | Built |
| Listing-clean (a host whose last listing was not clean takes no job) | `js-lease-server/src/agents.rs:495` (`note_listing`, memory), `:507` (`listing_clean`), `js-lease-server/src/sched/jobs_pass.rs:158` | another instance's placement does not know | Designed (G5, G7) |
| Held beats and their wakes | `js-lease-server/src/held_beat.rs:36` | the process's own wakes; a placement by another instance goes out on the host's next beat | Per instance |
| Log upload credentials per host | `js-lease-server/src/logs/mod.rs:155-307` | stateless provider credentials; several valid at once; a flip costs a mint | Per instance |

### 3.4 Placement and the queue

| Decision | Where | How it holds | Tag |
|---|---|---|---|
| A queued job is placed by exactly one instance | `js-jobs/src/store/whole.rs:128-132`, `:286` | `claim_decide` on `job/<id>` (`SKIP LOCKED`), the job written at `Cond::Version` | Built |
| A host is never booked past its room | `js-jobs/src/store/whole.rs:176-209` | the host's live allocations summed in the claimed transaction, `HostFull` when they leave no room, `hostbook/<host>` written at `Cond::Version` | Built |
| One host job (`isolation=host`) per host | `js-jobs/src/store/whole.rs:289`, `js-jobs/src/store/taints.rs:36` | `taint/<host>` put at `Cond::Absent` in the placing transaction | Built |
| No job on a cordoned host | `js-jobs/src/store/whole.rs:190-196` (`place_claimed`), `:290-291` (every placement), `js-jobs/src/store/cordons.rs:168` (`cordons`), `js-sched-placer/src/jobs.rs:708-713` | the row `cordon/<host>`, decided at the version read (`cordons.rs`); every placement transaction checks it absent (`HostCordoned` in `place_claimed`, a conflicting commit in `place` and `place_with`), whichever instance took the cordon; each pass also drops the hosts whose cordon the store holds from the open hosts | Built |
| No contained job beside a host job, no job on a tainted host | `js-sched-placer/src/jobs.rs:931-941` (`open_to_jobs`, registry view), `js-lease-server/src/sched/jobs_pass.rs:158` | the engine's memory only; the placing transaction reads neither the host's taint nor `taint/<host>` (it puts the row for a host job only), and `taint/<host>` is later deleted at `Cond::Any` (`js-jobs/src/store/taints.rs:72`) | Designed (G7) |
| A host port is held by one job per host | `js-sched-placer/src/jobs.rs:472` | the engine's memory only; the placing transaction does not compare ports | Designed (G7) |
| A host the scaler drains or releases takes no new job | `js-registry/src/registry/marks.rs:1-25` ("the marks are held in memory only") | a mark in one instance's registry; another instance's placement does not see it | Designed (G7, G9) |
| One mark row per whole-host job | `js-jobs/src/store/marks.rs:81-94` | `mark/<job>` at `Cond::Absent` or `Cond::Version`, with the job checked at its version | Built |
| Which host a whole-host job drains | `js-sched-placer/src/whole_jobs.rs:204-210`, `js-sched-placer/src/jobs.rs:741` | each instance chooses over its own hosts and deletes a mark whose host it never heard after its own start plus grace: two instances drain two hosts in turn (the rows stay consistent; the work is wasted) | Designed (G8) |
| Why a job waits (queue status note) | `js-sched-placer/src/jobs.rs:779`, `js-jobs/src/store/status.rs:38` | conditional, but its text depends on the instance's view, so two instances rewrite it in turn and each rewrite moves the head | Designed (G8) |
| The tables a pass decides from | `js-sched-placer/src/jobs_seen.rs:192` | a cache judged by the store's head (#880); every placement is re-decided in its transaction | Built |
| Which hosts an instance places on | `js-lease-server/src/sched/jobs_pass.rs:149` | the hosts whose connection it holds; each host is held by one instance and every instance reads the whole queue | Per instance |
| Queue order | `js-jobs/src/store/queue.rs:20` | a total order of row columns, the same on every instance | Built |
| Job sizing | `js-sched-placer/src/sizes.rs:545` | from the shared tables; the booking is checked in the transaction | Per instance |

### 3.5 The job API

| Decision | Where | How it holds | Tag |
|---|---|---|---|
| A client-chosen job id is recorded once | `js-jobs/src/store.rs:364-375` | `Cond::Absent`; a conflict answers from the stored job | Built |
| A minted job id is unique | `js-jobs/src/store.rs:390-399` | random ids, `Cond::Absent`, retried on a collision | Built |
| A cancel racing a placement on another instance | `js-jobs/src/store.rs:542-561` | `Cond::Version`; the loser writes nothing | Built |
| Job reads, results, logs | `js-lease-server/src/job_api/read.rs:220`, `js-lease-server/src/job_api/logs.rs:239-249` | read from the rows; a follow polls the row, no local wake decides | Built |
| A submit is refused `no-capable-host` | `js-lease-server/src/admit.rs:93-107`, `js-lease-server/src/job_api/submit.rs:264` | judged over this instance's registry: a host held by another instance is invisible, so a job it could run is refused for good | Designed (G5) |
| `ListHosts`, `GET /v1/hosts`, a job's `host_silent_since_ms` | `js-lease-server/src/hosts.rs:283`, `js-lease-server/src/job_api/read.rs:165` | this instance's registry: the answer depends on which instance is asked | Designed (G5) |
| Cordon, uncordon, clear taint through the API | see 3.2; `js-lease-server/src/agents_cordon.rs:60-63`, `:80`, `:112` | the cordon and its lift are decided on the store row `cordon/<host>` at the version read (`jobs.cordon_host`, `jobs.uncordon_host`), so a cordon holds on every instance; but the host must be in this instance's registry (`held_cordon` answers `NotHeld`, `404`, for a host held elsewhere), and clear taint is unchanged | Designed (G4) |
| Brake reset (`POST /v1/pools/{pool}/brake/reset`) | `js-lease-server/src/brake_api.rs:48`, `js-lease-server/src/scaler/mod.rs:199` | recorded in this instance's memory | Designed (G9) |
| Client tokens, rate and concurrency limits, the catalog cache, validation | `js-lease-server/src/job_api/auth.rs:34`, `js-lease-server/src/rate.rs`, `js-lease-server/src/catalogs.rs:57` | configuration, limiters and caches that only delay | Per instance |

### 3.6 Periodic work and the scaler

| Decision | Where | How it holds | Tag |
|---|---|---|---|
| The job pass (queue expiry, prepare expiry, retention, loss) runs once per period | `js-lease-server/src/job_pass.rs:179-180`, `js-jobs/src/store/duties.rs:42-58` | a due-time claim; each end in the pass is `Cond::Version` (`js-jobs/src/store/ending.rs:89-97`) | Built |
| The time the pass and the duty claims compare | `js-lease-server/src/job_pass.rs:168-169` | the process clock, against deadlines stamped with the store's clock: a skewed instance ends jobs early or late, or claims a period early | Designed (G2) |
| Times stored in host rows (join-token and credential expiry, which G3 removes; the binding's cordon end) | `js-lease-server/src/join_tokens.rs:450-455`, `:534-539`, `js-lease-server/src/agents_cordon.rs:127` | stamped and judged with the process clock; with the job service the cordon itself is now a store row `cordon/<host>` whose time to live is the store's (`js-jobs/src/store/cordons.rs:123-128`), so only the binding's copy of the end (`agents_cordon.rs:127`) and the registry's cordon use the process clock | Designed (G2; G3 for the token times) |
| A silent host's jobs are lost (settlement) | `js-lease-server/src/job_pass.rs:126` (`gone`), `:179` (while `gone` is not empty the claim is bypassed and the pass runs regardless), `js-lease-server/src/job_alloc.rs:140` | "absent" is this instance's registry forgetting the host; a host connected to another instance is never heard here, so this instance can lose its live work | Designed (G6) |
| `taint/<host>` rows are dropped once kept | `js-jobs/src/store/taints.rs:72` | `Cond::Any`: a taint row rewritten between the scan and the delete is lost | Designed (G4) |
| The scaler runs a step | `js-lease-server/src/server.rs:458`, `js-lease-server/src/scaler/mod.rs:189` | every instance with an elastic pool runs its own loop, with no claim | Designed (G9) |
| The scaler's records and unremoved hosts | `js-lease-server/src/scaler/kept.rs:51` | read at start, written whole at `Cond::Any`: two scalers overwrite each other | Designed (G9) |
| The scaler's caps and budgets | `js-scaler/src/budget.rs:88`, `js-scaler/src/caps.rs:97` | judged over this instance's records: N instances spend up to N times a cap | Designed (G9) |
| The scaler's view of hosts and its drain and release | `js-lease-server/src/scaler/live.rs:98`, `:222`, `:271-302` | this registry's hosts; an acquire id drawn per process | Designed (G9) |
| Alerts (webhook, log lines) | `js-lease-server/src/alerts.rs:38` | each instance's own; a duplicate notice, not a decision | Per instance |
| Metrics, TLS and token-file reload | `js-lease-server/src/metrics.rs`, `js-lease-server/src/agents.rs:789-826` (`refresh`: token and client-token reload, `mirror_cordons`, `save_bindings`; the TLS reload is in `js-lease-server/src/tls.rs`, not in `agents.rs`) | each instance's own | Per instance |

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

Rule 3 is not Built. The job rows are stamped with the store's clock (`js-jobs/src/store.rs:330`, `ending.rs:139`), but the pass,
the heartbeat and the host rows compare with the process clock (section 3.6), and `Store::now` answers the epoch when the database
cannot be read (section 3.1). G2 makes one clock:

- `Store::now` becomes fallible on Postgres (an added `try_now`, returning the store's error; SQLite and the fake answer their
  clocks as now). Every caller that stamps or compares a shared time takes it from `try_now` and fails the call, writing nothing,
  when it fails. That is rule 4 for time.
- The job pass, prepare expiry, the duty claims (`job_pass.rs:168-169`), the whole-host start (`agent_api.rs:608`) and the
  binding's cordon end (`agents_cordon.rs:127`) take the store's time, once per pass or request. (Join-token and credential expiry,
  `join_tokens.rs:450-539`, go with G3.) The whole-host mark's `claim_by` (`js-sched-placer/src/jobs.rs:741`) is replaced by G8, on this clock.
- The process clock stays for intervals that are the process's own: ticks, backoffs, the hello limiter, the held beat.

One more round trip per pass or request, and none per heartbeat that decides nothing.

**Load.** Two costs grow with the fleet, not with the instances. Every claimed decision (a placement) holds the commit head from
its start (3.1), so the placements of all instances are serialized there; each holds it for one job's rows, and more instances
spread the same placements without adding any. Presence (section 5) is one small write per host per `presence_refresh`: at the
default 5 s, 100 hosts are 20 writes a second. Neither is measured here; G5's pull request measures the presence write at the
deployment's host count, and `js-state/tests/postgres_head_queue.rs` exercises writers queued on the head.

## 5. Hosts across instances

One host's connection is held by one instance at a time: an agent keeps one connection, and a beat that reaches another instance
is refused and the agent says hello there. The design keeps that (a session is the holder's cache), and puts in rows what the
other instances must know about the host:

- **The connection** (G1, komira-ai/jobscheduler#883): `node/<host>`, written by the hello at the version it read. Its version is
  the connection's id. Every write of a heartbeat's job listing goes through a store that also checks that row is still at that
  version (`js-state` `Checked`), so a replaced connection commits nothing and its beat is answered `403 session-mismatch`.
- **Presence** (G5): the same row gains optional fields, written by the holder: the host's pool, agent instance, its claim and
  shape as the hello reported them, whether its last listing was clean, and `heard_at`, the store's time of the last applied beat,
  refreshed at most every `presence_refresh` (a config key, default 5 s; it must stay below half the shortest
  pool grace, which the server checks once at start, B20, not at run time). Each write goes through the same
  `Checked` store, so a superseded holder writes nothing. About one small write per host per 5 s, the rate the project owner
  accepted for the coordinator (komira-ai/hostkeeper's design, section 19, decision 4). This amends
  [jobscheduler-design.md](jobscheduler-design.md) section 10.1 ("a heartbeat is not a write"): a beat is still not a write;
  a coarse presence refresh is.
- **Identity** (G3, G4): a host is its certificate (G3 removes the tokens and host credentials). `hosts/binding/<host>` is read
  by key in the hello's transaction and written at the version read (`Cond::Version`, `Cond::Absent` for a new row); the registry
  is its cache. js-server keeps no per-host secret in a row: the session secret is the holding instance's, made at each hello, and
  a log upload credential is minted per instance from the host id (section 3.3).

What reads presence:

- **Admission** (`no-capable-host`): the union of the live presence rows (heard within grace by the store's time) and this
  registry. A row read is a necessary condition, so a cache of the rows that is a little old is enough; the refusal is never
  taken on less than the rows.
- **Host listings and `host_silent_since_ms`**: the presence rows, with this registry decorating the hosts it holds.
- **`HostLive`**: at a hello whose agent instance differs from the row's, a row heard within grace refuses it as today. The same
  agent instance takes the host over at once, on every instance: today's refusal of any hello while the local record is live
  (`js-registry/src/registry.rs:468-473`) is narrowed to a different agent instance, and #883's row makes the newest hello the
  one whose connection writes. That is what section 6 carries.
- **`PoolFull`**: the hello of a host new to the pool locks the pool's row `pool/<pool>` (a key read and written at
  `Cond::Version` in the hello transaction, so two such hellos of one pool are serialized) and counts the pool's live presence
  rows. A known host is admitted as today.
- **Settlement** (G6): the claimed job pass reads the presence rows. A host whose `heard_at` is older than grace plus retention by
  the store's time is lost: each of its live allocations is ended in a transaction that also checks `node/<host>` at the version
  read, so a hello that lands meanwhile makes the settlement a conflict and nothing is lost. The registry's `gone` map no longer
  decides, and the job pass no longer bypasses its claim for it (`job_pass.rs:179`). A host whose instance died is settled by
  whichever instance claims the pass, which is what an instance-local judgement cannot do. G6's tests: a host whose holding
  instance stops is settled by the other after grace plus retention on an injected store clock, and a hello landing between the
  read and the end makes the settlement a conflict that loses nothing.

## 6. Decisions already taken for js-server

Carried from komira-ai/hostkeeper's design, section 13:

| Decision | Here | Tag |
|---|---|---|
| A host enrolled in hostkeeper holds one per-host certificate the coordinator controls; no join tokens, no token passing, no per-service certificates, no revocation machinery (a compromised host is unplugged) | the certificate is the identity of every agent request (#922); G3 deletes the token and host-credential paths instead of making them N-safe | Built (the certificate); Designed (G3, the removal) |
| No agent stickiness beyond TCP keep-alive plus a server-side connection max age | the agent listener closes a connection older than `agent_connection_max_age` (a config key with a default) at the end of a request, so agents spread over instances after a deploy | Designed (G10) |
| The job-store cutover is a short pause with a dry-run copy, and a Postgres-to-SQLite export for rollback | #874 shipped the first start over Postgres with an empty job store ("drain the farm") and no copy or export tool | Designed (G12) |
| A hello with the same credential and agent instance takes the host over at once | #883's connection row makes the newest hello the writer (G1); the registry's `HostLive` still refuses a hello of the same agent while its local record is live, which G5 narrows | Designed (G1, G5) |
| Losing one unflushed usage buffer when an instance dies is accepted | the minute buffer flushes every few minutes (`js-usage/src/hub.rs:448`) | Built |

## 7. Compatibility (B8)

Every change of section 9 is additive but G3's. New key prefixes: `node/` (G1; beside, and not under, the usage rows' `usage/node/`),
`pool/` (G5), `hostmark/` (G7); new optional fields in the `node/` value (G5) and a stored deadline in the `mark/<job>` value (G8);
new duty rows under `periodic/` (G9, G11); a usage row per instance beside a host's hour pack (G11; a release before reads the pack alone and misses only the overlap's minutes, as today); new scaler rows under `acquires/scaler/` for a launch and for a pool's brake (G9). No
existing key is renamed, re-meant or written in a new shape. The binding and scaler-record rows keep their values; what changes is
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

Each lands after this document, is one subject, changes neither the replica count nor the update order, and turns its rows of section 3 to Built with paths.
They land in the order of section 12's "Order of safety" (the gaps an update's overlap reaches today first), then by number.

| # | Pull request | Closes (section 3) | After |
|---|---|---|---|
| G1 | **Finish komira-ai/jobscheduler#883** (open, conflicts with `main` in `agent_api.rs`): the connection row `node/<host>` and the `Checked` store; merge `main` into its branch, keep its tests (`two_servers_serve_one_store_place_each_job_once_and_keep_to_the_hosts_cores` among them) | which connection commits a listing; first-listing loss; same agent takes over at once | none |
| G2 | **One clock**: `try_now`, the store's time for every shared deadline, expiry and due time (section 4) | `Store::now` epoch; the pass's clock; host-row times; the whole-host start's clock | none |
| G3 | **Host identity is the host certificate**: delete the one-time join tokens (their mint, revoke and use-once paths, the cap of waiting tokens, the give-back of a refused hello), the pool tokens of `--agent-token-file`, and the host credentials a welcome issues and the tick renews; an agent request without a host certificate is refused; the agent's token and credential flags go. Nothing replaces them: the certificate names the host (#922), and no per-host secret is kept in a row (section 5). The scaler's mint and revoke of a launched machine's token (`js-lease-server/src/scaler/live.rs:209`, `:259`) go too, while the scaler is off; G9 launches machines with certificates. It lands once every agent presents a certificate: Linux hosts do (#922), the Macs do not yet, so until then the token hazards of section 12 stay live for the Macs | 3.2: a host without a certificate; 3.6: the token times | none |
| G4 | **Host bindings are rows**: `HostBound` judged on the binding row read in the hello's transaction (`Cond::Absent` for a first hello, `Cond::Version` otherwise), the registry record's taint taken from that row; a refused hello judged before its binding is written, so there is nothing to undo; `save_bindings` writes only what this instance changed, at the version read, and deletes nothing it did not explicitly forget, the binding's cordon end included; cordon, uncordon and clear taint take the host from the rows, so a host held by another instance is not `404` (`js-lease-server/src/agents_cordon.rs:60-63`, `agents_taints.rs:86`); `taint/<host>` dropped at the version scanned | 3.2: host bound, undo, saved, taint seen, cordon's `404`, clear taint; 3.5: cordon and clear taint through the API; `taint/` drop | G3: the hello's binding write shares one transaction with the token use until G3 removes it, so G4 makes the write conditional once it is the binding's alone |
| G5 | **Presence in the connection row** (section 5): pool, agent instance, claim and shape, listing-clean, `heard_at`; `presence_refresh`; admission, host listings, `host_silent_since_ms`, `HostLive` and `PoolFull` read it; `docs/design/jobscheduler-design.md` section 10.1 amended | 3.3: `HostLive`, `PoolFull`, listing-clean; 3.5: admission, listings | G1, G2 |
| G6 | **Settlement by rows**: the claimed job pass loses a host's allocations by its presence row and `node/<host>` version, never by the local `gone` map; the claim bypass goes | 3.6: settlement | G5 |
| G7 | **Placement re-checks the host in its transaction**: `place_claimed` reads the host's binding (taint; the cordon is Built, see below), `taint/<host>`, the scale-down mark and the ports of the host's live allocations in the claimed transaction and refuses the host; the scale-down mark becomes a row `hostmark/<host>`, written at `Cond::Version` by whoever marks, the registry mark its cache | 3.4: contained beside host job, taint, ports, scale-down mark (the cordon check is Built: `js-jobs/src/store/whole.rs:190-196`, `:290-291`) | G4, G5 |
| G8 | **Whole-host drain and queue notes without churn**: a whole-host mark held for a host live on another instance is kept until its stored deadline by the store's time (no `claim_by` from the process's start); the queue note's text says whether any live host could serve the job by the presence rows, not by this instance's hosts | 3.4: which host drains; queue notes | G2, G5 |
| G9 | **The scaler under the rule**: one step per pool and period by a duty claim (`periodic/scaler/<pool>`); its records and unremoved set written at the row's version, a conflict re-reading and re-deciding the step; each launch a row at `Cond::Absent` before the provider call, its acquire id derived from that row so a repeated call is the same machine; caps and budgets judged in the transaction that writes the launch row, under the pool's scaler row lock; the brake a row, its reset a conditional write; a release written to `hostmark/<host>` before the provider release; a machine it launches joins with a host certificate, since G3 removed the launch token, so the scaler stays off until a launched machine can be enrolled, as it is off on the deployment today | 3.6: scaler step, records, caps, host view; 3.5: brake reset | G2, G3, G7 |
| G10 | **Agent connection max age** (section 6) | section 6 | none |
| G11 | **Usage rows and the import**: the import's loser reads the marker and starts; a host-minute flushed by two instances keeps both: each instance's minutes are kept in a row of its own beside the hour pack (a new key under the host's hour, named by the instance's start), and a reader sums the hour pack and those rows, so a flush retried after a lost reply replaces its own row and never counts twice; the usage prune claimed by `periodic/usage-prune`; profile eviction at the scanned version; the stale "one writer" doc in `js-usage/src/store.rs` | 3.1: import; 3.7: minute, prune, profiles | G2 |
| G12 | **Job-store cutover tools** (section 6): a dry-run copy of `<state file>.jobs` into the store with counts and a checksum per prefix, and an export of the store's job and usage rows to a SQLite file, with the round trip as its test. Moot for a deployment whose job store is already in Postgres with nothing to bring back | section 6 | none |
| G13 | **Readiness the edge can reach** (section 12): `--ready-listen` accepts a private address as the agent and job listeners do (`js-lease-server/src/config.rs:417`, `js-lease-server/src/config/listen.rs:55`, `:79`), so the edge checks `/readyz` and sends no new connection to an instance from its first `503 draining`; the stop grace above `--drain-hold` plus `--drain-timeout` is checked once at deploy (B20), outside this repository | 3.1: readiness; section 12 | none |

After G1 to G11 and G13, any number of instances is safe in steady state and through an update's overlap from this repository's
side; the edge's check and the stop grace are the deployment's.

## 10. Supersession (B22)

- This text replaces the earlier draft of this file (komira-ai/jobscheduler#873, never merged), which designed presence rows,
  a `hostbook` gate and a placement lock order of its own before #877 to #881 built placement and the duties differently. What
  it found that still holds is in section 3 and section 9.
- [deployment.md](deployment.md), "(b) later" and "Single active writer, not leader election": a second replica needs no leader,
  no election and no fence; this document is the design. That file gets a pointer here.
- [jobscheduler-design.md](jobscheduler-design.md) section 10.1, "a heartbeat is not a write", is amended by G5 in its pull request.
- [configuration.md](configuration.md), "What it keeps": G3 removes the tokens and host credentials from that text and G4 makes
  "read once at start" untrue for the bindings; each changes it.
- [../lease-server-update.md](../lease-server-update.md) describes the host-native, one-server, stop-first update; the deployment's
  start-first update is section 12 here.

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
- **G3 removes the tokens instead of making them N-safe.** The owner ruled that an enrolled host's certificate is its one
  identity, with no join tokens and no revocation machinery, so the token rows get no conditional writes and no cap lock.
- **The live hazards go first.** Section 12's list is what an update does to the deployment today; G1, G4, G5, G7 and G11 close
  it, G2 before G5, and G3 closes the token hazards once the Macs hold certificates. G13 changes which address a flag accepts; it is not a runtime gate.
- **G12 stays Designed** although #874 shipped without it, because the owner's decision asks for it. A deployment that has no
  SQLite job store to move needs only the export.

## 12. Updating js-server live

The project owner's requirement: a control-plane service is updated live, with no moment at which no instance serves and no run
stopped by the update.

**How agents and clients reach an instance.** Agents (the agent listener) and job clients (the job listener; the build frontend
among them) connect through the edge proxy in front of the service, outside this repository. It passes TCP through, so TLS and a
host's client certificate reach the server unchanged, and it sends each new connection to a task it believes up. Today it believes
a task up while a TCP connect succeeds: `/readyz` (`--ready-listen`) is served on a loopback address only
(`js-lease-server/src/config.rs:100-105`, enforced by `js-lease-server/src/config/listen.rs:55`), so the edge cannot see a drain. An agent keeps its connection while its requests succeed
and opens a new one after a failure.

**The update is start-first today.** The deployment's stack (outside this repository; stated by its owner, not checked from here)
updates the service start-first, in updates and in rollbacks: the new task starts and serves beside the old one, and the old one is
then stopped and drains, within the stop grace. Two instances overlap
on every update now, whatever section 9 has built.

**What the old instance does on SIGTERM** (Built):

1. `/readyz` answers `503 draining` at once, and the instance keeps serving and ticking for `--drain-hold` (15 s;
   `js-lease-server/src/server.rs:800`, #876).
2. It stops accepting connections. A request in flight is answered and its connection closed (`Connection: close` on HTTP/1.1,
   `GOAWAY` on HTTP/2), an idle connection is closed, and what is still open after `--drain-timeout` (10 s) is cut
   (`server.rs:813-837`, `js-lease-server/src/listener.rs:286-290`, `:368-392`, `:422-426`).
3. It ticks once more (`server.rs:837`) and exits. The final tick is the whole tick (`js-lease-server/src/api.rs:187`): the job pass
   (claimed), the registry's advance, the refresh (token reload, the cordon mirror, the bindings save, the credential flush, the
   usage tick, the log grants), a placement pass and the taints' keeping; then the usage side's last flush (`server.rs:681-686`).
   The scaler does not run in it. Two hazards below come from this tick.

An agent whose connection was closed or refused retries with its backoff. The instance it reaches holds no session for it, so a
beat is answered `409 unknown-host` or `403 session-mismatch` and the agent says hello there (`js-agent/src/session.rs:32-45`).
Its jobs run on throughout: an agent keeps its jobs without a server for up to `--silence-stop` with no accepted reply (300 s by
default, `js-agent/src/cli.rs:311-316`), far more than the drain hold and drain timeout (25 s), and on the server's side silence
ends nothing before grace plus retention ([absence.md](absence.md)).

**Hazards of the overlap that are live now:**

| During the overlap | Effect | Closed by |
|---|---|---|
| A submit reaches the new instance before the hosts have said hello to it | refused `no-capable-host` (`422`) for good, though the hosts serve on the old instance | G5 |
| Either instance's tick saves a binding that differs from the copy it last wrote (a lapsed or mirrored cordon end, a forgotten host) | the whole binding row is put or deleted at `Cond::Any` from that instance's copy, so a taint the other instance stored in it is lost, or a host it holds loses its binding | G4 |
| A host job placed by one instance is a taint in that instance's memory, and the host's connection moves to the other | the other may place a contained job beside the host job | G4, G7 |
| A host has a connection on each instance for a moment (a beat in flight to the old one when the agent reconnects to the new one) | the old connection can commit a listing over the new one's | G1 |
| A host that joins by token (the Macs) presents a host credential or one-time token the other instance issued after it started | the other reads its tokens once at start, answers `401`, and the agent exits (`401` is fatal); one token can also admit two hosts | G3 (removes the tokens; live for the Macs until they hold certificates) |
| An agent that said hello to the new instance reaches the old one again on a new connection during its drain hold | its beat is `403 session-mismatch`, and its hello there is `409 host-live` while the old record is within its grace, so the agent backs off | G5 (the same agent takes over at once), G1 |
| Both instances admit hosts new to a pool | the pool's cap counts per instance | G5 |
| `ListHosts` and `GET /v1/hosts` on either instance | each lists only the hosts whose connection it holds | G5 |
| A host's minute of usage is recorded by both instances (it moved during the minute, and the old instance flushes at exit) | the later flush replaces the minute, so up to a minute of the host's usage rows is lost | G11 |
| New connections reach the old instance through its drain hold, and are refused by it once it closes its listener, until the edge notices | an agent retries; a job client's request fails and is retried by the client | G13, and a stop grace above `--drain-hold` plus `--drain-timeout` plus the final tick, checked once at deploy |

Not live in an update: settlement by the local registry (G6) needs a host unheard for grace plus retention, far longer than an
overlap; the clock (G2) is one clock while both tasks run on the one node; the scaler (G9) is off on the deployment; G8 and the
import and prune parts of G11 are economy; G10 spreads agents in steady state and is not needed for an update.

**Order of safety.** Each step is relied on only once the one before is Built:

1. Every write conditional and every duty claimed (section 2, Built): two instances never place one job twice, book a host past
   its room or run a duty twice.
2. The live hazards closed: G1, G4, G5, G7 and G11's minute fold, with G2 before G5; G3 once the Macs hold certificates.
3. The edge sees the drain (G13) and the stop grace covers it, so no connection is refused during an update.
4. The rest of section 9 (G6, G8 to G10, the rest of G11, G12), which matters for N instances in steady state or for a long overlap, not for one update.

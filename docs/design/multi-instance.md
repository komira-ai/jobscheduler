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
code against this shape and list what is left to build; section 9 marks each gap kept, shrunk, deleted or new (G15, G16 and G19) by it. Sections 0.1 and 0.2 say which instances may pull from
the queue (two rules) and how a placement reaches its host (a dispatch queue the agent pulls).

- **Any instance serves any agent.** The agent keeps one connection between its requests, idle for at most 8 s
  (`IDLE_LIMIT`, `js-client/src/client.rs`), and the server ends each connection after its `max_requests`th answer (100); the
  edge sends each new connection to any instance. That instance authenticates the request by the host's certificate (G3) and reads what it needs of the host
  from rows. No instance owns a host's session, so a request that lands on another instance than the last one is served as it is:
  there is no hello to repeat, and no `409 unknown-host` or `403 session-mismatch` for a host the database knows. A beat of an
  epoch the row has passed is still `409 stale-epoch`. An agent says hello when it starts, as today.
- **A host is rows.**
  - `node/<host>` (G15): the epoch of its last hello (and the digest of its session secret), its pool, claim, shape,
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
  cordon (Built), the taint, the ports and the marks (G7). Any instance places (section 0.1), over every live host (a `node/` row
  heard within grace), not over the hosts whose requests it happens to serve, and writes the start as a dispatch message (section
  0.2) in the same transaction.
- **LISTEN/NOTIFY wakes; rows decide** (G16). After its commit, an instance sends a best-effort `NOTIFY`: on the queue channel for
  a submit, a free in a listing and a cancel (it wakes the instances' placement), and on the host channel with the host's id for a
  dispatch message (it wakes the instance holding that host's dispatch poll). A lost notification costs one poll (the dispatch
  poll's 5 s, the tick). When the `LISTEN` connection is made again, every held poll is woken once, since it may have missed
  notices. Built (G16): the job store decides a commit's wakes from its keys (`js-jobs/src/wake.rs:68`: a queued entry put, a
  live allocation's entry deleted, a new `dispatch/<host>/` row; a cancel adds the queue's, `js-jobs/src/store.rs:623`) and
  sends them once the commit is made (`js-jobs/src/store/written.rs:152`, and the claimed placement,
  `js-jobs/src/store/whole.rs:168`), through `Store::notify` (`js-state/src/store.rs:237`), which the Postgres store sends with
  `pg_notify` on a session of its pool, a failure logged and dropped (`js-state/src/postgres/listen.rs:32`). The channels are
  `js_jobs_queue` (no payload) and `js_jobs_host` (the host's id). Each server listens on a connection of its own
  (`js-state/src/postgres/listen.rs:61`, made again 5 s after it is lost) on a thread of its own
  (`js-lease-server/src/listening.rs:80`, started at `js-lease-server/src/server.rs:297`); the queue pokes the placement poller,
  a host wakes its held beat and its held dispatch poll, and the connection made wakes every held beat and poll once and pokes
  the poller (`js-lease-server/src/listening.rs:100`).
- **Deadlines are the database's `now()`** (G2). Every time that one instance writes and another compares (`heard_at`, queue and
  start deadlines, retention, a cordon's end, a duty's due time) is the database's clock.
- **Periodic work is claimed by its due time** (Built, #881). Settling silent hosts, expiry, retention and the feed trim run once a
  period on whichever instance claims them; settlement judges a host's silence by `heard_at` (G6).

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
under the commit head behind every claimed placement (3.1). It moves the store's head, so the reading every pass's table cache is
judged by (`js-jobs/src/store/written.rs`, #880) reads the feed's rows since its last reading and moves only for a row outside
`node/` (Built, G15): a quiet beat leaves the tables cached and the pass skipped as before, at the cost of a short feed read. The
listing-clean flags a pass reads are kept the same way (`js-jobs/src/store/unclean.rs`): read whole once, then only the node rows
the feed names since the last pass (over a store with no head, which one process writes, from that process's own writes). The hosts a pass offers are read from the `node/`, `taint/`, `hostmark/` and binding rows before each pass that is not skipped and each that is (`js-lease-server/src/sched/jobs_rows.rs` `offered`), each `node/` row taken from the last read while it stands at the version it had then (`JobStore::live_nodes_reusing`): a scan per table per pass, the price of placing over every server's hosts. The other standard form is a plain
`node` table on the store's own connections (`js-state/src/postgres/extension.rs`, no head, no feed), with
`UPDATE ... WHERE epoch = $1 AND seq < $2 RETURNING` as the guard. It gives up the same-transaction `Checked` guard of the listing.
G15 chooses between the two by measuring both at the deployment's host count.

This amends [jobscheduler-design.md](jobscheduler-design.md) section 10.1: "live sessions, session secrets, heartbeat times" are
rows now (the secret as a digest). The whole-host `Committed`, `Ready` and `Restoring` states stay out of the database: a
ready or refused report rides on every beat until the job is `starting` (`job_whole.rs`), and `Restoring` is the stored phase.

### 0.1 Pulling from the queue

Decided by the project owner (2026-11-19). No state lives outside Postgres, so an old instance need not stay alive. Heartbeats,
the job API and dispatch all go through the edge to any instance. All the scheduling logic stays in js-server, with all the usage
data. A live update has two rules, and nothing else:

1. **No two instances pull the same queue element.** A queued job is claimed with `SELECT ... FOR UPDATE SKIP LOCKED` and the host
   booked by the conditional write of `hostbook/<host>` (Built, #879). A dispatch message is claimed the same way, with a
   `claim_until` column (section 0.2).
2. **On SIGTERM an instance stops pulling, finishes what it already pulled, and exits.** It claims no more jobs or messages,
   answers the dispatch polls it holds (the claim and the reply of an element it pulled), commits, and then stops. The stop grace
   covers one dispatch and the drain hold, so the instance is never killed mid-flight. The agent and job listeners and the drain
   hold of #935 stay as they are (section 12).

Any instance places, and any instance dispatches. Placement is serial on the commit head anyway (3.1), and the two rules are
enough for correctness, so no lease, no instance row and no leader is needed.

**The crash case.** An instance that dies mid-placement has its transaction rolled back, and the job stays queued. One that dies
after claiming a dispatch message, or after answering a poll whose reply is lost, leaves the message claimed until `claim_until`
(`dispatch_lease_secs`, a config key, default 10). The next poll of that host, on any instance, then takes it again. The agent's
start is idempotent on the job id, fenced by its allocation (Built: `js-agent/src/lifecycle/mod.rs:12-25`), so a retry never
starts a job twice. (The agent keeps a finished job's id in its bounded retired set, `RETIRED_CAP`; a message is deleted by
its ack or with its job's end, so a redelivery comes within one `claim_until` of the last delivery, far inside that set's span.) A stop of a job the agent no longer runs changes nothing.

**No fencing rows, and why.** The races considered for a per-instance active/draining row or a placer lease:

- *A paused dispatcher.* Instance A claims message M, stalls past `claim_until`, and B takes M and delivers it; A then delivers it
  too. The agent receives one start twice, and the idempotent start makes the second one change nothing.
- *An old release placing during the overlap.* Its placement is claimed and booked like any other (rule 1), and the message it
  writes is one the new release reads (version skew, below).
- *A draining instance placing after SIGTERM.* It stops pulling at the signal. A placement already in its transaction either
  commits, and is delivered like any other, or rolls back.
- *A paused placer.* A placement transaction whose instance stalls is cut by the store's own timeouts
  (`js-state/src/postgres/timeouts.rs`) and rolls back.

None of these starts a job twice, books a host past its room or loses a job, so none needs a fence. If a race is found that does,
the row is added then (B20).

**Version skew.** Old and new instances may pull side by side for seconds. So the queue message and the agent API stay compatible
across one version:

- A dispatch message changes only additively (B8): a new optional field that the release before ignores, never a renamed or
  re-meant one.
- A release reads and delivers every message that the release before writes.
- The agent's dispatch route and its acknowledgement change only additively. An agent of release N works with servers N and N+1,
  since servers are updated before agents.
- In G19's own overlap the release before writes no messages. So a start is delivered on whichever path its agent uses: the new
  release's placement writes the message, and the beat's reply still carries the starts derived from the rows (`per_job.rs`
  `starts_for`) until the release before is gone. The two deliveries of one start are the same idempotent start.
- The tests: the release before hands out a message the new placer wrote, and the agent starts it; a job placed by the release
  before reaches a polling agent through the beat's reply.

### 0.2 The dispatch queue

The scheduling logic decides a placement and writes what the host must do as a message. A dumb dispatcher hands it out.

- **The message.** `dispatch/<host>/<seq>` holds:
  - its kind (`start` or `stop`), the job id, the allocation id and the host;
  - the whole start spec (the `JobStartSpec` a beat's reply carries today), or the stop;
  - `claimed_by` and `claim_until`.

  The placement transaction writes a start; a whole-host job, which starts only once its host is ready, gets its start from the
  transaction that moves it to `starting`, where `starts_for` sends it today. The decisions that end a job write a stop in their own
  transactions: a cancel (`request_cancel`), an expiry, a loss, a cancel of a job its host no longer holds. A wall limit is the
  agent's own (it stops the job and reports it), so, like an exit, it writes no stop. A stop is written only when the start may have
  reached the host (it was handed out, the job is starting or running, or no start is queued for a job whose start was due); a
  start never handed out of a job not yet starting is deleted instead. Every end deletes its job's start, and the retention sweep deletes a job's messages with its row. Stops and cancels use
  the same queue. Built at the store (G19), and the route that hands it out is Built (next item): the key `js-jobs/src/keys.rs:51`, the record
  `js-jobs/src/record.rs:661` and its row `js-jobs/src/codec/dispatch_fields.rs`; the start written by
  `js-jobs/src/store/whole.rs:314` (every placement) and `js-jobs/src/store.rs:521` (a whole-host job's move to starting); the
  stops and the deleted starts by `js-jobs/src/store/dispatch.rs:310`, called from `js-jobs/src/store.rs:599` (a cancel),
  `js-jobs/src/store/ending.rs:106` (every end), `js-jobs/src/store/hosts.rs:88` (an end that frees) and
  `js-jobs/src/store/whole.rs:391` (a failed prepare).
- **Dispatch is an agent route that the agent long-polls through the edge.** `POST /v1/agent/dispatch` (a `POST`, not a `GET`: the
  poll carries the session secret, which a URL would leave in the logs of every proxy on the way), on the agent listener, goes
  to any instance. That instance claims the host's due messages with `SELECT ... FOR UPDATE SKIP LOCKED`, sets `claim_until`, and
  returns them. With none due, it holds the request up to the hold of a held beat today (`js-lease-server/src/held_beat.rs:25`,
  5 s), and wakes on a `NOTIFY` on the host's channel (G16). Built: the route
  `js-lease-server/src/agent_api/dispatch.rs:103` (`poll`; mounted at `js-lease-server/src/agent_api.rs:160`), the claim
  `dispatch.rs:260` (`claim_round`) and the hold `dispatch.rs:219` (`hand_out`), over the wire `js-agent-wire/src/dispatch.rs`. The
  session is judged as a beat's is and nothing is recorded (`Registry::check_heartbeat`). A held poll wakes on a placement this
  instance commits (`js-lease-server/src/sched/jobs_pass.rs:62`, its own wake beside the held beat's:
  `js-lease-server/src/dispatch_state.rs:20`), on another instance's notice for the host (G16,
  `js-lease-server/src/listening.rs:100`), and reads the queue again every `server.dispatch_recheck_ms` (500 ms), which finds a
  message whose notice was lost; a drain answers it empty at once and a claim after the drain began is never made.
- **Acks by allocation id.** The agent acks each message it has acted on, by its allocation id, in its next dispatch request, on
  any instance. The ack deletes the message, and an ack of a message already gone changes nothing. A message not acked by
  `claim_until` is handed out again. Delivery is at least once. An ack names the allocation and the kind (start or stop) and
  deletes only a message that was handed out, so the ack of a start never deletes the stop of the same allocation, which the
  agent may not have seen even when a server handed it out. Built at the store: the claim
  `js-jobs/src/store/dispatch.rs:183` (each due message in a claimed transaction of its own, `claim_until` on the store's clock,
  the lease `server.dispatch_lease_secs`, `js-lease-server/src/file/server.rs:327`) and the ack `js-jobs/src/store/dispatch.rs:242`,
  called by the route (`js-lease-server/src/agent_api/dispatch.rs:157`, `acknowledge`; an ack is applied by a draining instance too).
- **Heartbeats keep their own route.** Hello and the heartbeat go to any instance, applied to the rows (section 0). A beat's reply
  no longer carries starts and stops for an agent that polls dispatch. An agent that does not poll (an older release, which says
  so by the absence of a hello capability) gets them in its beat's reply from the same messages, as today, and acknowledges them
  by its listing. Built: the welcome says `dispatch_poll` to a host that reported the capability, from a server that holds the job
  tables (`js-lease-server/src/agent_api.rs:341`); the beat's reply of such a host leaves out each start and stop its queue
  carries, and keeps the rest, a job the release before placed (no message) among them (`js-lease-server/src/per_job.rs:181`,
  `commands_now`, and `:215`, `carried_by_queue`).
- **The agent's poller.** Built (`js-agent/src/dispatch/`, module header). An agent that runs jobs reports the hello capability
  `dispatch-poll` and polls only for a session whose welcome says `dispatch_poll`; it polls on a thread of its own, on a new
  connection for each poll (so each can reach any instance), with the welcome's host, epoch and session. A 200 is handed to the
  worker in one delivery and its messages are acked in the next poll when the worker took them (an invalid message is neither acted on
  nor acked; messages the worker had no room for are held, no poll is posted meanwhile, and they are handed over when the worker takes an
  order and acked in the poll posted right after, so they come again after `claim_until` only when the worker stays full past it); the
  acks stay until a poll that carried them is answered 200. A transport error and every status but 200 (404 from a server without the route during an update, 408, 429, 5xx, and
  the refusals 400, 401, 403, 409 of an instance that does not hold or will not serve the session) back off like a failed heartbeat
  and poll again with the same route, since the next poll may reach another instance and a halt would leave the starts that servers
  leave out of beat replies undelivered while heartbeats succeed. A welcome without the flag
  leaves the agent on the beat's reply.
- **A pull, not an inbound js-agent endpoint.** Agents dial out and have no inbound API. An inbound one would need a listener on
  every host, reachable through NAT and the hosts' egress rules (`docs/design/agent-egress.md`). It would also need mTLS both ways,
  that is a certificate of the coordinator's CA for js-server, which is the per-service certificate the owner ruled out
  (section 6). The pull crosses the edge as every request does, and with the wake it starts a job as fast as a push would.
- **Start latency against today.** Today a start rides the heartbeat reply: p50 66 ms and p90 296 ms from placement to delivery
  (the project owner's figures from the deployment).
  - With the long poll, a start reaches a host that holds a poll after the placement's commit, one `NOTIFY` and one reply.
    Inferred: a few milliseconds of database work and the two network legs.
  - The p90 today comes from a start waiting for the host's next request, and a held poll removes that wait.
  - Not measured here: the code does not exist yet, and no build runs on this host. G19 measures it on the deployment, as p50 and
    p90 from placement to the agent's ack, over the same starts with and without the poll, and its pull request reports both.

## 1. The rule

Direction from the project owner (2026-11-15, binding): **Postgres is the single source of truth, and any number of js-server
instances over one store just work.** There is no leader, no leader lease and no election. Every decision (placement, admission,
a host's session, a taint or cordon, a periodic duty, a scaler step) is made inside a Postgres transaction whose correctness does not
depend on which instance runs it or how many run it at once. Pulling from the queue follows the two rules of section 0.1.

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
   No step elects, leases, counts instances or relies on being the only process. An instance that dies at any point leaves no
   lock or lease that another must clear by a timeout, except the dispatch claims it held, which expire (section 0.1): its transaction committed or it did not. (A host that falls silent
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
- **A start opens the job store in one pass while another instance writes** (#923): a row the
  other instance changed between the open's read and its commit is read again alone; the open never starts over or waits.
- **A second server keeps the first's bindings, taints and tokens, and the drain moves agents at once** (#935): the bindings save
  deletes only what this server's registry held (`js-lease-server/src/agent_state.rs:358`, `:369`), a hello of a host the registry does not
  know reads its binding row (`js-lease-server/src/agents.rs:572`), and on SIGTERM the agent listener closes and nothing more is placed
  (`js-lease-server/src/drain.rs:28`).
- **A cordon is a row every placement checks** (#932): `cordon/<host>`, decided at the version read and checked absent by every
  placement transaction, whichever server took the cordon (`js-jobs/src/store/cordons.rs:107`, `js-jobs/src/store/whole.rs:211-217`,
  `:310-311`).

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
| The index backfill and the boot raise of deadlines at open | `js-lease-server/src/job_store.rs` (`booted`), `js-jobs/src/store/index.rs` (`backfill_indexes`), `js-jobs/src/store/ending.rs` (`raise_at_boot`) | each row in a transaction of its own at `Cond::Version`, many to a store batch; a row another instance beat is read again and decided again alone, so the open is one pass whatever the other instance writes; a raise is a max, so a second start only delays deadlines | Built |
| The first start imports the files beside the state file once | `js-lease-server/src/store/import.rs:99-123` | marker and rows `Cond::Absent` in one transaction, so one import wins; the loser's `Conflict` reads the marker again and starts (the files named stale), and refuses its start only while the marker is still absent | Built (G11) |
| Row expiry (TTL) | `js-state/src/postgres/tx.rs:146`, `:289` | judged on the database clock inside each transaction | Built |
| Feed trim | `js-lease-server/src/job_pass.rs:242`, `js-lease-server/src/job_store/feed_trim.rs:44` | a duty claimed per period; a trim by absolute revision is idempotent | Built |
| An unreachable database | `js-lease-server/src/job_pass.rs:190`, `:225`, `js-lease-server/src/store_health.rs` | every store call fails and nothing is decided; a hello's binding read or write or a taint's write answers `503`, a listing's commit `500` `state-write-failed` (`js-lease-server/src/agent_api.rs:366`, `:396`, `:666-669`), both retried by the agent | Built |
| `Store::now` when the database cannot be read | `js-state/src/postgres/mod.rs:920-931` | fails with the read's error (`Store::now` returns `Result<StoreTime, StoreError>`, `js-state/src/store.rs:121`); the `js-jobs` writes that stamp rows from it (`js-jobs/src/store.rs:343`, `:491`, `ending.rs:62`, `:147`) fail with it and write nothing | Built (#938) |
| Readiness, drain and shutdown | `js-lease-server/src/ready.rs:26`, `js-lease-server/src/server.rs:309` | one instance's own phases (#876); nothing waits on another. At SIGTERM the agent listener closes, held beats are answered at once and no job is placed (#935, `js-lease-server/src/drain.rs:28`, `server.rs:840`), so agents move at once; the route is served on a loopback address only, so the edge cannot see a drain (section 12) | Per instance; on SIGTERM the instance stops pulling and finishes what it pulled (section 0.1); a dispatch poll that has claimed is answered with its claim, a held one is answered empty at once and none claims after the drain began (Built, G19: `js-lease-server/src/agent_api/dispatch.rs:97`) |
| The host-native update hook's "is a server running" probe | `js-state/src/writer.rs:128`, `js-state/src/postgres/serving.rs:21`, `js-lease-server/src/update_hook/step/mod.rs` | reads `pg_stat_activity` by the server's application name; it serves the one-server install of `docs/lease-server-update.md`, not an N-instance deploy (which is the orchestrator's start-first update) | Per instance |

### 3.2 Hosts: identity, admission, taint, cordon

| Decision | Where | How it holds | Tag |
|---|---|---|---|
| A host proves who it is | `js-lease-server/src/host_cert.rs:86` (`host_of`), `js-lease-server/src/client_verifier.rs:29`, `js-lease-server/src/agent_api/host_cert.rs:34` (`admit`), `js-lease-server/src/agent_api.rs:206` (`host_cert::admit`), `:271` (`host_cert::check` in the hello), `:410` (in the heartbeat) | the host certificate hostkeeper issues to an enrolled host, verified against `--agent-client-ca`; the host id is its one DNS SAN (#922). A function of the certificate and the CA, so every instance answers alike | Built |
| A host is known by its certificate alone: a request without a verified host certificate is refused, and nothing else admits a host | `js-lease-server/src/agent_api.rs:179` (`authenticate`, the outermost layer of the agent router: no verified host certificate, `401` with `{"error":"host-certificate-required"}`; a bearer is not read), `js-lease-server/src/agent_api/host_cert.rs:25` (`HOST_CERTIFICATE_REQUIRED`, beside `CERTIFICATE_HOST_MISMATCH` at `:22`), `js-lease-server/src/host_cert.rs:86` (`host_of`), `js-lease-server/src/in_process.rs` (`as_host`, the way an in-process caller presents a host) | the one-time join tokens (mint, revoke, use-once, the cap of waiting tokens), the pool tokens of `--agent-token-file` and the host credentials a welcome issues and the tick renews are deleted, with the scaler's mint and revoke of a launched machine's token and the `js join-token` verb; the join-token routes (`POST` and `DELETE /v1/agent/join-tokens`) are an unknown route. No per-host secret is kept in a row for admission, so any instance knows the host from the certificate of the connection. `--agent-token-file`, a pool file's `join_token_file` and `--require-host-credential` (`server.require_host_credential`) are still accepted, hidden from help and not read, and each one given prints one start-line warning that it is ignored because agents are admitted by host certificate only: the deployed stack passes `--agent-token-file` and a release deploys automatically, so a refused flag would stop the server on its own deploy. The store prefix `hosts/join/` and the file `<state>.joins` are no longer read or written, and no row or file is deleted, so a release before this one still finds them | Built (G3) |
| A host id is bound to the credential and provider instance of its first hello (`HostBound`) | `js-lease-server/src/agent_api.rs:317-325` (`rows::refresh`, then `judge_hello`), `js-lease-server/src/agent_api/rows.rs:69` (`refresh`), `js-lease-server/src/agent_state.rs:639` (`stored`, which holds the row's version), `:378` (`cond`), `:732` (`change`) | judged on the binding row the hello read: `refresh` reads the row with its version and puts it on the registry (the node row carries the binding, `Registry::adopt`; a taint or cordon is put back or taken in), then `Registry::judge_hello` judges. The hello's binding write is conditional on that read (`Cond::Absent` for a first hello, `Cond::Version` otherwise, never `Cond::Any`), in the one transaction with the hello's other writes, so a binding another server wrote in between refuses the hello `503 state-write-failed` with nothing applied and the agent's next hello reads again. Without the job tables the registry learns a binding only from a taint or cordon it holds or from its own hello (a gap that was there before; the node row closes it with the tables). The certificate names the host, and the binding row still records it (`Credential::of_host_certificate`); G4's clean-up of `HostBound` is not done | Built |
| A refused hello is judged before anything is written | `js-lease-server/src/agent_api.rs` (`serve_hello_in`: `judge_hello`, then `write_hello`, then `take_hello`; `refused_hello`), `js-registry/src/registry/admit.rs` (`judge_hello`, `take_hello`) | the registry judges the hello on the refreshed rows, and draws the session secret, before the binding is written, so a refused hello has nothing to undo. The admission it gives is taken after the writes by `Registry::take_hello`, which does not judge again and cannot refuse, so there is no undo at all: a race inside the process cannot happen (hellos are taken one at a time from the judgement to the take), and a race with another server's hello is judged again on the row by the row's write (`js-jobs/src/store/nodes.rs`, `hello_node_in_pool`): a pool that filled answers `503 state-write-failed`, a host another agent process's hello made live answers `409 host-live`, and the registry gives the session up either way. A hello whose node row cannot be written after the registry took it is refused `503 state-write-failed` and the registry gives the session up; the binding stays (the host says hello again) | Built |
| Hellos of one host are serialized | `js-lease-server/src/agent_api.rs:310` (`host_writes`; `refresh` runs inside it, `:317`) | a process mutex; the binding writes are conditional now, so with G3 built it is an economy | Per instance |
| The bindings are saved | `js-lease-server/src/agents.rs:918` (`save_bindings`), `js-lease-server/src/agent_state.rs:675` (`save`), `:435` (`save_each`), `:396` (`writes_to`) | each changed binding is its own transaction, put or deleted at the version of the row this server last read or wrote (`Held::versions`), and a delete only of a binding this registry held and let go (#935). A write that loses its version is not made: the row is read again and held, and the binding it holds is answered, so `save_bindings` takes a taint the row holds that the registry does not into the registry (`agents_taints.rs:153` `take_in`) and the next save writes over it, or nothing when they agree; the other hosts of the save are written all the same (one transaction each, since the revision of a one-row commit is the row's version). A start's `restore` is one transaction at the versions the scan read (`agent_state.rs:710`) and refuses the start when a row changed since. After G3 the registry holds no binding to save and `save_bindings` goes | Built (the save goes with G3) |
| A taint a heartbeat reports is stored | `js-lease-server/src/agent_state.rs:647` (`commit_taint`, the `Cond::Version` put at `:668`) | `Cond::Version` of the row read, after checking the stored taint is the expected one. #940 adds the clear: a heartbeat that reports a taint cleared on a host whose registry holds none ends the stored taint when it is the cleared one (or a refused start's `host-tainted`), by the same conditional write (`agent_state.rs:686` `end_taint`) | Built |
| Every instance sees a host's taint | `js-lease-server/src/agents.rs:306` (`restore_job_taints` at start; defined `js-lease-server/src/agents_taints.rs:159`), `js-lease-server/src/agents.rs:541` (`restore_stored`, at a hello and before a binding write), `js-lease-server/src/agents_taints.rs:155` (`take_in`), `js-lease-server/src/sched/jobs_rows.rs:139` (`offered`: the `taint/` rows and the stored bindings, `js-lease-server/src/agent_state_scan.rs:23`, read before each pass; a `taint/<host>` row of a host this registry holds is taken on, `js-lease-server/src/sched/jobs_rows.rs:216`), `js-jobs/src/store/host_rows.rs:80` (`refuse_host`, in the placing transaction) | for placement, rows: every pass reads the `taint/` rows and every stored binding, and `place_claimed` reads `taint/<host>` and `hosts/binding/<host>` (checked at the version read) and every other placement's commit checks `taint/<host>` absent for a contained job, so a host job placed through A, or a taint A stored, keeps B's placement off the host. What stays per instance is the registry's own taint (the beat's reply, a whole-host start): loaded at start; at every hello, of a host the registry knows too (the row is read for its version, and a taint it holds that the registry does not is taken in); before a cordon, a lift or a clear writes the binding (`keep`); and when a save loses its row's version | Built for placement (G7); Shrunk, Designed (G4) for the rest |
| A cordon and its lift | `js-lease-server/src/agents.rs:617` (`keep`), `js-lease-server/src/agents_cordon.rs:173` (`cordon`), `:203` (`uncordon`), `:86` (`take_from_rows`), `:213` (`mirror_cordons`), `js-jobs/src/store/cordons.rs:107` (`cordon_host`), `js-jobs/src/store/whole.rs:212`, `:317` | with the job service on, the cordon is the row `cordon/<host>` (put with a TTL), decided in the store at the version read (`Cond::Absent` or `Cond::Version`; a lost race is `CordonConflict`, answered `503 cordon-unsaved`), and every placement transaction checks it absent, so a cordon placed on any instance keeps every instance's placement off the host and it lapses by the store's clock; each tick puts the rows on the registry (`mirror_cordons`), so a lift made on B reaches A's listing, scaler and probes at A's next tick. The binding's cordon end is written by `keep` after the row is read (`restore_stored`), at the version read, so it never overwrites a binding another instance wrote (a refused write is `503 cordon-unsaved`), and a host the registry does not hold is taken from the node and binding rows first (`take_from_rows`, the hello's `refresh`), so a host served by another instance is served, not `404`; a host no row and no binding knows is still `NotHeld`. Without the job service the registry decides in memory and a host it does not hold is `NotHeld` | Built |
| An operator clears a host's taint (`js-agent clear-taint` of #939 is the agent's own local clear, not this route) | `js-lease-server/src/agents_taints.rs:84` (`clear_taint`, `:89` `take_from_rows`) | the host is taken from the rows when the registry does not hold it (with the job tables), then the write is conditional (`commit_taint`): a host held by another instance is served, a host no row knows is `NotHeld` (`404`/`409 unknown-host` as before) | Built |
| Host certificate admission (`--agent-client-ca`) | `js-lease-server/src/host_cert.rs:86`, `js-lease-server/src/client_verifier.rs:29` | a function of the certificate and the configured CA | Per instance |
| The hello rate limit | `js-lease-server/src/agents.rs:72` | a limiter of this process | Per instance |

### 3.3 The session and the heartbeat

| Decision | Where | How it holds | Tag |
|---|---|---|---|
| Which connection of a host commits its job listing | `js-lease-server/src/agent_api/rows.rs:259` (`beat_row`: the beat applied to `node/<host>` by `beat_node`, guarded by its (epoch, sequence)), `js-lease-server/src/agent_api.rs:548` (`serve_jobs`: `lose_unlisted`, `job_whole::serve` and `per_job::serve` run on `jobs.guarded(host, version)`), `js-jobs/src/store/guard.rs:31` (`guarded`: every write carries a check of the row at the version the beat wrote; a newer beat or hello fails it `NodeMoved`), `js-lease-server/src/agent_api.rs:679` (`rows_answer`) | the beat's row write orders the connections: an older beat (by sequence) changes nothing, and a listing whose row version a newer beat or hello passed stops at its next write; either is answered 200 from the rows, with no acknowledgement | Built (G15) |
| A host's first listing after a hello loses the jobs it does not list | `js-lease-server/src/agent_api.rs:593` (`first_listing_pending` of the row the beat applied), `js-lease-server/src/agent_api/rows.rs:312` (`settle`: the beat that applied the listing clears it at the version it wrote), `js-jobs/src/store/nodes.rs:323` (`settle_node`), `js-lease-server/src/job_loss.rs:96-107` | the flag is the row's: a hello sets it, and only a listing that committed clears it, so a beat that reaches another server is not a first listing there, and a listing that fails leaves the next beat's listing (the agent's whole account) the first. A whole-host job's start, lost with its reply, rides on the next beat on any server (`per_job.rs` `starts_for`) | Built (G15) |
| A host's session, epoch, secret, last heard | `js-lease-server/src/agent_api/rows.rs:213` (`record_hello`: the hello's row, which numbers the epoch; the registry is renumbered, or abandons the session when the row cannot be written), `:69` and `:112` (`refresh`, `refresh_for_beat`: a beat of an epoch the registry does not hold adopts the row's session), `js-registry/src/registry/adopt.rs:57` (`adopt`), `js-jobs/src/store/nodes.rs:216` (`beat_node`) | the session is the row `node/<host>`: any instance serves a beat, checking its secret against the digest the row holds; no `409 unknown-host` or `403 session-mismatch` for a host the row knows; `409 stale-epoch` for an epoch the row passed. The registry is a cache refreshed from the rows | Built (G15) |
| `HostLive` (a second agent process on a live host) | `js-registry/src/registry/admit.rs` (`judge`), `js-lease-server/src/agent_api/rows.rs` (`refresh`, before the hello is judged; `record_hello`), `js-jobs/src/store/nodes.rs` (`hello_node_in_pool`, `live_elsewhere`) | judged on the registry refreshed from the row at the hello, so a hello to B of a host live on A is refused; a hello that names the agent process of the live session (`Hello::agent_process`, G15) takes the host over at once. Judged again in the row's own write, conditional on the row read: a row heard within the pool's grace by the store's clock whose agent process is not the hello's refuses it `409 host-live`, so two servers never both admit hellos of one live host by two agent processes | Built (G15, G5) |
| The pool's machine cap (`PoolFull`) | `js-lease-server/src/agent_api/rows.rs` (`room_in_pool`, before anything of the hello is written; `record_hello`), `js-jobs/src/store/nodes.rs` (`pool_admits`, `hello_node_in_pool`, `forget_silent_nodes`, from the claimed job pass, `js-lease-server/src/job_pass.rs` `forget_silent_rows`), `js-lease-server/src/scaler/live_marks.rs` (`forget_removed`) | a hello of a host that has no row is judged against the pool's rows not forgotten before anything of it is written, and refused `pool-full` there with no token used and no binding written; its row is then written with `pool/<pool>` at the version read, counting again, so of two instances admitting hosts into a pool one slot from its cap exactly one writes a row, and the other answers `503 state-write-failed` (the registry abandons the session) and its next hello is refused `pool-full`. A row silent past the longest grace plus retention that holds nothing is forgotten once per period, and a removed host's row is deleted with its scale-down mark. The operator's `RetireHost` (`js-lease-server/src/agents_retire.rs`, `js-jobs/src/store/nodes.rs` `retire_node`, `js-registry/src/registry/rows.rs` `forget_host`) deletes one row sooner, once it is silent past the grace and holds nothing (by the same judgement), with the host's stored binding and the serving instance's record; another instance's record of the host stays until it forgets it as lost or the call is made there | Built |
| Whole-host job states (placed, starting, running, prepare deadline) | `js-jobs/src/store/whole.rs:383`, `:388`, `:426`, `js-lease-server/src/job_whole.rs:143` | rows written at `Cond::Version` | Built |
| The whole-host start's taint and the clock it judges deadlines by | `js-lease-server/src/agent_api/held.rs:65` (`own.host.taint`), `js-lease-server/src/job_whole.rs:143` (`serve`) | the registry's taint; the clock is the store's, read only for a step that stamps a time (#938) | Built (the clock); Designed (G4, G7: the taint) |
| A job's start command, repeated by two instances | `js-lease-server/src/per_job.rs:166`, `js-lease-server/src/job_start.rs:120` | a pure function of the rows; the placement that creates it is claimed | Built |
| Listing-clean (a host whose last listing was not clean takes no job) | `js-lease-server/src/agent_api/rows.rs:312` (`settle`: `last_listing_clean` written by the beat that applied the listing), `js-lease-server/src/sched/jobs_rows.rs:174` (`unclean_nodes`, read before each pass), `:250` (an unclean host is not offered), `js-jobs/src/store/unclean.rs` (`unclean_nodes`: read whole once, then the rows the feed names since; a pass that cannot read them places nothing) | the row's flag, so every instance's placement sees it | Built (G15) |
| Held beats and their wakes | `js-lease-server/src/held_beat.rs:36` | the process's own wakes, and another instance's notice for the host wakes them as its own placement does (G16, `js-lease-server/src/listening.rs:100`); only a lost notice leaves a placement by another instance to the host's next beat, at most its hold later; a draining instance ends every hold at once and begins none (#935, `js-lease-server/src/agent_api/held.rs:95-113`) | Per instance, a held dispatch poll's wake beside it (`js-lease-server/src/dispatch_state.rs:20`, rechecked every `server.dispatch_recheck_ms`); Built (G16: woken across instances by the host's notice, and all at once when the listening connection is made again, `js-lease-server/src/held_beat.rs:187`) |
| Log upload credentials per host | `js-lease-server/src/logs/mod.rs:155-307` | stateless provider credentials; several valid at once; a flip costs a mint | Per instance |

### 3.4 Placement and the queue

| Decision | Where | How it holds | Tag |
|---|---|---|---|
| A queued job is placed by exactly one instance | `js-jobs/src/store/whole.rs:144-148`, `:310` | `claim_decide` on `job/<id>` (`SKIP LOCKED`), the job written at `Cond::Version` | Built |
| A host is never booked past its room | `js-jobs/src/store/whole.rs:197-210` | the host's live allocations summed in the claimed transaction, `HostFull` when they leave no room, `hostbook/<host>` written at `Cond::Version` | Built |
| One host job (`isolation=host`) per host | `js-jobs/src/store/whole.rs:313`, `js-jobs/src/store/taints.rs:40` | `taint/<host>` put at `Cond::Absent` in the placing transaction | Built |
| No job on a cordoned host | `js-jobs/src/store/whole.rs:211-217` (`place_claimed`), `:317-318` (every placement), `js-jobs/src/store/cordons.rs:168` (`cordons`), `js-sched-placer/src/jobs.rs:704-709` | the row `cordon/<host>`, decided at the version read (`cordons.rs`); every placement transaction checks it absent (`HostCordoned` in `place_claimed`, a conflicting commit in `place` and `place_with`), whichever instance took the cordon; each pass also drops the hosts whose cordon the store holds from the open hosts | Built |
| No contained job beside a host job, no job on a tainted host | `js-jobs/src/store/host_rows.rs:80` (`refuse_host`, from `js-jobs/src/store/whole.rs:219`), `js-jobs/src/store/taints.rs:33` (`with_taint`), `:75` (`drop_taints`), `js-lease-server/src/sched/jobs_rows.rs:139` | `place_claimed` reads `taint/<host>` and the host's binding (a text `taint_reason`) and refuses `HostTainted`, and checks the binding at the version read; every placement's commit checks `taint/<host>` absent for a contained job (puts it absent for a host job); `taint/<host>` is deleted at the version read and only while it names the job kept; the pass also leaves out every tainted host | Built |
| A host port is held by one job per host | `js-jobs/src/store/host_rows.rs:50` (`ports_held`), `:80`; `js-sched-placer/src/jobs.rs:234` (`port_held_on`), `:472` (the engine's choice) | `place_claimed` alone reads the host ports of the host's live allocations and refuses `PortTaken` | Built |
| A host the scaler drains or releases takes no new job | `js-jobs/src/store/host_rows.rs:161` (`mark_host`), `js-lease-server/src/scaler/live_marks.rs:55` (`keep_mark`, after the registry takes the call), `js-lease-server/src/agents_cordon.rs:269` (`mirror_host_marks`, each tick), `js-registry/src/registry/marks.rs:221` (`mirror_marks`), `js-jobs/src/store/whole.rs:318`, `js-lease-server/src/scaler/live_marks.rs:78` (`forget_removed`) | the mark is the row `hostmark/<host>`, written at the version read by whoever marks; `place_claimed` refuses `HostMarked` and every placement's commit checks it absent, every pass leaves it out, and the registry's marks are its cache, put back from the rows each tick | Built (G7); the scaler's step itself Designed (G9) |
| One mark row per whole-host job | `js-jobs/src/store/marks.rs:81-94` | `mark/<job>` at `Cond::Absent` or `Cond::Version`, with the job checked at its version | Built |
| Which host a whole-host job drains | `js-sched-placer/src/whole_jobs.rs:204-210`, `js-sched-placer/src/jobs.rs:741` | each instance chooses over its own hosts and deletes a mark whose host it never heard after its own start plus grace: two instances drain two hosts in turn (the rows stay consistent; the work is wasted) | Not a gap: churn, the rows stay consistent (G8 deleted) |
| Why a job waits (queue status note) | `js-sched-placer/src/jobs.rs:779`, `js-jobs/src/store/status.rs:38` | conditional, but its text depends on the instance's view, so two instances rewrite it in turn and each rewrite moves the head | Not a gap: cosmetic (G8 deleted) |
| The tables a pass decides from | `js-sched-placer/src/jobs_seen.rs` (`SchedPlacer::tables`) | a cache judged by the store's head (#880) that follows the store's change feed after a write, taking only the rows that changed and reading the tables in full only where the feed cannot tell (`JobStore::table_changes`, `js-jobs/src/store/table_changes.rs`), with the job of this instance's own submit added to it when that commit is the only one since (`SchedPlacer::note_inserted`, `js-jobs/src/store/written.rs`); every placement is re-decided in its transaction | Built |
| Which hosts an instance places on | `js-lease-server/src/sched/jobs_rows.rs:139` (`offered`, read before each pass from `JobStore::live_nodes_reusing`, `:159`), `js-lease-server/src/sched/jobs_pass.rs:139` (`pass_over_queue`: nothing once draining, `:143`) | every host whose `node/` row is live (heard within the longest grace by the store's clock), whichever instance took its hello, and every host this instance's registry holds live; the pass's skip rule sees a changed row as a changed host it offers | Built |
| No two instances pull one queue element | `js-jobs/src/store/whole.rs:144-148` (the job's claim), `js-jobs/src/store/dispatch.rs:202` (a dispatch message's claim) | `SELECT ... FOR UPDATE SKIP LOCKED` on the job; on a dispatch message, the same with `claim_until` (section 0.1) | Built (jobs; messages at the store; G19: the route that claims them, `js-lease-server/src/agent_api/dispatch.rs:248`) |
| Queue order | `js-jobs/src/store/queue.rs:20` | a total order of row columns, the same on every instance | Built |
| Job sizing | `js-sched-placer/src/sizes.rs:564` | from the shared tables; the booking is checked in the transaction | Per instance |

### 3.5 The job API

| Decision | Where | How it holds | Tag |
|---|---|---|---|
| A client-chosen job id is recorded once | `js-jobs/src/store.rs:378-389` | `Cond::Absent`; a conflict answers from the stored job | Built |
| A minted job id is unique | `js-jobs/src/store.rs:406-415` | random ids, `Cond::Absent`, retried on a collision | Built |
| A cancel racing a placement on another instance | `js-jobs/src/store.rs:560-584` | `Cond::Version`; the loser writes nothing; the cancel writes its stop message in its own transaction | Built (its stop message at the store; G19: the stop's delivery, `js-lease-server/src/agent_api/dispatch.rs:268`) |
| Job reads, results, logs | `js-lease-server/src/job_api/read.rs:220`, `js-lease-server/src/job_api/logs.rs:239-249` | read from the rows; a follow polls the row, no local wake decides | Built |
| A submit is refused `no-capable-host` | `js-lease-server/src/admit.rs:110` (`can_ever_serve`), `js-lease-server/src/node_rows.rs:57` (`read`), `js-lease-server/src/job_api/submit.rs:264` | judged over this instance's registry and every host's `node/` row not forgotten, so a host that said hello to another instance counts; a failed read of the rows admits (the refusal is never taken on less than the rows). Since #940 a tainted host counts as capable (`js-sched/src/admission_ever.rs`) | Built |
| `ListHosts`, `GET /v1/hosts`, a job's `host_silent_since_ms` | `js-lease-server/src/hosts.rs` (`listing_with_rows`), `js-lease-server/src/job_api/read/silence.rs` (`Silence::read`) | the listings merge this registry's hosts with the hosts known only by their rows; `host_silent_since_ms` is the host's row: the instant its pool's grace ran out after `heard_at`, by the store's clock, the same on every instance | Built (G5) |
| Cordon, uncordon, clear taint through the API | see 3.2; `js-lease-server/src/agents_cordon.rs:86`, `:173`, `:203`, `js-lease-server/src/agents_taints.rs:84` | the cordon and its lift are decided on the store row `cordon/<host>` at the version read (`jobs.cordon_host`, `jobs.uncordon_host`), so a cordon holds on every instance, and each of the three takes the host from the rows first (`take_from_rows`), so a host held by another instance is served | Built |
| Brake reset (`POST /v1/pools/{pool}/brake/reset`) | `js-lease-server/src/brake_api.rs:48`, `js-lease-server/src/scaler/mod.rs:199` | recorded in this instance's memory | Designed (G9) |
| Client tokens, rate and concurrency limits, the catalog cache, validation | `js-lease-server/src/job_api/auth.rs:34`, `js-lease-server/src/rate.rs`, `js-lease-server/src/catalogs.rs:57` | configuration, limiters and caches that only delay | Per instance |

### 3.6 Periodic work and the scaler

| Decision | Where | How it holds | Tag |
|---|---|---|---|
| The job pass (queue expiry, prepare expiry, retention, loss) runs once per period | `js-lease-server/src/job_pass.rs:186-187`, `js-jobs/src/store/duties.rs:42-58` | a due-time claim; each end in the pass is `Cond::Version` (`js-jobs/src/store/ending.rs:94-103`) | Built |
| The time the pass and the duty claims compare | `js-lease-server/src/job_pass.rs:197` (`jobs.now()`), `js-jobs/src/store/duties.rs:46-52` | the store's clock, read once per pass and inside `claim_duty`; `job_pass.rs:176` (`began`) is the pass's own duration | Built (#938) |
| Times stored in host rows (the binding's cordon end; the join-token and credential expiry times are gone with G3) | `js-lease-server/src/agents_cordon.rs:133` | the binding's cordon end is stored on the store's clock (#938, `agent_state.rs` `Skew`); with the job service the cordon itself is a store row `cordon/<host>` whose time to live is the store's (`js-jobs/src/store/cordons.rs:123-128`), so only the binding's copy of the end (`agents_cordon.rs:127`) and the registry's cordon use the process clock | Built (G2, G3) |
| A silent host's jobs are lost (settlement) | `js-lease-server/src/job_pass.rs:130` (`gone`), `:186` (while `gone` is not empty the claim is bypassed and the pass runs regardless), `js-lease-server/src/job_alloc.rs:140` | "absent" is this instance's registry forgetting the host; a host connected to another instance is never heard here, so this instance can lose its live work | Designed (G6) |
| `taint/<host>` rows are dropped once kept | `js-jobs/src/store/taints.rs:76` (`drop_taints`) | each row is deleted at `Cond::Version` of the read, and only while it still names the job whose taint the binding now holds; a row another host job wrote since is left, and a commit that loses a row's version reads the rows again | Built (G7) |
| The scaler runs a step | `js-lease-server/src/server.rs:658` (`ScalerLoop::spawn`), `:689` (`scaler.tick()`), `js-lease-server/src/scaler/mod.rs:189` | every instance with an elastic pool runs its own loop, with no claim | Designed (G9) |
| The scaler's records and unremoved hosts | `js-lease-server/src/scaler/kept.rs:51` | read at start, written whole at `Cond::Any`: two scalers overwrite each other | Designed (G9) |
| The scaler's caps and budgets | `js-scaler/src/budget.rs:88`, `js-scaler/src/caps.rs:97` | judged over this instance's records: N instances spend up to N times a cap | Designed (G9) |
| The scaler's view of hosts and its drain and release | `js-lease-server/src/scaler/live.rs:98`, `:222`, `:271-302` | this registry's hosts; an acquire id drawn per process | Designed (G9) |
| Alerts (webhook, log lines) | `js-lease-server/src/alerts.rs:38` | each instance's own; a duplicate notice, not a decision | Per instance |
| Metrics, TLS and token-file reload | `js-lease-server/src/metrics.rs`, `js-lease-server/src/agents.rs:847-885` (`refresh`: token and client-token reload, `mirror_cordons`, `save_bindings`; the TLS reload is in `js-lease-server/src/tls.rs`, not in `agents.rs`) | each instance's own | Per instance |

### 3.7 Usage

| Decision | Where | How it holds | Tag |
|---|---|---|---|
| A host-hour pack of minute rows | `js-usage/src/store.rs:101-106` | `Cond::Version` or `Cond::Absent`; a conflict keeps the minutes buffered | Built |
| The same host-minute flushed by two instances (a host that moved within the minute) | `js-usage/src/store.rs:156` (`with_instance`), `:260` (`range`) | a minute the hour's pack holds with other figures goes into the instance's own row `usage/node/<host>/<hour>/<instance>` (named by its start, pid and number); a read folds the pack and the instance rows, each figure to the higher (each part's CPU is the host's mean over its part, so a sum would count it twice; a part read twice is taken once) | Built (G11) |
| An ended allocation's history record | `js-usage/src/history.rs:348` | `Cond::Absent`, in the commit that frees the allocation | Built |
| The usage prune | `js-lease-server/src/usage_js.rs:221` (`prune_claimed`), `:232` | the deletes claimed by `periodic/usage-prune` for a day less an hour, so one instance deletes per period; the curves' day and the keys' touches stay each instance's own and run at every instance's prune | Built (G11) |
| Profile curves' eviction | `js-usage/src/profile_store.rs:241`, `:282` (`refresh`) | victims deleted at the version this store scanned or last read; a victim another instance rewrote fails the commit, is read again as just used, and the eviction is planned again (at most eight times); dormant (the server feeds it no results: `js-lease-server/src/usage_hub.rs:103`, `NoKeys`) | Built (G11) |
| The minute buffer, counters, the usage mutex | `js-usage/src/hub.rs:488-490`, `js-lease-server/src/usage_js.rs:154`, `:255` | the process's own; losing one unflushed buffer when an instance dies is accepted (section 6) | Per instance |

## 4. Time

Rule 3 is Built for the job store (#938), and not yet for the marks, presence or usage minutes. The job rows are stamped with the store's clock (`js-jobs/src/store.rs:343`, `ending.rs:147`), but the pass,
the heartbeat and the host rows compare with the process clock (section 3.6), and `Store::now` answers the epoch when the database
cannot be read (section 3.1). G2 makes one clock:

- `Store::now` is fallible (Built, #938: no `try_now` was added; Postgres returns the read's error, SQLite and the fake answer their clocks). Every caller that stamps or compares a shared time takes it from `try_now` and fails the call, writing nothing,
  when it fails. That is rule 4 for time.
- The job pass (`job_pass.rs:197`, `jobs.now()`), prepare expiry, the duty claims (`claim_duty(duty, period)` reads the clock itself, `js-jobs/src/store/duties.rs:46-52`), the whole-host start (`job_whole::serve`, only for a step that stamps a time) and the
  binding's cordon end (`agent_state.rs` `Skew`, the store's clock less the server's, read at start, at each save and before a cordon's commit) take the store's time (Built, #938). (The join-token and credential expiry times are gone with G3.) The whole-host mark's `claim_by` (`js-sched-placer/src/jobs.rs:743`, still the process clock) is replaced by a stored deadline on this clock, by G7, which places over every live host (G8 is deleted).
- The process clock stays for intervals that are the process's own: ticks, backoffs, the hello limiter, the held beat.

One more round trip per pass or request, and none per heartbeat: its `heard_at` is `now()` inside the beat's own write (section 0, G15).

**Load.** Two costs grow with the fleet, not with the instances. Every claimed decision (a placement) holds the commit head from
its start (3.1), so the placements are serialized there, each holds it for one job's rows, and more instances spread the same placements without adding any. The heartbeat is a row write per host per beat interval (section 0, its cost): at the
default 5 s, 100 hosts are 20 writes a second. Neither is measured here; G15's pull request measures the beat's write at the
deployment's host count, and `js-state/tests/postgres_head_queue.rs` exercises writers queued on the head.

## 5. Hosts across instances

Section 0 is the design: a host is its rows, and any instance serves its requests. This section says what each decision about a
host reads.

- **The session** (G15): `node/<host>`, written by the hello at the version it read (a first hello at `Cond::Absent`) and by every
  beat, as section 0 says. A beat on any instance is checked against the row (its epoch and the digest of its session secret) and
  applied by the guarded `UPDATE`; its listing's writes are conditional on the version that `UPDATE` wrote, through #883's
  `Checked` store.
- **Identity** (G3, G4): a host is its certificate. `hosts/binding/<host>` is read by key in the hello's transaction and written at
  the version read. js-server keeps no per-host secret in a row beyond the digest of the session secret (G3 leaves it as it was: the certificate proves the host, and the session string stays the proof of the agent process, with the epoch's `409 stale-epoch`): the session string a beat carries is checked against the row like
  its epoch, and a log upload credential is minted per instance from the host id (section 3.3).
- **Admission** (`no-capable-host`, G5): the live `node/` rows (heard within grace by the database's time). The refusal is never
  taken on less than the rows.
- **Host listings and `host_silent_since_ms`** (G5): the `node/` rows.
- **`HostLive`** (G5, G15): a hello of a host whose row was heard within grace is refused as today, judged on the row, so every
  instance answers alike: on the refreshed record before anything is written, and again in the row's own write. A hello of the same
  agent process (its welcome lost) takes the host over at once, through the optional hello field `Hello::agent_process` (`js-agent-wire/src/hello.rs:349`),
  judged at `js-registry/src/registry/admit.rs:103-107` and in the row's write (`js-jobs/src/store/nodes.rs`, `live_elsewhere`); it is built (G15). A new agent process, as after a restart, waits out the grace as now.
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
| A host enrolled in hostkeeper holds one per-host certificate the coordinator controls; no join tokens, no token passing, no per-service certificates, no revocation machinery (a compromised host is unplugged) | the certificate is the identity of every agent request (#922); G3 deleted the token and host-credential paths instead of making them N-safe | Built (the certificate and G3, the removal) |
| No agent stickiness beyond TCP keep-alive plus a server-side connection max age | an agent keeps one connection, idle for at most 8 s (`IDLE_LIMIT`, `js-client/src/client.rs`, komira-ai/jobscheduler#1146); the server bounds its life by `max_requests` (the `Connection: close` of the 100th answer, about 100 beats) and closes it after 10 s idle (`header_timeout`), so an agent moves to whichever instance the edge picks at least once in 100 beats. A job client may keep one, so the job listener closes a connection older than `client_connection_max_age` (a config key with a default) at the end of a request | Built (agents: keep-alive bounded by `max_requests` and `header_timeout`, no age key); not needed for job clients, since any instance serves them (G10 deleted) |
| The job-store cutover is a short pause with a dry-run copy, and a Postgres-to-SQLite export for rollback | #874 shipped the first start over Postgres with an empty job store ("drain the farm"); the tools came after it: `js-lease-server job-store copy` (`js-lease-server/src/cutover.rs:161`) reads `<state file>.jobs` and reports per key prefix a row count and a SHA-256 and what the store lacks, holds the same or holds otherwise, writes nothing without `--write`, and writes only rows the store lacks; `job-store export` (`js-lease-server/src/cutover.rs:191`) writes the store's job store rows and usage rows to two new SQLite files a server without `--store` runs on; the round trip is `js-lease-server/src/cutover_tests.rs:496` (SQLite) and `:526` (Postgres) | Built (G12) |
| A hello with the same credential and agent instance takes the host over at once | in section 0 no instance owns the host, so the hello is judged on the row on any instance; the same agent process is recognised by an optional hello field naming it (`Hello::agent_process`, `js-agent-wire/src/hello.rs:349`; judged at `js-registry/src/registry/admit.rs:103-107`); a new agent process waits out the grace, as today | Built (G15) |
| Losing one unflushed usage buffer when an instance dies is accepted | the minute buffer flushes every few minutes (`js-usage/src/hub.rs:448`) | Built |

## 7. Compatibility (B8)

Every change of section 9 is additive but G3's. New key prefixes: `node/` (G15; beside, and not under, the usage rows' `usage/node/`),
`pool/` (G5), `hostmark/` (G7); new optional fields in the `node/` value (G15) and a stored deadline in the `mark/<job>` value (G7); the messages `dispatch/<host>/<seq>` (G19);
a usage row per instance beside a host's hour pack (G11, Built; a release before reads the pack alone and misses only the other instances' parts of a minute two instances measured, as today); new scaler rows under `acquires/scaler/` for a launch and for a pool's brake (G9). No
existing key is renamed, re-meant or written in a new shape. G15 adds two optional wire fields, the beat's sequence within its
session and, in the hello, the agent process; an older agent does not send them and an older server ignores them; they are named as
a change of the wire in that pull request. `NOTIFY` (G16) carries no stored format. G19 adds `POST /v1/agent/dispatch`, its acks, a hello capability and `dispatch_lease_secs`, and changes what a beat's reply
carries for an agent that polls; its pull request names that change of the wire and the public interface. It is additive: an agent
that does not poll is served as today. The test of section 0.1's version skew is G19's. The binding and scaler-record rows keep their values; what changes is
that they are written at the version read, which a reader does not see. An older release ignores the new prefixes. G3 is the one change
that removes, and it is built: by the owner's ruling it deleted the join-token mint and revoke, the host credential a welcome
carries and the agent's token and credential flags, and refuses an agent request without a certificate. The flags that a deployed
unit or a deployed server still passes (`--agent-token-file`, `--require-host-credential`, the agent's `--token-file`,
`--instance-file` and host-credential path flags) stay accepted, hidden from help, and are not read: each one given prints one warning at start.
The wire drops `Welcome.host_credential` and `Hello.accepts_host_credential` (`js-agent-wire`, both `serde(default)` and skipped when empty:
an old agent's extra field is ignored by the new server, and an old server reads the absent field as false); the two names are retired and
must not be reused. The change landed once every deployed agent presented a certificate (the Macs moved first: `docs/runbooks/mac-host-certificate.md`).
It stops reading and writing `hosts/join/` without deleting any of its rows, so a release before it still finds them.

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
`js-state/tests/claim_decide.rs::writers_racing_for_one_row_place_it_once`; a start beside an instance that changes a job
before every call of the open is
`js-lease-server/src/job_store_busy_tests.rs::a_server_opens_its_job_store_while_another_changes_jobs_without_a_pause`. The whole update is
`js-lease-server/tests/server/live_update_load.rs`: `a_rolling_update_of_two_servers_under_load_fails_no_job` (two
start-first rolls under load, each old server signalled inside a placement it pulled and inside a dispatch claim),
`a_server_turns_ready_while_the_serving_server_changes_jobs_without_a_pause` (the new server starts and answers `/readyz`
while the old one changes a job before every store call the new one makes) and
`a_server_killed_mid_dispatch_leaves_the_start_to_the_other_after_the_lease` (the crash case of section 0.1).

## 9. The gaps, one pull request each

Each lands after this document, is one subject, changes neither the replica count nor the update order (G19 adds the dispatch route), and turns its rows of section 3 to Built with paths.
They land in the order of section 12's "Order of safety" (the gaps an update's overlap reaches today first), then in the table's order.

| # | Under section 0 | Pull request | Closes (section 3) | After |
|---|---|---|---|---|
| G2 | Kept: section 0's deadlines by `now()` | **One clock** (Built by #938; no `try_now`: `Store::now` itself is fallible): the store's time for every shared deadline, expiry and due time. Left to other gaps: the whole-host mark's `claim_by` (G7), presence (G5), usage minutes (G11) | `Store::now` epoch; the pass's clock; host-row times; the whole-host start's clock | none |
| G3 | Built: the certificate is how any instance knows the host | **Host identity is the host certificate** (Built): the one-time join tokens (their mint, revoke and use-once paths, the cap of waiting tokens), the pool tokens of `--agent-token-file`, and the host credentials a welcome issues and the tick renews are deleted; an agent request without a verified host certificate is refused by the one middleware of the agent API (`js-lease-server/src/agent_api.rs:179` `authenticate`, code `host-certificate-required` in `js-lease-server/src/agent_api/host_cert.rs:25`); the agent's token, instance and credential flags are accepted, hidden and ignored, and an agent started without `--host-cert` and `--host-key` refuses to start. Nothing replaces the tokens: the certificate names the host (#922), and no per-host secret is kept in a row for admission (section 5). The scaler's mint and revoke of a launched machine's token are deleted; the scaler is off on the deployment, and nothing replaces the launch token until G9 launches machines with certificates. An in-process caller (the simulation cell, the CPU replay) presents a host with `js-lease-server/src/in_process.rs` `as_host`, which inserts the extension the TLS listener inserts for a verified certificate. komira-ai/jobscheduler#935 (Built) read a token's row by key meanwhile; that went with the tokens | 3.2: a host is known by its certificate; 3.6: the token times | komira-ai/jobscheduler#1118 and the pull request of this change |
| G15 | New: section 0's heartbeat | **The heartbeat is a row `UPDATE` on any instance**: the session in `node/<host>` (section 0's fields), written by the hello at the version read and by every beat; a beat guarded by its (epoch, sequence), its listing's writes conditional on the version it wrote, through #883's `Checked` store; two optional, additive wire fields, the beat's sequence and, in the hello, the agent process (so the same process takes the host over at once), the agent sending them first; for an agent that sends no sequence, a confirmed container freed only after two applied beats in a row omit it; the first-listing and listing-clean flags as `node/` columns; the reply built from the rows; no `409 unknown-host` or `403 session-mismatch` for a host the row knows; the registry refreshed from the rows. It measures the beat's write through the `Store` against a plain `node` table and takes the cheaper one that keeps the guard (section 0, its cost). komira-ai/jobscheduler#883 is closed in favour of this pull request, which takes its `Checked` store and its tests; `docs/design/jobscheduler-design.md` section 10.1 amended | 3.3: which connection commits a listing, first listing, the session, listing-clean; the lost whole-host start (was G14) | G2, G3 |
| G4 | Shrunk: no bindings save is left to make conditional; each change of a binding is its own conditional write where it is decided. Built: the hello's binding write at the version read, the hello judged first, the save per host at the version held, the taint kept across servers, cordon and clear taint from the rows (3.2, 3.5); Designed: what goes with G3, and the `taint/` drop | **Host bindings are rows, and after G3 only the taint**: until G3, `HostBound` is judged on the binding row read in the hello's transaction (`Cond::Absent` for a first hello, `Cond::Version` otherwise) and a refused hello is judged before its binding is written and the judged hello is taken after it without a second judgement, so there is nothing to undo; after G3 the certificate names the host, so `HostBound`, the binding's credential and its copy of the cordon end (`cordon/<host>` is the cordon) go, and the row keeps the taint; the registry holds no bindings to save, so `save_bindings` goes; cordon, uncordon and clear taint take the host from the rows, so a host served by another instance is not `404` (`js-lease-server/src/agents_cordon.rs:61-64`, `agents_taints.rs:86`); `taint/<host>` dropped at the version scanned. Built by komira-ai/jobscheduler#935: the save deletes only a binding this registry held and let go (`js-lease-server/src/agent_state.rs:358`), and the hello of a host the registry does not know reads its binding row first (`js-lease-server/src/agents.rs:572`) | 3.2: host bound, undo, saved, taint seen, cordon's `404`, clear taint (Built); 3.5: cordon and clear taint through the API (Built); 3.6: `taint/` drop | G3 |
| G5 | Shrunk: presence is the beat's own `UPDATE` (G15), so no presence write or `presence_refresh` of its own; what is left is the readers | **Decisions read the `node/` rows**: admission, host listings, `host_silent_since_ms`, `HostLive` (judged on the row; the same agent process by G15's hello field) and `PoolFull` (under `pool/<pool>`, counting the rows not forgotten) read the `node/` rows (section 5) | 3.3: `HostLive`, `PoolFull`; 3.5: admission, listings | G15 |
| G6 | Kept, simpler: silence is `heard_at`, so the local `gone` map and the claim bypass go; any instance that claims the job pass runs it | **Settlement by rows**: the claimed job pass loses a host's allocations by `node/<host>`'s `heard_at` and version, never by the local `gone` map; the claim bypass goes | 3.6: settlement | G15 |
| G7 | Kept, over every live host, on any instance | **Placement re-checks the host in its transaction, over every live host**: the pass builds the engine's hosts (`host_up`, `heard` with the load, `OpenHost`) from the live `node/` rows it reads before each pass, not from the hosts this instance serves, and the pass's skip rule (`jobs_seen.rs`, engine change counts) counts a changed row, not a local call; `place_claimed` reads the host's binding (taint; the cordon is Built, see below), `taint/<host>`, the scale-down mark and the ports of the host's live allocations in the claimed transaction and refuses the host; the scale-down mark becomes a row `hostmark/<host>`, written at `Cond::Version` by whoever marks, the registry mark its cache | 3.4: contained beside host job, taint, ports, scale-down mark, which hosts an instance places on (the cordon check is Built: `js-jobs/src/store/whole.rs:211-217`, `:317-318`) | G4, G15 |
| G16 | New: section 0's wake | **LISTEN/NOTIFY wakes placement and the host's poll**: a submit, a listing's free and a cancel `NOTIFY` the queue channel after their commit; a dispatch message `NOTIFY`s the host's id on the host channel; each process `LISTEN`s on one connection of its own and wakes every waiter once when that connection is made again; the poll stays the guarantee (`js-state/src/postgres/mod.rs:25`). Tests: a submit on one instance is placed before the next tick; a dispatch message ends a poll held on another instance before its hold; with notifications off, both happen at the next poll. Built (section 0's wake names the code). The tests: `js-lease-server/src/server_listen_tests.rs` (a job another server queued is placed and started on a beat held here; a start another server wrote ends a beat held here, no tick in either), `js-lease-server/src/agent_api/dispatch_notice_tests.rs` (a host notice through Postgres ends a poll held on another server, the recheck set past the test; the listening connection made again wakes a held poll to what it missed), `js-lease-server/src/listening_tests.rs` (without a notice, the job another server queued is placed at the tick) with `a_held_poll_finds_a_message_another_server_wrote_at_its_recheck` (`js-lease-server/src/agent_api/dispatch_tests.rs`, the message at the next recheck), `js-state/tests/postgres_notify.rs` (the transport) and `js-jobs/src/store/wake_tests.rs` (which commits notify) | 3.3: held beats (a placement elsewhere waits for the next beat) | G15 |
| G19 | New: section 0.2 | **The dispatch queue and its route**: the messages `dispatch/<host>/<seq>` (a start written by the placement transaction, a whole-host job's by its move to starting; a stop by a cancel, an expiry or a loss, in their own transactions; Built at the store, `js-jobs/src/store/dispatch.rs`); the long poll `POST /v1/agent/dispatch` on the agent listener, any instance (claim with `SKIP LOCKED` and `claim_until` of `dispatch_lease_secs`, wake on the host's channel); acks by allocation id in the next poll, idempotent; on SIGTERM an instance claims no more messages, answers the polls it holds and commits, then exits; a hello capability by which an agent says it polls, the beat's reply still carrying starts and stops for an agent that does not (Built, the server's side: the route `js-lease-server/src/agent_api/dispatch.rs:97`, the config keys `server.dispatch_lease_secs` and `server.dispatch_recheck_ms` `js-lease-server/src/file/server.rs:327`, the held poll's wake in process with a recheck, and across instances by G16's `NOTIFY`; the wire is `js-agent-wire/src/dispatch.rs`); `js-agent` polls dispatch and acks (Built: the poller `js-agent/src/dispatch/poller.rs` and its thread `js-agent/src/dispatch/serve.rs`, the route the session hands it at each welcome that says `dispatch_poll` `js-agent/src/dispatch/cell.rs`, the hello capability `dispatch-poll` `js-agent/src/contract.rs`; the agent's start is idempotent on the job id already, `js-agent/src/lifecycle/mod.rs:12-25`). A change of the wire and of the public interface, named in its pull requests. It measures start latency, placement to ack, p50 and p90, with and without the poll. Tests: two instances never hand out one message at once; a message claimed by an instance that dies is handed out again after `claim_until` and the agent's second start changes nothing; an ack on another instance deletes it; a stop for a cancelled job reaches the host by the same queue; a held poll is answered on the host's notification; an instance given SIGTERM answers its held polls and claims nothing more; the release before hands out a message the new placer wrote; an agent without the capability gets its start in the beat's reply; a job the release before placed reaches a polling agent through the beat's reply | 3.5: cancel racing a placement (its stop) | G16 |
| G11 | Kept, and more needed: with any instance taking any beat, a host's minute is split over instances as a rule, not only at an update | **Usage rows and the import** (Built by komira-ai/jobscheduler#1140): the import's loser reads the marker and starts; a host-minute flushed by two instances keeps both: a minute the hour pack already holds with other figures is kept in the writing instance's own row beside the pack (a new key under the host's hour, named by the instance's start, pid and number), and a reader folds the hour pack and those rows, each figure to the higher (a sum would count a host's CPU twice), so a flush retried after a lost reply never counts twice; the usage prune claimed by `periodic/usage-prune`; profile eviction at the scanned version; the stale "one writer" doc in `js-usage/src/store.rs` | 3.1: import; 3.7: minute, prune, profiles | G2 |
| G9 | Kept: the scaler is off on the deployment, so it lands before the scaler is turned on, not before an update | **The scaler under the rule**: one step per pool and period by a duty claim (`periodic/scaler/<pool>`); its records and unremoved set written at the row's version, a conflict re-reading and re-deciding the step; each launch a row at `Cond::Absent` before the provider call, its acquire id derived from that row so a repeated call is the same machine; caps and budgets judged in the transaction that writes the launch row, under the pool's scaler row lock; the launch row at `Cond::Absent` and the acquire id derived from it make a repeated provider call the same machine; the brake a row, its reset a conditional write; a release written to `hostmark/<host>` before the provider release; a machine it launches joins with a host certificate, since G3 removed the launch token, so the scaler stays off until a launched machine can be enrolled, as it is off on the deployment today | 3.6: scaler step, records, caps, host view; 3.5: brake reset | G2, G3, G7 |
| G12 | Kept: the cutover is not about instances | **Job-store cutover tools** (section 6): a dry-run copy of `<state file>.jobs` into the store with counts and a checksum per prefix, and an export of the store's job and usage rows to SQLite files (the job store's rows to one, the usage rows to the other), with the round trip as its test. Moot for a deployment whose job store is already in Postgres with nothing to bring back. Built: `js-lease-server job-store copy` and `export` (`js-lease-server/src/cutover.rs`), runbook `docs/runbooks/state-database.md` section 6 | section 6 | none |
| G1 | Shrunk into G15: no connection owns a host, so #883's rule that the newest hello owns it and its `503` on a racing hello go; its mechanism, every listing write conditional on `node/<host>`, is G15's guard | ~~Finish komira-ai/jobscheduler#883~~: its `Checked` store and its two-server test (`two_servers_serve_one_store_place_each_job_once_and_keep_to_the_hosts_cores`) move into G15, and #883 is closed in favour of G15 | (now G15) | none |
| G8 | Deleted: two instances draining two hosts for one whole-host job is churn, not a wrong answer (the rows stay consistent), and the queue note is cosmetic (B20) | ~~Whole-host drain and queue notes without churn~~ | 3.4 | none |
| G10 | Deleted: an agent keeps no connection, and any instance serves any job client, so a connection age only spreads load (B20: no machinery without a need); a draining instance closes its connections (section 12) | ~~Job-client connection max age~~ | section 6 | none |
| G13 | Deleted: the edge's TCP check, the agent's retry and a stop grace that covers one dispatch and the drain hold are enough; nothing is cut mid-flight (section 12) | ~~Readiness the edge can reach~~ | section 12 | none |
| G14 | Deleted: a start is a function of the rows, so the next beat on any instance carries it again (G15), and with no hello on a change of instance the first-listing rule, a stored `node/` flag cleared in the beat's own writes, runs only after an agent restart, whose listing is the agent's whole account (`per_job.rs` `starts_for` sends a `starting` whole-host job's start on every beat until the host lists it, so a lost reply is sent again) | ~~A start whose reply was lost is sent again~~ | 3.3 (now G15) | none |
| G17 | Deleted: no race needs a per-instance active/draining row or a placer lease (section 0.1, the races considered) | ~~Instance rows and the placer~~ | section 0.1 | none |
| G18 | Deleted with G17 | ~~The operator drains and lists instances~~ | section 0.1 | none |
With G2 to G7, G9, G11, G15, G16 and G19 Built, any number of instances is safe in steady state and through an update's
overlap from this repository's side; the stop grace is the deployment's.

## 10. Supersession (B22)

- This text replaces the earlier draft of this file (komira-ai/jobscheduler#873, never merged), which designed presence rows,
  a `hostbook` gate and a placement lock order of its own before #877 to #881 built placement and the duties differently. What
  it found that still holds is in section 3 and section 9.
- [deployment.md](deployment.md), "(b) later" and "Single active writer, not leader election": a second replica needs no leader, no
  election and no fence; this document is the design. That file gets a pointer here.
- [jobscheduler-design.md](jobscheduler-design.md) section 10.1, "a heartbeat is not a write", is amended by G15 in its pull request.
- [configuration.md](configuration.md), "What it keeps": G3 has removed the tokens and host credentials from that text and G4 makes
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
- **G3 removed the tokens instead of making them N-safe.** The owner ruled that an enrolled host's certificate is its one
  identity, with no join tokens and no revocation machinery, so the token rows get no conditional writes and no cap lock.
- **One service, two rules for the queue** (the owner's ruling, section 0.1). All the scheduling logic stays in js-server. No two
  instances pull the same element, and an instance given SIGTERM stops pulling and finishes what it pulled. No instance rows, no
  placer lease and no edge flip: each race considered for them is harmless (section 0.1), and B20 adds machinery only for a race
  that needs it.
- **Dispatch is pulled by the agent, not pushed to it** (section 0.2). A push would need an inbound listener on every host and a
  per-service certificate for js-server, which the owner ruled out; the pull, woken by `NOTIFY`, is as fast. It is a route of the
  agent listener and goes to any instance.
- **The live hazards go first.** Section 12's list is what an update does to the deployment today; G15, G4, G5, G7, G19 and G11 close
  it, and G3 closed the token hazards. komira-ai/jobscheduler#935 (Built) landed parts of G3 and G4,
  and the drain section 12 relies on; each row says which part.
- **The standard shape (section 0) replaces sessions held by one instance.** The project owner found the earlier text too
  complicated for something fairly standard. With the heartbeat a row `UPDATE` on any instance, #883's connection ownership (G1),
  the connection max age (G10), the lost-start rule (G14), the whole-host drain rule (G8), readiness for the edge (G13) and the instance rows (G17, G18) go;
  presence (G5) and the bindings (G4) shrink. Section 9 says which, and why.
- **G12 is built** although #874 shipped without it, because the owner's decision asks for it. A deployment that has no
  SQLite job store to move needs only the export.

## 12. Updating js-server live

The project owner's requirement: a control-plane service is updated live, with no moment at which no instance serves and no run
stopped by the update.

**How agents and clients reach an instance.** Agents (the agent listener) and job clients (the job listener; the build frontend
among them) connect through the edge proxy in front of the service, outside this repository. It passes TCP through, so TLS and a
host's client certificate reach the server unchanged, and it sends each new connection to a task it believes up. Today it believes
a task up while a TCP connect succeeds: `/readyz` (`--ready-listen`) is served on a loopback address only
(`js-lease-server/src/config.rs:100-105`, enforced by `js-lease-server/src/config/listen.rs:55`), so the edge cannot see a drain.
An agent keeps one connection between requests, idle for at most 8 s, and each request has a time limit (`js-client/src/client.rs`,
`build_agent`); the server ends the connection after 100 answers (`max_requests`) or 10 s idle, and when it drains, so an agent
is held to one instance for at most that; a job client may keep its connection.

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
   usage tick, the log grants) and a placement pass that places nothing, because the instance is draining (#935, `js-lease-server/src/sched/jobs_pass.rs:143`); then the usage side's last flush (`server.rs:713-718`).
   The scaler does not run in it. Two hazards below come from this tick.

An agent whose connection was closed or refused retries with its backoff. Today the instance it reaches holds no session for it, so a
beat is answered `409 unknown-host` or `403 session-mismatch` and the agent says hello there (`js-agent/src/session.rs:32-45`); after G15 it serves the beat from the `node/` row and no hello is repeated.
Its jobs run on throughout: an agent keeps its jobs without a server for up to `--silence-stop` with no accepted reply (300 s by
default, `js-agent/src/cli.rs:311-316`), far more than the drain hold and drain timeout (25 s), and on the server's side silence
ends nothing before grace plus retention ([absence.md](absence.md)).

**The update, by the two rules of section 0.1.** Heartbeats, the job API and dispatch go through the edge to any instance, old or
new, all the time; nothing is routed by version, and no state lives in an instance.

1. The new release's instance starts and serves at once; it places and dispatches as soon as it runs.
2. The deploy sends the old instance SIGTERM. It stops pulling: no more claims of queued jobs or dispatch messages. It finishes what
   it pulled: a placement already in its transaction commits or rolls back, and the dispatch polls it holds are answered and their
   claims committed. Then it exits. The drain of #935 (Built) closes its agent listener at the signal, so agents move to another
   instance at once.
3. The stop grace covers one dispatch and the drain hold, so the instance is never killed mid-flight (a deploy setting, checked once
   at deploy: a grace above `--drain-hold` plus `--drain-timeout` plus the final tick).

An instance killed anyway, or one that crashes, leaves what it held to the crash case of section 0.1: its transaction rolls back,
and a claimed message is handed out again after `claim_until`, to an agent whose start is idempotent. Old and new pull side by side
for seconds, so the messages and the agent API stay compatible across one version (section 0.1, version skew).

**Hazards of the overlap that are live now:**

| During the overlap | Effect | Closed by |
|---|---|---|
| A submit reaches the new instance before the hosts have said hello to it | refused `no-capable-host` (`422`) for good, though the hosts serve on the old instance | closed for hosts whose rows the old instance wrote (G5, G7: admission and placement read the `node/` rows); a host that said hello only to a release before this one has no row |
| Either instance's tick saves a binding that differs from the copy it last wrote (a lapsed or mirrored cordon end, a forgotten host) | the whole binding row is put or deleted at `Cond::Any` from that instance's copy, so a taint the other instance stored in it is lost, or a host it holds loses its binding | closed: G4 (the delete of a binding never held, and the taint read at a hello: Built by komira-ai/jobscheduler#935; the put is conditional on the version read and a lost version keeps the row and takes its taint, `js-lease-server/src/agent_state.rs:435`) |
| A host job placed by one instance is a taint in that instance's memory, and the host's beats move to the other | the other may place a contained job beside the host job | closed (G7: placement checks `taint/<host>` and the binding in its transaction); G4 for the binding's writes |
| An agent's taint clear reaches the instance that holds no taint for the host (a beat after the update's start) | before #940 it was acknowledged and changed nothing, and the stored taint stayed until a later start put it back on a clean host | closed: komira-ai/jobscheduler#940 (Built, `js-lease-server/src/agent_state.rs:686`); G4 for the rest |
| Both instances run placement, in the placement poller that submits and heartbeats poke and in the tick, each over the hosts it serves | each placement is claimed and booked safely (Built), but two engines walk the queue, drain hosts for whole-host jobs and write queue notes from different views | not a gap: each placement is claimed and booked (rule 1); the rest is churn (G8 deleted) |
| A host has a connection on each instance for a moment (a beat in flight to the old one when the agent reconnects to the new one) | the old connection can commit a listing over the new one's | G15 |
| A host that joined by token (the Macs, before G3) presented a host credential or one-time token the other instance issued after it started | the other reads its tokens at start, and a token it does not hold is answered `401` (the agent exits; `401` is fatal) until #935 below; one token can also admit two hosts | closed: G3 (Built) removed the tokens, so no token or host credential is issued by one instance and presented to another. komira-ai/jobscheduler#935 (Built) had read an unknown token's row by key meanwhile |
| An agent that said hello to the new instance reaches the old one again (each new connection may; an agent keeps its connection for up to 100 requests and opens a new one after 8 s idle; since komira-ai/jobscheduler#935, only until the old one's SIGTERM) | its beat is `403 session-mismatch`, and its hello there is `409 host-live` while the old record is within its grace, so the agent backs off | G15 (no session to mismatch; the same agent is applied at once) |
| Both instances admit hosts new to a pool | the pool's cap counts per instance | closed (G5: the cap counts the `node/` rows under `pool/<pool>`) |
| `ListHosts` and `GET /v1/hosts` on either instance | each lists only the hosts whose session it holds | closed (G5: the listings read the `node/` rows) |
| A whole-host job's start rides on a ready report's reply that is lost (a request cut by the drain, a killed process) | the next first listing ends it `Lost` without its having run | G15 (was G14), and G19: an unacked dispatch message is handed out again |
| A host's minute of usage is recorded by both instances (it moved during the minute, and the old instance flushes at exit) | the later flush replaces the minute, so up to a minute of the host's usage rows is lost | G11 (Built: the later part goes into the instance's own row and a read folds both) |
| New connections reach the old instance through its drain hold, and are refused by it once it closes its listener, until the edge notices | an agent retries; a job client's request fails and is retried by the client | G13 (komira-ai/jobscheduler#935, Built, closes the agent listener at SIGTERM, so agents move at once, answers held beats at once and places nothing while draining; nothing more is needed, G13 deleted), and a stop grace above `--drain-hold` plus `--drain-timeout` plus the final tick, checked once at deploy |

Not live in an update: settlement by the local registry (G6) needs a host unheard for grace plus retention, far longer than an
overlap; the clock (G2) is one clock while both tasks run on the one node; the scaler (G9) is off on the deployment; the import and prune parts of G11 are economy; G10 is deleted.

**Order of safety.** Each step is relied on only once the one before is Built:

1. Every write conditional and every duty claimed (section 2, Built): two instances never place one job twice, book a host past
   its room or run a duty twice.
2. The live hazards closed: G15, G4, G5, G7, G19 and G11's minute fold, with G2 and G3 before G15; G3's removal of the tokens (built, after
   the Macs held certificates). komira-ai/jobscheduler#935 (Built) landed parts of G3 and G4, and the drain, ahead of them.
3. The stop grace covers one dispatch and the drain hold, a deploy setting checked once at deploy.
4. The rest of section 9 (G6, G9, G16, the rest of G11), which matters for N instances in steady state or for a long overlap, not for one update. G16 is Built (section 0's wake).

# A job's retry policy and its completion callback

Companion to [jobscheduler-design.md](jobscheduler-design.md), sections 3.2 (jobs), 5.3 (submit, place, cancel), 5.4 (whole-host
jobs), 6.3 (presence and loss) and 6.7 (restart recovery), and to [live-logs.md](live-logs.md). Status: **decided, not built**. The
owner of the public job API has answered every question this page raised (section 0). Nothing here is built yet. The page changes the
public job API (`crates/js-proto/proto/jobscheduler/v1/job.proto`, `service.proto`, `usage.proto`).

Two features are missing today:

1. **A retry policy.** A job runs at most once. A job that fails to start, or whose host is lost, ends there, and the client must
   submit a new job. No field asks the service to try again.
2. **A completion callback.** A client learns that a job ended only by polling `GetJob` (or `ListJobs`). Nothing calls the client, and
   there is no webhook and no watch call.

This page specifies both, how they meet, how they stay compatible with released clients, agents and servers, how they fail, which tests
prove each rule, and how the work splits into tasks that can run in parallel.

## 0. Decided

The API owner decided each question below. The rest of the page is written to these answers.

- **C1. When the callback is sent.** Each job gets one callback, sent only after its retries are exhausted, once the job reaches its
  final state: succeeded, failed with no retry left, or cancelled. A job never gets one per attempt (section 6).
1. **The service retries: yes, opt-in.** Section 2.2 of the design used to list "being a workflow engine" as a non-goal and say that
   "retry policy and result interpretation belong to the client". This pull request changes that line to: result interpretation belongs
   to the client, and the service retries only as a job's retry policy asks, which is never by default.
2. **SubmitJob's promise stays.** "A retry after a lost answer never runs the work twice" remains true of the submit. `RetryPolicy`'s
   comment says that each attempt runs the work again. It also names the one case where two attempts can overlap: a `static` host an
   operator removed while it was still running the job (section 3.6).
3. **Which outcomes retry: start failure and host lost, and nothing else.** A client chooses no conditions. A non-zero exit, a wall
   limit, a memory stop or an input failure while the job ran never retries. A cancel, `NO_CAPACITY` and `EXCLUSIVE_NOT_READY` never
   retry either (section 3.3).
4. **The callback is a webhook** that the server POSTs to. No watch stream and no long poll are added (section 5.1).
5. **Authentication: the submitter's key, with no allow-list.** The submitter gives the callback URL and a signing key with the job.
   The server signs each call with that key in the Standard Webhooks form, so only a holder of the key can verify it. No per-client URL
   prefixes and no per-client key file exist. The key is a secret the job carries (section 5.2). The server guards itself against a URL
   that points inward with an address rule (section 5.3).
6. **Names.** The new names are `RetryPolicy.max_attempts`, `Job.attempt` (the number of the current attempt, from 1) and
   `Job.attempt_history`. Field 34, `Job.attempts`, keeps its meaning: placements and machines refused for the job. Its comment adds
   "over all of the job's attempts".
7. **Bounds.** By default a job has no retries. The server sets no ceiling on `max_attempts`; the type's own range (`uint32`, up to
   4,294,967,295) bounds it. A callback's delivery deadline is cut to the job's retention end.
8. **The callback is a bare notification.** Its body is empty. It carries no job id, state, outcome, labels or paths: any content is
   surface for confusing the receiving service. The receiver knows which job a call is for from the URL it chose itself, and can put
   its own reference in that URL. It checks the signature, then reads the job with `GetJob` using its own token (sections 5.4 and 5.6).
9. **Every attempt's logs are kept, in v1.** The attempt goes into the live-log key, and `GetLogsRequest` gains `attempt`, defaulting
   to the latest (section 3.8). Same-host retries are then safe for the logs. The rule that a retry never returns to a host that held
   the job is replaced by a shorter bar, kept for the reasons section 3.5 gives.

## 1. What a client sees

Before:

- A job ends after one run (`SUCCEEDED`, `FAILED`, `CANCELLED`, `LOST`, `FAILED_TO_START`), and the service never runs it again.
- A client learns of the end by polling. kbf calls `GetJob` for each running job every `--executor-poll-ms` (default 1000 ms).
  `js-client`'s `wait_for_terminal` polls every 2 s (`crates/js-client/src/jobs/loops.rs`, `POLL_EVERY`).

After:

- A job may carry a `retry` policy: up to `max_attempts` attempts, and a backoff between them. An attempt that failed to start for a
  cause of its host, or whose host was lost, starts the next one. Between attempts the job is live again: it is `QUEUED`, with queue
  status `WAITING_FOR_RETRY` until its backoff ends. `Job.attempt` names the current attempt, and `Job.attempt_history` lists the
  attempts that have ended, with each one's host, times and outcome. `GetLogs` reads any attempt's logs.
- A job may carry a `callback`: a URL and a signing key. Once the job reaches its final state, after its retries are exhausted, the
  server sends one signed POST with an empty body to the URL. The call is retried until the receiver answers 2xx or the delivery
  deadline passes. Its progress is stored in Postgres and survives a server restart, and `Job.callback_status` shows it.

What does not change:

- A job without `retry` runs once, exactly as today. Only a job whose submitter set `retry` can go from a run back to `QUEUED`; a client
  that never sets it never sees that move. A job without `callback` is never called back.
- Polling keeps working and still says the truth: a job that will be retried is live, and a job that has ended has its final state.
- The wall limit, the start timeout and the pool's maximum queue wait bound each attempt, as they bound the one run today. The retention
  window counts from the job's final end.
- The placement-time requeues stay as they are: a failed whole-host prepare, and a start refused `host-held`, `no-capacity` or
  `bundle-pending`. None of them is an attempt (section 3.2).
- A host that is only silent still ends nothing (section 6.3 of the design), so silence never starts a retry.

## 2. What the design rests on (merged code today)

- **States and moves.** `JobState` and the legal moves are `may_move` in `crates/js-jobs/src/record.rs`. An ended state is terminal.
  Moves back to queued (the requeues) are store operations of their own and not entries of `may_move`. Nothing moves an ended job back
  to queued, and `job_loss.rs` says a lost job is never revived.
- **Ends funnel through one transaction.** Every end goes through `JobStore::end_job` and `end_txn` (`crates/js-jobs/src/store/ending.rs`).
  That one commit writes the job row at the version read (state, result, `retention_ends`, files digested), moves the allocation to
  releasing, and queues or deletes the stop. Ending a job that has already ended writes nothing.
- **Lost has two paths** (`crates/js-lease-server/src/job_loss.rs`). `lose_absent`: the provider reads the host absent, every live
  allocation on it is freed, and its jobs become lost in the same commit, whether they were placed, starting or running. `lose_unlisted`:
  after a hello, the host's first listing leaves out a starting or running job (the agent restarted, for example for an update without
  a handover). A host that returns after the provider read it absent is seated as awaiting and takes no start until its listing is clean.
- **Requeues before a run.** `requeue_failed_prepare` (`crates/js-jobs/src/store.rs`) and `fail_prepare_with`
  (`crates/js-jobs/src/store/whole.rs`) handle a failed whole-host prepare, bounded by `max_replace` (3). `held_or_end`
  (`crates/js-lease-server/src/job_held.rs`) handles the refusals `host-held` (counted, at most 3), `no-capacity` (counted, at most the
  pool's `capacity_requeues`) and `bundle-pending` (not counted; only the pool's `max_queue_wait` bounds it). Each bars the host in the
  record's `prepare_bars`, which is the existing way to keep a job off one host.
- **Counting.** `Job.attempts` counts placements and machines refused for the job (`crates/js-sched/src/queue_status.rs`), and a requeue
  is not one. `Job.events` keeps the newest 32 events.
- **One allocation per job at a time**, keyed `alloc/<job id>`. Its history row (`usage/ended/...`, one `UsageRecord` per booking) is
  written in the commit that frees it (`crates/js-lease-server/src/job_alloc.rs`).
- **Late reports.** A listed result names its allocation (the agent wire's `JobResult.allocation_id`,
  `crates/js-agent-wire/src/per_job.rs`). `placement_of` and `commit_listing` (`crates/js-lease-server/src/per_job.rs`) already answer a
  result whose allocation is not the job's live one, or whose end the store refuses as an illegal move, with `Ack::Stale`, and change
  nothing.
- **Idempotency.** `JobSpec::same_job_as` (`record.rs`) names every field of the spec, so a field added to the spec does not compile
  until it is either compared or deliberately left out. Files are compared by their SHA-256 digests once either side has ended.
- **Store.** Postgres holds an ordered key/value table with a version on each row (`crates/js-state/src/postgres/migrations/0001_state.sql`)
  and a change feed that copies each written row until it is trimmed (`--job-feed-retention`). A job is one row, `job/<id>`, a
  tag-length-value record whose unknown tags are carried and written back (`crates/js-jobs/src/codec.rs`). The next free job tag is 67
  (66 is the history key's, komira-ai/jobscheduler#1086).
  A job's file contents, which are secret, are held in that row while the job is live and replaced by their digests when it ends.
  Several instances share one store and no instance leads (`docs/design/multi-instance.md`). Work is claimed row by row by
  compare-and-set.
- **An outbox exists.** The dispatch queue (`crates/js-jobs/src/store/dispatch.rs`, `claim_dispatch`, `ack_dispatch`) writes a message in
  the same transaction as the state change. A row carries `claimed_by` and `claim_until`, is sent again after its claim lapses, and is
  deleted by its ack or with its job.
- **Boot.** `raise_at_boot` (`ending.rs`) raises each stored deadline to at least boot plus 120 s, so an outage deletes no result and
  fails no queued job.
- **Agents.** An agent that reports the `start-fence` capability runs the same job id under a new allocation as a new start
  (`per_job.rs`, `START_FENCE_CAPABILITY`). One without it "may judge a start by its job id alone".
- **Live-log keys are the server's.** The start's `log_sink.prefix` carries the job's prefix `P`, which the server builds as
  `prefix/H/job_id/`. The agent requires only that its last segment be the start's job id
  (`crates/js-agent-wire/src/log_sink.rs`), and the host's upload credential covers everything under `prefix/H/`
  ([live-logs.md](live-logs.md), section 2).
- **Outbound HTTP.** The alert webhook (`crates/js-alert/src/webhook.rs`, `http.rs`) uses sync `ureq` with no proxy, no redirects and a
  10 s limit. It retries a refused POST after 5 s, doubling to 10 minutes, but keeps its queue in memory, so a restart loses it. Its URL
  is the operator's. A callback URL is the first one a tenant supplies.
- **How kbf uses end states** (komira-ai/kbf, `crates/kbf-server/src/executor/outcome.rs`). kbf classes an end by its state, start
  failure code, result reason and exit code, and reruns infrastructure faults itself. Each rerun is a new job id ending `-<attempt>`. Its
  budgets are its own: a memory ladder, `FARM_RERUNS`, `NOT_STARTED_RERUNS` and `LOSS_REQUEUES`.

### 2.1 How this relates to the older `RetryPolicy`

[design.md](../design.md) section 8.1 and [retries-and-runs.md](retries-and-runs.md) describe a `RetryPolicy` of the generic scheduling
core from the lease design. It has budgets per axis (own-limit, platform fault, not started, loss, task failure) and an `evicted`
outcome that charges none (D63 in [decisions.md](decisions.md)). None of it is in the job API or in code. This page is the job API's
policy and supersedes those two sections for jobs; task I updates both to point here. Only two of their axes have a counterpart: not
started is a start failure, and loss is a lost host. A platform fault that ends a running attempt arrives as `LOST` and retries with it.
Task failure and the own-limit ladder stay with the client: the service never retries a job's own failure. Eviction is not built. When
it is, an evicted attempt is requeued without counting against `max_attempts`, as D63 says, and gets an outcome value of its own then.

## 3. Retry policy

### 3.1 The policy on the job

```proto
// job.proto (additions)

// Another attempt after a start failure or a lost host (docs/design/job-retry-and-completion-callback.md).
// Each attempt runs the work again. Only a job that sets this can go from a run back to queued. In one case
// two attempts can overlap: a static host an operator removed while it still ran the job.
message RetryPolicy {
  // The most attempts, the first included. 0 or 1: no retry.
  uint32 max_attempts = 1;
  // The wait before the second attempt, in seconds. Absent: 10. Cut to 3600.
  optional uint32 initial_backoff_seconds = 2;
  // The longest wait, jitter included. Absent: the larger of 300 and the initial wait. Cut to 3600.
  optional uint32 max_backoff_seconds = 3;
}

message JobSpec {
  ...
  // Absent: one attempt, as before.
  RetryPolicy retry = 70;
}
```

- **It sits in the spec,** so `same_job_as` compares it. A resubmit with another policy is `job-id-conflict`, because the policy is
  part of what the job is. Unlike the priority, a client cannot change it after the submit.
- **The wall limit is per attempt.** It bounds each attempt as it bounds the one run today.
- **Validation at submit.** A `max_backoff_seconds` given below the effective initial wait is an invalid-argument error. Nothing else
  is refused: `max_attempts` has no ceiling (decision 7).

### 3.2 What an attempt is

An **attempt** is one placement of the job, from the placement until the outcome that ends or retries the job. `Job.attempt` is set to
`n` when attempt `n` is placed. While the job waits for its next attempt, `Job.attempt` still names the attempt that ended, and
`retry_not_before_ms` is set. A job that ends while it waits (a cancel, or its queue wait running out) ends with no new attempt counted.
Its `attempt_history` keeps the attempts that ran.

The requeues before a run (a failed whole-host prepare, and a start refused `host-held`, `no-capacity` or `bundle-pending`) stay inside
the attempt they happen in. They keep their own bounds, as today, and are not counted against `max_attempts`. Each attempt gets its
own pre-run counters (`failed_prepares`, `held_refusals`, `capacity_refusals`). They are reset when the job is queued for its next
attempt, so attempt 2 gets its own `max_replace`.

**Lifetime.** A job's life is at most `max_attempts × (max_queue_wait + start_timeout + wall limit + 3600 s)`. The first term includes
the wait for the previous allocation to be freed (section 3.5), and 3600 s is the longest backoff. With no server ceiling, the client
chooses this bound through `max_attempts`.

### 3.3 Which outcomes retry

The outcome of an attempt is classed in one place, a pure function in `js-types` (section 10, task B). The table below is that
function. A row marked "retries" starts the next attempt when one is left. Every other row ends the job.

| Attempt ended as | Job state today | Retries? |
| --- | --- | --- |
| exit code 0 | `SUCCEEDED` | no |
| a non-zero exit | `FAILED`, reason `EXIT` | no |
| stopped at the wall limit | `FAILED`, reason `WALL_LIMIT` | no |
| stopped for memory | `FAILED`, reason `MEMORY` | no |
| a host path or host port failed while it ran | `FAILED`, reason `INPUT_FAILED` | no: the job ran |
| lost: the host was read absent, or did not list the job after a hello | `LOST` (placed, starting or running) | **retries** (host lost) |
| the start timed out | `FAILED_TO_START`, `START_TIMEOUT` | **retries** (start failure) |
| an input could not be read on the host | `FAILED_TO_START`, `INPUT_UNAVAILABLE` | **retries** (start failure) |
| the agent refused it with a code of the host | `FAILED_TO_START`, `AGENT_REFUSED` | **retries** (start failure) |
| the agent refused it with a code of the job | `FAILED_TO_START`, `AGENT_REFUSED` | no |
| the queue wait ran out | `FAILED_TO_START`, `NO_CAPACITY` | no: the pool's bound on waiting |
| `max_replace` prepares failed | `FAILED_TO_START`, `EXCLUSIVE_NOT_READY` | no: the whole-host bound already ran out |
| the client cancelled it | `CANCELLED` | no |

- **Agent refusal codes.** The codes that name the job's own fault are listed in the classifying function: those of
  `crates/js-agent-wire/src/refusal.rs` that a bad job gets (such as `invalid-job`, `invalid-file`, `invalid-image` and
  `image-not-allowed`), and `bundle-not-allowed`. Every other code is the host's, including a code this server does not know. A retry is
  bounded by `max_attempts`, and kbf reads an unknown code the same way today (infrastructure, rerun).
- **A drained host.** In merged code a drain never cuts work (section 6.4 of the design). A host that is updated or restarted without a
  handover loses its starting and running jobs through `lose_unlisted`, so "drained" arrives as `LOST` and retries.
- **A cancel wins.** A job with a cancel requested never starts another attempt. A cancel during a backoff ends the job `CANCELLED` at
  once, as a queued job's cancel does today.

### 3.4 Backoff

- Let `initial` and `max` be the effective values (section 3.1). The wait before attempt `n + 1`, with `n` counted from 1, is
  `min(max, initial × 2^min(n-1, 12) × (1 + j))`. The exponent is capped so that it cannot overflow. The jitter `j` lies in `[0, 0.1)`
  and is the first 8 bytes of SHA-256 of `job id || n`, scaled. No random number is drawn, so the decision stays pure (R4) and a
  simulation replays it. `js-types` gains `sha2.workspace = true` for it.
- The server writes the end of the wait as an instant, `retry_not_before`, using the store's clock, in the same commit that queues the
  next attempt. Placement passes over the job until that instant. The pool's `max_queue_wait` for the attempt counts from
  `retry_not_before`, not from the first submit.
- A restart does not move the instant. A not-before that has passed lets the job be placed, and nothing expires on it, so it needs no
  boot raise. The attempt's queue-wait deadline is raised at boot like any other waiting deadline.

### 3.5 Where a retry runs

Decision 9 puts the attempt in the log key, so two attempts on one host no longer share logs. The rule that a retry never returns to a
host that held the job is therefore replaced by these:

- **The failed attempt's host is barred for the job for 15 minutes,** with an entry in `prepare_bars`, which the placer already
  honours. Every retry follows a start failure or a lost host, which is the host's fault, so the job should try another host first. The
  bar lapses: a pool with one capable host can still retry, and 15 minutes is well under the default queue wait of one hour.
- **A host that held an earlier attempt takes a later one only if its agent reports `start-fence`.** An agent without that capability
  "may judge a start by its job id alone", so it could drop attempt 2's start as a job it already ran. This is the one lasting reason to
  keep a job off a host it ran on, and it applies only to such agents. The record keeps the set of distinct hosts that held any of the
  job's attempts (`attempt_hosts`, internal, not in the API). The set is bounded by the hosts that exist, not by the number of
  attempts, so the rule holds after `attempt_history` drops old entries.
- **One attempt at a time.** The next attempt is not placed until the previous attempt's allocation is freed: its host has confirmed
  the container gone, or the provider has read the host absent. This keeps "one allocation per job at a time" (`alloc/<job id>`) as it
  is. The cost: an attempt that timed out on a host that then went silent keeps its allocation booked (design section 3.2, "one limit is
  accepted"). Its retry then waits until the host is heard or read absent, at most until the next attempt's queue wait ends. `GetJob`
  shows the silence through `host_silent_since_ms`.
- **Late reports.** A result that the agent resends after `retry_txn` committed names the attempt's allocation, which is now releasing,
  and the job is queued. The store refuses that end as an illegal move, so it is answered `Ack::Stale` and dropped, as today.
- **A stop must not hit a later attempt.** A stop is not a stored message that can arrive late. `stops_for`
  (`crates/js-lease-server/src/per_job.rs`) works it out again on every heartbeat, per host, by job id. It stops a job the host lists
  that has no live allocation there (`unknown`), and a job that ended unreported. Such a stop names no allocation, and on a `start-fence`
  agent it "retires every fence of the job" on that host. So a later attempt on the same host is protected by its own live allocation:
  while the host holds it, the job id is not `unknown` there, and no stop is made for it. Each case in turn:
  - **An earlier attempt on another host** is stopped there only, because stops are per host.
  - **A host that returns after the provider read it absent** is seated as awaiting and takes no start until its listing is clean, so
    it never lists attempt 1 while it holds attempt 2.
- **Priority and order.** A retried job keeps its priority and its original queue time, so it does not go to the back of the queue.

### 3.6 Whole-host jobs

A whole-host attempt that was lost, or that failed to start for a cause of its host, goes back to the whole-host wait list once its
allocation is freed, after its backoff, with its host barred for 15 minutes. The mark pass treats it as any other waiting whole-host
job. Prepare failures stay inside the attempt and are bounded by `max_replace` per attempt (section 3.2).

The one case where two attempts can overlap is the same for both kinds of job. An operator removes a `static` host from its pool while
that host's agent is still running the job. The provider then reads the host absent, the allocation is freed with no confirmation, and
attempt 2 may start elsewhere while attempt 1 still runs. The existing rule bounds it: a host that later lists a lost job is told to stop
it. `RetryPolicy`'s comment says so (decision 2).

### 3.7 How attempts appear

New fields, all additive:

```proto
// How one attempt ended.
enum AttemptOutcome {
  ATTEMPT_OUTCOME_UNSPECIFIED = 0;
  ATTEMPT_OUTCOME_SUCCEEDED = 1;
  ATTEMPT_OUTCOME_FAILED = 2;         // the job ran and failed: the reason says how
  ATTEMPT_OUTCOME_HOST_LOST = 3;
  ATTEMPT_OUTCOME_START_FAILED = 4;
  ATTEMPT_OUTCOME_CANCELLED = 5;
}

// One attempt of a job that has ended.
message Attempt {
  uint32 number = 1;                  // from 1
  string host_id = 2;
  AttemptOutcome outcome = 3;
  JobTimes times = 4;                 // queued_at_ms: when this attempt became placeable
  optional int32 exit_code = 5;
  ResultReason reason = 6;
  StartFailureCode start_failure = 7;
  string start_failure_detail = 8;    // the agent's code, as on Job
  bool retried = 9;                   // another attempt was queued after it
}

message Job {
  ...
  uint32 attempt = 37;                      // the current attempt, or the last that ran; 0 (older record): 1
  repeated Attempt attempt_history = 38;    // the attempts that ended, oldest first: at most the newest 256
  optional uint64 retry_not_before_ms = 39; // while the job waits out a backoff
  CallbackStatus callback_status = 40;      // section 5.5; absent: the job has no callback
  uint32 dropped_attempts = 41;             // attempts dropped from attempt_history to keep its bound
}

message JobResult { ... uint32 attempt = 13; }            // the attempt this result is of; 0: 1
message UsageRecord { ... optional uint32 attempt = 24; } // absent: 1
enum QueueStatusCode { ... QUEUE_STATUS_CODE_WAITING_FOR_RETRY = 7; }
enum JobEventKind { ... JOB_EVENT_KIND_RETRY_SCHEDULED = 8; } // detail names the outcome and the wait
```

- **History.** Each ended attempt appends an `Attempt`, in the commit that queues the next attempt or ends the job, and a
  `RETRY_SCHEDULED` event when another follows. With no ceiling on `max_attempts`, the history keeps the newest 256 attempts and counts
  the rest in `dropped_attempts`, as `events` and `dropped_events` do. 256 entries of a few dozen bytes each keep a job row well under the
  store's row sizes. The 32-event bound of `Job.events` is unchanged. The bound limits only what `GetJob` returns. Every attempt's
  host and placement time are kept in the attempt rows below, with no limit on their number, so every attempt's logs stay readable
  (section 3.8).
- **Attempt rows.** Each attempt that ran gets a row of its own, `attempt/<job id>/<n, 10 digits>`. It holds the attempt's host id,
  `placed_at` and `ended_at`, in the same tag-length-value form as a job row, and it is written in the commit that ends or retries that
  attempt.
  - **Size.** About 100 bytes per attempt, beside the job row. The job row itself stays bounded.
  - **Write cost.** One more key in the commit of `retry_txn` or `end_txn`.
  - **Read cost.** One point read, and only for a `GetLogs` of an attempt no longer in `attempt_history`. `GetJob` never reads the rows.
  - **Sweep.** The retention sweep deletes a job's attempt rows by a prefix scan of `attempt/<job id>/`, in batches of the sweep's
    size, before it deletes the job row. So an interrupted sweep resumes from the job row's due entry and leaves no orphan. The sweep
    also deletes attempt rows whose job row is gone, which only a rolled-back build can leave, as for `callback/` rows (section 5.5).
    A job id becomes reusable only after its rows are gone, so a new job with that id never meets an old job's rows.
  - **Old builds.** It is a new key prefix, which an older build neither reads nor deletes.
- **The job's own fields are the current attempt's.** `host_id`, `times.placed_at_ms`, `times.started_at_ms`, `start_failure` and the
  result are the current attempt's. When the job is queued for a retry, they are cleared, as a failed prepare clears host and placement
  today, and the attempt's values move into `attempt_history`. `times.queued_at_ms` stays the first submit. An earlier attempt's tails
  are not kept, because none of the attempts that retry has a result. Its exit code and reason are in its `Attempt`, its usage in its
  `UsageRecord`, and its logs in the log store (section 3.8).
- **Usage.** Each attempt's booking is already its own `UsageRecord`. It gains `attempt`. Sizing reads usage records as today. A lost
  attempt has no result and teaches the class nothing.
- **Store.** The job record gains tags 67 (`retry`, in the spec), 68 (`callback`, in the spec, with its key replaced by the key's
  digest, section 5.2), 69 (`attempt`), 70 (`attempt_history`), 71 (`retry_not_before`), 72 (`dropped_attempts`) and 73 (`attempt_hosts`, section 3.5, not in the API). A record without
  them reads as attempt 1 with no history and no callback. `callback_status` is not on the job row: `GetJob` reads it from the job's
  `callback/` row (section 5.5). No migration is needed: the store is one key/value table.
- **The move.** A new store operation, `JobStore::retry_txn`, sits beside `end_txn` and outside `may_move`, as the existing requeues
  do, so no other path can move a live job to queued. In one commit it:
  - appends the attempt and clears the attempt's fields;
  - resets the pre-run counters and adds the host bar;
  - sets `retry_not_before` and puts the job in `idx/job/queued` again;
  - moves the allocation to releasing exactly as `end_txn` does.

  It keeps the job's files (no digest), sets no `retention_ends` and does not arm the callback: the job has not ended. The attempt's
  outcome is decided in the same commit, so the job never reads as ended between attempts.

### 3.8 Every attempt's logs

- **The key.** Attempt 1 keeps today's prefix `P = prefix/H/job_id/`, so a job without retries, and every log written before this
  change, reads exactly as before. Attempt `n ≥ 2` gets `P = prefix/H/S/job_id/`, where `H` is that attempt's host and `S` is the 64
  lower-case hex digits of SHA-256 of `job id "/" n`.
  - **No collision.** A job id is at most 63 bytes (`crates/js-types/src/job_id.rs`), so a 64-byte `S` is never a job id. No job's
    attempt-1 prefix `prefix/H/Y/` can contain another job's later attempt. Two later attempts differ in `S`.
  - **Within the host's grant.** The last segment is still the job id, as the agent requires. The key stays under the host's prefix,
    which the host's credential already covers.
  - **No agent changes:** the server builds `P` and sends it in the start's `log_sink` (section 2).
- **Length.** The base the server passes to `JobPrefix::new` (`crates/js-logstore/src/layout.rs`) may be at most `MAX_PREFIX_LEN`,
  512 bytes. That base is `prefix/H` for attempt 1 and `prefix/H/S` for a later one, 65 bytes longer. A configured prefix of at most
  476 bytes (the limit today) fits attempt 1, and one of at most 411 bytes fits every attempt.
  - **When it does not fit.** For a configured prefix of 412 to 476 bytes, an attempt `n ≥ 2` runs without live logs, as a job whose id
    is too long does today. The server logs a warning at start for such a prefix and refuses nothing, so no configuration that works
    today stops working.
  - **The reader runs the same check,** so it answers "no live logs" for exactly the attempts that got no sink.
- **Reading.** `GetLogsRequest` gains `optional uint32 attempt = 6`: the attempt whose output to read, from 1. Absent: the latest
  (`Job.attempt`). `attempt` is read only with a stream. A request without a stream answers the result's tails as before, and only the
  final attempt has a result.
  - **Errors.** An attempt of 0 is an invalid-argument error. An attempt above the current one is refused `OUT_OF_RANGE`. An attempt
    that was never placed has no row and no logs: the job ended while it waited, so no number was given to it. Every attempt that ran
    is readable, however many there were.
  - **The current attempt** is read exactly as a job is read today.
  - **An earlier attempt** is read as a job that ended without a result is read today. It is ended for that read, so no follow waits.
    Its prefix comes from its host and number, and the placement cutoff from its placement time. Both are taken from its `Attempt` in
    `attempt_history` when it is still there, and from its attempt row (section 3.7) when it is not. Its length comes from what the
    store holds, since it has no result. The job's own state does not decide it. Today's reader answers an empty page for a queued job
    (`crates/js-lease-server/src/job_api/logs.rs`). Task J limits that early answer to the current attempt, so attempt 1's logs can be
    read while the job waits in backoff.
- **Retention.** The service deletes no log object today ([live-logs.md](live-logs.md), section 10), and that does not change. The
  bucket's lifecycle rule covers every attempt's objects, because they all sit under the same configured prefix.

### 3.9 What kbf would set

- **v1: no policy.** kbf owns its budgets (the memory ladder, `FARM_RERUNS`, `NOT_STARTED_RERUNS`, `LOSS_REQUEUES`). Each rerun gets a
  new job id and a fresh upload key. A service-side retry under the same job id would reuse the upload key of an attempt that may have
  uploaded part of its output before its host was lost, and it would count a fault that kbf also counts. kbf therefore sets no `retry`.
- **Later, if kbf wants it:** one policy covers both `NOT_STARTED_RERUNS` and `LOSS_REQUEUES`, because a start failure and a lost host
  are exactly what the service retries. kbf would then drop both budgets in the same change, and give each job an upload key that a
  later attempt may overwrite.
- **ucdk's UJ1** counts a non-zero exit as a failed attempt. The service never retries a job's own failure (decision 3), so a ucdk
  adapter keeps that part of UJ1 itself, and uses `retry` only for the start-failure and lost-host part.

## 4. API, configuration and metrics

- **No new refusal code.** Every new check at submit is an invalid-argument error, because none of them is a fault of capacity or of
  a ceiling.
- **Configuration.** No new key and no new flag (B14). The number of delivery threads is fixed in code (8 per instance), and so is the
  address rule of section 5.3.
- **Metrics.** `js_job_retries_total{outcome}` (outcome: `start-failed` or `host-lost`), `js_job_attempts_exhausted_total`,
  `js_callback_deliveries_total{result}` and `js_callback_pending`.
- **Gateway.** No new route: the fields ride on `POST /v1/jobs`, `GET /v1/jobs/{job_id}` and `GET /v1/jobs/{job_id}/logs` (the new
  `attempt` query parameter). The schema test's word filter (`CAPACITY_WORDS` in `crates/js-proto/tests/schema.rs`) refuses none of the
  new names.

## 5. Completion callback

### 5.1 Why a webhook (decision 4)

| | A. Webhook: the server POSTs to a URL | B. gRPC server stream `WatchJobs` | C. Unary long poll on `GetJob` |
| --- | --- | --- | --- |
| Client needs | an inbound HTTPS endpoint | a held connection, a gRPC client | an outbound call, like polling |
| HTTP gateway | not involved | cannot route it (unary only, `crates/js-api-gateway/src/annotation.rs`) | routes it as a GET |
| Survives a server restart | yes, the delivery row is in Postgres | the client reconnects and must not miss an end in the gap | the client calls again |
| At-least-once | the server's job, with retries to a deadline | the client's job, with a cursor | the client's job |

The webhook is the one option where the server owns delivery: a client that is down when its job ends still learns of it, with no state
of its own. B and C are not built.

### 5.2 How a client asks for one, and how the key is kept

```proto
// A bare signed POST with no body and no job data, sent once the job reaches its final state
// (sections 5.4 and 6).
message Callback {
  // Where the server POSTs: an https URL with no userinfo or fragment. Its address must pass the
  // server's address rule (docs/design/job-retry-and-completion-callback.md, section 5.3).
  string url = 1;
  // The key the server signs each call with: 32 to 64 bytes. Secret: no response of the service
  // carries it, so this field is empty in every job a read returns.
  bytes signing_key = 2;
  // How long the server keeps trying, in seconds from the job's end. Absent: 86400. Cut to the
  // job's retention end.
  optional uint32 deadline_seconds = 3;
}

message JobSpec { ... Callback callback = 71; }
```

- **In the spec,** so the callback is part of idempotency like the retry policy. The key is compared by its SHA-256 digest, as an
  ended job's files are, so a resubmit with the same URL and key is the same job and one with another key is `job-id-conflict`.
- **The key is held in one place.** In the submit's commit the server writes the job row with the key replaced by its digest, and
  writes the key itself into the job's `callback/<job id>` row (section 5.5), which is armed but not yet due. No other row holds it.
- **The key is never shown or recorded.**
  - It is not logged. A log line names the job and the URL's host only, never its path or query, which may carry the receiver's own
    reference.
  - `GetJob`, `ListJobs` and `SubmitJob`'s answer return the callback with `signing_key` empty, as `JobFile.contents` is.
  - It is never in an event, a usage row, a metric label, a refusal detail or an error text.
  - A test asserts each of these by exact equality on the output.
- **It is kept only as long as delivery needs it.** The key is erased from the `callback/` row when the delivery reaches a final state
  (delivered, expired or abandoned). The row itself is deleted with the job at the retention end. A job that never ends keeps its key
  while it is live, as it keeps its files.
- **At rest.** The key has the same posture as a live job's file contents today: it is in the store's table, and in the change feed's
  copy of the row until the feed is trimmed. Whoever can read the store can read it. The feed copy is not new exposure: it is how file
  contents are held today, and the key leaves the feed at the same trim.
- **Checked at submit:** the URL's form (https, a host, no userinfo, no fragment, at most 2048 bytes), a literal address in it
  against the address rule, and the key's length. A bad one is an invalid-argument error whose text repeats neither the URL nor the
  key.

### 5.3 The address rule: no allow-list, and no call inward

With no allow-list, the server must still never be turned against itself or its cluster. The smallest rule that does that:

1. **https only.** No plain http, not even to loopback. TLS is checked against the WebPki roots, as the alert webhook checks it.
2. **Resolve, check, then connect to the checked address.** At each try the server resolves the host once. It refuses the try if
   any address is denied, and connects only to an address it checked. The check runs inside ureq's resolver hook (the `Resolver` trait
   of `ureq::unversioned::resolver`, given to the agent with `Agent::with_parts`), so ureq connects to exactly what the rule returned.
   A name that resolves inward on a later try (DNS rebinding) is caught on that try. ureq marks that module as outside its semver
   promise, so the workspace pins ureq to an exact version, and task E checks that the pinned version carries the hook.
3. **Admitted, and nothing else:**
   - global unicast addresses;
   - the private ranges (`10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `fc00::/7`).

   **Denied, within what is admitted:**
   - the addresses providers serve metadata on outside the link-local range: `fd00:ec2::254` (which is inside `fc00::/7`), and
     `168.63.129.16`, which looks global;
   - the cluster's own addresses.

   Every other address is denied by default. That covers:
   - loopback, unspecified and `0.0.0.0/8`;
   - link-local, which holds the usual metadata address `169.254.169.254`;
   - carrier-grade NAT (`100.64.0.0/10`, which holds `100.100.100.200`);
   - the rest of the IANA special-purpose ranges, such as `192.0.0.0/24`, multicast, broadcast and the deprecated site-local
     `fec0::/10`.

   **Addresses that embed an IPv4 one** are judged by that IPv4 address: IPv4-mapped, IPv4-compatible, NAT64 (`64:ff9b::/96`,
   `64:ff9b:1::/48`), 6to4 (`2002::/16`) and Teredo (`2001::/32`). Numeric host forms (`2130706433`, `0x7f.1`) are parsed as the address
   they name.

   **The cluster's own addresses** are this instance's listen addresses, the store host's resolved addresses, and the peer address each
   host's heartbeat arrives from. The server does not record the last of these today. Task G adds it, in memory, per instance.
4. **No redirect is followed, no proxy is used, and the answer's body is not read.** A redirect counts as a failure.
5. **A small oracle, stated.** A submitter sees `callback_status`: the state, the tries, the time of the last try, and
   `last_failure`, which holds only the class `refused-address` or `failed`. So of a URL it named, the submitter learns three things:
   - whether the URL was refused by the rule, which it could work out itself;
   - whether it answered 2xx, 410, or neither;
   - when it was last tried.

   It never sees a status code, a connect error or a time taken. That detail goes to the operator's log, without the URL's path or
   query.

**What it leaves.** A private address (`10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `fc00::/7`) that is not the cluster's own is
not refused, because a receiver such as kbf-server often lives on one. What a job can make the server send there is one POST with an
empty body and three signature headers. It learns only whether that POST was answered 2xx, 410 or neither. An operator who wants less blocks the server's egress at the
network.

**How tests reach a receiver.** The sender takes three constructor arguments that only tests set otherwise:
- the address rule, which a test sets to one that admits loopback;
- the resolver, which a test fakes;
- the TLS roots. Production passes the WebPki roots. A test passes a test CA made with `rcgen`, whose certificate the loopback receiver
  (`tokio-rustls`) presents. Both crates are already in the workspace.

### 5.4 What a call carries

Nothing but the signature. The body is empty and there is no content type. The receiver learns:

- **which job:** from the URL, which it chose itself and can give its own reference, for example a path segment it maps to the job;
- **that the call is genuine:** from the signature, which only a holder of the job's key can make;
- **what happened:** by reading the job with `GetJob` (and `GetResult`, `GetLogs`) with its own token. That read is authenticated
  and limited by the caller's role, and it gives the job's final state, attempts and outcome.

A forged or confusing call can therefore make the receiver do no more than read a job it can already read. The receiver acts only on
what `GetJob` says.

### 5.5 Delivery, at least once

- **The outbox row.** `callback/<job id>` is written at submit, holding the URL, the key, the deadline in seconds and a `webhook-id`
  minted then (16 bytes from the server's id source, base64url, opaque, unrelated to the job id). It is armed but not due. `end_txn`
  makes it due in the same commit as the job's final end: it sets `next_try_at` to now and `deadline_at`, and adds the index entry
  `idx/callback/due/<next_try_at ns>/<job id>`. The row later holds `tries`, `claimed_by`, `claim_until`, `last_failure` and the delivery
  state. The end and the row commit together, so an end can never lose its callback, and a callback is never sent for a job that did
  not end.
- **The worker.** Each instance runs 8 delivery threads, off the tick path like the alert notifier. A thread claims a due row by
  compare-and-set (`claimed_by` = this instance, `claim_until` = now + 30 s), sends it within a 10 s limit, and commits the answer at the
  claimed version. Two instances never hold a claim on one row at once. An instance that dies mid-send leaves a claim that lapses, and
  the row is sent again.
- **What an answer means.** A 2xx answer delivers the callback. A 410 abandons it: the receiver says it does not want it. Any other
  status, a timeout, a refused connection, a redirect or a refused address is a failure, and the next try waits 5 s, doubling to at most
  10 minutes (the alert webhook's values).
- **Deadline.** `deadline_at` is the job's end plus `deadline_seconds`, cut to the job's `retention_ends`. Past it, the row is
  `EXPIRED` and nothing more is sent. At boot, `raise_at_boot` raises each due row's deadline to at least boot plus 120 s, as it does a
  retention end, so an outage of the server expires no callback.
- **Sweep.** The retention sweep deletes the row with its job row (`delete_job`, `delete_due`). It also deletes a `callback/` row whose
  job row is gone, which only a rolled-back build can leave.
- **Status.**

  ```proto
  enum CallbackState {
    CALLBACK_STATE_UNSPECIFIED = 0;   // a value this reader does not know
    CALLBACK_STATE_ARMED = 1;         // the job has not ended
    CALLBACK_STATE_PENDING = 2;
    CALLBACK_STATE_DELIVERED = 3;
    CALLBACK_STATE_EXPIRED = 4;       // its deadline passed first
    CALLBACK_STATE_ABANDONED = 5;     // the receiver answered 410
  }
  message CallbackStatus {
    CallbackState state = 1;
    uint32 tries = 2;
    optional uint64 last_try_at_ms = 3;
    string last_failure = 4;          // refused-address or failed (section 5.3)
    optional uint64 delivered_at_ms = 5;
    optional uint64 deadline_at_ms = 6;
  }
  ```

  `GetJob` and `ListJobs` read the job's `callback/` row and answer it as `Job.callback_status`.
- **What at-least-once means here.** A receiver may get one callback more than once: after a lost answer, a timeout, or a claim that
  lapsed. It dedupes on `webhook-id`. It gets none only if it never answers 2xx before the deadline, and `callback_status` then says
  `EXPIRED`.

### 5.6 Signing, and the receiver's side

The server signs in the [Standard Webhooks](https://www.standardwebhooks.com/) form, so a receiver can use a library that already
exists:

- **The headers.** `webhook-id` is the row's opaque id, the same on every try. `webhook-timestamp` is the seconds since the epoch at
  this try, by the store's clock. `webhook-signature` is `v1,<base64 HMAC-SHA256(key, id "." timestamp ".")>`: the body is empty, so the
  signed content ends at the second dot.
- **The key's encoding.** The server HMACs with the raw key bytes. A Standard Webhooks library takes its secret as `whsec_` followed by
  the standard base64 of those bytes, and decodes it the same way, so a receiver gives its library `whsec_` + base64(key).
- **One scheme.** One key per job, and no bearer token or mTLS beside it. The workspace already depends on `hmac` and `sha2`.

A receiver, for example kbf-server:

1. At submit, it mints a key and a reference of its own, and submits the job with
   `callback { url: "https://kbf.example/js-done/<its reference>", signing_key: <key> }`. It keeps the reference, the key and the job id.
2. On a POST to that path, it finds the reference and its key, and checks the signature and that the timestamp is within 5 minutes of
   its own clock, so an old call cannot be replayed. It drops a `webhook-id` it has already handled. A call that fails a check gets 401
   and changes nothing.
3. It answers 2xx at once, then calls `GetJob` for its job id with its own token and acts on what that answer says.

## 6. How the two meet

- **Rule C1 (decided): one callback per job, sent only after retries are exhausted.** The callback is sent once the job reaches its
  final state, and never per attempt. The final state is one of:
  - succeeded;
  - failed with no retry left: an outcome that does not retry, or the last allowed attempt. This covers `FAILED`, `LOST` and
    `FAILED_TO_START`;
  - cancelled.

  The store enforces it. Only `end_txn` makes the callback row due, and `end_txn` runs only when no attempt follows; `retry_txn` never
  touches the row. A job retried three times that then succeeds is called back once. `GetJob` then shows `state: SUCCEEDED`,
  `attempt: 4` and the four attempts in `attempt_history`.
- **The final state** is the state the job ended in. That is the last attempt's outcome when the attempts ran out or the outcome does not
  retry. It is `CANCELLED`, or `FAILED_TO_START` with `NO_CAPACITY`, when the job ended while it waited for its next attempt. In every
  case `attempt_history` carries what each attempt did. A client sees, for example, that attempt 1's host was lost before the retry found
  no capacity.
- **A cancel during a backoff** ends the job `CANCELLED` in one commit through `end_txn`, so the callback becomes due in that commit.
- **No callback per attempt,** by rule C1. A client that wants to see attempts as they happen reads `GetJob`.

## 7. Compatibility

The evolution rule in `service.proto`'s header holds for every change: fields, enum values and messages are appended, nothing is
renumbered, and no shipped field takes another meaning.

- **Old clients, new server.** A job without `retry` or `callback` behaves exactly as today. Only a job whose submitter set `retry` can
  go from a run back to `QUEUED`, so a client that never sets it never sees that move. A client that reads jobs another client gave a
  policy skips the new fields. An unknown `QueueStatusCode` or `JobEventKind` reads as unspecified, as those enums already say. A
  `GetLogs` request without `attempt` reads the latest attempt, which for a job without retries is its only attempt, under its old key.
- **New client, old server.** An old server's decoder skips fields 70 and 71, so the job runs once with no callback. The answer's
  `job.spec` does not echo them. That is how a client tells: a client that sent a policy and sees no `retry` in the returned spec knows
  the server dropped it. `js-client` checks this and reports it (task I).
- **Old agents.** The agent wire does not change. A retry is a new placement with a new allocation id. It reaches a host that ran the
  job before only if that host's agent reports `start-fence` (section 3.5). The attempt's log prefix is built by the server and passes
  every agent's check (section 3.8).
- **Stored rows.** Older builds carry the new job tags (`Carried`). The `callback/` rows, the `idx/callback/` entries and the `attempt/`
  rows are new key prefixes, which an older build neither reads nor deletes.
- **Instances of two versions over one store** (multi-instance.md, section 7). During a rolling update, an older instance could end a
  job that has a policy without retrying it, or end a job with a callback without making its row due. So each feature lands in two
  releases:
  1. **Act:** the release that can retry and deliver. `SubmitJob` still refuses a job carrying `retry` or `callback`
     (`INVALID_ARGUMENT`, `retry-not-enabled` or `callback-not-enabled`). The refusal is a field of the server's internal `Config`
     struct, not a configuration key or flag. Tests of tasks F, G and H build the server with it off, so the act release runs end to end
     before anything accepts.
  2. **Accept:** a later release removes the refusal. Every instance that can meet such a job then already acts on it.

  The proto fields land first (task A), with the refusal in place, so no server of this repository from task A on takes a field it
  ignores. Servers released before task A skip the fields, as above.
- **Rollback.** Once the accept release has run, rolling the fleet back below the act release is not supported: an older build would end
  policy jobs without retrying them and callback jobs without a due row. A rollback to the act release is safe. The release notes of the
  accept release say so.
- **B8's previous-release test.** The repository cannot run a previous server release in a hermetic test (T12 forbids fetching it). The
  tests below stand in for it. Each runs this build's codec and store with the new tags stripped, which is exactly what an older build
  reads: `js-jobs` `a_build_without_the_new_tags_carries_them` and `an_older_store_reader_leaves_callback_rows_intact` (section 9).

## 8. Failure modes

| Failure | What happens |
| --- | --- |
| The server crashes between an attempt's end and `retry_txn` | Nothing was committed. The agent resends the unacknowledged result, and one retry is made. |
| Two instances judge one attempt's end | Both read the same version. One commit wins, and the other is stale and re-reads. One retry is made. |
| A cancel races a retry decision | Both write the job row at the version read. One wins. If the cancel wins, the job ends cancelled. If the retry wins, the cancel then meets a queued job and ends it at once. |
| A late or resent result of an earlier attempt | `Ack::Stale`. Attempt 2 is unchanged. |
| The previous attempt's host goes silent | The next attempt waits for its allocation to be freed, at most until its queue wait ends (section 3.5). |
| Only the failed host is capable | The retry waits out the 15-minute bar, then may return there if the host reports `start-fence`. Otherwise it ends `NO_CAPACITY` at its queue wait. |
| An operator removes a running `static` host | Attempt 1 may still run while attempt 2 starts elsewhere. A host that lists a lost job is told to stop it (section 3.6). |
| A log prefix too long for an attempt's segment | That attempt runs without live logs, as a too-long job id does today. A warning is logged at start. |
| The receiver is down | Tries back off to 10 minutes until the deadline. The job is unaffected. The status is `EXPIRED`. |
| The receiver is slow past 10 s | A timeout and a retry. The receiver may process both tries and dedupes on `webhook-id`. |
| An instance dies holding a claim | The claim lapses after 30 s. Another instance sends the same `webhook-id`. |
| The server is down for longer than the deadline | `raise_at_boot` gives each due callback at least 120 s after boot. |
| A URL that resolves inward (metadata, loopback, the cluster) | The try is refused before any connection, and the status says `refused-address`. A later try resolves again. |
| A name that resolves outward, then inward | Each connection uses an address checked on that try, so the inward answer is refused. |
| A submitter probes addresses through callbacks | It learns only whether a URL it named answered 2xx, 410 or neither, never a status code, an error or a time (section 5.3). |
| A captured call is replayed | The receiver refuses a timestamp older than 5 minutes, and a repeat `webhook-id`. |
| The store is read by someone who should not | They can read live jobs' callback keys, as they can read live jobs' files today (section 5.2). |
| A job id is used again after retention | The old row was swept with its job. The new job has its own `webhook-id`. |

## 9. Tests

The tests follow the repository's rules:

- **Rust only.**
- **No timing (T10).** Every wait uses the store clock or `ManualClock`, never a sleep, and no test bounds an elapsed time. The
  callback sender's time limit is a constructor argument. A test sets it short and keeps the receiver blocked on a channel that is never
  released, so the timeout is the only possible answer and no timing is asserted.
- **Hermetic (T12).** Receivers listen on loopback port 0 with a test CA's certificate, admitted by the test-only address rule and
  roots (section 5.3). Name resolution goes through a resolver the sender takes as an argument, faked in tests, so no test does DNS.
- **Three backends.** Postgres is reached through `js_testkit::database`. Each store rule is in the shared `job_store_tests!` macro, so
  it runs on the fake, SQLite and Postgres backends.

| Rule | Test | Task |
| --- | --- | --- |
| No policy: one attempt, ended as today | `js-jobs` `job_without_a_policy_ends_after_one_attempt` | C |
| Each row of the table in 3.3 | `js-types` `retry::each_outcome_is_classed_as_the_table_says` (one case per row) | B |
| Exit, wall limit, memory and input failure never retry, with attempts left | `js-types` `retry::a_jobs_own_failure_never_retries` | B |
| An unknown agent code is the host's; a job-fault code never retries | `js-types` `retry::an_unknown_refusal_code_is_the_hosts`, `retry::a_job_fault_refusal_never_retries` | B |
| `NO_CAPACITY`, `EXCLUSIVE_NOT_READY` and a cancel never retry | `js-types` `retry::an_exhausted_pool_bound_or_a_cancel_never_retries` | B |
| `max_attempts` 0 and 1 mean one attempt; the last attempt ends the job; `u32::MAX` is accepted | `js-types` `retry::zero_and_one_mean_one_attempt`, `retry::the_last_attempt_ends_the_job`, `retry::the_largest_max_attempts_is_accepted` | B |
| Backoff: doubling, the capped exponent, effective defaults, the cap with jitter, the same wait for the same job and attempt | `js-types` `retry::backoff_doubles_to_its_cap_with_bounded_jitter` | B |
| `max_backoff` below the effective initial wait is refused | `js-lease-server` `submit_refuses_a_backoff_cap_below_its_initial_wait` | F |
| A resubmit with another `retry`, or another callback URL or key, is `job-id-conflict`; the same one is the same job | `js-jobs` `a_resubmit_with_another_policy_or_callback_conflicts` | C, D |
| A job in backoff is not placed before `retry_not_before`, and is placed after it | `js-lease-server` `a_retried_job_waits_out_its_backoff` | F |
| The queue wait of an attempt counts from its backoff end | `js-lease-server` `queue_wait_of_an_attempt_counts_from_its_backoff_end` | F |
| A cancel in backoff ends the job at once and makes its callback due | `js-jobs` `cancel_in_backoff_ends_the_job_and_makes_its_callback_due` | C, D |
| A cancelled attempt never retries | `js-lease-server` `a_cancelled_attempt_never_retries` | F |
| A cancel and a retry on one version: one wins | `js-jobs` `a_cancel_and_a_retry_on_one_version_make_one_change` | C |
| Two retry decisions on one version make one attempt | `js-jobs` `two_retry_decisions_on_one_version_make_one_attempt` | C |
| The failed host is barred for 15 minutes; after the bar, it takes the job only if it reports `start-fence` | `js-lease-server` `a_retry_bars_its_failed_host_then_needs_start_fence_to_return` | F |
| The next attempt waits for the previous allocation, also when its host is silent | `js-lease-server` `the_next_attempt_waits_for_the_previous_allocation`, `a_silent_hosts_attempt_holds_its_retry_until_the_queue_wait` | F |
| A result resent after `retry_txn` is `Ack::Stale` | `js-lease-server` `a_result_resent_after_a_retry_is_stale` | F |
| A host that holds a later attempt's live allocation gets no stop for the job; a host that lists an earlier one does | `js-lease-server` `a_stop_for_an_earlier_attempt_never_retires_a_later_one` | F |
| Each start failure (`START_TIMEOUT`, `INPUT_UNAVAILABLE`, a host-code `AGENT_REFUSED`) and both loss paths (`lose_absent`, `lose_unlisted`) make a second attempt through the server | `js-lease-server` `each_retrying_outcome_makes_a_second_attempt` | F |
| The `start-fence` rule holds past the history bound | `js-jobs` `attempt_hosts_outlives_the_history_bound` | C |
| `retry_txn` keeps files, sets no retention end and leaves the callback armed | `js-jobs` `a_retry_keeps_files_and_leaves_the_callback_armed` | C, D |
| One `Attempt` and one `UsageRecord` per attempt; `queued_at_ms` stays the first submit | `js-lease-server` `each_attempt_has_its_history_entry_and_usage_record` | F |
| History keeps the newest 256 and counts the rest | `js-jobs` `attempt_history_keeps_its_bound_and_counts_the_dropped` | C |
| Every attempt that ran has its row, written in the commit that ends or retries it, with its host and times | `js-jobs` `each_attempt_writes_its_row_in_its_commit` | C |
| The sweep deletes every attempt row of a job before the job row, across batches and an interrupted sweep, and deletes orphans | `js-jobs` `the_sweep_deletes_a_jobs_attempt_rows_first`, `the_sweep_deletes_orphan_attempt_rows` | C |
| Attempt 1's log prefix is today's; attempt `n ≥ 2`'s is `prefix/H/S/job_id/` and passes the agent's prefix check; a job whose id looks like an attempt segment (`_a2`, or 63 hex digits) never shares a key with another job's attempt | `js-lease-server` `each_attempt_gets_its_own_log_prefix`; `js-agent-wire` `an_attempt_prefix_passes_the_start_check` | J |
| A configured prefix of 412 to 476 bytes: no live logs for attempts after the first, the reader agrees, a warning at start | `js-lease-server` `an_attempt_prefix_that_does_not_fit_runs_without_live_logs` | J |
| `GetLogs` `attempt`: absent reads the latest; each attempt reads its own bytes; attempt 1 is readable while the job waits in backoff; 0 is an invalid argument; above the current is `OUT_OF_RANGE`; an attempt dropped from `attempt_history` (attempt 1 of a job with 300 attempts) still reads its own bytes, through its row | `js-lease-server` `get_logs_reads_the_attempt_asked_for`, `an_earlier_attempts_logs_read_during_backoff`, `a_dropped_attempts_logs_read_through_its_row` | J |
| A crash between an attempt's end and its retry makes one retry | `js-sim-cell` scenario `restart_between_attempts` | H |
| A whole-host attempt lost while running is retried with `max_replace` counted again | `js-sim-cell` scenario `whole_host_retry` | H |
| Over every run: no job runs more than `max_attempts` times; no two attempts of a job hold an allocation at once | `js-sim-cell` `Invariant::AttemptsBounded`, `Invariant::OneAttemptAtATime` | H |
| A record from before the fields reads as attempt 1 | `js-jobs` codec `a_record_without_attempt_tags_reads_as_attempt_one` | C |
| An older reader carries the new tags and leaves callback rows | `js-jobs` `a_build_without_the_new_tags_carries_them`, `an_older_store_reader_leaves_callback_rows_intact` | C, D |
| New proto fields: goldens; an old reader skips them; old bytes give the defaults | `js-proto` `tests/retry.rs`, `tests/callback.rs`, rows in `golden/fields.txt` | A |
| Until the accept release, `retry` and `callback` are refused | `js-lease-server` `submit_refuses_retry_and_callback_until_enabled` (the accept PR replaces it with the accepting test) | A |
| A bad URL or key at submit is refused: plain http, userinfo, fragment, a literal denied address, a key under 32 or over 64 bytes; the error text holds neither | `js-lease-server` `submit_refuses_each_bad_callback` | G |
| The key is in the `callback/` row only; the job row holds its digest | `js-jobs` `the_signing_key_is_held_in_the_callback_row_only` | D |
| The key never leaves: `GetJob`, `ListJobs` and `SubmitJob` answers, events, usage rows, metrics and log lines, each compared by exact equality | `js-lease-server` `the_signing_key_is_never_returned_or_logged` | G |
| The key is erased at a final delivery state, and the row deleted at the retention end | `js-jobs` `the_signing_key_is_erased_when_delivery_ends`, `the_sweep_deletes_a_callback_with_its_job`, `the_sweep_deletes_an_orphan_callback` | D |
| The address rule refuses each denied range, `168.63.129.16`, `fd00:ec2::254`, each embedded-IPv4 form, numeric host forms and each cluster address; admits a public and a private address | `js-callback` `the_address_rule_refuses_each_denied_range` | E |
| The connection goes to the address the rule returned, through ureq's resolver hook | `js-callback` `the_sender_connects_only_to_a_checked_address` | E |
| A name that resolves inward on a later try is refused on that try; the connection uses the checked address | `js-callback` `a_name_that_turns_inward_is_refused_on_that_try` | E |
| `last_failure` is coarse; the detail is only in the operator's log | `js-callback` `a_failure_reports_only_its_class` | E |
| Rule C1: a retried job at each final state (succeeded, failed with no retry left, cancelled) gets exactly one callback, after its last attempt, and none before | `js-sim-cell` scenario `one_callback_after_retries_are_exhausted` | H |
| The callback becomes due in the end's commit, never without it | `js-jobs` `an_end_makes_its_callback_due_in_the_same_commit` | D |
| 2xx delivers once | `js-callback` `a_2xx_delivers` | E |
| 500, a timeout, a refused connection and a redirect are retried with the doubling wait; no redirect is followed | `js-callback` `each_failure_is_retried_with_backoff` | E |
| 410 abandons | `js-callback` `a_410_abandons` | E |
| The deadline: expires when past; cut to the retention end | `js-jobs` `a_callback_past_its_deadline_expires`, `a_deadline_is_cut_to_the_retention_end` | D |
| A restart with callbacks due delivers them; deadlines raised to at least boot plus 120 s | `js-jobs` `boot_raises_due_callback_deadlines`; `js-sim-cell` `restart_with_due_callbacks` | D, H |
| A lapsed claim is taken again with the same `webhook-id`; never two claims on one row | `js-jobs` `a_lapsed_callback_claim_is_taken_again`, `one_claim_per_callback_row` | D |
| The signature equals the Standard Webhooks reference computation for an empty body, and the spec's own `whsec_` test vector | `js-callback` `signature_matches_the_reference_for_an_empty_body` | E |
| The request is exactly: POST, the three headers, an empty body, no content type (exact equality) | `js-callback` `the_request_is_a_bare_signed_post` | E |
| The `webhook-id` is opaque: it does not hold the job id, and differs for a reused job id | `js-callback` `the_webhook_id_reveals_nothing_of_the_job` | E |
| `callback_status` on `GetJob` follows the row | `js-lease-server` `get_job_shows_the_callback_status` | G |
| A policy the server dropped is reported by `js-client` | `js-client` `a_dropped_policy_is_reported` | I |
| The reference service and the real one agree, and each scenario fails against its `Defect` | `js-conformance` `retry_on_host_loss`, `retry_on_start_failure`, `no_retry_on_nonzero_exit`, `no_retry_on_cancel`, `callback_once_after_retries` | H |

Why this list is complete:
- it covers every row of the classing table in 3.3;
- every bound: `max_attempts` up to its type's maximum, backoff and its cap, the queue wait per attempt, the host bar, the wait for the
  previous allocation, the history bound, the log prefix length, the delivery deadline and the retention sweep;
- every actor that can race: two instances, a cancel, a restart, a late agent report, a stop and a lapsed claim;
- every answer a receiver can give: 2xx, 410, another status, a timeout, a refused connection and a redirect;
- every way the key could leave the server, and every address class the rule names;
- every compatibility path of section 7: old reader, old record, old agent, new client against an old server, `GetLogs` without
  `attempt`, and the act-then-accept gate.

## 10. Implementation tasks

The interfaces land first (P7). After that, tasks in the same row of the plan can run at once, because each depends only on the agreed
interfaces of task A.

| Task | What | Depends on | Crates |
| --- | --- | --- | --- |
| **A. Interfaces** | The proto additions of sections 3.1, 3.7, 3.8, 5.2 and 5.5, with `golden/fields.txt` rows and goldens. `SubmitJob` refuses `retry` and `callback`. The Rust signatures below, with bodies that never retry and never deliver. | none | js-proto, js-types, js-jobs, js-callback (new), js-lease-server |
| **B. Retry decision** | The classing table, the job-fault refusal codes, the backoff and jitter. Pure (R4). | A | js-types |
| **C. Attempt store** | Tags 67 and 69 to 73, `retry_txn`, the reset of the pre-run counters, the history bound, the `attempt/` rows and their sweep, `UsageRecord.attempt`, the shared store tests. | A | js-jobs, js-usage |
| **D. Callback store** | Tag 68 and the key's digest; the `callback/` row written at submit and made due in `end_txn`; claim, answer, expire and key erasure; the boot raise; the sweep. | A | js-jobs |
| **E. Delivery** | The Standard Webhooks signer, the address rule with its resolver argument, the sender (`ureq`, no proxy, no redirects, no body read, a time limit), the answer classes. | A | js-callback |
| **J. Attempt logs** | The attempt's log prefix and its length rule; `GetLogsRequest.attempt` and the reader of an earlier attempt, from `attempt_history` or its `attempt/` row (the row's read through the store interface task A fixes). | A | js-lease-server, js-agent-wire (test only) |
| **F. Server retry (act)** | `end_of` and the loss paths ask `decide`. Placement honours `retry_not_before`, the 15-minute bar, the `start-fence` rule and the freed previous allocation. The queue wait counts from the backoff end. The API mapping; metrics. | B, C | js-lease-server, js-sched-placer |
| **G. Server callback (act)** | The submit checks, the delivery threads, `callback_status`, the key's redaction from every output, metrics. | D, E | js-lease-server |
| **H. Simulation and conformance** | The invariants and scenarios of section 9, and the reference service. | F, G, J | js-sim-cell, js-conformance |
| **I. Accept** | A release after F, G and J. Remove the two refusals; `js-client` reports a dropped policy. Update `design.md` 8.1, `retries-and-runs.md`, `queue-status.md`, `live-logs.md` (the attempt segment), `http-gateway.md` and the job sections of `jobscheduler-design.md` (3.2, 5.3). | F, G, J released | js-lease-server, js-client, docs |
| **kbf (komira-ai/kbf)** | Read the new fields in `kbf-job-client` and its goldens. Take the callback with its own key and reference to wake the executor (section 5.6). Set no retry policy (section 3.9). | I released | kbf-job-client, kbf-server |

B, C, D, E and J run in parallel once A is merged. F and G run in parallel once their inputs are merged. Each task's PR names the
tests of section 9 that it adds. Task A adds `js-callback` (runtime layer) to `layers.toml`, `coverage-baseline` and BUCK.

The interfaces task A fixes:

```rust
// js-types (pure)
pub struct RetryPolicy { pub max_attempts: u32, pub initial_backoff: Option<Duration>, pub max_backoff: Option<Duration> }
pub enum AttemptEnd { /* one variant per row of the table in 3.3 */ }
pub enum Decision { Retry { wait: Duration }, End }
pub fn decide(policy: &RetryPolicy, end: &AttemptEnd, attempt: u32, job: &JobId) -> Decision;

// js-jobs
impl JobStore {
    pub fn retry_txn(&self, id: &JobId, end: AttemptRecord, not_before: StoreTime) -> Result<JobRecord, JobStoreError>;
    pub fn claim_callbacks(&self, by: &InstanceId, now: StoreTime, limit: usize) -> Result<Vec<CallbackClaim>, JobStoreError>;
    pub fn answer_callback(&self, claim: CallbackClaim, answer: CallbackAnswer, now: StoreTime) -> Result<(), JobStoreError>;
    pub fn callback_status(&self, id: &JobId) -> Result<Option<CallbackStatus>, JobStoreError>;
    pub fn attempt_row(&self, id: &JobId, attempt: u32) -> Result<Option<AttemptRow>, JobStoreError>; // host, placed_at, ended_at
}
// insert_queued takes the callback's key and writes it into the armed callback/ row; end_txn makes the row due.

// js-callback (runtime)
pub struct SigningKey(/* 32 to 64 bytes; Debug prints nothing of it */);
pub fn check_url(url: &str) -> Result<CallbackUrl, UrlError>;                 // form only; the address is checked at send
pub fn sign(key: &SigningKey, webhook_id: &str, timestamp: u64) -> String;   // the webhook-signature value, empty body
pub struct AddressRule { /* the denied set of section 5.3, plus the cluster's own addresses */ }
pub struct Sender { /* ureq agent: no proxy, no redirects, body not read */ }
impl Sender {
    pub fn new(limit: Duration, rule: AddressRule, resolver: Box<dyn Resolve + Send + Sync>, roots: TlsRoots) -> Self;
    pub fn send(&self, url: &CallbackUrl, key: &SigningKey, webhook_id: &str, timestamp: u64) -> CallbackAnswer;
}
pub enum CallbackAnswer { Delivered, Abandoned, Failed(FailureClass) }   // FailureClass: RefusedAddress | Failed
```

## Decisions this page makes

The API owner's decisions are in section 0. Beyond them, each choice below is the one most consistent with the merged code; the
owner may reverse any of them.

- **An attempt is a placement**, counted when it is placed. The requeues before a run stay inside it, so `max_attempts` counts work
  the client can see and the pool's own bounds keep their meaning.
- **A retry keeps the job id.** It is not a child job, so polling, cancel, the result, retention and the callback all stay keyed on the
  one id the client knows.
- **One allocation per job at a time.** The next attempt waits for the previous allocation to be freed. So the allocation key stays
  `alloc/<job id>`, two attempts on heard hosts never overlap, and a stop for an earlier attempt never meets a later one. The cost, a
  retry held back by a silent host, is bounded by the queue wait and stated.
- **The failed host is barred for 15 minutes, and a return needs `start-fence`.** With logs per attempt, these are the two reasons
  left to keep a job off a host: the host's fault caused the retry, and an agent without `start-fence` may drop a known job id.
- **Attempt 1 keeps today's log key,** and later attempts add a segment no job id can be. Old jobs and logs read unchanged, and no
  agent changes.
- **The hosts a job ran on are kept as a set apart from the history,** so the `start-fence` rule does not depend on the history's
  bound.
- **The history keeps the newest 256 attempts, and every attempt keeps a row of its own.** With no ceiling on `max_attempts`, the job
  row needs a bound, and `events` already sets the pattern of a bound with a drop count. The small per-attempt row keeps every
  attempt's logs readable, as decision 9 requires. It costs about 100 bytes and one key per attempt, plus a point read for an old
  attempt's logs.
- **The other option, listing the store under the attempt's hash segment,** was rejected. The segment sits below the host segment, so
  a read would need one listing call per host the job ever ran on, and it would lose the attempt's placement time, which the reader
  uses as its cutoff.
- **Unknown agent refusal codes are the host's**, as kbf reads them today, and `max_attempts` bounds the cost.
- **The jitter comes from the job id**, so the retry decision stays pure and replays in simulation.
- **The key lives only in the callback row**, while delivery needs it. The job row holds its digest, so idempotency works without a
  second copy.
- **Private addresses are not refused.** Without an allow-list, refusing them would cut off receivers that live on a private network.
  A bare, signed, answer-blind POST is all a job can aim there.
- **A later attempt's log segment is a 64-byte hash,** one byte longer than any job id, so it never meets a job's own key.
- **The `webhook-id` is minted, not derived from the job,** so the call carries nothing about the job.
- **Two releases per feature,** act then accept, so instances of two versions over one store never mishandle a job. The gate is an
  internal `Config` field, not a setting.

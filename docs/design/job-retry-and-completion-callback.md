# A job's retry policy and its completion callback

Companion to [jobscheduler-design.md](jobscheduler-design.md), sections 3.2 (jobs), 5.3 (submit, place, cancel), 5.4 (whole-host
jobs), 6.3 (presence and loss) and 6.7 (restart recovery). Status: **proposal**. Nothing here is built. The page changes the public job API
(`crates/js-proto/proto/jobscheduler/v1/job.proto`, `service.proto`, `usage.proto`), so the owner of that API decides the open questions
below before any of it is implemented.

Two features are missing today:

1. **A retry policy.** A job runs at most once. A job that fails to start, whose host is lost, or that exits non-zero ends there, and
   the client must submit a new job. No field asks the service to try again.
2. **A completion callback.** A client learns that a job ended only by polling `GetJob` (or `ListJobs`). Nothing calls the client, and
   there is no webhook and no watch call.

This page specifies both, how they meet, how they stay compatible with released clients, agents and servers, how they fail, which tests
prove each rule, and how the work splits into tasks that can run in parallel.

## Open questions for the API owner

Each needs a decision before section 10's tasks start. Each has a recommendation; the rest of the page is written as if every
recommendation is taken.

**Settled by the API owner: when the callback is sent.** The callback is sent only after retries are exhausted. Each job gets one
callback, sent once the job reaches its final state, and never one per attempt. The final state is one of: succeeded; failed with no
retry left (an outcome no condition names, or the last allowed attempt); or cancelled. Section 6 states this as rule C1. The questions
below no longer include it.

1. **Should the service retry at all?** Section 2.2 of the design lists "being a workflow engine" as a non-goal and says that "retry
   policy and result interpretation belong to the client". kbf's own design says "the job service retries nothing". This page proposes a
   narrow exception: an opt-in, bounded policy over outcomes the service can judge (the host's faults, and a non-zero exit only when the
   client asks). **Recommendation: yes, opt-in, with no retry by default.** If it is taken, section 2.2's line changes to "result
   interpretation belongs to the client; the service retries only what a job's retry policy names" in the PR that builds the policy.
2. **SubmitJob's promise.** `SubmitJob` says that "a retry after a lost answer never runs the work twice". That stays true of the submit.
   A job with `max_attempts` above 1 asks for its work to run up to that many times, and the work must tolerate it. **Recommendation:**
   keep the submit promise as it is. Add to `RetryPolicy`'s comment that each attempt runs the work again, and that in one case two
   attempts can overlap: a `static` host an operator removed while it was still running the job (section 3.6).
3. **Which outcomes may retry.** Section 3.3 proposes five conditions a client can name: start failure, host failure, non-zero exit,
   wall limit and memory. A cancel never retries. `NO_CAPACITY` (the pool's queue wait ran out) and `EXCLUSIVE_NOT_READY` (the whole-host
   prepare already failed `max_replace` times) never retry, because each is the end of a bound the pool already sets.
   **Recommendation: as section 3.3 states.** If the client names no conditions, the default is start failure plus host failure.
4. **Callback or watch.** Section 5.1 compares a webhook the server calls with a watch the client holds open (a gRPC stream, or a unary
   long poll like `GetLogs` follow). **Recommendation: build the webhook,** stored in Postgres and delivered at least once. Add a
   long-poll field on `GetJob` later, as its own small change, if a client that cannot take inbound calls asks for one. Do not build a
   streaming RPC: the HTTP gateway serves unary methods only.
5. **How the callback is authenticated, and where it may go.** **Recommendation:** each `[[client]]` table in the server's configuration
   lists the URL prefixes that client's callbacks may use, and names the file of an HMAC key. The server signs each call in the
   [Standard Webhooks](https://www.standardwebhooks.com/) format. A job carries no secret. A URL outside its client's prefixes is refused
   at submit (section 5.2).
6. **Names.** `Job.attempts` (field 34) has shipped. It counts *placement* attempts: each placement and each machine refused for the
   job. Under B8 it cannot take another meaning. **Recommendation:** name the new things `RetryPolicy.max_attempts`, `Job.attempt` (the
   number of the current attempt, from 1) and `Job.attempt_history`. Field 34's comment then says that it counts placements over all of
   the job's attempts.
7. **Bounds.** **Recommendation:** the server's ceiling on `max_attempts` is set by `server.max_job_attempts`, default 5, at most 10. A
   callback is tried for `deadline_seconds` after the job's end, default 24 hours, cut to the job's retention end. No second server
   ceiling is added for it: the retention window already bounds it.
8. **What a callback carries.** **Recommendation:** the job's id, state, attempts and outcome, with times and labels. It carries no log
   tail, argument, environment variable or file: a tail is untrusted bytes that may hold a secret, and kbf puts secrets in arguments and
   files. Where the outputs are is given as the API paths of `GetResult` and `GetLogs`, which the receiver reads with its own token
   (section 5.3).
9. **Logs of earlier attempts.** Live logs are stored under `{prefix}/{host}/{job}/{stream}/{offset}` (`live-logs.md`, section 2), with
   no attempt in the key. Two attempts on one host would write to the same keys. **Recommendation:** v1 serves the logs of the current
   (or final) attempt only, and never places a later attempt on a host that held an earlier one (section 3.5). A later change, task J,
   puts the attempt in the log key, after which same-host retries and an `attempt` field on `GetLogsRequest` become possible.

## 1. What a client sees

Before:

- A job ends after one run (`SUCCEEDED`, `FAILED`, `CANCELLED`, `LOST`, `FAILED_TO_START`), and the service never runs it again.
- A client learns of the end by polling. kbf calls `GetJob` for each running job every `--executor-poll-ms` (default 1000 ms).
  `js-client`'s `wait_for_terminal` polls every 2 s (`crates/js-client/src/jobs/loops.rs`, `POLL_EVERY`).

After:

- A job may carry a `retry` policy: up to `max_attempts` attempts, the outcomes that start another, and a backoff between them. Between
  attempts the job is live again: it is `QUEUED`, with queue status `WAITING_FOR_RETRY` until its backoff ends. The job ends only when an
  attempt's outcome does not retry, the attempts run out, or the job ends while it waits for its next attempt (a cancel, or the queue
  wait running out). `Job.attempt` names the current attempt, and `Job.attempt_history` lists the attempts that have ended, with each
  one's host, times and outcome.
- A job may carry a `callback`: a URL the server sends one signed POST to once the job reaches its final state, after its retries
  are exhausted, and never per attempt (rule C1, section 6). The call is retried until the receiver
  answers 2xx or the delivery deadline passes. Its progress is stored in Postgres and survives a server restart, and `Job.callback_status`
  shows it.

What does not change:

- A job without `retry` runs once, exactly as today. Only a job whose submitter set `retry` can go from a run back to `QUEUED`. A
  client that never sets it never sees that move. A job without `callback` is never called back.
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
  record's `prepare_bars`, which is the existing way to keep a job off one host (`HELD_BAR`, one hour, for `host-held`).
- **Counting.** `Job.attempts` counts placements and machines refused for the job (`crates/js-sched/src/queue_status.rs`), and a requeue
  is not one. `Job.events` keeps the newest 32 events.
- **One allocation per job at a time**, keyed `alloc/<job id>`. Its history row (`usage/ended/...`, one `UsageRecord` per booking) is
  written in the commit that frees it (`crates/js-lease-server/src/job_alloc.rs`).
- **Late reports.** A listed result names its allocation (the agent wire's `JobResult.allocation_id`,
  `crates/js-agent-wire/src/per_job.rs`). `placement_of` and `commit_listing` (`crates/js-lease-server/src/per_job.rs`) already answer a
  result whose allocation is not the job's live one, or whose end the store refuses as an illegal move, with `Ack::Stale`, and change
  nothing.
- **Idempotency.** `JobSpec::same_job_as` (`record.rs`) names every field of the spec, so a field added to the spec does not compile
  until it is either compared or deliberately left out. Priority and client sit on the record, not the spec, and are not compared.
- **Store.** Postgres holds an ordered key/value table with a version on each row (`crates/js-state/src/postgres/migrations/0001_state.sql`).
  A job is one row, `job/<id>`, a tag-length-value record whose unknown tags are carried and written back (`crates/js-jobs/src/codec.rs`).
  The next free job tag is 66. Several instances share one store and no instance leads (`docs/design/multi-instance.md`). Work is claimed
  row by row by compare-and-set.
- **An outbox exists.** The dispatch queue (`crates/js-jobs/src/store/dispatch.rs`, `claim_dispatch`, `ack_dispatch`) writes a message in
  the same transaction as the state change. A row carries `claimed_by` and `claim_until`, is sent again after its claim lapses, and is
  deleted by its ack or with its job.
- **Boot.** `raise_at_boot` (`ending.rs`) raises each stored deadline to at least boot plus 120 s, so an outage deletes no result and
  fails no queued job.
- **Agents.** An agent that reports the `start-fence` capability runs the same job id under a new allocation as a new start
  (`per_job.rs`, `START_FENCE_CAPABILITY`). One without it "may judge a start by its job id alone".
- **Outbound HTTP.** The alert webhook (`crates/js-alert/src/webhook.rs`, `http.rs`) uses sync `ureq` with no proxy, no redirects and a
  10 s limit. Its URL rule (`check`) takes `https`, or plain `http` to a loopback address literal only. It retries a refused POST after
  5 s, doubling to 10 minutes, but keeps its queue in memory, so a restart loses it. That is the gap this page closes for callbacks.
- **How kbf uses end states** (komira-ai/kbf, `crates/kbf-server/src/executor/outcome.rs`). kbf classes an end by its state, start
  failure code, result reason and exit code, and reruns infrastructure faults itself. Each rerun is a new job id ending `-<attempt>`. Its
  budgets are its own: a memory ladder, `FARM_RERUNS`, `NOT_STARTED_RERUNS` and `LOSS_REQUEUES`. An action's own non-zero exit is a
  result, not a failure: kbf's task tool exits 0 when the action ran.

### 2.1 How this relates to the older `RetryPolicy`

[design.md](../design.md) section 8.1 and [retries-and-runs.md](retries-and-runs.md) describe a `RetryPolicy` of the generic scheduling
core from the lease design. It has budgets per axis (own-limit, platform fault, not started, loss, task failure) and an `evicted`
outcome that charges none (D63 in [decisions.md](decisions.md)). None of it is in the job API or in code. This page is the job API's
policy and supersedes those two sections for jobs. Task I updates both to point here. Their axes map onto the conditions of section 3.3
as follows: not started is `START_FAILURE`; platform fault and loss are `HOST_FAILURE`; task failure is `EXIT_NONZERO`. The own-limit
ladder has no counterpart: `MEMORY` retries with the same booking unless the job's needs are sized from its class. Eviction is not built.
When it is, an evicted attempt is requeued without counting against `max_attempts`, as D63 says, and gets an outcome value of its own
then.

## 3. Retry policy

### 3.1 The policy on the job

```proto
// job.proto (additions)

// An outcome of an attempt that may start another (section 3.3 of
// docs/design/job-retry-and-completion-callback.md).
enum RetryCondition {
  RETRY_CONDITION_UNSPECIFIED = 0;    // what an unknown value reads as; it matches no outcome
  RETRY_CONDITION_START_FAILURE = 1;  // the attempt did not run, for a cause of its host
  RETRY_CONDITION_HOST_FAILURE = 2;   // the attempt ran and its host failed it
  RETRY_CONDITION_EXIT_NONZERO = 3;   // the job's process exited non-zero
  RETRY_CONDITION_WALL_LIMIT = 4;     // the attempt was stopped at its wall limit
  RETRY_CONDITION_MEMORY = 5;         // the attempt was stopped for memory
}

// Each attempt runs the work again; only a job that sets this can go from a run back to
// queued. In one case two attempts can overlap (section 3.6).
message RetryPolicy {
  // The most attempts, the first included. 0 or 1: no retry. Above the server's ceiling:
  // refused attempts-above-ceiling.
  uint32 max_attempts = 1;
  // The outcomes that start another attempt. Empty: START_FAILURE and HOST_FAILURE. A list
  // that holds only values this server does not know retries on nothing.
  repeated RetryCondition retry_on = 2;
  // The wait before the second attempt, in seconds. Absent: 10. Cut to 3600.
  optional uint32 initial_backoff_seconds = 3;
  // The longest wait, jitter included. Absent: the larger of 300 and the initial wait.
  // Cut to 3600.
  optional uint32 max_backoff_seconds = 4;
}

message JobSpec {
  ...
  // Absent: one attempt, as before.
  RetryPolicy retry = 70;
}
```

- **It sits in the spec,** so `same_job_as` compares it. A resubmit with another policy is `job-id-conflict`, because the policy is
  part of what the job is. Unlike the priority, a client cannot change it after the submit.
- **The wall limit is per attempt.** That is ucdk's UJ2, and it is what a retry needs: a hung attempt that is stopped at its limit may
  be retried.
- **Validation at submit.** A `max_attempts` above `server.max_job_attempts` is refused with a new `RefusalCode`,
  `REFUSAL_CODE_ATTEMPTS_ABOVE_CEILING = 5`. It changes how many times the work runs, so the server never lowers it quietly, as it
  refuses a wall limit above its ceiling (jobscheduler-design.md, section 13, question 8). A `max_backoff_seconds` given below the
  effective initial wait is an invalid-argument error, and so is a condition named twice.
- **Unknown conditions.** A value this server does not know reads as `UNSPECIFIED`, which matches no outcome. So a newer client's
  condition never widens what an older server retries. The default applies only to a list the client sent empty.

### 3.2 What an attempt is

An **attempt** is one placement of the job, from the placement until the outcome that ends or retries the job. `Job.attempt` is set to
`n` when attempt `n` is placed. While the job waits for its next attempt, `Job.attempt` still names the attempt that ended, and
`retry_not_before_ms` is set. A job that ends while it waits (a cancel, or its queue wait running out) ends with no new attempt counted.
Its `attempt_history` keeps every attempt that ran.

The requeues before a run (a failed whole-host prepare, and a start refused `host-held`, `no-capacity` or `bundle-pending`) stay inside
the attempt they happen in. They keep their own bounds, as today, and are not counted against `max_attempts`. So `max_attempts` bounds
the work the client sees, and the placement machinery keeps the bounds it has.

Each attempt keeps its own pre-run counters (`failed_prepares`, `held_refusals`, `capacity_refusals`). They are reset when the job is
queued for its next attempt, so attempt 2 gets its own `max_replace`. Host bars (`prepare_bars`) are kept across attempts.

**Lifetime.** A job's life is bounded by `max_attempts × (max_queue_wait + start_timeout + wall limit) + (max_attempts − 1) × 3600 s`
(the longest backoff). The first factor includes the wait for the previous allocation to be freed (section 3.5). The retention window
follows the final end.

### 3.3 Which outcomes retry

The outcome of an attempt is classed in one place, a pure function in `js-types` (section 10, task B). The table below is that function.

| Attempt ended as | Job state today | Condition that retries it |
| --- | --- | --- |
| exit code 0 | `SUCCEEDED` | never |
| a non-zero exit | `FAILED`, reason `EXIT` | `EXIT_NONZERO` |
| stopped at the wall limit | `FAILED`, reason `WALL_LIMIT` | `WALL_LIMIT` |
| stopped for memory | `FAILED`, reason `MEMORY` | `MEMORY` |
| a host path or host port failed while it ran | `FAILED`, reason `INPUT_FAILED` | `HOST_FAILURE` (the result says the fault is the host's) |
| lost after it started (absent host, or not listed after a hello) | `LOST`, with a start time | `HOST_FAILURE` |
| lost before it started (absent host while placed or starting) | `LOST`, with no start time | `START_FAILURE` |
| the start timed out | `FAILED_TO_START`, `START_TIMEOUT` | `START_FAILURE` |
| an input could not be read on the host | `FAILED_TO_START`, `INPUT_UNAVAILABLE` | `START_FAILURE` |
| the agent refused it with a code of the host | `FAILED_TO_START`, `AGENT_REFUSED` | `START_FAILURE` |
| the agent refused it with a code of the job | `FAILED_TO_START`, `AGENT_REFUSED` | never |
| the queue wait ran out | `FAILED_TO_START`, `NO_CAPACITY` | never: the pool's bound on waiting |
| `max_replace` prepares failed | `FAILED_TO_START`, `EXCLUSIVE_NOT_READY` | never: the whole-host bound already ran out |
| the client cancelled it | `CANCELLED` | never |

- **Agent refusal codes.** The codes that name the job's own fault are listed in the classifying function: those of
  `crates/js-agent-wire/src/refusal.rs` that a bad job gets (such as `invalid-job`, `invalid-file`, `invalid-image` and
  `image-not-allowed`), and `bundle-not-allowed`. Every other code is the host's, including a code this server does not know: a retry is
  bounded by `max_attempts`, and that is how kbf reads an unknown code today (infrastructure, rerun).
- **A drained host.** In merged code a drain never cuts work (section 6.4 of the design). A host that is updated or restarted without a
  handover loses its starting and running jobs through `lose_unlisted`, so "drained" arrives as `LOST` and retries under
  `HOST_FAILURE`.
- **A cancel wins.** A job with a cancel requested never starts another attempt. A cancel during a backoff ends the job `CANCELLED` at
  once, as a queued job's cancel does today.

### 3.4 Backoff

- Let `initial` and `max` be the effective values (section 3.1). The wait before attempt `n + 1`, with `n` counted from 1, is
  `min(max, initial × 2^(n-1) × (1 + j))`. The jitter `j` lies in `[0, 0.1)` and is the first 8 bytes of SHA-256 of `job id || n`,
  scaled. No random number is drawn, so the decision stays pure (R4) and a simulation replays it. `js-types` gains
  `sha2.workspace = true` for it.
- The server writes the end of the wait as an instant, `retry_not_before`, using the store's clock, in the same commit that queues the
  next attempt. Placement passes over the job until that instant. The pool's `max_queue_wait` for the attempt counts from
  `retry_not_before`, not from the first submit.
- A restart does not move the instant. A not-before that has passed lets the job be placed, and nothing expires on it, so it needs no
  boot raise. The attempt's queue-wait deadline is raised at boot like any other waiting deadline.

### 3.5 Where a retry runs

- **Never on a host that held an earlier attempt of the job,** until task J puts the attempt in the log key (open question 9). Each
  attempt's host is barred for the job's remaining life, with an entry in `prepare_bars`, which the placer already honours. This also
  means a retry never depends on `start-fence`: no host is offered the same job id twice. A pool with a single capable host therefore
  cannot retry. The job waits for another host until its queue wait ends with `NO_CAPACITY`, so a client should not set a policy for
  work only one host can run.
- **After task J,** an exit, wall-limit or memory retry may use the same host (the fault was the job's), but only on a host whose agent
  reports `start-fence`. A start or host failure keeps barring its host, for one hour (`HELD_BAR`).
- **One attempt at a time.** The next attempt is not placed until the previous attempt's allocation is freed: its host has confirmed
  the container gone, or the provider has read the host absent. This keeps "one allocation per job at a time" (`alloc/<job id>`) as it
  is. The cost: an attempt that timed out on a host that then went silent keeps its allocation booked (design section 3.2, "one limit is
  accepted"). Its retry then waits until the host is heard or read absent, at most until the next attempt's queue wait ends.
  `GetJob` shows it, with `host_silent_since_ms` on the attempt's host.
- **Late reports.** A result that the agent resends after `retry_txn` committed names the attempt's allocation, which is now releasing,
  and the job is queued. The store refuses that end as an illegal move, so it is answered `Ack::Stale` and dropped, as today. A listing
  of the earlier attempt from its host is answered with a stop. That stop names no allocation and retires every fence of the job on
  that host. That is safe because that host is barred and never holds a later attempt. A host that returns after the provider read it
  absent is seated as awaiting and takes no start until its listing is clean.
- **Priority and order.** A retried job keeps its priority and its original queue time, so it does not go to the back of the queue.

### 3.6 Whole-host jobs

A whole-host attempt that ran and was lost, or whose outcome a condition names, goes back to the whole-host wait list once its
allocation is freed, after its backoff, with its host barred. The mark pass treats it as any other waiting whole-host job. Prepare
failures stay inside the attempt and are bounded by `max_replace` per attempt (section 3.2).

The one case where two attempts can overlap is the same for both kinds of job. An operator removes a `static` host from its pool while
that host's agent is still running the job. The provider then reads the host absent, the allocation is freed with no confirmation, and
attempt 2 may start elsewhere while attempt 1 still runs. The existing rule bounds it: a host that later lists a lost job is told to stop
it. The policy's comment says so (open question 2).

### 3.7 How attempts appear

New fields, all additive:

```proto
// How one attempt ended.
enum AttemptOutcome {
  ATTEMPT_OUTCOME_UNSPECIFIED = 0;
  ATTEMPT_OUTCOME_SUCCEEDED = 1;
  ATTEMPT_OUTCOME_EXITED_NONZERO = 2;
  ATTEMPT_OUTCOME_WALL_LIMIT = 3;
  ATTEMPT_OUTCOME_MEMORY = 4;
  ATTEMPT_OUTCOME_INPUT_FAILED = 5;
  ATTEMPT_OUTCOME_HOST_LOST = 6;
  ATTEMPT_OUTCOME_START_FAILED = 7;
  ATTEMPT_OUTCOME_CANCELLED = 8;
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

// Where a completion callback is (section 5.5).
enum CallbackState {
  CALLBACK_STATE_UNSPECIFIED = 0;     // no callback, or a value this reader does not know
  CALLBACK_STATE_PENDING = 1;
  CALLBACK_STATE_DELIVERED = 2;
  CALLBACK_STATE_EXPIRED = 3;         // its deadline passed first
  CALLBACK_STATE_ABANDONED = 4;       // the receiver answered 410, or its URL left the prefixes
}

message CallbackStatus {
  CallbackState state = 1;
  uint32 tries = 2;
  optional uint64 last_try_at_ms = 3;
  string last_failure = 4;            // a code such as http-503, timeout, connect, redirect
  optional uint64 delivered_at_ms = 5;
  uint64 deadline_at_ms = 6;
}

message Job {
  ...
  uint32 attempt = 37;                      // the current attempt, or the last that ran; 0 (older record): 1
  repeated Attempt attempt_history = 38;    // the attempts that ended, oldest first; at most the ceiling
  optional uint64 retry_not_before_ms = 39; // while the job waits out a backoff
  CallbackStatus callback_status = 40;      // absent: the job ended without a callback, or has not ended
}

message JobResult { ... uint32 attempt = 13; }            // the attempt this result is of; 0: 1
message UsageRecord { ... optional uint32 attempt = 24; } // absent: 1
enum QueueStatusCode { ... QUEUE_STATUS_CODE_WAITING_FOR_RETRY = 7; }
enum JobEventKind { ... JOB_EVENT_KIND_RETRY_SCHEDULED = 8; } // detail names the outcome and the wait
```

- **History.** Each ended attempt appends an `Attempt`, in the commit that queues the next attempt or ends the job, and a
  `RETRY_SCHEDULED` event when another follows. `attempt_history` is bounded by the ceiling (at most 10 entries of a few dozen bytes
  each), so it needs no drop counter. The 32-event bound of `Job.events` is unchanged.
- **The job's own fields are the current attempt's.** `host_id`, `times.placed_at_ms`, `times.started_at_ms`, `start_failure` and the
  result are the current attempt's. When the job is queued for a retry, they are cleared, as a failed prepare clears host and placement
  today, and the attempt's values move into `attempt_history`. `times.queued_at_ms` stays the first submit. An earlier attempt's tails
  and usage are not kept on the job: its exit code and reason are in its `Attempt`, and its usage in its `UsageRecord`.
- **Usage.** Each attempt's booking is already its own `UsageRecord`. It gains `attempt`. Sizing reads usage records as today. A lost
  attempt has no result and teaches the class nothing.
- **Store.** The job record gains tags 66 (`retry`, in the spec), 67 (`callback`, in the spec), 68 (`attempt`), 69 (`attempt_history`)
  and 70 (`retry_not_before`). A record without them reads as attempt 1 with no history and no callback. `callback_status` is not on
  the job row: `GetJob` reads it from the job's `callback/` row (section 5.5). No migration is needed: the store is one key/value table.
- **The move.** A new store operation, `JobStore::retry_txn`, sits beside `end_txn`, outside `may_move`, as the existing requeues do,
  so no other path can move a live job to queued. In one commit it appends the attempt, clears the attempt's fields, resets the pre-run
  counters, adds the host bar, sets `retry_not_before`, and puts the job in `idx/job/queued` again. It moves the allocation to releasing
  exactly as `end_txn` does. It keeps the job's files (no digest), sets no `retention_ends` and writes no callback row: the job has not
  ended. The attempt's outcome is decided in the same commit, so the job never reads as ended between attempts.

### 3.8 What kbf would set

- **v1: no policy.** kbf owns its budgets (the memory ladder, `FARM_RERUNS`, `NOT_STARTED_RERUNS`, `LOSS_REQUEUES`). Each rerun gets a
  new job id and a fresh upload key, and an action's own failure is a result. A service-side retry under the same job id would count a
  fault twice and reuse an upload key. kbf therefore sets no `retry` until it moves a budget into the service on purpose.
- **Later, if kbf wants it:** `retry { max_attempts: 4, retry_on: [START_FAILURE] }` could replace `NOT_STARTED_RERUNS` (3 reruns plus the
  first attempt), because a start failure ran no part of the action and wrote nothing to its upload key. kbf would then drop that budget
  in the same change, so nothing counts twice.
- **ucdk's UJ1** (one budget over platform faults, not-started and task failures) maps to
  `retry_on: [START_FAILURE, HOST_FAILURE, EXIT_NONZERO]` with `max_attempts = max_retries + 1`.

## 4. API, configuration and metrics

- **Refusals.** `REFUSAL_CODE_ATTEMPTS_ABOVE_CEILING = 5` and `REFUSAL_CODE_CALLBACK_NOT_ALLOWED = 6` are appended. `RefusalCode`'s rule
  already says an unknown code reads as unspecified.
- **Configuration**, all checked once at start (B14). Nothing is read from the environment.

  | Key | Flag | Default | Range |
  | --- | --- | --- | --- |
  | `server.max_job_attempts` | `--max-job-attempts` | 5 | 1 to 10 |
  | `[[client]] callback_url_prefixes` | (file only) | none: the client cannot ask for a callback | each a URL ending in `/`, at most 16 |
  | `[[client]] callback_key_file` | (file only) | none; required with prefixes | an owner-only file of one or two base64 keys of at least 32 bytes |

  The number of delivery threads is fixed in code (8 per instance), not a setting.
- **Metrics.** `js_job_retries_total{condition}`, `js_job_attempts_exhausted_total`, `js_callback_deliveries_total{result}` and
  `js_callback_pending`.
- **Gateway.** No new route: the fields ride on `POST /v1/jobs` and `GET /v1/jobs/{job_id}`. The schema test's word filter
  (`CAPACITY_WORDS` in `crates/js-proto/tests/schema.rs`) refuses none of the new names.

## 5. Completion callback

### 5.1 Push or watch

| | A. Webhook: the server POSTs to a URL | B. gRPC server stream `WatchJobs` | C. Unary long poll on `GetJob` |
| --- | --- | --- | --- |
| Client needs | an inbound HTTPS endpoint | a held connection, a gRPC client | an outbound call, like polling |
| HTTP gateway | not involved | cannot route it (unary only, `crates/js-api-gateway/src/annotation.rs`) | routes it as a GET |
| Survives a server restart | yes, the delivery row is in Postgres | the client reconnects and must not miss an end in the gap, so it needs a cursor | the client calls again |
| At-least-once | the server's job, with retries to a deadline | the client's job, with a cursor | the client's job |
| New attack surface | the server calls a URL a tenant gave, so an allow-list is needed | none | none |
| Server cost | a delivery worker and an outbox | one task per watcher | one wait slot per call (as `max_follow`) |
| Who it serves | services, serverless receivers, ucdk | long-lived clients | any client that already polls |

**Recommendation: A for v1** (open question 4). It is the only option where the server owns delivery: a client that is down when its
job ends still learns of it, with no state of its own. kbf's design asks for "a long-poll or pushed result". kbf-server can receive the
call and use it to wake its executor before the next poll. B is rejected: the gateway cannot serve it, and it brings back a connection
the client must hold. C is cheap and useful (new optional `GetJobRequest` fields `wait_seconds` and `known_state`, with the shape of
`GetLogs` follow in `crates/js-lease-server/src/job_api/logs.rs`). It is a second way to learn of an end, so it is left out until a
client asks.

### 5.2 How a client asks for one

```proto
message Callback {
  // Where the server POSTs the completion: https, or http to a loopback address literal.
  // It must lie under one of the URL prefixes the server's configuration lists for the
  // submitting client; else refused callback-not-allowed.
  string url = 1;
  // How long the server keeps trying, in seconds from the job's end. Absent: 86400.
  // Cut to the job's retention end.
  optional uint32 deadline_seconds = 2;
}

message JobSpec { ... Callback callback = 71; }
```

- **The allow-list is the server's** (`[[client]] callback_url_prefixes`, section 4). This is the first URL a tenant supplies that the
  server calls, so the operator's prefix list is what stops a job from aiming the server at an internal address. The URL must pass
  js-alert's rule (`check` in `crates/js-alert/src/http.rs`: https, or http to a loopback literal, with no userinfo and no fragment).
  It is then compared with each prefix as a parsed URL, not as a string:
  - the scheme and port are equal, and the host is equal after lower-casing;
  - its path starts with the prefix's path, and every prefix path ends in `/`, so `https://a.example/` never admits
    `https://a.example.evil/`;
  - a path with a dot segment (`.` or `..`), an encoded dot (`%2e`), an encoded slash (`%2f`) or a backslash is refused, so no
    receiver or proxy in front of it can turn an admitted path into another one.
  The server follows no redirects and uses no proxy, as the alert webhook does. Each try checks the stored URL against the client's
  current prefixes again. A URL whose prefix the operator removed is abandoned with `last_failure: prefix-removed`.
- **In the spec,** so the callback is part of idempotency like the retry policy. It holds no secret, so nothing in it is digested at
  the end.
- **Configuration is checked once at start** (B14, B20). A prefix that is not a valid URL, or a key file that is missing, not
  owner-only or too short, stops the server with the key named. The key file is read again on each tick so that a key can be rotated
  without a restart, as client tokens are today. A file that cannot be read keeps the keys in use and logs a warning.

### 5.3 What it carries

The body is the proto3 JSON form of a new message, so it follows the evolution rule like every other message:

```proto
message JobCompletion {
  string callback_id = 1;        // the idempotency key, opaque: "<job id>:<queued_at_ms>"
  string job_id = 2;
  JobState state = 3;            // the final state
  uint32 attempts = 4;           // how many attempts ran (were placed)
  repeated Attempt attempt_history = 5;
  optional int32 exit_code = 6;  // the final attempt's, when it exited
  ResultReason reason = 7;
  StartFailureCode start_failure = 8;
  JobTimes times = 9;
  map<string, string> labels = 10;
  optional string client = 11;
  string result_path = 12;       // "/v1/jobs/<id>/result"
  string logs_path = 13;         // "/v1/jobs/<id>/logs"
}
```

- **No tails, arguments, environment or files** (open question 8). The callback says *that* the job ended and how. The receiver reads
  the result and the logs with its own token, through calls that already check the caller's role. A forged call can therefore at worst
  make the receiver read a job it can read anyway.
- **The idempotency key** is `callback_id`, which is also the `webhook-id` header. It is the same on every try of one callback. A
  receiver treats it as opaque. It includes the job's first queue time, because a job id can be used again once the retention window of
  the job that held it has passed, and a new job with that id must not be taken as a duplicate. The `:` cannot occur in a job id, so the
  key is never ambiguous.

### 5.4 Signing

The server signs in the [Standard Webhooks](https://www.standardwebhooks.com/) form, so a receiver can use a library that already
exists:

- Headers `webhook-id` (the `callback_id`), `webhook-timestamp` (seconds since the epoch at this try, by the store's clock) and
  `webhook-signature: v1,<base64 HMAC-SHA256(key, id "." timestamp "." body)>`.
- With two keys in the file, both signatures are sent, separated by a space, so a key can be rotated without a missed call.
- The receiver checks the signature and refuses a timestamp more than 5 minutes from its own clock, so an old call cannot be replayed.
- No bearer token and no mTLS: one scheme, with no second way to do the same thing. The workspace already depends on `hmac` and `sha2`.

### 5.5 Delivery, at least once

- **The outbox row.** `end_txn` writes `callback/<job id>`, in the same commit as the job's final end and only for a job with a
  callback. The row holds the URL, the body, `next_try_at`, `deadline_at`, `tries`, `claimed_by`, `claim_until`, `last_failure` and the
  delivery state. An index entry `idx/callback/due/<next_try_at ns>/<job id>` orders the pending rows. The end and the row commit
  together, so an end can never lose its callback, and a callback is never sent for a job that did not end. The write is unconditional:
  it replaces a stale row that a rolled-back build left behind (section 7).
- **The worker.** Each instance runs 8 delivery threads, off the tick path like the alert notifier. A thread claims a due row by
  compare-and-set (`claimed_by` = this instance, `claim_until` = now + 30 s), sends it within a 10 s limit, and commits the answer at the
  claimed version. Two instances never hold a claim on one row at once. An instance that dies mid-send leaves a claim that lapses, and
  the row is sent again.
- **What an answer means.** A 2xx answer delivers the callback. A 410 abandons it: the receiver says it does not want it. Any other
  status, a timeout, a refused connection or a redirect is a failure, and the next try waits 5 s, doubling to at most 10 minutes (the
  alert webhook's values).
- **Deadline.** `deadline_at` is the job's end plus `deadline_seconds`, cut to the job's `retention_ends`. Past it, the row is
  `EXPIRED` and nothing more is sent. At boot, `raise_at_boot` raises each pending row's deadline to at least boot plus 120 s, as it does
  a retention end, so an outage of the server expires no callback.
- **Sweep.** The retention sweep deletes the row with its job row (`delete_job`, `delete_due`). The sweep also deletes a `callback/`
  row whose job row is gone, which only a rolled-back build can leave.
- **Status.** `GetJob` and `ListJobs` read the job's `callback/` row and answer it as `Job.callback_status`.
- **What at-least-once means here.** A receiver may get one callback more than once: after a lost answer, a timeout, or a claim that
  lapsed. It dedupes on `webhook-id`. It gets none only if it never answers 2xx before the deadline, and `callback_status` then says
  `EXPIRED`.

## 6. How the two meet

- **Rule C1: one callback per job, sent only after retries are exhausted.** The callback is sent once the job reaches its final
  state, and never per attempt. The final state is one of:
  - succeeded;
  - failed with no retry left: the outcome is one no condition names, or it is the last allowed attempt. This covers `FAILED`, `LOST`
    and `FAILED_TO_START`;
  - cancelled.

  The store enforces this. `retry_txn` writes no callback row. Only `end_txn` writes one, and `end_txn` runs only when no attempt
  follows. A job retried three times that then succeeds is called back once, with `state: SUCCEEDED`, `attempts: 4` and the four
  attempts in `attempt_history`.
- **The final state** is the state the job ended in. That is the last attempt's outcome when the attempts ran out or the outcome was not
  retryable, for example `LOST`. It is `CANCELLED` or `FAILED_TO_START` with `NO_CAPACITY` when the job ended while it waited for its next
  attempt. In every case `attempt_history` carries what each attempt did, so a client sees, for example, that attempt 1 exited non-zero
  before the retry found no capacity.
- **A cancel during a backoff** ends the job `CANCELLED` in one commit through `end_txn`, so the callback row is written in that commit.
- **No callback per attempt,** by rule C1. A client that wants to see attempts as they happen reads `GetJob`. No option asks for a
  call per attempt. Such an option would multiply deliveries and make "once" depend on the policy.

## 7. Compatibility

The evolution rule in `service.proto`'s header holds for every change: fields, enum values and messages are appended, nothing is
renumbered, and no shipped field takes another meaning.

- **Old clients, new server.** A job without `retry` or `callback` behaves exactly as today. Only a job whose submitter set `retry` can
  go from a run back to `QUEUED`, so a client that never sets it never sees that move. A client that reads jobs another client gave a
  policy skips the new fields, and an unknown `QueueStatusCode` or `JobEventKind` reads as unspecified, as those enums already say.
- **New client, old server.** An old server's decoder skips fields 70 and 71, so the job runs once with no callback. The answer's
  `job.spec` does not echo them. That is how a client tells: a client that sent a policy and sees no `retry` in the returned spec knows
  the server dropped it. `js-client` checks this and reports it (task I).
- **Old agents.** The agent wire does not change. A retry is a new placement with a new allocation id, on a host that never held the
  job (section 3.5), so it needs nothing of an agent that it does not already do.
- **Stored rows.** Older builds carry the new job tags (`Carried`). The `callback/` rows and `idx/callback/` entries are a new key
  prefix, which an older build neither reads nor deletes.
- **Instances of two versions over one store** (multi-instance.md, section 7). During a rolling update, an older instance could end a
  job that has a policy without retrying it, or end a job with a callback without writing its row. So each feature lands in two
  releases:
  1. **Act:** the release that can retry and deliver. `SubmitJob` still refuses a job carrying `retry` or `callback` (`INVALID_ARGUMENT`,
     `retry-not-enabled` or `callback-not-enabled`). The refusal is a field of the server's internal `Config` struct, not a configuration
     key or flag. Tests of tasks F, G and H build the server with it off, so the act release runs end to end before anything accepts.
  2. **Accept:** a later release removes the refusal. Every instance that can meet such a job then already acts on it.
  The proto fields land first (task A), with the refusal in place, so no server of this repository from task A on takes a field it
  ignores. Servers released before task A skip the fields, as above.
- **Rollback.** Once the accept release has run, rolling the fleet back below the act release is not supported: an older build would end
  policy jobs without retrying them and callback jobs without a row. A rollback to the act release is safe. The release notes of the
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
| Every capable host has held an attempt | The job waits, and ends `NO_CAPACITY` at its queue wait. `attempt_history` keeps what ran. |
| An operator removes a running `static` host | Attempt 1 may still run while attempt 2 starts elsewhere. A host that lists a lost job is told to stop it (section 3.6). |
| The receiver is down | Tries back off to 10 minutes until the deadline. The job is unaffected. The status is `EXPIRED`. |
| The receiver is slow past 10 s | A timeout and a retry. The receiver may process both tries and dedupes on `webhook-id`. |
| An instance dies holding a claim | The claim lapses after 30 s. Another instance sends the same `webhook-id`. |
| The server is down for longer than the deadline | `raise_at_boot` gives each pending callback at least 120 s after boot. |
| The key file becomes unreadable | The keys in use are kept, with a warning. A server that starts without it refuses to start. |
| A tenant aims a callback at an internal address | Refused at submit unless the operator listed its prefix (section 5.2). |
| A captured call is replayed | The receiver refuses a timestamp older than 5 minutes, and a repeat `webhook-id`. |
| A job id is used again after retention | The new job has a new `callback_id`. The old row was swept with its job. |

## 9. Tests

The tests follow the repository's rules. They are in Rust only. Every wait uses the store clock or `ManualClock` and never a sleep, and
no test bounds an elapsed time (T10). The callback sender's time limit is a constructor argument. A test sets it short and keeps the
receiver blocked on a channel that is never released, so the timeout is the only possible answer and no timing is asserted. Receivers
listen on loopback port 0, with no DNS (T12). Postgres is reached through `js_testkit::database`. Each store rule is in the shared
`job_store_tests!` macro, so it runs on the fake, SQLite and Postgres backends.

| Rule | Test | Task |
| --- | --- | --- |
| No policy: one attempt, ended as today | `js-jobs` `job_without_a_policy_ends_after_one_attempt` | C |
| Each row of the table in 3.3 | `js-types` `retry::each_outcome_is_classed_as_the_table_says` (one case per row) | B |
| An outcome outside `retry_on` ends the job | `js-types` `retry::an_outcome_outside_retry_on_ends_the_job` | B |
| An unknown agent code is the host's; a job-fault code never retries | `js-types` `retry::an_unknown_refusal_code_is_the_hosts`, `retry::a_job_fault_refusal_never_retries` | B |
| `NO_CAPACITY` and `EXCLUSIVE_NOT_READY` never retry, whatever is named | `js-types` `retry::an_exhausted_pool_bound_never_retries` | B |
| `max_attempts` 0 and 1 mean one; the last attempt ends the job | `js-types` `retry::zero_and_one_mean_one_attempt`, `retry::the_last_attempt_ends_the_job` | B |
| Empty `retry_on` takes the default; a list of only unknown values retries nothing | `js-types` `retry::an_empty_list_takes_the_default`, `retry::only_unknown_conditions_retry_nothing` | B |
| Backoff: doubling, effective defaults, the cap with jitter, the same wait for the same job and attempt | `js-types` `retry::backoff_doubles_to_its_cap_with_bounded_jitter` | B |
| `max_attempts` above the ceiling refused; nothing recorded | `js-lease-server` `submit_refuses_attempts_above_the_ceiling` | F |
| `max_backoff` below the effective initial wait, or a condition named twice, refused | `js-lease-server` `submit_refuses_a_bad_backoff_or_a_repeated_condition` | F |
| A resubmit with another `retry` or `callback` is `job-id-conflict` | `js-jobs` `a_resubmit_with_another_policy_or_callback_conflicts` | C |
| A job in backoff is not placed before `retry_not_before`, and is placed after it | `js-lease-server` `a_retried_job_waits_out_its_backoff` | F |
| The queue wait of an attempt counts from its backoff end | `js-lease-server` `queue_wait_of_an_attempt_counts_from_its_backoff_end` | F |
| A cancel in backoff ends the job at once and writes its callback row | `js-jobs` `cancel_in_backoff_ends_the_job_and_writes_its_callback` | C, D |
| A cancelled attempt never retries | `js-lease-server` `a_cancelled_attempt_never_retries` | F |
| A cancel and a retry on one version: one wins | `js-jobs` `a_cancel_and_a_retry_on_one_version_make_one_change` | C |
| Two retry decisions on one version make one attempt | `js-jobs` `two_retry_decisions_on_one_version_make_one_attempt` | C |
| A later attempt is never placed on a host that held an earlier one | `js-lease-server` `a_retry_never_returns_to_a_host_that_held_the_job` | F |
| The next attempt waits for the previous allocation, also when its host is silent | `js-lease-server` `the_next_attempt_waits_for_the_previous_allocation`, `a_silent_hosts_attempt_holds_its_retry_until_the_queue_wait` | F |
| A result resent after `retry_txn` is `Ack::Stale` | `js-lease-server` `a_result_resent_after_a_retry_is_stale` | F |
| `retry_txn` keeps files, sets no retention end and writes no callback row | `js-jobs` `a_retry_keeps_files_and_writes_no_callback` | C |
| Rule C1: a job retried to each final state (succeeded, failed with no retry left, cancelled) gets exactly one callback, sent after its last attempt, and none before it | `js-sim-cell` scenario `one_callback_after_retries_are_exhausted` | H |
| One `Attempt` and one `UsageRecord` per attempt; `queued_at_ms` stays the first submit | `js-lease-server` `each_attempt_has_its_history_entry_and_usage_record` | F |
| A crash between an attempt's end and its retry makes one retry | `js-sim-cell` scenario `restart_between_attempts` | H |
| A whole-host attempt lost while running is retried with `max_replace` counted again | `js-sim-cell` scenario `whole_host_retry` | H |
| Over every run: no job runs more than `max_attempts` times; no two attempts of a job hold an allocation at once | `js-sim-cell` `Invariant::AttemptsBounded`, `Invariant::OneAttemptAtATime` | H |
| A record from before the fields reads as attempt 1 | `js-jobs` codec `a_record_without_attempt_tags_reads_as_attempt_one` | C |
| An older reader carries the new tags and leaves callback rows | `js-jobs` `a_build_without_the_new_tags_carries_them`, `an_older_store_reader_leaves_callback_rows_intact` | C, D |
| New proto fields: goldens; an old reader skips them; old bytes give the defaults | `js-proto` `tests/retry.rs`, `tests/callback.rs`, rows in `golden/fields.txt` | A |
| Until the accept release, `retry` and `callback` are refused | `js-lease-server` `submit_refuses_retry_and_callback_until_enabled` (the accept PR replaces it with the accepting test) | A |
| A callback outside the client's prefixes, or from a client with none, is refused | `js-lease-server` `submit_refuses_a_callback_outside_its_prefixes` | G |
| Each bad URL form is refused: userinfo, fragment, plain http off loopback, `https://a.example.evil/` under `https://a.example/`, `..`, `%2e`, `%2f`, backslash | `js-callback` `url_rule_refuses_each_bad_form` | E |
| A prefix not ending in `/`, or a bad key file, stops the server at start, naming the key | `js-lease-server` config `callback_keys_are_checked_at_start` | G |
| An unreadable key file on a later tick keeps the current keys | `js-lease-server` `an_unreadable_key_file_keeps_the_current_keys` | G |
| The callback row is committed with the end, and is replaced if stale | `js-jobs` `an_end_with_a_callback_writes_its_row_in_the_same_commit` | D |
| 2xx delivers once | `js-callback` `a_2xx_delivers` | E |
| 500, a timeout, a refused connection and a redirect are retried with the doubling wait; no redirect is followed | `js-callback` `each_failure_is_retried_with_backoff` | E |
| 410 abandons; a removed prefix abandons | `js-callback` `a_410_abandons`; `js-lease-server` `a_removed_prefix_abandons_its_callbacks` | E, G |
| The deadline: expires when past; cut to the retention end | `js-jobs` `a_callback_past_its_deadline_expires`, `a_deadline_is_cut_to_the_retention_end` | D |
| A restart with callbacks pending delivers them; deadlines raised to at least boot plus 120 s | `js-jobs` `boot_raises_pending_callback_deadlines`; `js-sim-cell` `restart_with_pending_callbacks` | D, H |
| A lapsed claim is taken again with the same `webhook-id`; never two claims on one row | `js-jobs` `a_lapsed_callback_claim_is_taken_again`, `one_claim_per_callback_row` | D |
| The signature equals the Standard Webhooks reference vector; two keys sign twice | `js-callback` `signature_matches_the_reference_vector`, `two_keys_sign_twice` | E |
| The body is exactly the completion (exact equality: no tail, argument, environment or file) | `js-callback` `the_body_is_exactly_the_completion` | E |
| A reused job id gets a new `callback_id` | `js-callback` `a_reused_job_id_gets_a_new_callback_id` | E |
| The sweep deletes a callback with its job, and a row whose job is gone | `js-jobs` `the_sweep_deletes_a_callback_with_its_job`, `the_sweep_deletes_an_orphan_callback` | D |
| `callback_status` on `GetJob` follows the row | `js-lease-server` `get_job_shows_the_callback_status` | G |
| A policy the server dropped is reported by `js-client` | `js-client` `a_dropped_policy_is_reported` | I |
| The reference service and the real one agree, and each scenario fails against its `Defect` | `js-conformance` `retry_on_host_loss`, `no_retry_on_cancel`, `callback_once_after_retries` | H |

Why this list is complete: it covers every row of the classing table in 3.3. It covers every bound: `max_attempts`, the ceiling,
backoff and its cap, the queue wait per attempt, the wait for the previous allocation, the delivery deadline and the retention sweep. It
covers every actor that can race: two instances, a cancel, a restart, a late agent report and a lapsed claim. It covers every answer a
receiver can give: 2xx, 410, another status, a timeout, a refused connection and a redirect. It covers every compatibility path of
section 7: old reader, old record, old agent, new client against an old server, and the act-then-accept gate.

## 10. Implementation tasks

The interfaces land first (P7). After that, tasks in the same row of the plan can run at once, because each depends only on the agreed
interfaces of task A.

| Task | What | Depends on | Crates |
| --- | --- | --- | --- |
| **A. Interfaces** | The proto additions of sections 3.7, 5.2 and 5.3, with `golden/fields.txt` rows and goldens. `SubmitJob` refuses `retry` and `callback`. The Rust signatures below, with bodies that never retry and never deliver. | none | js-proto, js-types, js-jobs, js-callback (new), js-lease-server |
| **B. Retry decision** | The classing table, the job-fault refusal codes, the backoff and jitter. Pure (R4). | A | js-types |
| **C. Attempt store** | Tags 66 to 70, `retry_txn`, the reset of the pre-run counters, `UsageRecord.attempt`, the shared store tests. | A | js-jobs, js-usage |
| **D. Callback store** | The `callback/` row and its index, written in `end_txn`; claim, answer and expire; the boot raise; the sweep. | A | js-jobs |
| **E. Delivery** | The Standard Webhooks signer, the URL rule, the sender (`ureq`, no proxy, no redirects, a time limit), the answer classes. | A | js-callback |
| **F. Server retry (act)** | `end_of` and the loss paths ask `decide`; placement honours `retry_not_before`, the bars and the freed previous allocation; the queue wait from the backoff end; the API mapping; metrics. | B, C | js-lease-server, js-sched-placer |
| **G. Server callback (act)** | The `[[client]]` keys and checks, the delivery threads, `callback_status`, metrics, the prefix check behind the refusal. | D, E | js-lease-server |
| **H. Simulation and conformance** | The invariants and scenarios of section 9, and the reference service. | F, G | js-sim-cell, js-conformance |
| **I. Accept** | A release after F and G: remove the two refusals; `js-client` reports a dropped policy; update `jobscheduler-design.md` (2.2, 3.2, 5.3), `design.md` 8.1, `retries-and-runs.md`, `queue-status.md`, `configuration.md`, `http-gateway.md`. | F, G released | js-lease-server, js-client, docs |
| **J. Attempt in the log key** | The attempt in the live-log key and an `attempt` on `GetLogsRequest`; then same-host retries behind `start-fence` (section 3.5). | I | js-agent, js-agent-wire, js-lease-server |
| **kbf (komira-ai/kbf)** | Read the new fields in `kbf-job-client` and its goldens; use the callback to wake the executor; set no retry policy (section 3.8). | I released | kbf-job-client, kbf-server |

B, C, D and E run in parallel once A is merged. F and G run in parallel once their inputs are merged. Each task's PR names the tests of
section 9 that it adds. Task A adds `js-callback` (runtime layer) to `layers.toml`, `coverage-baseline` and BUCK.

The interfaces task A fixes:

```rust
// js-types (pure)
pub struct RetryPolicy { pub max_attempts: u32, pub retry_on: Vec<RetryCondition>,
                         pub initial_backoff: Option<Duration>, pub max_backoff: Option<Duration> }
pub enum AttemptEnd { /* one variant per row of the table in 3.3 */ }
pub enum Decision { Retry { wait: Duration }, End }
pub fn decide(policy: &RetryPolicy, end: &AttemptEnd, attempt: u32, job: &JobId) -> Decision;

// js-jobs
impl JobStore {
    pub fn retry_txn(&self, id: &JobId, end: AttemptRecord, not_before: StoreTime) -> Result<JobRecord, JobStoreError>;
    pub fn claim_callbacks(&self, by: &InstanceId, now: StoreTime, limit: usize) -> Result<Vec<CallbackClaim>, JobStoreError>;
    pub fn answer_callback(&self, claim: CallbackClaim, answer: CallbackAnswer, now: StoreTime) -> Result<(), JobStoreError>;
    pub fn callback_status(&self, id: &JobId) -> Result<Option<CallbackStatus>, JobStoreError>;
}
// end_txn takes the JobCompletion body (built by js-lease-server from the record) when the spec has a callback.

// js-callback (runtime)
pub fn check_url(url: &str, prefixes: &[Prefix]) -> Result<CallbackUrl, UrlError>;
pub fn sign(keys: &Keys, id: &str, timestamp: u64, body: &[u8]) -> String;           // the webhook-signature value
pub struct Sender { /* ureq agent: no proxy, no redirects */ }
impl Sender { pub fn new(limit: Duration) -> Self;
              pub fn send(&self, url: &CallbackUrl, headers: &Signed, body: &[u8]) -> CallbackAnswer; }
pub enum CallbackAnswer { Delivered, Abandoned, Failed(FailureCode) }
```

## Decisions this page makes

Each choice below is the one most consistent with the merged code. The owner may reverse any of them.

- **An attempt is a placement**, counted when it is placed. The requeues before a run stay inside it, so `max_attempts` counts work
  the client can see and the pool's own bounds keep their meaning.
- **A retry keeps the job id.** It is not a child job, so polling, cancel, the result, retention and the callback all stay keyed on the
  one id the client knows.
- **One allocation per job at a time.** The next attempt waits for the previous allocation to be freed. So the allocation key stays
  `alloc/<job id>`, and two attempts on heard hosts never overlap. The cost, a retry held back by a silent host, is bounded by the queue
  wait and stated.
- **A retry never returns to a host that held the job** until task J. One rule then covers both the shared log keys and agents without
  `start-fence`.
- **Retry and callback sit in the spec**, so they are compared for idempotency. A resubmit that changes them is a conflict, not a quiet
  change to how many times the work runs or where its result goes.
- **Unknown agent refusal codes are the host's**, as kbf reads them today, and `max_attempts` bounds the cost. **Unknown retry
  conditions match nothing**, so an older server never retries more than the client asked.
- **The jitter comes from the job id**, so the retry decision stays pure and replays in simulation.
- **One authentication scheme:** HMAC in the Standard Webhooks form, with per-client keys and URL prefixes in the server's configuration,
  and no secret on the job.
- **`max_attempts` is refused above its ceiling; a deadline is cut to the retention end.** The attempt count changes how many times the
  work runs. The deadline only bounds how long the server tries.
- **The callback body carries no output.** The receiver reads the outputs with its own token.
- **Rule C1 is settled by the API owner,** not a choice of this page: one callback per job, sent only after retries are exhausted.
- **Two releases per feature,** act then accept, so instances of two versions over one store never mishandle a job. The gate is an
  internal `Config` field, not a setting.

# Why a queued job waits: queue status, attempts and events

Companion to [docs/design.md](../design.md), sections 3.2 and 6.1. Status: built in the pull request that adds this page.

A `Job` read through `GetJob` or `ListJobs` says *that* a job is queued. Until now it did not say *why*. A job waiting for room,
one waiting for a whole host to empty, one whose machine was refused for quota and one that is failing over to the next offer
all read as the same job. This page specifies the four fields that tell them apart, who sets them and when, and how they are
stored.

## 1. What a reader sees

`Job` gains four fields. Each is additive (rule B8): a reader that predates them skips them, and a job recorded before them
reads with their defaults.

| Field | Number | Type | Meaning |
| --- | --- | --- | --- |
| `queue_status` | 33 | `QueueStatus` | Why the job is where it is now. Absent when nothing has been recorded. |
| `attempts` | 34 | `uint32` | How many placement attempts the job has had (section 3). |
| `events` | 35 | `repeated JobEvent` | The job's newest transitions, oldest first, at most 32 (section 4). |
| `dropped_events` | 36 | `uint32` | How many older events were dropped to keep the bound. |

`QueueStatus` is `{ code = 1: QueueStatusCode, detail = 2: string, since_ms = 3: uint64 }`. `since_ms` is when the code last
*changed*, in milliseconds since the Unix epoch. A change of `detail` alone leaves it as it was. So "how long has this job been
waiting for a whole host" is `now - since_ms` even while the host being emptied for it changes.

`JobEvent` is `{ at_ms = 1: uint64, kind = 2: JobEventKind, detail = 3: string, host_id = 4: optional string }`. `host_id` is
set on the events that name a host: a placement and a requeue.

`detail` is text for a person. A program acts on `code` and `kind` only.

### 1.1 `QueueStatusCode`, a closed enum

| Value | Number | Set when |
| --- | --- | --- |
| `UNSPECIFIED` | 0 | Nothing has been recorded yet, or a reader met a value it does not know. |
| `WAITING_FOR_CAPACITY` | 1 | No open host has room for the job now. The detail says whether any open host could run it at all, or that a machine was launched for it. |
| `WAITING_FOR_WHOLE_HOST` | 2 | The job books a whole host and none is free. The detail names the host being emptied for it, when one is. |
| `QUOTA_BACKOFF` | 3 | An acquire for the job was refused for quota: its instance type is passed over in that region until the time the detail gives. |
| `FAILING_OVER` | 4 | An acquire for the job was refused for capacity: pass 2 asks the next zone, offer or provider. |
| `PLACED` | 5 | The job was placed on a host. |

The server writes only these values. A later release may append values; it never renumbers or reuses one. A reader that meets a
value it does not know reads it as `UNSPECIFIED`, and so does the store (section 5).

### 1.2 What a job past its queue shows

A placed, starting or running job shows `PLACED`. So does a job that ended after it was placed. A job that ended without
being placed has its status left out: a code such as `WAITING_FOR_CAPACITY` would be false once the job is no longer queued.
Its `attempts` and `events` are shown in every state, so the history of a job that failed to start with `no-capacity` is still
there to read.

## 2. Who sets the fields, and from what

The fields are written by one writer: the placement pass of the engine (`SchedPlacer::place_jobs_with`), through the job store
(`js-jobs`). The one exception is the requeue after a failed whole-host prepare. The time pass of the tick
(`job_pass.rs`, through `job_whole::expire_prepares`) moves the job back to queued, and the transaction that does so writes the
status the engine decided for it (`FailedPrepare::queue`).

What to write is decided by a pure function, `js_sched::queue_status::note`. It takes the outcome a pass reached for a job and
the job's current code, and returns a `QueueNote`: the code, the detail, the event to append (if any), the host, and whether this
was an attempt. It returns nothing when the outcome changes nothing. Nothing reads a clock to decide; the store stamps
what it writes with its own clock, as it stamps every other job time.

The outcomes (`js_sched::queue_status::Outcome`):

| Outcome | From | Code | Event | Attempt |
| --- | --- | --- | --- | --- |
| `Waiting { capable }` | pass 1: the walk placed a share job nowhere | `WAITING_FOR_CAPACITY` | on a code change | no |
| `WholeHost { emptying }` | pass 1: a whole-host job was not committed | `WAITING_FOR_WHOLE_HOST` | on a code change | no |
| `Refused(Refused)`, capacity | pass 2: an acquire made for the job's request was refused for capacity | `FAILING_OVER` | `FAILING_OVER`, every time | yes |
| `Refused(Refused)`, quota | pass 2: as above, for quota | `QUOTA_BACKOFF` | `QUOTA_BACKOFF`, every time | yes |
| `Launched { machine }` | pass 2: a machine was launched for the job's request | `WAITING_FOR_CAPACITY` | on a code change | no |
| `Placed { host }` | the placement commit | `PLACED` | `PLACED`, with the host | yes |
| `Requeued { host }` | a failed whole-host prepare sent the job back | `WAITING_FOR_WHOLE_HOST` | `REQUEUED`, with the host | no |

Two rules join pass 1 and pass 2:

- **A pass-2 reason outlives "no room".** Pass 1 sees every queued job that it placed nowhere as waiting for room. When the job
  already shows `QUOTA_BACKOFF` or `FAILING_OVER`, that is the better reason, so `Waiting` leaves it as it is. It changes when the
  job is placed, when a machine is launched for it, or when another refusal comes.
- **Pass 2 hands its outcomes to pass 1 and writes nothing.** The scaler runs on its own task. After each step it maps each
  refusal (`Refused::request`, new) and each launch to the job of its request, and hands them to the placer
  (`SchedPlacer::note_outcomes`). The next placement pass takes them in queue order before its own outcome for the job. So no
  second writer ever races the placement commit for a job's row. A job whose request id also names a lease request has no pass-2
  outcome, because the scaler does not map such a request to a job.

A job whose constraints are no platform requirements, and a whole-host job that runs on the host itself, are not walked by
pass 1, so pass 1 records nothing for them. They keep `UNSPECIFIED` unless pass 2 says something.

## 3. Attempts

An attempt is a placement commit or an acquire refused for the job. A whole-host job requeued after a failed prepare and placed
again has two attempts. A launch that was accepted is not counted: the placement onto its machine is. `attempts` saturates at
`u32::MAX`.

## 4. Events: the bound and eviction

The record keeps at most `MAX_JOB_EVENTS = 32` events, oldest first. Appending the 33rd drops the oldest, and each event dropped
adds one to `dropped_events` (saturating). So `dropped_events + events.len()` is the number of events the job ever had, and a
reader can tell a short history from a cut one. Events are not kept anywhere else; a streaming or watch API is out of scope.

An event is appended when the code changes, and on each attempt or requeue even when the code does not change (a second capacity
refusal while `FAILING_OVER`). A change of detail alone appends nothing. So a job that waits for a long time writes its row once
when it starts waiting, not on every tick.

## 5. Storage

The fields live on the job record (`JobRecord::queue`, a `QueueTrail`) and in its row, under new tags after the existing ones:

| Tag | Field | Encoding | Written when |
| --- | --- | --- | --- |
| 48 | code | one byte | not `UNSPECIFIED` |
| 49 | detail | UTF-8 text | not empty |
| 50 | since | a time | set |
| 51 | attempts | `u32` | above zero |
| 52 | events | a list of events | any |
| 53 | dropped events | `u32` | above zero |

A field at its default is not written, so the row of a job with no queue status has the bytes it had before. A row without these
tags reads with the defaults. An unknown code or event kind byte reads as `UNSPECIFIED` rather than making the row unreadable:
the status is derived again on the next pass, and refusing the row would hide the job from an older server after a rollback. A
row holding more than 32 events (a later build with a larger bound) is read as written; the next append trims it.

Writes go through the job store only:

- `JobStore::note_queue(id, notes)` applies notes to a queued job in one transaction at the version read. It writes nothing when
  the job is not queued or the notes change nothing. A row changed in between makes it stale; the pass logs it and tries on
  the next pass.
- `JobStore::place_with` applies `Placement::queue` in the placement transaction, so a placed job never lacks its `PLACED`
  status and event.
- `JobStore::fail_prepare_with` applies `FailedPrepare::queue` in the requeue transaction.

## 6. What does not change

Placement, quota and failover decide exactly as before. These fields only report what they decided. Admission refusals at
submit are unchanged. No CLI flag, endpoint or configuration key is added. The CLI shows the new fields because it prints every
field the server sets (`js job get`), with no code of its own for them.

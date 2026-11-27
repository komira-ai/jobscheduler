# A host that goes silent: one rule

**Tag: Proposed.** This note needs the owner's yes. Once approved, komira-ai/jobscheduler#1277, komira-ai/jobscheduler#1278 and komira-ai/jobscheduler#1280 change as its last sections say. After that, this note, `jobscheduler-design.md` and `crates/js-lease-server/src/job_loss.rs` all state the same rule.

## Why one rule is needed

On 2026-11-27 two healthy build hosts lost their route to the server for over seven hours. Three open pull requests each fix part of what happened, and each relies on something another one removes:

- **#1278.** The agent keeps its running jobs through any silence. Past `--silence-stop` (300 s) it only stops taking new work.
- **#1280.** The server ends the jobs of a host silent for 30 minutes as `LOST`, and the build service then runs them again. Its "why no job runs twice" relies on the agent stopping every job after 300 s of silence. #1278 removes that.
- **#1277.** The agent exits only on refusals an operator must fix, and hands its running jobs to the next process. It bounds a refusal that never clears by "`--silence-stop`, as for any unreachable server". #1278 removes that too.

Merged as they are, a host cut off for an hour would go on running jobs that the server has ended and the build service has started elsewhere. That costs duplicate work, and a job with side effects (a deploy step) would run twice.

The rule below keeps what each pull request is for. It meets three principles set after that day's outages:

- **(a)** An answer the agent can parse proves the server alive, whatever its status.
- **(b)** Anything held while waiting has a limit that expires without the other side's help.
- **(c)** A layer that loses its neighbour keeps running work and stops taking new work.

It also carries out the owner's decision that **silence means lost**: the jobscheduler ends the jobs of a host that goes silent for good. That reverses question 25 of `jobscheduler-design.md` (option A, "silence ends nothing"). Section 3.2, section 6.3 ("Nothing times out for silence alone"), rows 41 and 43, and the "Silence ends nothing" paragraph of `job_loss.rs` say the same and change with #1280.

## The rule

1. **The server ends the jobs of a silent host at one time: the silence limit `L`.** `L` defaults to 30 minutes and is counted from the host's last applied beat (`heard_at`). At `L`, every placed, starting and running job of the host ends `LOST` with cause `HOST_SILENT`. No job of a silent host ends earlier, and that includes the start timeout: while the host is past its grace, the start timeout of its jobs waits.
2. **The agent stops every job it holds before `L`, so a job is never ended while its old copy can still run.** The agent counts from the same beat. At `A = L − S − 70 s` (`S` is the stop grace), it sends SIGTERM to every job at once. After one shared stop grace it kills all survivors in one sweep. The last container is gone 70 s before `L` by the agent's count, and at least 60 s before `L` by the server's.
3. **Until `A` the agent keeps running work. Past `--silence-stop` it takes no new work** (#1278 as written).
4. **A job the server has ended `LOST` is never run again by the server.** The build service starts it again under a new job id once it reads `LOST`. When the host comes back, every lost job it lists is told to stop, and its fence on disk is retired, so its start can never run again.

**Why the agent goes first.** In the other order the agent stops after `L`, so the build service starts a second copy while the first still runs. That order would be safe only if the re-run waited for the agent's bound, and the server cannot know that bound: it is a per-host flag that hosts run as high as two hours. With the agent first, the server's `L` is the only number, and the agent derives its own bound from it.

**Why (c) does not mean "keep running work for ever".** A running job is worth keeping only while its result can still be accepted. After `L` the server has ended the job and drops any result the host sends for it, so going on is waste and races the re-run. The agent therefore keeps running work through every silence the server will forgive, and stops it just before the server gives the job away. This is the limit (b) asks for.

### Two clocks in the agent

| Clock | Restarted by | Governs |
|---|---|---|
| **Alive** | Any answer whose body is a JSON object, whatever its status (#1278) | Only the intake bound `--silence-stop` |
| **Heard** | A 200 welcome or a 200 heartbeat | The job bound `A` |

The heard clock needs the stricter event because the server's `L` counts from `heard_at`, and only an applied hello or beat writes it. A refused beat (413, 422, 401, 503 `state-write-failed`) leaves `heard_at` where it was, so the server keeps counting. If the agent counted such answers, it would keep a job past the server's end.

Two details keep the clocks close:

- The server writes `heard_at` while it processes the beat, before the reply (held for at most 5 s), and the agent's request limit is 10 s. So the agent's count runs at most 10 s behind the server's. The margin covers this.
- A 200 for a beat whose sequence number is not newer (`Beat::Superseded`) writes nothing. The agent's beats carry increasing sequence numbers, so this happens only for a retried beat that another server already applied. Its `heard_at` is at most one request limit older, which the margin also covers.

The heard clock survives a restart of the agent process, and a pause cannot make it late. The agent writes to its state root, at each heard beat:

- the wall-clock time;
- the boot id and boot-time clock (Linux `CLOCK_BOOTTIME` and the boot id; macOS `mach_continuous_time` and `kern.bootsessionuuid`);
- the last `L` it was told.

The age of the silence is the larger of the wall-clock age and the boot-time age. A suspended VM, a sleeping Mac or a clock jump can only make the agent stop early, never late. A new process on the same boot reads this record and applies the bound before its first hello, so a run it adopts past `A` is stopped. After a reboot the containers are already gone.

### Where `L` comes from

- **The welcome carries `L`.** It goes in a new optional field, `silence_limit_ms`. The agent advertises a new capability, `silence-bound`, in its hello.
- **The server records what it promised.** It stores on the host's row the limit it sent and the epoch of that welcome. It ends a host's jobs for silence only at that stored limit, and only while that epoch is still the row's. So:
  - A host with an older agent, which never advertised the capability, keeps today's rule: nothing ends. That agent's own stop is a flag the server cannot read.
  - A later hello answered by an older server instance, whose welcome has no field, replaces the epoch. The gate is then off.
  - A changed `server.silence_limit` takes effect for a host at its next hello.
- **A welcome without the field** clears the agent's stored `L`. The agent then stops no running job for silence, and the server will end none.
- **A welcome whose limit leaves no positive `A`** (`S + 70 s ≥ L`) is refused, naming `--stop-grace`, the same way the agent refuses a grace that does not fit today. With `L` = 30 minutes this means a stop grace of 1730 s or more. Such hosts must lower the flag before the server sets a limit.
- **`--silence-stop` is never refused for this.** The agent uses `min(--silence-stop, A − 1 s)` as its intake bound and logs once if that clamps it. Hosts that run `--silence-stop` at 7200 s go on registering.

## Timeline: a host cut off at t = 0

`t = 0` is the host's last applied beat. The numbers are the defaults:

- grace 15 s;
- request limit 10 s;
- `--silence-stop` 300 s;
- `L` 30 min;
- the packaged crun unit's stop grace, 120 s, which gives `A = 1800 − 120 − 70 = 1610 s`.

Starts reach the agent on heartbeat replies and through the dispatch poll, and the cut-off cuts both.

### While the host is cut off

| Time | Agent: running jobs | Agent: new work | Server | Build service |
|---|---|---|---|---|
| 0 to 15 s | Run | Taken; none arrives | Nothing | Waits |
| 15 s (grace) | Run | Taken; none arrives | **The host is lost** in the design's sense: `GetJob` shows `host_silent_since_ms`, and placement skips the host. This ends nothing. Start timeouts of the host's jobs now wait for `L` | Waits. It can read that the job's host is silent |
| 300 s (`--silence-stop`) | Run. A create already under way finishes and runs | Refused `silence-stop`. Waiting starts end `failed_to_start` / `silence-stop` | Nothing | Waits |
| 26 min 50 s (`A`) | Fences retired on disk, then SIGTERM to every job at once. A create still under way is marked, and torn down before its process starts when it answers. Results (`killed` / `silence-stop`) are held in the state root | Refused | Nothing | Waits |
| 28 min 50 s (`A` + `S`) | Every survivor killed in one sweep. Nothing of the host's jobs runs. By the server's clock this is at most 29 min 00 s | Refused | Nothing | Waits |
| 30 min (`L`) | (none) | Refused | Each placed, starting and running job ends `LOST`, `lost_cause = HOST_SILENT`. The attempt is unchanged and the server does not retry it. Its allocation moves to releasing, not freed, so the host's room stays booked | Reads `LOST` (`js job wait` exits 13). Starts the job again under a new id on another host. Anything waiting on it (a CI step, a rollout place) is released |

### When the host returns

| Returns at | Agent | Server | Build service | Two copies? |
|---|---|---|---|---|
| **20 s** | A retry in its backoff is answered 200, and both clocks restart. Nothing was stopped or refused | Applies the beat. Starts and stops held for the host ride on this reply. A start timeout that came due during the silence applies now | Notices nothing but a short `host_silent_since_ms` | No |
| **10 minutes** | Running jobs still run. Results of jobs that ended during the silence go on this beat. Starts are taken again | Accepts the results. Waiting starts the agent ended arrive as `failed_to_start` / `silence-stop`: a host cause, retried by policy. A job whose start never reached the host and whose start timeout passed ends now (`start-timeout`), and its stop rides on this reply | Gets real results, up to ten minutes late | No |
| **2 hours** | Its jobs were stopped at 26 min 50 s. On the first 200 it lists their held results | The jobs have been `LOST` since 30 min. Each held result is acknowledged and dropped. The listing confirms the containers gone, so the allocations are freed and the host takes work again. A lost job still listed as running gets a stop on every reply until it is gone, and its start is never sent again | Has run the jobs again since 30 min | No: the old copies ended by 29 min, the new ones started at 30 min |
| **Never** | (gone) | The jobs have been `LOST` since 30 min. The room stays booked until the provider reads the host absent or an operator retires it. `RetireHost` accepts a host whose only allocations are releasing under ended jobs (#1280) | Has run the jobs again since 30 min | No, while the agent was running (see below) |

**A return between `A` and `L`** (say at 28 min) needs one more server rule. The agent has stopped its jobs and reports `killed` / `silence-stop`, but the server has not ended them yet. The server reads this as a host cause, not as the job's own exit. The attempt ends `HOST_LOST` and the retry policy decides. Without a policy, the job ends `LOST` with a new cause, `AGENT_SILENCE_STOP`, so readers can tell it from a host that never came back. The listing confirms the containers gone, so a retry is placed only after the old copy has ended. The same happens when the agent's count started earlier than the server's: the agent missed the reply to a beat the server applied, and stopped its jobs while the server still counted the host as heard. Today the server reads `killed` as exit −1 and retries nothing.

So a job with a retry policy is retried by the server if its host returns at 28 minutes, and not if it returns at 31 minutes. In both cases the old copy has ended. Decision 3 below is about this difference.

## The one case with two copies

The agent counts the wall limit, and the agent stops jobs at `A`. If the agent process cannot run at all for longer than `A`, nothing on the host stops its containers. The containers outlive the agent (crun `KillMode=process`), and there is then **no bound from the scheduler**: not `A` and not the wall limit. A process that starts at all, even one in a crash loop, stops what is past `A` as soon as it reads its state root. So this case needs an agent that cannot start for 27 minutes or more on a host whose containers keep running.

For a build job the cost is duplicate work: the re-run has a new job id and a new upload key. A job with side effects needs the next section.

## A job with side effects

A deploy step, a publish, or anything else that changes the world outside its own outputs needs:

1. **The scheduler's order: the old copy ends before the job is reported lost.** The rule above gives this whenever the agent can run.
2. **No automatic re-run on `LOST` / `HOST_SILENT`.** Its client does not start such a job again by itself after a silence loss. It runs an observe job first, which reads what the old copy may already have done, as the build service already does for its appliers.
3. **A fence at the target.** The step writes under a token that only grows: the job id with its attempt, or a generation the client takes from the target before it submits. The target refuses a write with an older token. This is the only protection in the case above.

## The numbers and why they fit

| Number | Value | Set by | Must be | Why |
|---|---|---|---|---|
| Heartbeat interval | 5 s | Server (welcome) | Below the grace | Existing |
| Grace `G` | 15 s per pool (placement reads the longest of any pool) | Server | Below `--silence-stop` | Existing. The host reads lost for `GetJob` and placement. Ends nothing |
| Request limit | 10 s | Agent | Below the margin | Existing. The agent hears an applied beat at most 10 s after the server writes `heard_at` |
| `--silence-stop` `I` | 300 s, range 1 to 7200 s | Agent flag | Above `G` + `S` (checked today). Used as `min(I, A − 1 s)` | Existing value with #1278's meaning: intake only. A server restart costs a well agent at most 160 s (`lifecycle/silence.rs`) |
| Stop grace `S` | 10 s; 120 s in the packaged crun unit; flag allows up to 3600 s | Agent flag | `S < L − 70 s` | Existing. One shared grace for all jobs at `A`, not one grace per job |
| Margin | 70 s | Constant | Above request limit + kill sweep | 10 s for the request limit and 60 s to kill and reap the survivors. The heard age is the larger of the wall-clock age and the boot-time age, so a pause or a clock jump only shortens it |
| Agent job bound `A` | `L − S − 70 s`: 1610 s with crun, 1720 s with the default `S` | Derived from the welcome | `0 < A < L` | Not a flag, so it cannot be set past `L` |
| Silence limit `L` | 30 min, config `server.silence_limit` | Server config file | At least 10 min (refused at load) | Six times `I` and twice the start timeout. Long enough that the outages of a normal week end nothing: a server restart, a deploy, a store failover, a lost switch port. Short enough that a client waiting on a dead host is released within half an hour, not never. The 10 minute minimum keeps `A` above the default `I` at the packaged stop grace (600 − 120 − 70 = 410 s) |
| Start timeout | 900 s | Pool | Any | Existing, for a heard host. For a silent host it waits, and the job ends at `L`. A pool whose start timeout is above `L` ends its silent host's jobs at `L` |

`L` is the only new setting. `A` is derived from it.

#1280's hold-off after a boot (the server's count does not start before its own first pass after a boot or a failed pass) only moves the server's end later, so it keeps the order of rule 2. Its cost is the other way: a server or store outage longer than `A` stops running jobs that the server would still have taken back. They come back as `killed` / `silence-stop` and are retried as a host cause.

## What the owner must decide

1. **Reverse question 25: silence ends a host's jobs, at `L` = 30 minutes.** Recommended: yes, as #1280 builds it, with the changes below.
2. **The agent stops running jobs just before `L`, instead of never.** The cost: a cut-off, or a control-plane outage, longer than 27 minutes loses up to 27 minutes of each running job. A host whose agent missed the reply to its last applied beat may lose them a little earlier. The gain: no job runs twice while the agent can run. The alternative, never stopping while the server ends jobs anyway, runs two copies of every job of every host cut off for more than 30 minutes. Recommended: yes.
3. **Who runs a lost job again.** Under #1280 it is the client. The owner's earlier answer was "the retry policy then runs a job again". The server's retry places the next attempt only after the old allocation is freed (`js-sched-placer/src/jobs_retry.rs`). #1280 keeps that allocation releasing until the host returns, so a server retry would wait for the host. A client re-run gets a new job id, and with it a new fence. Recommended: the client, for jobs lost at `L`. The server's retry policy applies when the host returns before `L` (the 28-minute case above). Opening the server's retry at `L` would need a second live allocation per job, which is a separate design.
4. **Accept the one case with two copies**: an agent that cannot start for 27 minutes or more while its containers live on. Side-effect jobs rely on the client rule and the target fence above. Recommended: accept. File, as a separate change to the public interface, a fencing token a job can read: its job id and attempt as environment variables.
5. **A beat the server refuses does not count as heard.** A host that gets 413 or 401 for half an hour needs an operator or a deploy, and the server cannot tell it from a dead host without trusting a body it refused. Recommended: it does not count.

## What changes in komira-ai/jobscheduler#1278 (the agent keeps its jobs)

**Kept.**
- The alive clock: any JSON answer counts as contact.
- The intake bound: past `--silence-stop`, new starts are refused and waiting starts end `failed_to_start` / `silence-stop`.
- A start that arrives through the dispatch poll during the silence is refused, not dropped.

**Changed.**
- **The heard clock and the bound `A`, as in "The rule" and "Two clocks".**
  - Past `A`, fences are retired on disk, every job gets SIGTERM at once, and survivors are killed in one sweep after one stop grace. Today `supervise` stops jobs one by one, each taking up to its own grace. Each job is reported `killed` / `silence-stop`.
  - A create under way at `A` is marked, and its container is torn down before its process starts. If the runtime cannot hold the process, it is killed as soon as the create answers.
  - The heard record is persisted and applied on adoption, before the first hello.
- **The wire addition belongs to this pull request.** It adds to `js-agent-wire` the optional welcome field `silence_limit_ms` and the capability constant `silence-bound`, with the `agent-wire.md` text and the goldens. The PR body's "no field added" and "what does not change: the server, the wire" go. Its tests cover each direction:
  - the previous agent reads a welcome that has the field;
  - this agent reads a welcome without it and stops nothing;
  - an old state root without the heard record stops nothing, and the previous agent ignores the record.
- **The welcome rules of "Where `L` comes from"**: the clamp of `--silence-stop`, and the refusal when `A` is not positive.
- **Text that must say "keeps running jobs until `A`, then stops them"**:
  - the "Why keeping a running job is safe" section, which holds only against a server without a limit;
  - the `--silence-stop` help, the `lib.rs` flag table and the module docs;
  - the `START_FENCE_CAPABILITY` doc;
  - `absence.md`, `agent-suite.md`, `mac-lease-teardown.md`, `multi-instance.md`;
  - the report's row 5 and the Status row;
  - the failure-mode paragraph that relies on the start timeout ending a starting job of a silent host (it now waits for `L`).
- **Conformance.** `SilenceKeepsRunningLeases` asserts that running leases run at `A − 1 ms` and are all gone by `A + S + 60 s`. The fake's defect keeps them past that.
- **New tests.**
  - Several running jobs, all ignoring SIGTERM, are all dead by `A + S + reap`.
  - A 503 or 409 does not restart the heard clock but keeps starts taken.
  - A process restarted during the silence stops an adopted run at the original `A`.
  - A boot-time clock that stops while the wall clock moves on stops jobs at the wall-clock age.
  - A create that answers after `A` never runs its process.
  - A welcome without a limit stops nothing at ten times any bound.
  - `--silence-stop` 7200 s is clamped, not refused.
  - A stop grace of `L − 70 s` is refused.

## What changes in komira-ai/jobscheduler#1280 (the server ends a silent host's jobs)

**Kept.**
- `server.silence_limit`, 30 minutes by default.
- `LOST` with `lost_cause = HOST_SILENT`, never retried by the server.
- The allocation releasing, not freed.
- Stops for the lost jobs a returning host lists.
- `RetireHost` taking a host that is owed only the stops of ended jobs.
- The hold-off after a boot or a failed pass, and the guarded commit against a beat that lands meanwhile.

**Changed.**
- **Send `L` and gate on it.** Fill `silence_limit_ms` in the welcome. Store the limit and the welcome's epoch on the host's row, and end jobs for silence only at that limit while that epoch stands. Name the cross-version tests:
  - the previous agent (no capability) against this server: nothing ends;
  - a later hello answered without the field turns the gate off;
  - a lowered limit applies from the next hello.
- **Defer the start timeout of a silent host's jobs.** `job_expiry` skips a placed or starting job whose host is past its grace. The job ends at `L`, or at once when the host returns if its timeout came due. This rewrites section 3.2 ("the start timeout still ends a job that never started"), row 43, the `job_expiry.rs` table and the sim-cell test `a_silent_host_never_requeues_a_placed_job_and_start_timeout_ends_it`. It adds tests for both sides.
- **Read `killed` / `silence-stop` on a running job as a host cause.** In `per_job.rs`, it no longer maps to an exit. It ends the attempt `HOST_LOST` through the retry path, or ends the job `LOST` with the new cause `AGENT_SILENCE_STOP` (additive in `js-proto` and the job row). The result's exit code and log tails are kept in the attempt history for diagnosis. Test: a report before `L` retries a job that has a policy, and ends one without a policy with that cause.
- **Refuse `server.silence_limit` below 10 minutes** at load, naming the key. Add this to the `configuration.md` row.
- **Restate every text that says the agent's `--silence-stop` stops the job, or that sizes `L` against 300 + 120 + 30 s, as the `A`-before-`L` rule.** This covers:
  - the PR body's "why no job runs twice" point 4, its bounds table and its failure modes;
  - the "very large value keeps the old behaviour" decision (it now also needs `S < L − 70 s`);
  - `configuration.md`, the `job_expiry.rs` and `job_loss.rs` docs;
  - `job-retry-and-completion-callback.md`, `agent-wire.md`, `multi-instance.md`;
  - section 6.3 of the design;
  - the public site's "A silent host" paragraph, which should say that the host's agent stops the job shortly before the limit.
- **The case with two copies** in its failure modes is "an agent that cannot start for longer than `A`", with no scheduler bound, not "an agent that is stopped".

## What changes in komira-ai/jobscheduler#1277 (the agent ends only on operator codes)

**Kept.** The list of operator codes. Every other 4xx retried with the session's backoff. Running jobs handed to the next process on a fatal refusal. The lifecycle step before a park.

**Changed.**
- **Three places say a refusal that never clears is bounded by `--silence-stop`:** two failure modes ("a retried answer that never clears", "a refusal that lasts") and the decision "413 is retried, not fatal". Under #1278 a 413 or 422 is JSON, so it counts as alive and `--silence-stop` does not bound it. The bound is `A` on the heard clock, which a refused beat does not restart. The runs go on until they end, reach their wall limit, or reach `A`. Each restarted process continues the persisted count; it does not start a new one.
- **No second check.** The lifecycle step that runs before a park is #1278's, and it applies `A` from the persisted clock. #1277 adds nothing for silence.
- **New tests.**
  - A 401 to every hello across several restarts, with a running job: the job is stopped at `A`, counted from the last 200 before the first refusal.
  - A `--drain-wait` shorter than the stop grace: the next process finishes the kill from the persisted clock.

## Merge order

| | Server without the limit (main, or before #1280) | Server with the limit (#1280 changed as above) |
|---|---|---|
| **Agent before #1278** | Today | No capability, so nothing ends for silence; the agent keeps its own 300 s stop |
| **Agent with #1278** | No limit in the welcome, so the agent stops no running job and the server ends none | The rule of this note |

No cell runs two copies, so #1278 and #1280 may merge in either order once each has the changes above. #1280 as it stands today, without the gate, is not safe beside #1278. #1277 merges after #1278, because its bound for a refusal that never clears is #1278's heard clock.

All three pull requests edit `jobscheduler-design.md`, and two of them `agent-wire.md`, `multi-instance.md` and the generated `docs/report/jobscheduler.html`. Each one that merges later resolves those files and regenerates the report. Question 25 and section 6.3 then point to this note.

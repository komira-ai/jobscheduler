# ucdk Jobs on top

Companion to [docs/design.md](../design.md), section 11. Section numbers refer to design.md.

## 11.1 Placement of the scheduler and the two roles
**One scheduler, one environment.** A jobscheduler instance is bound to exactly one (app, environment): that environment's account,
region, pools and run store; its configuration declares the environment's providers and pools (5.7), and a Job runs as a container on a
`js-agent` host of one of them. It is deployed into the environment as an ordinary ucdk concept and shown as machinery in the plan. It
holds standing cloud access: the role its aws provider assumes to launch and terminate instances. This replaces the earlier rule that
the scheduler holds no standing credential and acts only as the caller. A launch a caller makes still acts with the caller's identity.
Scheduled triggers (11.2), pool scaling (5.8), the reaper and the leftover sweep (12.3) run with no caller present, so they use a
delegated identity fixed when the definition or environment is created, scoped to launching that definition's runs, to launching and
terminating tagged instances, and to sweeping by label; that standing access is a risk (K7). ucdk 1.0 keeps its direct launcher (`ucdk
run` or the generated workflow, against the cloud). jobscheduler is an optional second realization of Job; it is offered as supported
only after a conformance kit passes on a real cloud. Its run store is not the environment ledger; ledger entries name the scheduler instance and the
Job definitions, and runs are not nodes.

- **Launcher.** `launch` submits a request and returns after the run record is durable (UL1); `status` reads the record (UL2); `logs`
  returns text from a cursor with a `complete` flag (UL3); `cancel` calls `stop` and reports what was done (UL4). The ucdk `run` verb
  and the generated run workflow authenticate and call this API.
- **Infrastructure provider.** The ucdk-side crate that implements ucdk's `Provider` trait for Job definitions and schedules, its emulator
  and its kit run (`ucdk/crates/ucdk-provider/src/kit.rs`) live in the ucdk repository and call jobscheduler's public client API.
  jobscheduler exposes only definitions, schedules, runs and a list-by-label call, and names no ucdk type.

Definitions are infrastructure, runs are runtime events. Launcher first (D5).

## 11.2 Mapping
| ucdk Job field | Request field |
|---|---|
| image, args, environment | payload; the `image` runs as a container on the agent; secret references resolved by the executor, never stored in the record |
| size (abstract CPU and memory) | resource vector, booked on a host of a pool whose shape holds it, where many Jobs share the host (D56); shown in the plan; the booking and the ceiling (11.3) |
| max_retries | one budget over every failed attempt (8.1) |
| timeout | per-attempt deadline enforced by the agent |
| trigger schedule | one owner per realization (below) |

`Qos::Batch` orders the queue only. With no preemption (N1) it does not make a job interruptible, and the ucdk `Job` message has no
`interruptible` field today, so no row maps one.

**Schedule ownership.** A Job has exactly one scheduler. Under the direct realization the cloud scheduler role node calls `launch`
(or the cloud's own job API) and `overlap: forbid` is a guard there. When Job is realized by jobscheduler, the lowering drops the cloud
scheduler role node and emits the trigger into the definition, where jobscheduler owns tick dedupe, overlap, missed ticks (skipped, no
catch-up) and zone (UJ3). The choice is in the lowering table, and UJ3 is written once as a kit case that both realizations pass.

## 11.3 Gaps
- **Fence.** kbf hermetic actions may run on through a lost connection; infrastructure Jobs are not idempotent and must self-fence, or
  carry an explicit idempotency declaration. The class declares the default; run-on only when the definition declares hermetic or
  idempotent (D6). The fence field is on `Start` (10.3).
- **Run store.** ucdk's ledger is infrastructure state with leases (`ucdk/crates/ucdk-ledger`), not a run store. The scheduler owns run
  history (8.3). Retention is an open portable minimum in ucdk.
- **Retries.** The scheduler can do more than UJ1 (backoff, ladder). These are capabilities a Job may use, not part of the portable
  contract.
- **Sizing.** A ucdk Job's declared size is shown in the plan, so for ucdk classes it is the booking and a hard ceiling (`Declared`
  mode, 7.2). Learned history only informs a warning or a suggested size and never raises it (D7).
- **Tenant.** For ucdk the tenant is the (app, environment) pair, taken from the authenticated caller (K7).

# Agent conformance suite

Moved from design.md section 13.2. Section numbers refer to design.md.

## What exists now

The suite is the `agent` module of `js-conformance`, beside the provider suite. It is wire-neutral: the scenarios speak only to the
`AgentHarness` trait (deliver a Start with its validity bound, a fenced Start naming a job and its allocation, a Start breaking one
rule of the start contract, a host Start and a Start under a whole-node lease; deliver a Cancel; answer a result with a verdict;
end a job on its own or with a failure; acknowledge a taint; make the runtime fail a removal or the post-job check find something;
give the operator's consent to host jobs; restart the agent; hold the host whole and ask whether it is ready; answer a heartbeat;
drop and restore the connection; advance an injected clock; read what the agent reported). One harness per transport implements the
trait; the scenarios do not change. Time is injected, so no scenario sleeps or asserts how long anything took. `FakeAgent` is the
correct in-process agent; each `Defect` is the same agent missing one behaviour, and the tests assert that each scenario fails on its
broken agent and that no other scenario does.

| Scenario | Checks | Fails on |
|---|---|---|
| `StartIsIdempotent` | a repeated Start launches the job once | `StartRunsTwice` |
| `CancelUntilAbsent` | a Cancel lost on a dropped connection changes nothing; resent while the lease is listed, the lease leaves the listing; a further Cancel is harmless; a Start for the cancelled id does not run (the id is retired) | `CancelKeepsListed`, `ForgetsRetiredIds` |
| `LateStartIsDropped` | a Start that waited longer than its validity is reported never started and launches nothing; one that waited exactly its validity runs; validity 0 is unbounded and runs | `RunsLateStart` |
| `SelfFencesAfterSilence` | one tick short of the agent's own fence window the lease runs, the window counts from the last contact, the full window stops and reports it fenced; an agent with no fence fails | `NeverFences` |
| `UnackedResultIsResent` | an unacknowledged result is repeated in every heartbeat, including the first on a new connection, and not while the link is down; an acknowledged one is not | `ForgetsUnackedResult` |
| `RetiredFenceIsRefused` | a fenced Start repeated while its job runs launches it once; one whose fence (the job with its allocation) the agent retired, the job having ended and its result been acknowledged, launches nothing; the same job under a new allocation runs | `RunsRetiredFence`, `FenceKeyedByJobAlone` |
| `AnsweredResultIsDropped` | a result answered with a verdict the agent knows (accepted, duplicate, stale, unknown lease, mismatch) is not sent again; one answered with a verdict it does not know is; an answer for a lease with no result, or one the agent does not hold, changes nothing | `KeepsRefusedResult`, `DropsUnrecognisedAnswer` |
| `UsageIsReportedOnEveryEnd` | a job's result carries its usage when it ended on its own, failed, or was killed by a stop | `DropsUsageOnFailure`, `DropsUsageOnKill` |
| `FlawedStartIsRefused` | a start with no wall time, a reserved label, a fence with no allocation, a mount outside the agent's roots, a GPU on an agent with none, or a VM on an agent with no VM backend is refused and launches nothing; a well-formed start after them runs | `RunsFlawedStart(flaw)`, one per flaw |
| `CleanupFreesTheSlotLast` | the container and the scratch are reported gone before the slot is freed; a container that cannot be removed frees no slot and taints the host | `FreesSlotBeforeCleanup`, `FreesSlotOnFailedRemoval`, `FailedRemovalTaintsNothing` |
| `HostJobNeedsConsent` | a start whose job may change the host is refused, launches nothing and taints nothing until the operator consents; after the consent it runs | `RunsHostWithoutConsent` |
| `HostJobTaintsWhateverItsEnd` | a host job leaves the host tainted when it passed, failed, or was cancelled (each on a fresh host) | `HostJobTaintsOnlyOnSuccess` |
| `TaintingCheckTaintsOnItsOwn` | a process of the job's user left after the job, and a check that could not run, each taint the host on its own, are a finding of the run, and keep its slot (each on a fresh host) | `LeftoverProcessTaintsNothing`, `UnrunCheckPasses` |
| `TaintIsKept` | a taint is in every heartbeat until acknowledged; the acknowledgement stops the report, not the taint; a restart reports it again and the answer to its first heartbeat does not clear it; a tainted host refuses a start | `ForgetsUnackedTaint`, `AckClearsTaint`, `ForgetsTaintAtRestart`, `StartsOnTaintedHost` |
| `RestartKeepsWhatItHolds` | the first heartbeat after a restart lists the run that kept running, which is not launched again, and repeats the unacknowledged result | `ForgetsRunsAtRestart`, `ForgetsResultsAtRestart` |
| `WholeNodeWaitsForQuiet` | a host held whole is not ready while anything is held on it, starts no run of its lease before it is ready and no run of another lease at all, and is ready, running its lease's runs, once nothing is held | `ReadyWhileBusy`, `RunsBesideWholeLease` |

How an adapter maps a transport (full text in the module documentation of `agent`):

- Start validity is relative (`valid_for`, 0 unbounded; dropped when `waited > valid_for`). The legacy agent measures `waited` from
  receipt on the agent's clock (`valid_for_ms != 0 && waited > valid_for_ms`); the worker wire counts `valid_for_ms` from the send
  time of the heartbeat the Start names.
- The fence window is the transport's own, or none; an agent with no self-fence fails `SelfFencesAfterSilence` with that reason. The
  js-agent's window is its silence bound (`--silence-stop`, 300 s by default).
- A fenced Start is a job start (`job_start`, fenced by the job id with the allocation id); `start` stays a run start, so it keeps
  its validity bound.
- `launches` and `never_started` are derived: the legacy agent emits a skipped-stale event to read; the worker wire has none, so an
  adapter derives them from the Results and refusals that reached it and from what its job runner was asked to launch.
- A flawed start (`start_flawed`) puts its flaw where the transport carries it, and `refused` reads either refusal: a command the
  wire's own check refuses (no wall time, a reserved label, a fence with no allocation) or a start the agent refuses when it admits
  it (a mount outside its roots, a GPU its shape lacks, a VM with no backend). A reserved label is a run start's, a fence a job
  start's, since each start kind carries only one of them.
- The runtime and the post-job check are the adapter's: `remove_fails` makes every removal of a run's container fail and
  `check_finds` makes the check find a leftover process of the job's user or fail to run. `cleanup` is the order the agent
  reported the container gone, the scratch gone and the slot free.
- A restart (`restart`) is the agent's process starting again over the same runtime and state directory, followed by its first
  heartbeat and a reply that says nothing of a taint. The js-agent adapter gives the operator's consent to host jobs
  (`consent_to_host_jobs`) as a restart with `allow_host_jobs`.
- A whole-node lease (`hold_whole`) is named by every reply after it, and `whole_ready` is the agent's answer to whether the host
  is quiet for it (the js-agent's `prepare`, which its session asks on each heartbeat).

The js-agent adapter (`crates/js-conformance/tests/js_agent_suite.rs`) drives the agent's real lifecycle over the runtime in
memory (`js_conformance::agent::memory`) and a clock the suite moves, stamping the agent's contact where its session would (a
delivered message is an accepted reply), and passes every scenario. It does not go through the session or the transport: those are
covered by the agent's own tests (`crates/js-agent/tests/agent/jobs/silence.rs`). The fake cancels without producing a result and
does not model grace periods or a cap on retired ids; it refuses a run start on a tainted host or on one held whole, and no
fenced, host or whole-node start for either reason.

## Outside the agent suite

Every agent behaviour of the earlier prose list that the agent's lifecycle can show is a scenario above. What is left is one of three
kinds, and each is tested where it lives:

*The scheduler's half, not the agent's.* The suite reads what an agent does; these are what the scheduler decides.

- Which verdict a stale or a duplicate result gets: `crates/js-lease-server/src/per_job.rs` and its tests. The agent's half, that
  every known verdict ends the resending, is `AnsweredResultIsDropped`.
- Reconciliation: an absent lease is freed and an unknown one kept, and a lease left out of a heartbeat is absent only once its
  `Start` is due (5.2, graded by session and process as in 4.4). Silence is not absence. Tested in
  `crates/js-sched/tests/sim_f2_failures.rs` and the core suite (`RecoveredLeaseWaitsForItsWorker`, `StoreOutageIsWaitedOut`).
  The agent's half, that a lease is listed while it is held and silence stops it, is `RestartKeepsWhatItHolds` and
  `SelfFencesAfterSilence`.
- An unknown phase value read as quarantined: the wire's decoding, `crates/js-agent-wire/tests/decode.rs`
  (`unknown_phase_reads_quarantined`).
- Retries are not passed on: the agent wire carries no retry. The start contract's retry rule is the scheduler's, and
  `crates/js-types/src/start.rs` says it is not modelled yet.
- Per session (5.1): the scheduler cancels none of a leasee's placements, and answers `Cancel` to a hello that announces a lease
  not committed to its host id. The session's own half (cancel, grace, ack, never-started) is the scenarios above.
- The pool's and the consumer's consents to host jobs (`crates/js-lease-server/src/job_start.rs`), the scheduler's own taint
  record of a host job that a Result, hello or heartbeat does not undo (`crates/js-lease-server/src/agents_taints.rs`), and the
  whole-node lease token minted only after `exclusive_ready`, with `exclusive-not-ready` after a timeout
  (`crates/js-lease-server/src/agent_api_whole_tests.rs`). The agent's half is `HostJobNeedsConsent`, the taint scenarios and
  `WholeNodeWaitsForQuiet`.

*Agent behaviour the lifecycle cannot show.* These live in the agent's session, worker or VM runtime, not its lifecycle, so the
agent's own tests cover them:

- the prepare timeout of a whole-node lease (`crates/js-agent/tests/agent/exclusive.rs`);
- a VM destroyed before its slot is freed, a failed destroy as a failed removal, and a restart destroying every clone
  (`crates/js-agent/tests/agent/vm/runtime.rs`, `crates/js-agent/tests/agent/vm/rescue.rs`); the lifecycle half of a failed
  removal is `CleanupFreesTheSlotLast`;
- a GPU VM job refused before anything is cloned, which needs a VM backend (`crates/js-agent/tests/agent/vm/lifecycle.rs`); a VM
  start on an agent with no backend is a flaw of `FlawedStartIsRefused`;
- a package install while jobs run noted and taken into the baseline, not tainted: the integrity checks' own tests, under
  `crates/js-agent/src/integrity` (see [taint-self-clearing.md](taint-self-clearing.md)).

*Not built in the agent yet.* Each becomes a scenario when it is built:

- the VM run rules of 5.14 ([vm-backend.md](vm-backend.md)): `vm-boot-timeout` for a boot that never says ready, a hostile or
  out-of-order guest frame ending the run `vm-guest-protocol` without a taint, a read-write mount copied back by the agent
  (komira-ai/jobscheduler#1155, komira-ai/jobscheduler#1187), an image whose digest differs from the pinned one is
  `image-unavailable` (komira-ai/jobscheduler#1189), and the VM store check finding an unaccounted directory
  (komira-ai/jobscheduler#1188);
- one VM per run (no VM id serves two runs) and VMs counted in `vms` and in the offer in the order of 5.15, as the agent's
  half of the accounting the scheduler does (`crates/js-sched/src/admission/horizon.rs`); these need the fake VM runtime of
  vm-backend.md's tests in the suite's adapter;
- at `preparing` of a whole-node lease, no VM running (one found is a finding and is destroyed) (komira-ai/jobscheduler#1188).

# Agent conformance suite

Moved from design.md section 13.2. Section numbers refer to design.md.

## What exists now

The suite is the `agent` module of `js-conformance`, beside the provider suite. It is wire-neutral: the scenarios speak only to the
`AgentHarness` trait (deliver a Start with its validity bound, deliver a Cancel, acknowledge a result, end a job, answer a heartbeat,
drop and restore the connection, advance an injected clock, read what the agent reported). One harness per transport implements the
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

How an adapter maps a transport (full text in the module documentation of `agent`):

- Start validity is relative (`valid_for`, 0 unbounded; dropped when `waited > valid_for`). The legacy agent measures `waited` from
  receipt on the agent's clock (`valid_for_ms != 0 && waited > valid_for_ms`); the worker wire counts `valid_for_ms` from the send
  time of the heartbeat the Start names.
- The fence window is the transport's own (the worker daemon's fence T), or none. The legacy agent has no self-fence and reads no
  clock, so `SelfFencesAfterSilence` is a check it cannot pass today; the scenario fails with that reason.
- `launches` and `never_started` are derived: the legacy agent emits a skipped-stale event to read; the worker wire has none, so an
  adapter derives them from the Results and refusals that reached it and from what its job runner was asked to launch.

Not yet built: no harness exists for a real transport (the legacy `js-agent` wire, or the worker wire the host agent will speak).
The fake cancels without producing a result and does not model grace periods or a cap on retired ids.

## Still prose

Everything in the list below that is not one of the five scenarios above.

*Agent suite:*

- start is idempotent on lease id; a `Cancel` is resent while the lease is listed and the lease leaves the heartbeat;
- a stale-lease result is refused; a duplicate is acknowledged and ignored;
- reconciliation frees an absent lease and keeps an unknown one; a lease omitted from a heartbeat is absent only once its `Start` is due
  (5.2, graded by session and process as in 4.4) and pending before then, and silence is not absence;
- a Start past its validity bound produces `NeverStarted`, no Result and no listing; the heartbeat lists unacknowledged results and the
  first heartbeat after re-adoption is complete;
- an unknown phase value is read as quarantined; usage is reported on failure and on kill;
- each start-contract rule (5.2) refuses before any call: non-positive wall time, unstated fence, reserved label key, retries passed on;
- after a job the container and scratch are gone before the slot is freed, and a failed removal frees nothing and taints; a `host` `Start`
  is refused without all three consents (5.12); a host job taints on completion whatever its outcome; each post-job check (stray
  process, file outside the roots, mounts, users, kernel modules, cleanup failure) taints on its own, and a check that cannot run is a
  finding; a taint is resent until acknowledged, survives an agent restart, and is not cleared by a later clean hello; a tainted host
  takes no `Start`; a fake agent that reports no taint after a host job still leaves the host tainted at the scheduler (recorded at the
  `host_jobs` grant or the host `Start`), which grants it no new lease, and a Result, hello or heartbeat does not untaint it;
- per session (5.1): the leasee's session obeys the cancel, grace, ack and never-started rules for its placements, and the scheduler
  cancels none of them; a hello that announces a lease not committed to its host id is answered `Cancel`; a mount outside the agent's
  roots is refused;
- VM runs (5.14, [vm-backend.md](vm-backend.md)): the VM is destroyed before the slot is freed, and a failed destroy frees nothing and
  taints; no VM id serves two runs; a `host` VM job, a GPU VM job and a `vm` job on an agent with no VM backend (`vm-unavailable`) are
  refused before anything is cloned; a boot that never says ready ends `vm-boot-timeout`; a hostile or out-of-order guest frame ends the
  run `vm-guest-protocol` and never taints; a read-write mount is copied back by the agent and the guest never holds a writable host path;
  an agent restart destroys every clone and reports none held; an image whose digest differs from the pinned one is `image-unavailable`;
  the VM store check finds an unaccounted directory;
- one VM per run: a lease with two runs sees two VM ids, the first destroyed before the second is cloned; VMs count in `vms` and in the
  offer in the order of 5.15;
- whole-node lease (5.17; the reservation, drain and cap tests are in [whole-node.md](whole-node.md), section 11): at `preparing` no VM is running (one found is a finding and is destroyed), the lease token is not minted before
  `exclusive_ready`, a timeout gives the lease up as `exclusive-not-ready`, and release restores;
- GPUs (D32): a request with `gpus > 0` is never started on an agent whose shape has none, and a VM job with a GPU index is refused.


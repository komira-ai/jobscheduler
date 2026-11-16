# A taint that clears itself after a clean re-check

Status: built. komira-ai/jobscheduler#875 settled the judgement and named what it blames; the pull request that builds the
self-clearing below also carries the server's side (the agent's heartbeat reports the clear, section "The server").

## Problem

A tainted js-agent refuses every job until an operator moves `taint.json` aside (`taint.json.cleared-<time>`). On 2026-11-15
that took every build node out of service at once ("no capable host", 622 kbf operations failed), for taints whose cause was
gone seconds after it was seen. The owner's direction for hostkeeper is that halts clear themselves; a node that needs a login
to come back after a residue that no longer exists is the same defect.

## What was measured (read-only, node-02 journal of js-agent-crun, 2026-11-15 12:42:28)

Jobs `...-61693-0`, `-61694-0` and `-61695-0` started in the same second and ended in the same second. The check after `-61693-0`
named pid 465618 (the taint); the check after `-61694-0` named 465618, 465619 and 465620; the check after `-61695-0` found
nothing. Three consecutive pids, present for two checks a few milliseconds apart and gone for the third, cannot be a real
escape; they are short-lived processes of the host or of a neighbouring job being started or torn down (a container monitor
forked by the runtime has parent init until it is moved into its scope). The finding recorded only the pid, so which process it
was is not known and cannot now be known; the pull request adds command name, state and control group to every process finding so
that the next one is diagnosable. Not measured: what pids 465618-465620 were.

## Proposal

A taint with reason `residue` or `unchecked` (a judgement of the host, not a failed removal or a record that checks were owed)
clears itself when all of these hold:

1. no run is held and no container is on the host (the agent's own idle test, the same that lets the checks take a new baseline);
2. a full check (every check of the table, with the settle wait) passes against the baseline taken before the tainting job;
3. that holds again on a second check at least `--taint-recheck-ms` later (a config key with a default, per the tunables rule).

When it clears: the agent logs a finding naming the taint and the two passing checks, renames `taint.json` to
`taint.json.cleared-<time>` (the file is kept, as an operator's clear keeps it), and accepts jobs again. A taint whose reason is
`cleanup-failed`, `check-owed` or a mount, user, group, module or watched-directory change is not cleared by the agent: those are
state the host really holds and a re-check that sees no difference does not prove the job's change was undone.

## As built

- **Which taints.** `clearable` (js-agent, `lifecycle/taint.rs`) is an allow-list: reason `residue` with check
  `process-as-job-user` or `daemonised-child`, and reason `unchecked` with an input the checks could not read
  (`processes-unreadable`, `mounts-unreadable`, `users-unreadable`, `groups-unreadable`, `modules-unreadable`,
  `watch-unreadable`, `watch-over-bound`). Everything else stays: `cleanup-failed`, `check-owed`, `host-job`,
  `marker-unreadable`, a residue of a mount, user, group, module or watched directory, and `unchecked` with
  `host-not-visible` (that taint says the agent cannot see the host its jobs run on; a re-check of what the agent can
  see does not show that it now can). A taint that cannot clear, sealed beside one that can (the marker keeps the first),
  keeps the host tainted for the life of the process; after a restart the same sources seal again (a host job taken over,
  owed checks, a container that cannot be removed), so the pin is rebuilt.
- **Idle.** Every run is over; a run whose checks found the taint (`Blocked`) counts as over, since its container and
  scratch directory are gone and it would otherwise hold the host busy for good.
- **The wait.** `--taint-recheck-ms`, default 60000 (a minute outlasts a service manager's fork of a unit and the
  few-millisecond churn of the measurement above by three orders of magnitude, and brings a node back in a few minutes;
  1 to 3600000, needs `--container-bin`). The first re-check is one wait after the agent first sees the taint
  clearable and idle, the second one wait after the first; a re-check that finds anything, or a run held, starts the
  count again. The check is the post-job check with nothing live, so a pass takes the baseline again.
- **The clear waits for the server.** After the second clean re-check the agent stops reporting the taint and reports
  `taint_cleared` (the same `Taint`) in every heartbeat, and stays tainted (every start is refused `host-tainted`) until a
  reply says `taint_cleared_ack` to a heartbeat that carried it (a flag of its own: the late `taint_ack` of a report that was in flight
  when the agent began to clear must not end the taint before the server has heard of the clear). Only then does it remove the `owed-*` records of its blocked
  runs, rename `taint.json` to `taint.json.cleared-<unix seconds>`, free the blocked runs and their slots, and log the
  finding `taint-cleared`. Doing it in this order means a restart, or a lost acknowledgement, before the server agrees
  finds the marker and starts over: the agent and the server never disagree for good. A step of the clear that fails
  (`taint-clear-failed`) leaves everything as it was and is tried again at the next acknowledgement.

## The server

The server held a host's first reported taint for good. It now clears it on one report only: a heartbeat whose
`taint_cleared` equals the taint the registry holds for the host, field for field. The binding without the taint is
committed (the store, or the `.agents` file) before the registry forgets it and before the reply says `taint_cleared_ack`, as
for a taint report. A cleared report of any other taint (a host job's taint the server set, say) changes nothing and is
acknowledged all the same (`taint_cleared_ack`), so the agent stops sending it. A start refused `host-tainted` in the beat that carries the
clear taints nothing (those results were made before the clear); a taint the agent makes again is reported in `taint`
and stands. A server that does not know the field does not acknowledge, so the agent stays tainted: roll the servers
out before the agents.

## Left out on purpose

Clearing `process-as-job-user` or `daemonised-child` residue that is still there at the re-check: it stays tainted, because a
process that outlives the container and the wait is the escape the check exists for.

## Compatibility

Additive: one new optional flag with a default; one new optional heartbeat field (`taint_cleared`, absent when none, ignored
by a server that does not know it); the taint file keeps its format; the cleared copy uses the name operators already
use. An older agent reading the directory sees no `taint.json` and starts clean, as after an operator's clear.

## Tests

js-agent `tests/agent/lifecycle/taint_clear.rs` (a residue taint on an idle host clears after two clean re-checks and the
acknowledgement, and the file is kept as `taint.json.cleared-<time>`; a re-check that still finds the process keeps the
taint and the count starts again; `cleanup-failed`, `check-owed`, `host-job` and the mount, user, group, module and
watched-directory residues never clear; a run held keeps the re-checks off), `tests/agent/integrity/lifecycle.rs` (the
real checks over the in-memory host), js-registry `tests/taint_cleared.rs` and js-lease-server `tests/server/host_taints.rs`
(the server's side), js-agent-wire `tests/golden.rs`, `previous_release.rs` and `hostile_heartbeat.rs` (the field).

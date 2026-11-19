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
  `watch-unreadable`, `watch-over-bound`) or `cgroup-not-found`. Everything else stays: `cleanup-failed`,
  `check-owed`, `host-job`, `marker-unreadable`, a residue of a mount, user, group, module or watched directory, and
  `unchecked` with `host-not-visible` (that taint says the agent cannot see the host its jobs run on; a re-check of
  what the agent can see does not show that it now can). `cgroup-not-found` is the crun runtime's own blindness and is
  different: a run the runtime reports running whose container group (`<lease cgroup>/ctr`) does not exist a settle
  wait (`--settle-ms`) after the agent first saw it, while the container's first process (the pid `crun state` names,
  still in the agent's `/proc`) runs in another group; the finding names that group. A group that is missing because
  the container is not there taints nothing: the run is running from the moment its supervisor says it is ready,
  before crun has made the container (a slow creation on a loaded node, or a crun that fails before making the group,
  where `crun state` names no process), and it is still read as running after the container ended and crun removed
  its group, until the supervisor's exit record is read (the named process is gone). The agent asks again a settle
  wait after each such answer (`Runtime::whereabouts`). The post-job checks do not depend on that group (they judge
  processes by user, parent and cgroup against the baseline taken before the job, and mounts, users, groups, modules
  and watched directories), so what such a job leaves is found by them and by a re-check, and the taint clears after
  two clean re-checks of an idle host. It is a new value of the open `check` string; `host-not-visible` keeps its
  meaning. The same code is also a finding that names the run and the settle wait. A taint that cannot clear, sealed
  beside one that can (the marker keeps the first), keeps the host tainted for the life of the process, and for every
  process after: the agent then writes a second file in the state root, `taint.pinned`, holding the marker's taint
  (same serialisation as the marker, written atomically and synced), so a restart finds that a restorable taint was
  pinned (see "After a restart"). A failure to write it is the finding `taint-write-failed`, the pin stands in memory
  and the write is tried again at the next step.
- **After a restart.** A taint read from the marker at start clears only when it is a residue of a process
  (`process-as-job-user` or `daemonised-child`). The re-check compares with the baseline taken at start, which already holds
  a process the tainting job left (the pre-job baseline is not persisted), so for such a taint the re-check also applies two
  rules that need no baseline, and a process the baseline holds is not exempt from them. A process (seen in both looks of
  the settle wait, like the normal leftover rule) is a finding when (a) its uid is in the job users' range (the range
  `process-as-job-user` uses) and it is not of a live slot (the host is idle at a re-check, so none is live), or (b) its
  parent is `init` and its control group is one a job can be in (see "A process outside the groups a job can be in"). The
  agent's own process is never a finding. The rest of the re-check is the normal full check against the baseline. Every
  other restored taint stays pinned until an operator moves the marker aside: `unchecked` of any kind, `cleanup-failed`,
  `check-owed`, `host-job`, `marker-unreadable`, and a mount, user, group, module or watched-directory residue. A restored
  taint beside which a non-clearable one is sealed at start (owed records make `check-owed`, a host job taken over) is
  pinned too: the seal pins it. So is one the pin file names: when the pin happened in an earlier process, `taint.pinned`
  holds the marker's taint, and a restored residue equal to it, field for field, stays pinned; a pin file that cannot be
  read pins; one that names another taint is a stale file of an operator-cleared taint and is ignored. The decision is
  `StateRoot::restored_rechecks`, which `TaintState::load` and the update hook share. An
  operator's clear (`js-agent clear-taint`) keeps `taint.json`, `taint.pinned` and the `owed-*`
  records aside. The code is `TaintState::load`, `CheckHook::run_restored` and `Integrity::compare`. Not
  measured: how often an agent restarts with a taint.
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
for a taint report. A clear that reaches a server whose registry holds no taint for the host (the two servers of a
start-first update: the agent reported its taint to one and its clear reaches the other) ends the taint the store holds
when it is the cleared one, field for field, by the same compare-and-set, before the reply; the server's memory does not
change. Without that the clear was acknowledged with the taint left in the store, the agent never sent it again, and every
later start put the taint back on a clean host, which then took no job. A cleared report of any other taint (a host job's
taint the server set, say) changes nothing and is acknowledged all the same (`taint_cleared_ack`), so the agent stops sending
it. A start refused `host-tainted` in the beat that carries the
clear taints nothing (those results were made before the clear); a taint the agent makes again is reported in `taint`
and stands. A server that does not know the field does not acknowledge, so the agent stays tainted: roll the servers
out before the agents.

A taint report or clear is a compare-and-set on the stored binding, not a blind put: the
server reads the binding, refuses (503 `state-write-failed`, nothing recorded, the agent reports
again) when the taint it holds for the host differs from the taint the store holds, and writes
with the version it read, so a write of another server in between is refused too. Two servers
over one store (the two of a start-first update) therefore cannot erase each other's newer
taint with a clear, or revive a cleared taint with a report. Left stuck on purpose, and
visible as `state-write-failed`: a server whose memory differs from the store keeps refusing
until it restarts and reads the store.

### The operator's clear

A taint the agent will not report cleared stays stored for good: an agent whose marker an
operator moved aside by hand, a reimaged host, an agent too old to report a clear. The job
service's `ClearHostTaint` (operator only, `POST /v1/hosts/{host_id}:cleartaint`,
`js host host-taint --host-id ID`) clears the stored taint. It is the heartbeat's clear made without
the agent's report, through the same code: under the same lock the binding without the
taint is committed by the same compare-and-set (a store that holds another taint refuses it,
`UNAVAILABLE` `taint-unsaved`, and nothing changes), then the registry forgets the taint, then
in file mode the file is saved. The answer says whether a taint was cleared (`cleared`) and
its reason (`cleared_reason`); a host with no taint answers `cleared` false and nothing is
written; an unknown host is `NOT_FOUND`. The call does not reach the agent: an agent whose
marker still holds the taint refuses the next start `host-tainted`, and the server takes that
refusal as the host's taint again, so the operator moves the agent's `taint.json` and its
`taint.pinned` file and `owed-*` records aside first (`js-agent clear-taint`, see
docs/runbooks/clear-a-taint.md), or lets the agent clear it itself. A host job's taint row still
in the job store is put back on the binding at the next start, so that clear follows the start.

## The update hook

hostkeeper keeps a halt that a hook raised standing across new desired versions until an operator
clears it, and the halt is group-wide. The js-agent update hook (`prepare`, `cutover`, `verify` and
`rollback`) therefore does not halt for a taint the agent may clear itself:
`StateRoot::restored_rechecks` says which: the taints the restarted agent will re-check, by the rule
`TaintState::load` applies, namely a residue of a process (`process-as-job-user` or `daemonised-child`)
that `taint.pinned` does not name. The step goes on as for a clean host; the taint stays in the marker,
the restarted agent restores it, and its own re-checks clear it. Every unchecked taint (a check that could
not read its input, `watch-over-bound`, `cgroup-not-found`) still halts the hook, because a restart restores
it pinned and it never clears; so does a residue the pin file names. A halt there would keep the release that carries
the clearing from reaching the host, so one tainted host would stop every release of the group. A
taint the restarted agent does not re-check (`cleanup-failed`, `check-owed`, `host-job`, a residue of a mount,
user, group, module or watched directory, a check that found a change of host state, a taint with no
check named) still halts the hook with reason `tainted` (76), and so does an `owed-*` record. The
success line names no taint: the report has a detail on success only for a changed unit, and a
second detail would need a format change. A rollback onto a release that predates self-clearing
leaves such a taint standing, and that agent refuses starts until an operator clears it; that is
accepted, because a halt would stop every release of the group.

## A process outside the groups a job can be in

`daemonised-child` once counted every process with parent `init` that the baseline did not have and that lived past the
settle wait, except a service of the system slice, the user slice, a Podman monitor's scope and the runner's helpers. Any
other transient process of the host with parent `init` (a `.scope` under the system slice, the root group, `init.scope`,
another slice) that outlived the wait tainted the host. A contained job cannot place a process outside its own control
group: the control group file system is read-only in its container, and the job runs in a user namespace without
capabilities. So a process with parent `init` outside the agent's own group and every group that names a container is not
a job's, and `daemonised-child` no longer judges it. The groups a job can be in are the agent's own group and below it, a
group that names a container (an engine's group, or the actions tree), and, as before, a process with no unified path (judged
conservatively). `process-as-job-user` is unchanged: a uid of the job users' range is a job's wherever the process is. The
same predicate is rule (b) of the re-check of a restored taint. That rule has no exemption for a helper of the agent that is
itself a child of `init` in the agent's group: such a process, other than the agent's own, keeps a restored taint until it
ends. Code: `job_can_be_in` in `integrity/mod.rs`.

## Left out on purpose

Clearing `process-as-job-user` or `daemonised-child` residue that is still there at the re-check: it stays tainted, because a
process that outlives the container and the wait is the escape the check exists for.

## Compatibility

Additive: one new optional flag with a default; one new optional heartbeat field (`taint_cleared`, absent when none, ignored
by a server that does not know it); one new job-service call, `ClearHostTaint`, with its own request and answer messages; the taint file keeps its format; the cleared copy uses the name operators already
use; the pin file `taint.pinned` is a new file in the state root that an agent without it never reads. An older agent reading the directory sees no `taint.json` and starts clean, as after an operator's clear.

## Tests

js-agent `tests/agent/lifecycle/taint_clear.rs` (a residue taint on an idle host clears after two clean re-checks and the
acknowledgement, and the file is kept as `taint.json.cleared-<time>`; a restored residue taint clears the same way and a restored
non-residue taint stays pinned; a re-check that still finds the process keeps the
taint and the count starts again; `cleanup-failed`, `check-owed`, `host-job` and the mount, user, group, module and
watched-directory residues never clear; a run held keeps the re-checks off), `tests/agent/integrity/lifecycle.rs` (the
real checks over the in-memory host, and a restored residue taint re-checked without the baseline), js-registry `tests/taint_cleared.rs` and `tests/taint_host.rs`,
js-lease-server `tests/server/host_taints.rs`, `tests/server/live_update.rs` (a clear that reaches the other server of an update ends the stored taint), `src/agent_api_store_clear_tests.rs` and `src/job_api/fleet_tests.rs` (the server's side, and the
operator's clear), js-agent `tests/agent/update_hook/steps_taint_clearable.rs` (the hook does not halt for a
residue the restarted agent re-checks and does for any other taint, a pinned residue included),
`tests/agent/lifecycle/taint_pin.rs` (a pin made in memory survives a restart), js-agent-wire `tests/golden.rs`, `previous_release.rs` and
`hostile_heartbeat.rs` (the field).

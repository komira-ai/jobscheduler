# A taint that clears itself after a clean re-check

Status: proposal. Not built by the pull request that links it (komira-ai/jobscheduler#875); that pull request settles the
judgement, names what it blames, and leaves the taint as it is. The self-clearing below is the next pull request.

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

## Left out on purpose

Clearing `process-as-job-user` or `daemonised-child` residue that is still there at the re-check: it stays tainted, because a
process that outlives the container and the wait is the escape the check exists for.

## Compatibility

Additive: one new optional flag with a default; the taint file keeps its format; the cleared copy uses the name operators already
use. An older agent reading the directory sees no `taint.json` and starts clean, as after an operator's clear.

## Tests (to land with the implementation)

A residue taint with a later clean idle check clears and keeps the file; the same with a process still present does not; a
cleanup-failed taint does not; a clear needs two passing checks; the cleared taint is logged.

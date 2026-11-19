# Clear a js-agent taint

A tainted agent refuses every job (`host-tainted`) until its taint ends. A taint the agent
judged itself (a leftover process, a check that could not read its input) clears by itself once
two re-checks pass (docs/design/taint-self-clearing.md). Every other taint (`cleanup-failed`,
`check-owed`, a change of mounts, users, groups, modules or a watched directory, a pinned residue,
an unreadable marker) stays until the host is reimaged or an operator clears it. This is how an
operator clears it, once the cause is understood and the host is known to be clean.

A host job's taint (`host-job`) is not cleared this way, and the verb refuses it (`host_job`):
the machine is reimaged (docs/runbooks/host-jobs.md).

## 1. Find out why

`js host list` (or the hostkeeper host page) shows the taint the server stores for each host: its
reason, check and run. On the host, `journalctl -u js-agent-crun` has the finding that tainted it.
Decide that what the check found is gone, or was never the job's.

## 2. Clear the agent's taint

On the host, as root:

```text
js-agent clear-taint --by <you> --reason '<what was found and why the host is clean>'
```

The defaults are the packaged unit's: `--state-root /var/lib/js-agent/state`,
`--scratch-root /var/lib/js-agent/scratch/runs`, `--unit js-agent-crun.service`. Give the unit's
own values when they differ.

The verb:

- does nothing, and says so, when the state root holds no `taint.json`, `taint.pinned` or
  `owed-*` record;
- refuses with exit 75 (`run_held`) while the agent runs a job (a lease in its crun ledger):
  run it again once the job has ended. It changes nothing then;
- stops the unit and waits for it (a tainted agent with no job running exits at once);
- keeps each `owed-<run>` as `owed.<run>.cleared-<time>-by-<you>`, `taint.pinned` as
  `taint.pinned.cleared-<time>-by-<you>` and `taint.json` as `taint.json.cleared-<time>-by-<you>`,
  in that order (nothing is removed; a clear cut short leaves the marker, so the host stays
  tainted until it is run again);
- records the clear in `taint.cleared-<time>-by-<you>.json` in the state root and in one line of
  the journal: when, who, why, the taint and each file kept;
- starts the unit again.

It prints one JSON line last, as an update hook does, and exits 0 when it cleared (or found
nothing to clear), 75 when it changed nothing and should be tried later, and 1 when it failed;
the line's `reason` and `detail` say why, and so does standard error:

| reason | what happened | what to do |
|---|---|---|
| `bad_args` | a flag is wrong | fix the flag |
| `state_invalid` | the state root, its lock or the ledger cannot be used | read the detail |
| `locked` | another clear is running | wait for it |
| `host_job` | the taint is a host job's; nothing was done | reimage the machine |
| `run_held` | a job runs | run it again when the job has ended |
| `stop_failed` | the unit did not stop within 300 s; nothing moved, the unit was started again | look at the journal, then run it again |
| `clear_failed` | a file could not be kept aside; the detail says which were, the unit was started again | fix the cause, run it again |
| `record_failed` | the files were kept aside but the record was not written | keep the journal line |
| `start_failed` | the clear stands but the unit did not start | `systemctl status js-agent-crun` |

## 3. Clear the server's taint

The lease server keeps its own record of the taint. Once the agent has restarted, clear it with
the operator's credential:

```text
js host host-taint --host-id ID
```

Do it after step 2: while the agent's marker is there, the agent refuses the next start, and the
server takes that refusal as the host's taint again.

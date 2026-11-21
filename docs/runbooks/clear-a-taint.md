# Clear a js-agent taint

A tainted agent refuses every job (`host-tainted`) until its taint ends. The post-job checks taint only
for what a contained job could have left behind: a process running as a job's user after the job ended
(`process-as-job-user`), or a process table the agent could not read (`processes-unreadable`). Those
clear by themselves once two re-checks pass (docs/design/taint-self-clearing.md): when the process is
gone, or the table can be read again, nothing is needed. What the host does itself never taints: a
package install while jobs run (new users and groups, mounts, services, kernel modules, a changed
`/etc`) is one journal line naming what changed (`js-agent: job: Finding(...)` with the code
`user-changed`, `group-changed`, `new-mount`, `kernel-module-changed`, `watch-dir-changed` or
`daemonised-child`), and the agent goes on. A marker written by an agent that predates this change, with
one of those codes, is re-checked at the first start of the new one and clears the same way, so it needs
no operator either. The taints that stay until the host is reimaged or an operator clears them are
`cleanup-failed`, `check-owed`, a pinned residue, an unchecked taint the restarted agent holds, and an
unreadable marker. This is how an operator clears one, once the cause is understood and the host is
known to be clean.

On a Mac, the native agent's post-job check is the helper's leak scan (docs/design/mac-lease-teardown.md,
"After the run: the leak scan"). A run that left a login item or a Background Task Management entry
taints the host as `scan-residue`, and a scan that could not read an item or could not be asked taints
it as unchecked (`scan-unreadable`, `scan-failed`). The unchecked ones clear themselves once two
re-checks of the idle host pass; `scan-residue` does not. The taint's line names
the scan's items, and the helper's log line `leak scan` lists every item. Remove what the run left
before you clear the taint.

A host job's taint (`host-job`) is not cleared this way, and the verb refuses it (`host_job`):
the machine is reimaged (docs/runbooks/host-jobs.md).

## 1. Find out why

`js host list` (or the hostkeeper host page) shows the taint the server stores for each host: its
reason, check and run. On the host, `journalctl -u js-agent-crun` has the finding that tainted it,
and a line `tainted: <reason> (check <check>, run <run>): <what changed>` naming what the check saw. A
process taint names the process: `pid <pid> (<command>, state <state>, cgroup <cgroup>, owner <user>
(<uid>), cmd "<command line>") is running after job <run>`, so the line says whether it is the host's
own helper or a job's leftover. The agent's health route (`/healthz`) gives the same text in its
reason, `the agent holds a taint on this host: <what changed>`. A taint read back from the marker after
a restart has no such line (the marker holds no text); read the journal of the run that made it.
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
  `owed-*` record; a `taint.pinned` alone (stale, of a taint already cleared) is kept aside and
  recorded without touching the unit;
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
| `state_invalid` | the state root, its lock or the ledger cannot be used (a ledger that is not there means `--scratch-root` is not the agent's) | read the detail |
| `locked` | another clear is running | wait for it |
| `host_job` | the taint is a host job's; nothing was done | reimage the machine |
| `run_held` | a job runs | run it again when the job has ended |
| `stop_failed` | the unit did not stop within 300 s; nothing moved, the unit was started again | look at the journal, then run it again |
| `clear_failed` | a file could not be kept aside; the detail says which were; the marker is still there and the unit is left stopped | fix the cause, run it again (it starts the unit) |
| `record_failed` | the files were kept aside but the record was not written | keep the journal line |
| `start_failed` | the clear stands but the unit did not start | `systemctl status js-agent-crun` |

## 3. Check the server's taint

The lease server keeps its own record of the taint, but the agent's marker is the truth of every
taint the agent made (a residue, an unchecked input, a failed cleanup, an unreadable marker,
owed checks, the mirror of a refused start). Every heartbeat says whether the agent holds a
taint (`tainted`), so the agent that step 2 restarted, which holds no marker, ends the server's
record at its first heartbeat: the host is listed without a taint (`js host list`) within one
heartbeat interval, and the server's journal has one line, "a host's taint is cleared", with
`by="agent-holds-none"` and the taint's reason. Nothing more is needed.

The agent's word does not end two taints the server makes itself: a host job's (`host-job`) and
a refused whole-host prepare's (`host-unsafe`). Clear one of those, or the record held for an
agent too old to send `tainted`, with the operator's credential:

```text
js host host-taint --host-id ID
```

Do it after step 2 (and, for a host job's taint, after the machine is reimaged): while the
agent's marker is there, the agent says it is tainted at every heartbeat, and the server takes
that as the host's taint again (reason `host-tainted`).

While a taint stands, the server logs once, as a warning, that the placement pass skips the host
for it ("a live host is skipped by placement for the taint the server holds"), with the host, the
reason, the check and the run, so a job that stays queued has its cause in the journal. On a host
whose agent runs with `--health-listen` (the packaged unit does), the agent's health route
(`/healthz`) reports it not placeable for the same cause.

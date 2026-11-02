# Mac lease user teardown

How `js-mac-session` ends a lease user on macOS (`kill-uid`, then `user-delete`), what each step proves, and what happens when one fails. The native runtime (`crates/js-agent/src/native`) calls both verbs when a run is removed and again, from its journal, at the next start. The two-run isolation test (`crates/js-agent/tests/native_isolation`) checks the result on a real Mac.

## What can outlive a user

- **Per-user launchd agents.** launchd keeps a `user/<uid>` domain (and a `gui/<uid>` domain after a login) and restarts the agents in it, `distnoted` among them, for as long as the domain exists. A live process of the uid makes `user/<uid>` again. So booting the domain out while the job still runs does nothing, and killing the processes alone lets launchd restart an agent as the uid after the kill.
- **The per-user folders entry.** `/private/var/folders/<xx>/<hash>/` (with `0`, `C` and `T`) is keyed by the user record's UUID and uid, not by the home. Daemons make folders in it for the user, such as `C/com.apple.ScreenTimeAgent`, that outlive the account.

## `kill-uid`

The verdict is the outcome, never what a `launchctl` call answered: no process of the uid is left, and the uid's launchd domains no longer print.

1. Look once, before anything is killed, whether the uid has a GUI login session: it owns a `loginwindow` process (the login window runs as the user of each GUI session). A pool user never logs in, so it has none, and its `gui/<uid>` is never asked about. A whole-machine lease's user or the VM user, logged in, has one; the kill in step 2 ends it, which is why the look comes first.
2. Kill every process of the uid.
3. Boot out `user/<uid>` (and `gui/<uid>`, only with a session). The bootout's own exit status is not a verdict: it fails for a domain that is not there, and can fail after it ended the domain. A failure is logged.
4. Look at the outcome:
   - a process of the uid is left, or a domain still prints: go to 2;
   - neither: done.

   The loop runs at most `KILL_ROUNDS` (10) attempts. The bound is a count, never a time. If the outcome still does not hold after the last attempt, `kill-uid` fails naming what remains (the processes, the domains, and why each domain was not told gone), and the uid is quarantined (see below).
5. Remove the crontab and `at` jobs, then run 2–4 again.

A domain prints when `launchctl print` exits 0. Any other exit status is a domain launchd does not print. No exit status is matched as "not found": launchd's answer for a domain it does not have varies (on macOS 26, `launchctl print gui/<uid>` for a user that never logged in has exited 112 and 113, "Could not find domain", and 125, "Domain does not support specified action", from run to run). A `launchctl` that a signal ended, or that could not be run, gave no answer: the domain is not taken as gone, and the next attempt looks again.

This reading is the teardown's only. A domain wrongly read as gone there is caught by the look at the processes. The readiness checks (`session-ready`, `role-session-ready`, `role-run`, and the session switch's rest at the VM user) act on a "not up": the switch logs the VM user in again. So they keep the strict reading: exit 0 is up, exit 112 or 113 with "Could not find domain" is not up, and any other answer (125 included) is an error that changes nothing.

## `user-delete`

1. **The entry.**
   - While the record exists, ask for the uid's `DARWIN_USER_DIR` (`getconf DARWIN_USER_DIR`, run as the uid and its group with an empty environment, `/` as its working directory and empty input). Take only an answer of the form `/var/folders/<xx>/<hash>/0`; the entry is `<user_folders>/<xx>/<hash>`. Never guess the path from the uid.
   - If the record is already gone (a delete repeated after it went), there is no `DARWIN_USER_DIR` of the uid's own. Use every directory two levels down in the per-user folders that the uid owns. An `<xx>`, or such a directory, on another filesystem (something mounted there) refuses the delete: the sweep never crosses a filesystem, so what is behind the mount is never taken as clean.
2. **Settle.**
   - No process may remain.
   - Boot out `user/<uid>`, as in `kill-uid` (its failure logged); if it still prints, or `launchctl print` gave no answer, the delete is refused. `gui/<uid>` is not asked about: with no process of the uid left there is no login session, and `kill-uid` judged its domain.
   - Look for processes again, once, without waiting: a job launchd started before the bootout ended its domain is found here, and any process seen refuses the delete at once. Such a job may have written again what the sweep removed before exiting, so it is never waited out; the repeated delete sweeps everything again.
3. Remove the crontab and `at` jobs. No process may remain.
4. Sweep the home and the other places. Remove the entry whole, and only if the uid owns it:
   - nothing is followed;
   - nothing past the entry's filesystem is touched;
   - a subtree too deep to remove in place is moved up *inside* the entry, so a removal cut short leaves everything under the entry, where the next removal finds it.
5. Settle again.
6. Delete the record, then record the deletion in the ledger.

## Failure: the uid is held

A step that fails leaves the lease live in the helper's ledger: the uid is quarantined. A live lease's uid is never handed to another lease. The ledger is a file and survives a restart. A boot reset that cannot end the user fails and keeps it. The refusal names the step, and the helper logs that the uid is held. The native runtime keeps the lease in its journal and repeats the delete at the next start. The uid is free again only once a delete completes.

## The isolation test's `no-path` verdict

A launch agent kind (`user/<uid>`, `gui/<uid>`) is covered without being seeded only when all of these hold. Otherwise it is not exercised and the test cannot pass.

- The job's own `launchctl bootstrap` into the domain was refused by launchd with a domain-level refusal ("Could not find domain", "Domain does not support specified action", or "Bootstrap failed: 1: Operation not permitted"). A generic failure such as "Bootstrap failed: 5: Input/output error" does not count.
- The job's plist passes `plutil -lint`.
- Root's attempts all ran and none loaded the agent.
- Root's `launchctl print` shows the domain absent, or present without the agent.

A job whose bootstrap works has seeded the agent, and an agent that survives fails the test.

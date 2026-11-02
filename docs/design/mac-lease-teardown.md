# Mac lease user teardown

How `js-mac-session` ends a lease user on macOS (`kill-uid`, then `user-delete`), what each step proves, and what happens when one fails. The native runtime (`crates/js-agent/src/native`) calls both verbs when a run is removed and again, from its journal, at the next start. The two-run isolation test (`crates/js-agent/tests/native_isolation`) checks the result on a real Mac.

## What can outlive a user

- **Per-user launchd agents.** launchd keeps a `user/<uid>` domain (and a `gui/<uid>` domain after a login) and restarts the agents in it, `distnoted` among them, for as long as the domain exists. A live process of the uid makes `user/<uid>` again. So booting the domain out while the job still runs does nothing, and killing the processes alone lets launchd restart an agent as the uid after the kill.
- **The per-user folders entry.** `/private/var/folders/<xx>/<hash>/` (with `0`, `C` and `T`) is keyed by the user record's UUID and uid, not by the home. Daemons make folders in it for the user, such as `C/com.apple.ScreenTimeAgent`, that outlive the account.

## `kill-uid`

1. Kill every process of the uid.
2. Ask launchd (`launchctl print`) for `gui/<uid>` and `user/<uid>`, and boot out each one it has. A pool user never logs in, so it has no `gui/<uid>`, and once its agents are gone no `user/<uid>`: a domain launchd does not have is not booted out, and that is not an error.
3. Look:
   - A process of the uid is left: go to 1.
   - Neither a process nor a domain is left: done.
   - A domain is left: go to 1.

   The loop runs at most `KILL_ROUNDS` (10) rounds. The bound is a count, never a time.
4. Remove the crontab and `at` jobs, then run 1–3 again.

A domain counts as gone only on launchd's not-found answer to `launchctl print`: exit 112 or 113 together with "Could not find domain" on standard error. On macOS 26, `launchctl print gui/<uid>` for a user that never logged in exits 112 and prints "Bad request." and "Could not find domain for user gui: <uid>". Any other answer (another exit status, or 112 or 113 without those words) is an error naming the domain, never absence.

## `user-delete`

1. **The entry.**
   - While the record exists, ask for the uid's `DARWIN_USER_DIR` (`getconf DARWIN_USER_DIR`, run as the uid and its group with an empty environment, `/` as its working directory and empty input). Take only an answer of the form `/var/folders/<xx>/<hash>/0`; the entry is `<user_folders>/<xx>/<hash>`. Never guess the path from the uid.
   - If the record is already gone (a delete repeated after it went), there is no `DARWIN_USER_DIR` of the uid's own. Use every directory two levels down in the per-user folders that the uid owns. An `<xx>`, or such a directory, on another filesystem (something mounted there) refuses the delete: the sweep never crosses a filesystem, so what is behind the mount is never taken as clean.
2. **Settle.**
   - No process may remain.
   - Boot out each of the two domains that launchd has, as in `kill-uid`; one that stays refuses the delete.
   - Look for processes again, once, without waiting: a job launchd started before the bootout ended its domain is found here, and any process seen refuses the delete at once. Such a job may have written again what the sweep removed before exiting, so it is never waited out; the repeated delete sweeps everything again.
3. Remove the crontab and `at` jobs. No process may remain.
4. Sweep the home and the other places. Remove the entry whole, and only if the uid owns it:
   - nothing is followed;
   - nothing past the entry's filesystem is touched;
   - a subtree too deep to remove in place is moved up *inside* the entry, so a removal cut short leaves everything under the entry, where the next removal finds it.
5. Settle again.
6. Delete the record, then record the deletion in the ledger.

## Failure: the uid is held

A step that fails leaves the lease live in the helper's ledger. A live lease's uid is never handed to another lease. The ledger is a file and survives a restart. A boot reset that cannot end the user fails and keeps it. The refusal names the step, and the helper logs that the uid is held. The native runtime keeps the lease in its journal and repeats the delete at the next start. The uid is free again only once a delete completes.

## The isolation test's `no-path` verdict

A launch agent kind (`user/<uid>`, `gui/<uid>`) is covered without being seeded only when all of these hold. Otherwise it is not exercised and the test cannot pass.

- The job's own `launchctl bootstrap` into the domain was refused by launchd with a domain-level refusal ("Could not find domain", "Domain does not support specified action", or "Bootstrap failed: 1: Operation not permitted"). A generic failure such as "Bootstrap failed: 5: Input/output error" does not count.
- The job's plist passes `plutil -lint`.
- Root's attempts all ran and none loaded the agent.
- Root's `launchctl print` shows the domain absent, or present without the agent.

A job whose bootstrap works has seeded the agent, and an agent that survives fails the test.

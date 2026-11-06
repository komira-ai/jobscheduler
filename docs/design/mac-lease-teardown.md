# Mac lease user teardown

How `js-mac-session` ends a lease's use of its user on macOS (`kill-uid`, then `user-delete`), what each step proves, and what happens when one fails. A lease's user is a pool user (`kbf-lease-pool-<slot>`): `user-delete` resets it for the next lease, and deletes it (then makes a new one in the slot) only when the reset is not clean. The deletion below is that replacement's, and is also how the user of a lease that a release before the pool created is ended. The native runtime (`crates/js-agent/src/native`) calls both verbs when a run is removed and again, from its journal, at the next start. The two-run isolation test (`crates/js-agent/tests/native_isolation`) checks the result on a real Mac.

## What can outlive a user

- **Per-user launchd agents.** launchd keeps a `user/<uid>` domain (and a `gui/<uid>` domain after a login) and restarts the agents in it, `distnoted` among them, for as long as the domain exists. A live process of the uid makes `user/<uid>` again. So booting the domain out while the job still runs does nothing, and killing the processes alone lets launchd restart an agent as the uid after the kill.
- **The per-user folders entry.** `/private/var/folders/<xx>/<hash>/` (with `0`, `C` and `T`) is keyed by the user record's UUID and uid, not by the home. Daemons make folders in it for the user, such as `C/com.apple.ScreenTimeAgent`, that outlive the account.

## `kill-uid`

The verdict is the outcome, never what a `launchctl` call answered: no process of the uid is left, and the uid's launchd domains no longer print.

1. Before anything is killed, decide whether the uid may have a GUI login session. It may when any of these holds:
   - the session switch's intent record names the account (the login window logs in only the user it names, so a login may be on its way);
   - the uid owns a `loginwindow` process (the login window runs as the user of each GUI session), or a process whose name cannot be read (which does not rule one out);
   - an earlier teardown left its GUI domain owed (below).

   A pool user has none of these, so its `gui/<uid>` is never asked about. A process named `loginwindow` without a session only costs a bootout of `gui/<uid>` that fails and a look that finds it gone. A session seen at a later look of the processes (step 4) counts from the next attempt on. The decision comes before the kill because the kill ends the `loginwindow`.

   From the moment a session is seen, `gui/<uid>` is owed a teardown: a record `gui-teardown.<uid>` in the helper's state directory, written before the kill. It is removed only once a teardown judges `gui/<uid>` gone. So a retry after a failed GUI teardown (whose kill ended the `loginwindow`), the boot reset after a restart of the helper, and `user-delete` all still boot out and judge it.

   The record is read first, before anything else decides the session, and is opened never through a link: a link there refuses the teardown before anything is killed or replaced.

   Across releases: the record is a file of its own, and no release lists the state directory; each opens only the files it names (the ledger, the intent record, the scan's baseline and window). A previous release therefore never sees it, and a release that finds no record (a state directory a previous release left) owes nothing. `helper::tests::previous_release` runs the previous release's own ledger (v0.1.128's `ledger.rs`, pinned by its SHA-256) on this release's state, and this release on state that ledger wrote. A record left behind by a rolled-back release, for a uid that a new lease has since received, costs that lease's `kill-uid` a bootout and a `launchctl print` of `gui/<uid>` in each of its two passes (it does not print, so it reads as gone), and is then discharged.
2. Kill every process of the uid.
3. Boot out `user/<uid>` (and `gui/<uid>`, only with a session). The bootout's own exit status is not a verdict: it fails for a domain that is not there, and can fail after it ended the domain. A failure is logged.
4. Look at the outcome:
   - a process of the uid is left, or a domain still prints: go to 2;
   - neither: done.

   The loop runs at most `KILL_ROUNDS` (10) attempts. The bound is a count, never a time. If the outcome still does not hold after the last attempt, `kill-uid` fails naming what remains (the processes, the domains, and why each domain was not told gone), and the uid is quarantined (see below). For the VM user (`role-kill-uid`), which is no lease, the refusal says the verb can be repeated instead. A process table that cannot be read stops the teardown at once (not retried); the uid stays held and the caller repeats it.
5. Remove the crontab and `at` jobs, then run 2–4 again.

A domain prints when `launchctl print` exits 0. Any other exit status is a domain launchd does not print. No exit status is matched as "not found": launchd's answer for a domain it does not have varies (on macOS 26, `launchctl print gui/<uid>` for a user that never logged in has exited 112 and 113, "Could not find domain", and 125, "Domain does not support specified action", from run to run). A `launchctl` that a signal ended, or that could not be run, gave no answer: the domain is not taken as gone, and the next attempt looks again.

This reading is the teardown's only. A domain wrongly read as gone there is caught by the look at the processes. The readiness checks (`session-ready`, `role-session-ready`, `role-run`, and the session switch's rest at the VM user) act on a "not up": the switch logs the VM user in again. So they keep the strict reading: exit 0 is up, exit 112 or 113 with "Could not find domain" is not up, and any other answer (125 included) is an error that changes nothing.

## `user-delete` of a pool user: the reset

The lease is recorded over (the ledger's `deleted`, then the slot's `resetting`), then the user is reset. Each step is judged by its outcome; the first that does not hold makes the reset not clean:

1. `kill-uid`'s teardown, as above.
2. The user's record must exist; the entry is taken from its `DARWIN_USER_DIR`, as in the deletion's step 1.
3. Settle, as in the deletion's step 2. Remove the crontab and `at` jobs; no process may remain.
4. Remove the uid's System V IPC objects: every object `ipcs -a` lists with the uid (or its name) as owner or creator, removed with `ipcrm` (a failure only logged), then `ipcs` again: an object still listed fails the step. POSIX shared memory and semaphores have no listing on macOS and are not found.
5. Sweep the other places and remove the home whole; make the home again, new, empty and the user's.
6. Empty the entry, keeping its `0`, `C` and `T` (each emptied).
7. Settle again.
8. Ask the directory that the user is not in `admin` (`dseditgroup -o checkmember`): only its "no" passes.

A clean reset records the slot free. One that is not replaces the user: the kill again, the deletion below (with step 4 of the reset before its sweep), a new record with the same name, uid and home, and step 8. The slot is free only once that holds. The user of an administrator's lease is always replaced, never reset; its `resetting` record says so, so the recovery after a crash replaces it too.

## The deletion (`user-delete` of a lease from before the pool, and a pool user's replacement)

1. **The entry.**
   - While the record exists, ask for the uid's `DARWIN_USER_DIR` (`getconf DARWIN_USER_DIR`, run as the uid and its group with an empty environment, `/` as its working directory and empty input). Take only an answer of the form `/var/folders/<xx>/<hash>/0`; the entry is `<user_folders>/<xx>/<hash>`. Never guess the path from the uid.
   - If the record is already gone (a delete repeated after it went), there is no `DARWIN_USER_DIR` of the uid's own. Use every directory two levels down in the per-user folders that the uid owns. An `<xx>`, or such a directory, on another filesystem (something mounted there) refuses the delete: the sweep never crosses a filesystem, so what is behind the mount is never taken as clean.
2. **Settle.**
   - No process may remain.
   - Boot out `user/<uid>`, and `gui/<uid>` while it is owed, as in `kill-uid` (a failure logged); if one still prints, or `launchctl print` gave no answer, the delete is refused. Otherwise `gui/<uid>` is not asked about: with no process of the uid left there is no login session. After the last settle an owed `gui/<uid>` is discharged.
   - Look for processes again, once, without waiting: a job launchd started before the bootout ended its domain is found here, and any process seen refuses the delete at once. Such a job may have written again what the sweep removed before exiting, so it is never waited out; the repeated delete sweeps everything again.
3. Remove the crontab and `at` jobs. No process may remain. Remove the uid's System V IPC objects, as in the reset's step 4.
4. Sweep the home and the other places. Remove the entry whole, and only if the uid owns it:
   - nothing is followed;
   - nothing past the entry's filesystem is touched;
   - a subtree too deep to remove in place is moved up *inside* the entry, so a removal cut short leaves everything under the entry, where the next removal finds it.
5. Settle again.
6. Delete the record. For a lease from before the pool, then record the deletion in the ledger.

## Failure: the uid is held

A pool user is lent only from a free slot. A reset that is not clean replaces the user; a replacement that fails leaves the slot broken: it is lent to no one, `pool-status` and the pool-full refusal name it, and the next `pool-create` (the boot reset runs one) replaces it again. A slot found resetting after a restart, or leased to a lease that is not live, is reset (or replaced) by the boot reset before it is free. A ledger write that fails leaves the slot not free. Either way no other lease gets the uid until a reset or a replacement completes.

For a lease from before the pool, a step that fails leaves the lease live in the helper's ledger: the uid is quarantined. A live lease's uid is never handed to another lease, nor made a pool slot. The ledger is a file and survives a restart. A boot reset that cannot end the user fails and keeps it. The refusal names the step, and the helper logs that the uid is held. The native runtime keeps the lease in its journal and repeats the delete at the next start. The uid is free again only once a delete completes.

## While no agent runs

The helper holds no deadline of a run's own: its `run` verb takes the program, its environment and the file-size and process limits, and nothing ends the process but `kill-uid`. A native run's wall limit and the agent's silence bound (`--silence-stop`, which stops every run the agent holds once it has not heard the server for the bound) are counted by the agent and enforced through `kill-uid`, at once, with no grace. So while the agent is down, has stopped or is restarting, a native run goes on with no bound; the next agent to start ends it from its journal (kill, then `user-delete`), as does the rescue of an agent whose worker died. A deadline the helper enforces by itself is a change to the helper that has not been made.

## The isolation test's `no-path` verdict

A launch agent kind (`user/<uid>`, `gui/<uid>`) is covered without being seeded only when all of these hold. Otherwise it is not exercised and the test cannot pass.

- The job's own `launchctl bootstrap` into the domain was refused by launchd with a domain-level refusal ("Could not find domain", "Domain does not support specified action", or "Bootstrap failed: 1: Operation not permitted"). A generic failure such as "Bootstrap failed: 5: Input/output error" does not count.
- The job's plist passes `plutil -lint`.
- Root's attempts all ran and none loaded the agent.
- Root's `launchctl print` shows the domain absent, or present without the agent.

A job whose bootstrap works has seeded the agent, and an agent that survives fails the test.

# Mac lease user teardown

How `js-mac-session` ends a lease's use of its user on macOS (`kill-uid`, then `user-delete`), what each step proves, and what happens when one fails. A lease's user is an account of its own, `kbf-lease-<term>-<seq>`, made by `user-create` when the lease starts: `user-delete` deletes it and shows it gone before its uid can be given again (`mac-account-per-job.md`). Deleting an account needs the helper to hold Full Disk Access (`mac-hostkeeper-package.md`); without it the delete is refused, the lease stays live and its uid held, and the agent refuses native jobs on such a Mac anyway. The native runtime (`crates/js-agent/src/native`) calls both verbs when a run is removed and again, from its journal, at the next start. The two-run isolation test (`crates/js-agent/tests/native_isolation`) checks the result on a real Mac.

## What can outlive a user

- **Per-user launchd agents.** launchd keeps a `user/<uid>` domain (and a `gui/<uid>` domain after a login) and restarts the agents in it, `distnoted` among them, for as long as the domain exists. A live process of the uid makes `user/<uid>` again. So booting the domain out while the job still runs does nothing, and killing the processes alone lets launchd restart an agent as the uid after the kill.
- **The per-user folders entry.** `/private/var/folders/<xx>/<hash>/` (with `0`, `C` and `T`) is keyed by the user record's UUID and uid, not by the home. Daemons make folders in it for the user, such as `C/com.apple.ScreenTimeAgent`, that outlive the account.
- **POSIX shared memory and named semaphores.** One lasts until it is unlinked or the Mac restarts, and no interface of macOS lists the names, so no step below can find one. A run cannot create shared memory: the job profile's floor denies `ipc-posix-shm-write-create`, and `shm_open` with `O_CREAT` fails with `EPERM` (measured on macOS 26.4 and 27.0.1, where clang, swiftc, codesign, git and Python's `multiprocessing` pools still ran under the rule; `crates/js-mac-pool/tests/runner.rs` holds the profile to it on a macOS runner). A program that makes its own POSIX shared memory fails there, for example Python's `multiprocessing.shared_memory`, or a PostgreSQL started with `dynamic_shared_memory_type = posix`. Opening an object that exists is still allowed. Named semaphores are not denied, because Python's `multiprocessing` locks are made with `sem_open`: a semaphore a run left holds a count, not data, and the next run of the same uid can open it by its name until the Mac restarts.
- **Login items and Background Task Management entries.** The deletion's sweep removes the uid's `loginitems.<uid>.plist` with the swept places, but not a Background Task Management entry under the uid. The post-job leak scan below finds both.

## `kill-uid`

The verdict is the outcome, never what a `launchctl` call answered: no process of the uid is left, and the uid's launchd domains are gone (they no longer print, or print as a domain that holds nothing).

1. Before anything is killed, decide whether the uid may have a GUI login session. It may when any of these holds:
   - the session switch's intent record names the account (the login window logs in only the user it names, so a login may be on its way);
   - the uid owns a `loginwindow` process (the login window runs as the user of each GUI session), or a process whose name cannot be read (which does not rule one out);
   - an earlier teardown left its GUI domain owed (below).

   A lease's account has none of these, so its `gui/<uid>` is never asked about. A process named `loginwindow` without a session only costs a bootout of `gui/<uid>` that fails and a look that finds it gone. A session seen at a later look of the processes (step 4) counts from the next attempt on. The decision comes before the kill because the kill ends the `loginwindow`.

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

A domain prints when `launchctl print` exits 0, except a domain that holds nothing: `launchctl print user/<uid>` exits 0 for any uid, even one with no account, because the print makes the user domain (measured on macOS 27.0.1: an idle account, a uid after its lease and uid 799 with no account all print the same domain), so a print whose own top-level `services = {` and `unmanaged processes = {` blocks (one tab deep, not an endpoint's) are both empty is a domain that is gone. A print with anything in either block, or without either block, is a domain that stays. Any other exit status is a domain launchd does not print. No exit status is matched as "not found": launchd's answer for a domain it does not have varies (on macOS 26, `launchctl print gui/<uid>` for a user that never logged in has exited 112 and 113, "Could not find domain", and 125, "Domain does not support specified action", from run to run). A `launchctl` that a signal ended, or that could not be run, gave no answer: the domain is not taken as gone, and the next attempt looks again.

This reading is the teardown's only. A domain wrongly read as gone there is caught by the look at the processes. The readiness checks (`session-ready`, `role-session-ready`, `role-run`, and the session switch's rest at the VM user) act on a "not up": the switch logs the VM user in again. So they keep the strict reading: exit 0 is up, exit 112 or 113 with "Could not find domain" is not up, and any other answer (125 included) is an error that changes nothing.

## `user-delete`: the deletion

The account is deleted, and only then is the lease recorded over (the ledger's `deleted`). Each step is judged by its outcome; the first that does not hold refuses the delete, names the step, and leaves the lease live.

1. **The entry.**
   - While the record exists, ask for the uid's `DARWIN_USER_DIR` (`getconf DARWIN_USER_DIR`, run as the uid and its group with an empty environment, `/` as its working directory and empty input). Take only an answer of the form `/var/folders/<xx>/<hash>/0`; the entry is `<user_folders>/<xx>/<hash>`. Never guess the path from the uid.
   - If the record is already gone (a delete repeated after it went), there is no `DARWIN_USER_DIR` of the uid's own. Use every directory two levels down in the per-user folders that the uid owns. An `<xx>`, or such a directory, on another filesystem (something mounted there) refuses the delete: the sweep never crosses a filesystem, so what is behind the mount is never taken as clean.
2. **Settle.**
   - No process may remain.
   - Boot out `user/<uid>`, and `gui/<uid>` while it is owed, as in `kill-uid` (a failure logged); if one still prints, or `launchctl print` gave no answer, the delete is refused. Otherwise `gui/<uid>` is not asked about: with no process of the uid left there is no login session. After the last settle an owed `gui/<uid>` is discharged.
   - Look for processes again, once, without waiting: a job launchd started before the bootout ended its domain is found here, and any process seen refuses the delete at once. Such a job may have written again what the sweep removed before exiting, so it is never waited out; the repeated delete sweeps everything again.
3. Remove the crontab and `at` jobs. No process may remain. Remove the uid's System V IPC objects: every object `ipcs -a` lists with the uid (or its name) as owner or creator, removed with `ipcrm` (a failure only logged). POSIX shared memory and semaphores have no listing on macOS and are not found (see above: a run cannot create shared memory).
4. Sweep the home and the other places. Remove the entry whole, and only if the uid owns it, even when the system placed it (`sunlnk`, `com.apple.rootless`): the entry is emptied and then removed, and the proof below judges whether it is gone.
   - nothing is followed;
   - nothing past the entry's filesystem is touched;
   - a subtree too deep to remove in place is moved up *inside* the entry, so a removal cut short leaves everything under the entry, where the next removal finds it.
5. Settle again.
6. Delete the record, then the home whole.
7. **Show it gone.** Each of these must hold, and the first that does not names itself in the refusal (for example "not shown gone: a record has uid 612"):
   - no record by the name, and none by the uid;
   - no process of the uid, and its launchd domains read as gone, as `kill-uid` judges them;
   - the home path `<homes>/<name>` does not exist;
   - no directory two levels down in the per-user folders is owned by the uid;
   - none of the swept places holds an entry owned by the uid: a read-only scan finds no path of a named template (with the user and uid filled in) and no entry owned by the uid in the places that are walked (`/Users/Shared`, `/private/tmp`, `/private/var/tmp`, `/private/var/at/jobs`);
   - no System V IPC object of the uid, by a fresh `ipcs -a`, matched by uid number or name.
8. Only then is the deletion recorded in the ledger, and the uid is free.

Deleting a record that is not there is not an error. The same steps and proof delete an account found by name with no lease (below, "The boot reset").

The helper refuses a `user-create` that carries a grant (the field the protocol kept from releases where a gate's signed grant made a lease's user an administrator), and has no grant keys and no way to add a user to `admin`: no lease user is ever an administrator, a native job never gets administrator rights, and a job that needs them runs in a VM.

## Failure: the uid is held

A step that fails, or a proof that does not hold, leaves the lease live in the helper's ledger: the uid is held. A live lease's uid is never handed to another lease. The ledger is a file and survives a restart. The refusal names the step, and the helper logs that the uid is held. The native runtime keeps the lease in its journal and repeats the delete at the next job's end and at its next start. The held uid stays out of the helper's range, so the host runs one native job fewer for each held account (a Mac agent has no native slot count of its own; the range, 100 uids by default, holds one uid fewer to give). The agent notes a held account (`account-held`); two held accounts taint the host (`account-not-deleted`), which the agent never clears itself. The uid is free again only once a delete completes.

A new uid is the next free one after the uid of the ledger's last `created` record (wrapping at the end of the range), so a uid is used again as late as the range allows. A uid is free when it is in the range, is held by no live lease, has no record, has no process, owns no per-user folder entry, and owns nothing in the swept places.

## The boot reset

The helper's boot reset (before it serves any request) leaves the lease that the session switch's intent record names and brings every other account to gone:

1. With the grant, it lists the local accounts whose names start with `kbf-lease-` and whose uid is in the range, and deals with those the ledger has no live lease for. An account `kbf-lease-pool-<n>` (left by a release before the account per job) is deleted by name with the steps and proof above and no ledger record; before the first such deletion the pool file is removed and the directory synced, so a rollback to a release with the pool makes its pool afresh. A `kbf-lease-<term>-<seq>` whose lease the ledger does not know is recorded `created` and then handled with the ledger's leases. Any other name with that prefix (or a lease the ledger has as deleted) is deleted by name. A by-name deletion that fails makes the boot reset fail, naming the account and the step ("the boot reset is incomplete: <account>: <step>"): such accounts exist only at the first start after the pool or after an account made by hand, and a Mac that cannot delete must not start running accounts per job. Without the grant none of this runs.
2. Every live ledger lease but the kept one is killed (`kill-uid`) and deleted as above. A lease whose account is not shown gone does not fail the boot reset: it stays live with its uid held, is logged, and is returned in the reply's `held` list, which the agent journals and retries.

## After the run: the leak scan

After each native run is removed, the agent's post-job check (`NativeChecks`, `crates/js-agent/src/leak_scan.rs`) asks the helper for its leak scan (`scan`, `crates/js-mac-session/src/scan.rs`). The scan reports what changed since the node's recorded baseline. The check compares that with the scan it took when the host was last idle, and judges only what is new:

- An added login item or Background Task Management entry of a uid other than root's taints the host as residue (`scan-residue`), and so does an added `and <n> more` line, which may hide one. A run can make its own entries, which deleting its account does not undo, and a later run of the same uid would inherit them. The taint does not clear itself.
- Every other change is a note (`scan-host-changed`) and never taints. The launch agent and daemon folders and the auto-login state are root's, a lease's account is the job's own, and a removal is not something a lease user can make to another user's entries. The scan does not report the users of live leases, so a scan while other leases run is not dirty by their users.
- A scan that could not read an item (`scan-unreadable`), or one the helper did not answer (`scan-failed`), taints the host as unchecked. That taint clears itself once two re-checks of the idle host pass, as the Linux checks' unreadable inputs do.
- Lines are compared by their text. When the idle host already has more changes than a reply holds (20 per list), the `and <n> more` count can change because of a root-owned change and taint the host, and a run's entry behind an unchanged count is missed. A node whose drift fits in a reply has neither problem; record the baseline again after a software change.
- A node whose baseline was never recorded is not judged. The helper refuses the scan (`no baseline was recorded`), the check notes it once (`scan-no-baseline`), and every run passes until a baseline is recorded. The baseline is recorded only in its window, which the root tool that applied a software set or erased the Mac opens. No tool opens that window yet, so on a node today the check judges nothing.

A change by another job running beside the one that ended is judged with the one that ended: the host is tainted either way, and the finding names the run that ended.

## While no agent runs

The helper holds a deadline of a run's own: the agent's `run` request carries `deadline_ms`, the job's wall limit plus the agent's stop grace, and the helper ends the lease's processes (as `kill-uid` does) once it passes, whether or not the agent is still connected. A native run's wall limit and the agent's silence bound (`--silence-stop`, which stops every run the agent holds once it has not heard the server for the bound) are counted by the agent and enforced through the helper's `stop`: `SIGTERM` to the run's process group, then `kill-uid` once the grace has passed. So while the agent is down, has stopped or is restarting, a native run is bounded by its deadline; the next agent to start adopts it (the lease journal's run record and the helper's `watch`), and ends from its journal only a lease with no run record, as does the rescue of an agent whose worker died. A helper from before `stop` and `deadline_ms` refuses them as requests it cannot read; the agent then kills at once and starts runs without the deadline, so such a run is bounded only while an agent runs.

## The isolation test's `no-path` verdict

A launch agent kind (`user/<uid>`, `gui/<uid>`) is covered without being seeded only when all of these hold. Otherwise it is not exercised and the test cannot pass.

- The job's own `launchctl bootstrap` into the domain was refused by launchd with a domain-level refusal ("Could not find domain", "Domain does not support specified action", or "Bootstrap failed: 1: Operation not permitted"). A generic failure such as "Bootstrap failed: 5: Input/output error" does not count.
- The job's plist passes `plutil -lint`.
- Root's attempts all ran and none loaded the agent.
- Root's `launchctl print` shows the domain absent, or present without the agent.

A job whose bootstrap works has seeded the agent, and an agent that survives fails the test.

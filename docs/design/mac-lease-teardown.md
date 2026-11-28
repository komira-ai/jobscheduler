# Mac account teardown

What ends a native job's use of its account on macOS, what each step proves, and what happens when one fails. This page began as a description of the root helper `js-mac-session` (its `kill-uid` and `user-delete` verbs, the pool of reused users and their reset in place, the helper's deadline for a run). That helper is retired (`mac-one-program.md`, section 13): the agent makes each job's account and deletes it afterwards, in its own process, in `crates/js-agent/src/mac_accounts`, and this page keeps what that code still does and the judgement behind it. The pool, its slots as lent users, the reset in place, `pool-create`, `pool-status` and the helper's deadline are gone; `mac-account-per-job.md` has the account's life, and `mac-one-program.md`, sections 11 to 13, the agent's side. The two-run isolation test (`crates/js-agent/tests/native_isolation`) checks the result on a real Mac.

## What can outlive a user

- **Per-user launchd agents.** launchd keeps a `user/<uid>` domain (and a `gui/<uid>` domain after a login) and restarts the agents in it, `distnoted` among them, for as long as the domain exists. A live process of the uid makes `user/<uid>` again. So booting the domain out while the job still runs does nothing, and killing the processes alone lets launchd restart an agent as the uid after the kill.
- **The per-user folders entry.** `/private/var/folders/<xx>/<hash>/` (with `0`, `C` and `T`) is keyed by the user record's UUID and uid, not by the home. Daemons make folders in it for the user, such as `C/com.apple.ScreenTimeAgent`, that outlive the account. The system protects the entry from removal, so the teardown empties it and keeps it, and the next account of the same slot gets the same one.
- **POSIX shared memory and named semaphores.** One lasts until it is unlinked or the Mac restarts, and no interface of macOS lists the names, so no step below can find one. A run cannot create shared memory: the job profile's floor denies `ipc-posix-shm-write-create`, and `shm_open` with `O_CREAT` fails with `EPERM` (measured on macOS 26.4 and 27.0.1, where clang, swiftc, codesign, git and Python's `multiprocessing` pools still ran under the rule; `crates/js-mac-pool/tests/runner.rs` holds the profile to it on a macOS runner). A program that makes its own POSIX shared memory fails there, for example Python's `multiprocessing.shared_memory`, or a PostgreSQL started with `dynamic_shared_memory_type = posix`. Opening an object that exists is still allowed. Named semaphores are not denied, because Python's `multiprocessing` locks are made with `sem_open`: a semaphore a run left holds a count, not data, and the next run of the same uid can open it by its name until the Mac restarts.
- **Login items and Background Task Management entries.** The sweep of the shared places removes the uid's `loginitems.<uid>.plist`, but not a Background Task Management entry under the uid. A post-job leak check would find both (below); none runs on a Mac today.

## Ending the uid's processes

Two pieces end processes, and each judges by the outcome, never by what a `launchctl` call answered.

**The kill rounds of a retirement** (`Pool::kill_rounds`, `crates/js-agent/src/mac_accounts/teardown.rs`). For at most `KILL_ROUNDS` (10) rounds: kill every process of the uid, boot out its launchd domains, look at the outcome, and stop when no process of the uid is left and its login domain does not print. The bound is a count, never a time. After the last round the retirement fails naming what remains (the processes, the domains and why each domain was not told gone) and the account is held (`mac-account-per-job.md`, "A deletion that does not complete"). A process table that cannot be read stops the retirement at once, not retried. After the first rounds the crontab and `at` jobs of the account are removed, and the rounds run again for a job that fired before they went.

Asking launchd is not reading it: `launchctl print user/<uid>` makes the user domain of any uid it names, and launchd then retries that domain's per-user services every few seconds, for ever for a uid with no account. So launchd is asked only about the uid of a record the pool made and has not deleted (`gui/<uid>` and `user/<uid>` are booted out while the record is there, only `gui/<uid>` is printed) and nothing after the deletion; the rounds for a uid with no record judge by its processes alone (`crates/js-agent/src/mac_accounts/launchd.rs`). A bootout's own exit status is not a verdict: it fails for a domain that is not there, and can fail after it ended the domain. A failure is logged. launchd's answer for a domain it does not have varies, so no exit status is matched as "not found", and a `launchctl` that a signal ended, or that could not be run, gave no answer: the domain is not taken as gone, and the next round looks again.

**The sweep of a run** (`crates/js-agent/src/mac_sweep`). While a job runs and after its stop grace, and again at its removal, the native runtime ends every process of the job in rounds: every process in the job's audit session whatever its uid (a set-uid program the job ran is found too), and every process whose effective or real uid is the slot's (the per-user daemons launchd starts for an account). It never targets pid 0 or 1, the agent, or a process of another uid outside the session. A job that left its process group, its session and its process tree (a double fork, `setsid`, a `nohup` orphan) is found by the audit session.

## The shared places and System V IPC

After the processes are gone the retirement removes what the job left outside its account, and judges by the outcome (`tree.rs`, `ipc.rs` in `crates/js-agent/src/mac_accounts`):

1. **The shared places** (`/Users/Shared`, `/private/tmp`, `/private/var/tmp` and the launchd per-uid records) are swept of every entry the uid owns. Nothing is followed: every directory is opened relative to its parent with `O_NOFOLLOW` and checked to be the directory that was examined, a link is unlinked and never followed, and a walk stays on its filesystem. A directory that cannot be searched fails the step naming the path: what was not searched is never read as holding nothing of the uid. A tree too deep to remove in place is moved up inside the directory being emptied, so a removal cut short leaves everything where the next one finds it.
2. **System V IPC.** Every shared memory object, semaphore and message queue `ipcs -a` lists with the uid (or its name) as owner or creator is removed with `ipcrm` (a failure is only logged), then `ipcs` is read again: an object still listed fails the step. POSIX shared memory and named semaphores have no listing on macOS and are not found (above).
3. **The record, the home and the per-user folders** are deleted and emptied, and the account is shown gone before its uid is free again (`mac-account-per-job.md`, "After the job: delete and show it gone").

A step that fails leaves the account held, its lease live and its uid unused; the agent keeps the lease in its journal and repeats the retirement at the next job's end and at its next start. A `kbf-acct-*` record or a lost ledger entry is found by the pool's open and deleted the same way (`crates/js-agent/src/mac_accounts/sweep.rs`).

No account is ever an administrator: a native job never gets administrator rights, and a job that needs them runs in a VM.

## The leak check

`crates/js-agent/src/leak_scan.rs` holds a post-job check, `NativeChecks`, for what a run can leave outside its account: a login item or a Background Task Management entry, which deleting the account does not undo. The agent's post-job check on a Mac is `NoChecks` (`crate::lifecycle::NoChecks`): the check is not wired in, and nothing produces the scan it judges, because the root helper's scan and its baseline went with the helper. What the module keeps is its judgement of a scan's answer and its taint codes:

- An added login item or Background Task Management entry of a uid other than root's would taint the host as residue (`scan-residue`), and so would an added `and <n> more` line, which may hide one. The taint does not clear itself.
- Every other change would be a note (`scan-host-changed`) and never taint.
- A scan that could not read an item (`scan-unreadable`), or one that could not be had (`scan-failed`), would taint the host as unchecked. That taint clears itself once two re-checks of the idle host pass, as the Linux checks' unreadable inputs do (`docs/design/taint-self-clearing.md`).
- A node with no baseline to compare with is not judged: the check notes it once (`scan-no-baseline`) and every run passes.

A change by another job running beside the one that ended would be judged with the one that ended: the host is tainted either way, and the finding names the run that ended.

## The isolation test's `no-path` verdict

A launch agent kind (`user/<uid>`, `gui/<uid>`) is covered without being seeded only when all of these hold. Otherwise it is not exercised and the test cannot pass.

- The job's own `launchctl bootstrap` into the domain was refused by launchd with a domain-level refusal ("Could not find domain", "Domain does not support specified action", or "Bootstrap failed: 1: Operation not permitted"). A generic failure such as "Bootstrap failed: 5: Input/output error" does not count.
- The job's plist passes `plutil -lint`.
- Root's attempts all ran and none loaded the agent.
- Root's `launchctl print` shows the domain absent, or present without the agent.

A job whose bootstrap works has seeded the agent, and an agent that survives fails the test.

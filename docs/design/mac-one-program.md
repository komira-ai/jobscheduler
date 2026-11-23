# One agent program on a Mac

Status: accepted, 2026-11-22 (the owner's answers are in section 10); the root probe of section 4.3 (step 1) has held in the system domain. On a Mac, `js-agent` does the work of the root helper `js-mac-session` itself, behind the agent's platform layer. Each native run gets a supervisor process of its own, built on the mechanism Linux already uses for a crun container. That supervisor keeps the run alive while the agent stops, restarts or updates. The package ships one program and one launchd job. Apart from the accounts of section 11, nothing here is built yet. The way there is in section 7, the questions for the owner are in section 8, and the case for keeping the helper is answered in section 9.

## 1. What changes for an operator

| | Today | After |
|---|---|---|
| Programs in the package | `js-agent`, `js-mac-session`, `js-mac-provision` | `js-agent` |
| launchd jobs | `js-agent`, `js-agent.session` (the root helper), `js-agent.pf-pool` (loads the pf anchor at boot and every 30 s) | `js-agent` |
| What runs while a job runs | the agent, the helper (the job's parent), and the job | the agent, one supervisor per run (`js-agent __supervise-native ...`, whose parent is `launchd` once the agent that started it has exited), and the job |
| An agent update with jobs running | the host drains first: the update waits until no job runs (`Drain::NotSystemd`, `crates/js-agent/src/update_hook/step/prepare.rs:251`) | the jobs keep running: the old agent hands them over and the new agent adopts them, as on a Linux crun host |
| The update hook on a Mac | stops the agent, then boots out and bootstraps the helper, then the pf job, then starts the agent (`update_hook/step/helper.rs`, `step/pf.rs`) | stops the agent and starts the agent |
| Socket, group, code-signature pin | `/var/run/js-mac-session/socket`, group `_jsagent`, the agent's cdhash baked into the helper's plist | none |
| Flags in the agent's plist | `--native-helper-socket /var/run/js-mac-session/socket` turns native jobs on | no such flag: `--native-accounts` and the slot range, in the host's flags file, turn native jobs on (section 13) |

Some things do not change: the pool of reused users (uids 600-699, `kbf-lease-pool-<n>`) and its ledger files, the Seatbelt profile of each run, the pf rules per uid, what a job sees, the wire to the lease server, and Linux.

**If accounts are made and deleted per job instead of reused.** A separate change proposes a new account for each job, deleted after it. That needs Full Disk Access for the agent's program, signed with a Developer ID Application certificate. This design holds either way, with these differences:

- **The supervisor.** It takes the run's uid, gid and home in its arguments as before, and never makes or deletes an account. Its deadline kill is unchanged: it kills the uid's processes and boots out its domains, under the lease's lock.
- **Removal.** The agent's `remove` ends the supervisor, then kills the uid, then deletes the account, where it reset the slot before. A uid is never given to a new account while a supervisor of an earlier lease that names it may still live. Removal deletes only after the supervisor is gone, and a new account skips every uid that a `supervisor` file of a live lease names.
- **The reset at start.** It keeps the accounts of adopted leases and deletes every other account the ledger names. The ledger's `created`/`deleted` records per lease (`ledger.rs`) are the account list, and the `pool` file goes. The ledger line is written before the account is made, so a crash between the two leaves a name the next reset deletes, never an unnamed account.
- **Privacy.** With Full Disk Access granted to the agent's program, the supervisor holds it too: it is the same program. The agent starts the supervisor, and the supervisor starts the job, without the disclaim, so the supervisor and the job answer for the agent and hold what it holds, Full Disk Access included. What keeps a job from the rest of the grant is the slot user it runs as and the Seatbelt profile applied in the job's child before the program runs. Section 4.2 gives the rule and the measurements, and `spawn_sites` holds every start that keeps the responsibility to a listed file.

## 2. Why there are two daemons today, and which reason holds

The recorded reason is privilege separation. The table row of `js-mac-session` in `docs/design/mac-runtime.md:25` says that the agent's process "holds no right to create users or kill other uids". That reason does not hold. Both daemons run as root: the agent's plist sets `UserName` `root` (`crates/js-mac-provision/tests/golden/agent.plist`), and the agent refuses native jobs unless it is root (`crates/js-agent/src/platform/macos.rs:411-416`). The helper was moved here from another product with its socket protocol unchanged (`docs/design.md:775`). No document compares it with the agent doing the work in-process. On Linux the agent is root and starts its jobs itself, with no helper (`docs/design/agent-privilege.md`, option B, the owner's ruling).

One function of the split is real. A Mac job outlives an agent stop, restart or update because the helper is the job's parent. The helper waits for the job and holds its deadline with no agent running (`crates/js-mac-session/src/helper/runs.rs:1-20`), and a restarted agent finds the run again through the helper's `watch` (`crates/js-agent/src/native/adopt.rs:1-15`). The helper's boot reset runs once per start of the helper (`helper/switch.rs:19-20`), so an agent restart today resets no pool user. Merging the two naively would end or orphan every running job at every agent update, and would run the reset at every agent start. Section 4 keeps both properties without a second daemon.

## 3. The platform layer

**No new trait.** The seam exists twice already, and each trait has two implementations:

- `trait Runtime` (`crates/js-agent/src/lifecycle/runtime.rs:389`) is what the lifecycle drives. `CrunRuntime` (`crates/js-agent/src/crun`) implements it on Linux and `NativeRuntime` (`crates/js-agent/src/native`) on macOS. It stays as it is.
- `trait Host` (`crates/js-mac-session/src/helper.rs:152`) is the helper's view of the operating system: users, launchd domains, processes, file flags. `MacHost` (`macos/host.rs`) implements it on a Mac and `FakeHost` (`src/testing.rs`) in tests on every platform. It stays as it is.

What changes is the line between the two. Today `NativeRuntime` holds a socket `Client` (`native/mod.rs:281-283`) and every pool operation is a request to another process. After the change it holds the helper's `Helper` value (renamed `Pool`; it owns a `Box<dyn Host>`, `helper.rs:375`) and calls its methods. `user_create`, `user_delete`, `kill_uid`, `boot_reset`, `pool_status` and `scan` become function calls with the same arguments. The fds the agent passes today by `SCM_RIGHTS` become `OwnedFd` arguments.

- **Only `Pool` is reachable from the agent.** The agent's code never holds the `Host` itself. Every destructive call stays keyed by a lease and resolves its uid from the ledger, inside the pool's range (`live_uid`, `helper.rs:534-548`). A bug elsewhere in the agent therefore cannot reach a uid outside the pool, as the helper's verb set ensures today.
- **Pool calls run on blocking threads.** They can sleep and retry (`Host::pause`), so they never run on the agent's async runtime.
- **The ledger lock is short.** It is never held across a supervisor's ready wait (section 4.2).

A third trait for "run a job on this host" would have one implementation per target, which R9 rules out, and `mac-runtime.md` section 1 already says no second trait is needed.

**What happens to each crate.** Line counts are `wc -l` of the files named, on main.

| Today | After |
|---|---|
| `js-mac-session` `helper.rs`, `helper/{pool, teardown, folders, ipc, leak, switch}`, `ledger*`, `lease.rs`, `limits.rs`, `sweep*`, `scan.rs`, `xattr.rs`, `spawn.rs`, `disclaim*`, `macos/host.rs`, `testing.rs` | Stay as library code, called by the agent in its own process. The crate keeps its name and its layer (`app`) and stops being a program. |
| `js-mac-session` `helper/runs.rs` (258), the in-memory run table and its waiter threads | Replaced by the supervisor (section 4) |
| `js-mac-session` `proto.rs` (569), `server.rs` (271), `client.rs` (551), `macos/caller.rs` (131), `start.rs`'s socket binding, `macos/mod.rs`'s `main`, `main.rs`; tests `proto/tests.rs` (521), `server/tests.rs` (1080), `tests/wire_golden.rs` (385) | Deleted: the protocol, the socket, the nonce, the code-signature check of the caller |
| `js-mac-session` verbs no code in this repository calls: `session-ready`, `session-login`, `session-idle`, `role-*`, `image-*`, and the `baseline` and `reboot-dirty` verbs (the leak scan's baseline itself stays, the agent uses it, `leak_scan.rs`); `macos/gui.rs` (`as-user`) | Deleted, if the owner agrees (section 8, Q2). Otherwise they move with the rest, and `as-user` becomes the agent's hidden mode `__as-user`. |
| `js-mac-session` the administrator lease: the MDM gate's admin grant (`grant.rs`), the grant keys (`Args::grant_keys`, `start.rs:63-69`), `user-create`'s `grant` argument, and the pool's "replace the slot's user by an administrator" path (`helper/pool.rs:18,35`) | Removed by a separate change, before this design's switch, and not carried into the agent. A native job on a Mac never gets administrator rights; a job that needs them runs in a VM (the `js-vmm` path, `crates/js-agent/src/vm`). So the one program needs no admin grant, no gate key and no verb for either. |
| `js-mac-session` `node-admin` (`node_admin.rs`, `phase0.rs`, `role_user.rs`, `macos/admin.rs`), which works on the earlier product's tree `/usr/local/kbf` and account `_kbf` | Deleted, if the owner agrees (Q3). Otherwise it becomes `js-agent node-admin`. |
| `js-agent` `native/bound.rs`, `native/adopt.rs`, `native/pool_user.rs`, `crates/js-agent/src/leak_scan.rs`'s client, the `old_helper` fallbacks | Rewritten for in-process calls and supervisors. The `old_helper` fallbacks go: there is no older helper to talk to. |
| `js-agent` flag `--native-helper-socket` | Removed (D6, section 13): `--native-accounts` is the switch now, and the other `--native-*` flags require it. |
| `js-agent` `update_hook/step/helper.rs` (95), `step/pf.rs` (87), `helper_unit`, `pf_unit` and `socket_group` in `params.rs` and `settings.rs`; tests `tests/agent/update_hook/steps_helper.rs` (296), `steps_pf.rs` (264), and their cases in `params.rs`, `validate.rs`, `steps_host_setup*.rs`, `fake_mac.rs` | Deleted (sections 5 and 6) |
| `js-mac-provision` the program (`main.rs` and the verb dispatch in `cli.rs`) | Deleted. The plist verbs move to `cargo xtask pkg` (CI renders the plist). `check` is what the agent already does at start. The flags-file reader of `install --flags-file` (`install_from`, `cli.rs:203`) and the constants the agent already imports from `cli.rs` (`DEFAULT_ANCHOR`, `DEFAULT_STATE_DIR`, `native/pf.rs:15`) move to a library module `flags.rs`, which the agent calls at its start. |
| `js-mac-provision` the library (`provision.rs`, `policy.rs`, `render.rs`, `pfctl.rs`, `daemon.rs`) | Stays as library code. The agent links the crate already, for the anchor check and its constants (`native/pf.rs:15-19`). |
| `js-contain` `oci/supervise.rs` (770) | Split. The platform-free core moves to a new crate, `js-supervise` (layer `runtime`, added to `layers.toml` in the same pull request; `js-contain`, `js-mac-session` and `js-agent` are at that layer or above, and `js-supervise` depends on none of them). The crun arm stays in `js-contain`. Section 4.2 has the details. |

**Pool users are permanent.** Measured on macOS 27: root cannot delete a pool user's account, its home or its per-user system folders under `/var/folders`. Root may empty them, and may write the account's shell, hidden flag and primary group and change its group membership. It may not write the account's home attribute. So the pool's users are made once and kept for good:
- A reset empties the home and the per-user folders in place. It never removes and recreates them; removing the home is the `EPERM` the first Mac installs hit.
- The pool's `replace` (delete the user and make it again with the same name and uid, `helper/pool.rs:27`) is not possible. It becomes setting the attributes root may write back to the pool's values and emptying the home again. A slot whose reset still is not clean stays broken, as a failed replace leaves it today.
- With no administrator lease there is no user to turn back from an administrator.

This is pool code that moves into the agent unchanged in shape. It is a change of the pool's reset that lands on its own, before or after the switch, with its tests against `FakeHost`.

**What stays pure and tested on Linux.** The Seatbelt profile builder and the Xcode catalog (`js-mac-pool`, layer `pure`) stay as they are. The pool's bookkeeping, teardown, reset, sweep and leak scan stay tested against `FakeHost` (`helper/tests.rs`, `helper/pool/tests.rs`, `sweep/tests.rs` and the rest of the in-crate tests). Those tests lose their socket and keep their assertions. The supervisor's core (the record, the handshake, the process identity's comparison) is tested on Linux in every pull request, and so are the macOS arm's decisions: the deadline, what a kill does, and the arguments. They are written as code that builds on every target, with the macOS system calls behind `Host`, so the Linux coverage job measures them. Only the thin system-call layer is macOS-only.

## 4. Jobs that outlive an agent restart

### 4.1 What Linux already does

On a Linux crun host, `Oci::start_supervised` (`crates/js-contain/src/oci/supervise.rs:594`) starts the agent's own binary in the mode `__supervise-run` (`oci/helper.rs:70`; the agent dispatches it first thing in `main`, `crates/js-agent/src/lib.rs:497-500`, under `cfg(target_os = "linux")`). The supervisor:

- leads a process group of its own (`.process_group(0)`, `supervise.rs:440`);
- blocks `SIGHUP`, `SIGINT` and `SIGTERM` (`supervise.rs:681-692`);
- drops to the job user before it runs anything (`JobDrop`, `oci/crun.rs`);
- makes its files and then writes a ready word (`supervising\n`), which the agent waits up to 30 s for (`READY_WAIT`);
- is the parent of `crun run`, captures its output and, when the run ends, writes `exit.json` whole (temporary file, sync, rename, directory sync).

The agent records `{pid, start time}` of the supervisor in the run's sidecar. A later agent adopts the run when the supervisor is still that process, or reads its exit record (`crates/js-agent/src/crun/adopt.rs`). The unit sets `KillMode=process` (`packaging/js-agent/js-agent-crun.service:40-48`), so systemd signals only the agent. On a build node each running job shows as `js-agent __supervise-run /usr/bin/crun ...` whose parent is systemd. An agent update there is a hand-over: the old agent exits leaving its runs, and the new one adopts them (`crates/js-agent/src/lifecycle/handover.rs`). A Linux run's wall limit holds only while an agent runs (the same unit file's comment).

### 4.2 The same mechanism on a Mac

A native run starts the same way, with a macOS arm where Linux has `crun`. Two things differ from Linux, both kept from the helper: the supervisor stays root and only the job drops to the pool user (D5), and the supervisor holds the run's deadline.

```text
launchd ─ js-agent (root)                         js-agent's launchd job, its process group
launchd ─ js-agent __supervise-native ... (root)  own process group; parent launchd once the agent is gone
            └─ sandbox-exec -p <profile> prog     the pool user (uid 600-699); own process group
```

**Shared code: the crate `js-supervise`.** It is moved out of `oci/supervise.rs` with its tests, so that a target without crun builds it.

- The exit record, version 1: `encode`, `decode`, `write_record`, `read_exit_record`, the bounds on what is read, and `GONE`.
- The handshake: reading the ready word from the supervisor's standard output within `READY_WAIT`, and killing and reaping a supervisor that fails it. The caller hands it a child the caller has already started. The core starts no process, so the spawn check (`crates/xtask/src/spawn_sites.rs`) is unchanged: on a Mac every start still goes through `disclaim::Program`.
- `SupervisorId {pid, start}` with `alive`, `runs_as` and `supervises`. On Linux they read `/proc/<pid>/stat` and `cmdline`, as today. On macOS they read `proc_pidinfo(PROC_PIDTBSDINFO)` (start time, zombie state, uid), which `MacHost` already calls (`macos/host.rs:495-520`), and `KERN_PROCARGS2` for the arguments.
- `Supervised`, the handle (`try_end`, `wait`, `force`), and the `supervisor` file.
- The signal mask the supervise mode sets before it starts any thread.

**The macOS arm, in `js-mac-session`.**

- **Mode.** `js-agent __supervise-native <state dir> <lease> <uid> <gid> <home> <deadline ms|-> <file cap> <processes> -- <argv>`. Everything comes in the arguments, nothing from the environment. It is a separate mode word from Linux's, so each arm parses only its own arguments (D2). `main` gains a macOS dispatch beside the Linux one.
- **Start.** The agent starts the supervisor through `disclaim::Program` as root, in a process group of its own, and keeps the responsibility (`keep_responsibility(Kept::Supervisor)`), so the supervisor answers for the agent. The supervisor starts the job the way the helper starts it today (`crates/js-mac-session/src/spawn.rs:122-155`): `Program` with `.uid()`/`.gid()` (std drops the ids in the child), `.process_group(0)`, the child steps (`fchdir` into the run directory, `setrlimit` for `RLIMIT_FSIZE` and `RLIMIT_NPROC`, close-on-exec), and the program executed with the responsibility kept (`keep_responsibility(Kept::Job)`): the standard library's exec, not the disclaiming `posix_spawn` with `POSIX_SPAWN_SETEXEC`, so the job answers for the agent too. The argv is the `sandbox-exec` line the agent builds today (`native/profile.rs`).
- **Output.** The run's standard output and error are files in its scratch directory, which only the agent may write (`native/mod.rs:14`). The supervisor opens them as root, without following a link, and gives them to the job. It writes the record's `stdout` and `stderr` with the files' sizes and empty tails. The native runtime reads tails and live logs from the files, as it does now (`native/logs.rs`).
- **Records.** A new directory per run, `<state root>/native/<lease>/`, root's and mode 0700, holds the exit record, the `supervisor` file and a lock file. The run's `exec` and `tmp` directories belong to the pool user (`native/mod.rs:982-986`), so a job that wrote its own `exit.json` there could make up how it ended. The supervisor runs as root, so the pool user cannot signal it either. Unlike Linux, where the agent writes the `supervisor` file after the handshake, the Mac supervisor writes its own `supervisor` file before its ready word. Every supervisor that may run a job can therefore be found from its lease's directory.
- **Deadline with no agent.** When the run's deadline passes (counted on the host's clock from the start) and the job still runs, the supervisor does what `kill-uid` does today: it kills every process of the uid and boots out its launchd domains, through the same `Host` methods (`helper/teardown.rs`). It then records how the job ended. That is the helper's behaviour today (`runs.rs:15-20`), kept (Q4).
  - Before it kills, it takes the lease's lock file and checks that the uid is in the pool's range.
  - The agent takes the same lock before any kill or reset of that lease's uid, so the two never kill or reset the same uid at once (D7).
- **End.** When the job's process ends, the supervisor writes the record and exits. An end by a signal is recorded as `{"signal": <n>}`, a new `end` kind (section 7, step 5, has the cross-version test).

**The agent's side.**

- **Starting.** `create_and_start` journals the lease (as today) and lends it a pool user. It then starts the supervisor, with no ledger lock held, and waits for its ready word. Last it writes the journal's `=` record with a new optional field, `supervisor: {pid, start}`. `RunRecord` (`native/journal.rs:36`) has no `deny_unknown_fields`, and the field is `#[serde(default)]`.
- **Polling.** `poll` reads the exit record.
- **Stopping.** `stop` sends `SIGTERM` to the job's process group, waits out the grace, then kills the uid, as the helper's `stop` does today.
- **Removing.** `remove` ends the supervisor (`force`, which waits until that process is gone), then kills the uid, collects outputs and resets the user. A pool user is lent again only after the reset, so a supervisor of an earlier lease can never kill the uid of a later one. The job leads its own group, so `force` kills only the supervisor. The uid's kill ends the job.
- **Adopting.** A new agent reads the journal. A run whose record names a supervisor that is still that process, or whose exit record is there, is adopted. One whose supervisor is gone with no record ended in failure, as on Linux (`GONE`).
- **The reset at start.** This is a change to `boot_reset`. Today its `held` list only decides whether the GUI auto-login lease is kept. Every other live lease is ended (`remove_leftover`), and every slot leased to a lease that is not live is reset (`reset_orphans`), whatever the list says (`helper/switch.rs:213-257`).
  - `boot_reset` gains a second list, the leases the agent adopted. It neither ends those leases nor resets their slots.
  - The agent adopts first and resets second, every time it starts.
  - A lease the journal lost, or one whose supervisor is gone without a record, is not adopted, so it is reset as before.
  - An agent that aborts in the middle of a teardown leaves the slot `resetting` in the ledger. The next start's reset finishes it (`reset_orphans`), as after a helper crash today.
- **A supervisor the journal does not know.** The agent can crash between the supervisor's start and the journal's `=` line. The next agent ends that lease as it ends any lease with no run. Before it does, it ends the supervisor that the lease's `supervisor` file names.

**One process at a time.** The ledger (`/var/db/js-mac-session/ledger` and `pool`) was written by one helper process. The agent opens it at that path, so no file moves at the switch, and holds an exclusive `flock` on `ledger.lock` beside it for its whole life (D3). The kernel releases the lock when the agent dies, so a crash leaves no stale lock. Supervisors never open the ledger. launchd runs one `js-agent` at a time anyway, and the lock makes a second agent started by hand fail at start instead of corrupting the pool.

**Supervisors of different releases.** A supervisor speaks to agents only through files (the exit record and the `supervisor` file) and signals. The record is versioned and later forms only add to it, so the next release's agent can adopt this release's supervisors. That is the Linux rule too.

**Privacy attribution.** `docs/design/spawn-disclaim.md` has the rule and the table of which start keeps the responsibility. The native runtime starts every native job through the supervisor (section 13), and the rule below is the one in force.

- `js-agent` holds Full Disk Access, by the privacy profile `packaging/js-agent-macos/mdm/js-agent-full-disk-access.mobileconfig`, which device management installs. The helper holds none.
- The agent starts the supervisor without the disclaim, and the supervisor starts the job without the disclaim. Both keep the responsibility, so they answer for the agent and hold what the agent holds, Full Disk Access included. Every process between the agent and the job is started so. A job's own children (compilers, shells, `buck2`) follow their parent and answer for the agent too.
- Why the job is not disclaimed. Measured on a Mac (macOS 27): a job started disclaimed is judged by its own code signature and is refused every read of tierstore's NFS loopback mount, where its build trees are served from. Apple's own tools (`ls`, `cat`, `python3`, the compilers) are refused too. The reads fail with "Operation not permitted", and the system log shows the network-volumes privacy service denying them. Started as ordinary children of the signed, profile-granted agent, the same reads as the same pool user succeed.
- What keeps a job from using the rest of the grant is the slot user it runs as (Unix permissions) and the Seatbelt profile applied in the job's child before the program runs. Measured on a Mac, the prototype's profile does not keep a job out of the places only the privacy controls protect: a job under it, counted as the agent, read the system privacy database's directory, the database itself and `/Library/Preferences/com.apple.TimeMachine.plist`, which the same job started disclaimed was refused; how that is closed is not decided (`docs/design/spawn-disclaim.md`, "The fence, measured").
- Every other program the agent starts stays disclaimed, which is the default of `disclaim::Program`: the host probes, `pfctl`, `launchctl`, `ioreg`, `getconf` as the lease user (so a slot-user process the agent starts for itself never carries the grant), the `sandbox-exec` self-test, and the agent re-executing itself in place.
- Not decided: the VM launcher (`js-vmm` lease) stays disclaimed for now. The options are disclaimed (it answers for itself by its own signature and entitlement, with no Full Disk Access) or kept (it answers for the agent). Also not decided: the GUI-session path (`launchctl asuser`, then `disclaim::exec`) stays disclaimed, because launchd starts it in the user's session, so keeping the responsibility there is not simply a flag, and it is not measured. A fact for that choice: after an unattended restart a Mac has no logged-in user, so a start that needs a user's graphical session cannot happen until someone logs in. Also not measured: what macOS does with the responsible process when the agent that started a supervisor exits, as in a restart or update while a job runs.
- `spawn_sites` holds every start in `js-agent` and `js-mac-session` to `disclaim::Program`, and a second rule holds a call of `keep_responsibility` or a use of `disclaim::Kept` to the files listed in `KEEPERS` (`crates/xtask/src/spawn_sites.rs`). `KEEPERS` lists the file that starts the supervisor and the file that starts the job: `crates/js-agent/src/mac_supervisor/launch.rs` and `run.rs` (section 12).

The design document is updated in step 5.

**GUI sessions.** The session verbs need nothing that one process cannot do. They write files, run `defaults` and `launchctl` as root, and `as-user` re-runs its own program under `launchctl asuser`, which the agent's binary can do as well. `launchctl reboot userspace` (`restart_userspace`, `autologin.rs:417`) and a reboot end every process, supervisors included. That ends the helper and its jobs today too.

### 4.3 What launchd does to the supervisor (a probe, not memory)

The question is whether launchd leaves a supervisor running when the agent's job stops, restarts or crashes.

**Apple's documentation.** `launchd.plist(5)`, read on macOS 26.6 with `man launchd.plist`:

> AbandonProcessGroup <boolean>
> When a job dies, launchd kills any remaining processes with the same process group ID as the job. Setting this key to true disables that behavior.

> ExitTimeOut <integer>
> The amount of time launchd waits between sending the SIGTERM signal and before sending a SIGKILL signal when the job is to be stopped.

So launchd sends `SIGTERM` to the job's own process, then `SIGKILL` after `ExitTimeOut` (120 s in the agent's plist), and then kills what is left in the job's process group. No page we found says launchd kills descendants that left that group.

**The probe.** It ran on a Mac with macOS 26.6, as an unprivileged user, in that user's `gui/<uid>` launchd domain. A `KeepAlive` job started three children: A in the job's process group, B in a group of its own (`setpgid`), and C in a session of its own (`setsid`). The job was then stopped three ways, each with and without `AbandonProcessGroup`:

| How the job ended | A (same group) | B (own group) | C (own session) |
|---|---|---|---|
| `launchctl bootout` | killed | runs on, parent pid 1 | runs on, parent pid 1 |
| `launchctl kickstart -k` (a restart) | killed | runs on, parent pid 1 | runs on, parent pid 1 |
| `kill -9` of the job, which `KeepAlive` restarts | killed | runs on, parent pid 1 | runs on, parent pid 1 |
| any of the three, with `AbandonProcessGroup` true | runs on | runs on | runs on |

In that domain a process group of its own is enough, and a supervisor already gets one from `process_group(0)`. The session does not matter, and no plist key is needed. The design leaves `AbandonProcessGroup` unset (D4): launchd should still clean up anything else the agent leaves in its own group, such as a `pfctl` it was running when it was stopped.

**Not proven yet: the system domain.** The agent runs in the `system` domain, as root, which needs root to probe. The same probe, written for the system domain and adding a child that drops to an unprivileged uid, is written as a script for an operator to run as root on a Mac with no agent installed; running it is step 1 of section 7, which records the result here. If the system domain kills a separate group, step 1 tries `AbandonProcessGroup` true there (which kept even same-group children in the user domain). If that fails too, the design goes back to the owner before step 4: the fallback is section 9's helper started by the agent.

**Who reaps and who signals.** Once the agent is gone, the supervisor's parent is `launchd` (pid 1), which reaps it when it exits, as the probe's `ps` shows. Root can signal any process, so the next agent (root) can end a supervisor. It names the supervisor by `{pid, start}`, so a reused pid is never signalled. The pool user cannot signal the supervisor (root) or the agent.

### 4.4 The alternatives, and why they lose

- **A launchd job per run** (a plist bootstrapped per lease). launchd would be the run's parent and would keep it through an agent restart. But a stop of the agent's job would no longer reach the runs, and each start would go through launchd's throttle (`ThrottleInterval` 30 s in the package's plists, `crates/js-mac-provision/src/daemon.rs:53`). There would also be one plist per run to write, remove and recover after a crash, and the run's end would come from `launchctl print` instead of an exit status. It also differs from Linux in every respect.
- **No supervisor: the job is the agent's child, found again by pid.** When the agent exits, the job is reparented to launchd, and nobody can collect its exit status: only a parent can `wait` for a child. The new agent could see that the pid is gone, but not how the job ended, so every adopted job would end as `Gone`. Nobody would hold the deadline while no agent runs, and a pid can be reused.
- **Keep the helper, in some form.** Section 9.

## 5. Updates with one launchd job

**Cutover.**

1. If the hook hands over (below), write the `handover` file in the state root.
2. Boot out `js-agent`.
3. Start `js-agent` from the staged release.

The new agent applies the pf anchor itself (section 6). It adopts the runs, keeping their users, and lists them in its first heartbeat. The `helper` and `pf` steps go, and so do the `helper_unit`, `pf_unit` and `socket_group` parameters.

**Rollback.** The same three steps with the releases swapped.

**Hand-over or drain.** `hands_over` (`prepare.rs:249`) today requires systemd and crun. It gains a second allowed case: launchd with the native runtime, when the agents of both the previous and the staged release list a new word, `native-handover`, on a `stops:` line of `--version`. The staged agent must adopt the runs, and the previous agent is the one a rollback starts. A macOS agent prints no `stops:` line today (`platform/macos.rs:31`; the Linux line is `platform/linux.rs:34`), and its stop handling is the lifecycle's shared code (`run.rs`, `lifecycle/handover.rs`), wired for every platform at `lib.rs:917`. Every other case drains, as every Mac does today. A drain cordons the host and waits for its jobs to end, so it never cuts one either.

**A running job is never cut by an agent update.** A job either outlives the update and is adopted, or the update waits for it. That covers each case:

- **The switch itself.** The previous release has a helper and no `native-handover`, so the switch drains. The helper's runs end before the helper does.
- **Each update after the switch.** The update hands over.
- **A rollback to a release from before the switch.** That agent lacks the word, so the rollback drains. It would otherwise ask a helper that does not exist and end the runs as `Gone`.
- **A crash or a stop by hand.** The supervisors stay, and the next agent adopts them, as on Linux.

**The two retired jobs.** The switch release's package no longer carries `js-agent.session.plist` or `js-agent.pf-pool.plist`. Its cutover boots out both jobs if launchd has them loaded and removes their two plist files from `/Library/LaunchDaemons`, so a reboot does not start a helper whose program is gone. A rollback to a two-daemon release bootstraps the helper and the pf job from that release's tree, as `helper::replace` and `pf::replace` do today, and that release's agent plist brings back `--native-helper-socket`.

## 6. The other two pieces

**`js-mac-provision`: folded in.**

- **The anchor.** Its one verb with an automated caller, `install --flags-file`, becomes a call the agent makes at its start: the flags-file reader (moved to the library, section 3) and then `Provisioner::install`. With `--native-network on`, the agent makes it before the check it already runs at start and at each job's admission (`pf_in_force`, `native/mod.rs:596-620`, called at 844 and 856).
- **The plists.** `agent-plist`, `session-plist` and `pf-pool-plist`, used only by the package workflow (`.github/workflows/js-agent-macos-pkg.yml`), move to `cargo xtask pkg`.
- **Operator verbs.** `check`, `remove`, `render` and `probe` have no automated caller. A check command on a host is what B14 rules out. The root test `crates/js-mac-provision/tests/pf_as_root.rs` runs the program, through `js-mac-provision-privileged-tests.yml`, so it moves to calling the library.

**`js-agent.pf-pool`: folded in.** The job exists for three reasons (`crates/js-mac-provision/src/launchd.rs:1-6`, `crates/js-mac-provision/README.md`): pf rules are gone after a reboot, an OS update may reload `/etc/pf.conf`, and an install that runs before the system's own pf start fails once (`NotReferenced`) and succeeds on the next try. The agent covers all three:

- it loads the anchor at its start, and at boot it starts after the flags file exists;
- it loads the anchor again every 30 s, the job's interval, on a thread of its own;
- it retries a first `NotReferenced` failure on the next tick, and refuses jobs until the anchor is in force.

The pf enable token is kept in a file (`<state dir>/pf-token`), not held by a process, so the agent stopping does not release it. While the agent is down (an update, a crash and launchd's restart), the anchor stays in the kernel, so the surviving runs stay fenced. The only change: an OS reload of `pf.conf` while no agent runs is repaired when the agent starts, instead of within 30 s. Nothing found forces the job to stay.

## 7. The way there

Each step is a pull request that leaves main shippable. Steps 2 and 3 depend on nothing and can land in either order. Step 4 waits for steps 1 and 3, step 5 for steps 2 and 4, and step 6 for step 5. Each step says what is reverted if it fails on a Mac.

**Step 1. The system-domain probe and a supervisor probe on the hosted runner.**

- Run the root probe (section 4.3) and record its result in this document.
- Add a probe to `js-mac-probe` that runs unprivileged on the beta stage's macOS runner. It starts a process group under a `launchd` job, boots the job out, and asserts that the group runs on with parent pid 1. It catches a macOS release that starts killing the descendants of a stopped job.

**Step 2. The pf job folded into the agent.**

- The agent applies the anchor at start and every 30 s.
- The package drops `js-agent.pf-pool.plist`, and the hook drops `step/pf.rs` and `pf_unit`, retiring the loaded job as section 5 says.
- Tests that change: `steps_pf.rs` goes. The start tests in `crates/js-agent/tests/agent/native/pf_gate.rs` gain "the agent loads the anchor before it checks it" and "a first refusal is retried on the next tick". `crates/xtask/tests/macos_pkg.rs` holds the payload with two plists.
- Reverted by a rollback, which bootstraps the pf job from the previous tree.

**Step 3. `js-supervise`, a refactor with no change in behaviour.**

- Move the core out of `oci/supervise.rs` and add the crate to `layers.toml`.
- Linux behaviour does not change. `supervise_tests.rs` and `crates/js-contain/tests/supervised.rs` pass unchanged, and the coverage records in `coverage-baseline` move with the code to the new crate.
- The macOS `SupervisorId` (from `proc_pidinfo`) gets its tests in the beta stage on macOS. On Linux, the comparison logic is tested.

**Step 4. The macOS supervise arm, in the binary but not started yet.**

- `__supervise-native` in the agent's binary, dispatched on macOS. The per-lease lock and the pool's range check it uses are functions of `js-mac-session` (beside `Pool`), so the agent and the supervisor call the same code.
- Tests on Linux against `FakeHost`:
  - a deadline that passes kills the uid and records the end;
  - a job that ends before its deadline is never killed;
  - a deadline kill waits for the lease's lock and refuses a uid outside the pool's range;
  - the `supervisor` file is written before the ready word;
  - arguments that do not parse exit 2 with no ready word;
  - a record that cannot be written exits 1.
- On the beta stage's macOS runner, the supervise mode runs a program as the runner's own user and records how it ended.

**Step 5. The switch.** This step cannot be split without a release in which two processes write the ledger. It deletes 3,508 lines (the sum of the `wc -l` counts in section 3 for `proto.rs`, `server.rs`, `client.rs`, `caller.rs` and their tests, measured), plus a rewrite whose size is not estimated here of `native/{bound, adopt, pool_user, mod}`.

- `NativeRuntime` calls the pool in-process and starts every run through a supervisor. Adoption reads exit records, and `boot_reset` gains the adopted list. The agent locks the ledger.
- The package drops `js-mac-session` and the helper's plist, the cutover retires `js-agent.session`, and the agent's plist drops `--native-helper-socket`.
- The socket, protocol, client and caller check are deleted. The hand-over gate does not change yet, so this release's update still drains.
- Workflows that change: `js-agent-macos-pkg.yml` (one program and one plist, no cdhash), `js-agent-native-privileged-tests.yml`, `js-mac-provision-privileged-tests.yml`.
- Documents updated in the same pull request: `docs/design/mac-runtime.md` (the helper's row), `docs/design/spawn-disclaim.md`, `docs/design/mac-lease-teardown.md`, `docs/design/mac-hostkeeper-package.md`, `docs/design.md`, `packaging/js-agent-macos/README.md`.
- `crates/js-agent/tests/agent/native/*` lose `fake_helper.rs` (772 lines, the socket stand-in, also used by `runtime.rs`, `live_logs.rs`, `checks.rs`, `bundle.rs` and `jobs/fake.rs`) and run the runtime over a `Pool` with a `FakeHost`.
- Tests that are kept, with new bodies:
  - `adopt.rs`: `a_restarted_runtime_adopts_a_live_run_and_reports_its_exit`, `a_run_the_helper_no_longer_has_is_listed_ended` (renamed for a supervisor gone with no record);
  - `bound.rs`: `the_helper_ends_a_run_at_its_deadline_with_no_agent`, now the supervisor;
  - `journal.rs`: `a_run_an_earlier_process_left_is_ended_when_the_next_runtime_reclaims`.
- New tests:
  - a restart keeps the users of adopted leases and resets the rest;
  - a lease the journal lost is reset;
  - a supervisor whose `=` line was never written is ended before its lease;
  - a second agent fails to start while the first holds the ledger;
  - the previous release's journal reader reads a journal with `supervisor` fields;
  - an exit record with a `signal` end is read by this release's reader, and a record without one by the next.
- **The real-macOS restart test lands here, before any Mac takes the switch.** It runs on the beta stage's macOS runner, as root through `sudo` (the beta runner already runs `sudo` without a password, `beta.yml:64`; step 1's probe confirms there that a root job can bootstrap and boot out a LaunchDaemon), as a job of the beta stage whose failure holds back `promote-macos`. It never runs in a pull request, so `pr_linux_only` holds. The test:
  1. installs the package;
  2. starts a native job that runs for longer than the test takes to restart the agent;
  3. boots out `js-agent` and bootstraps it again;
  4. asserts that the job's processes ran on throughout, that the new agent lists the run in its first heartbeat, and that the job's exit is reported once.

  No test today kills a real agent process while a real job runs: on Linux, adoption is tested by opening a second runtime in the same process.
- Reverted by a rollback, which drains and brings the helper back from the previous tree.

**Step 6. Hand-over on a Mac.**

- A `stops: native-handover` line in the macOS `--version`, and the launchd-plus-native case in `hands_over`.
- `tests/agent/update_hook/steps_handover.rs` gains the launchd cases:
  - a Mac cutover hands over when both agents list the word;
  - it drains when the previous agent does not;
  - a rollback to an agent without the word drains.
- The real-macOS test of step 5 gains a second case: an update to the next build with hand-over, asserting the same.

**Step 7. If the owner agrees (Q2, Q3).** Delete the uncalled verbs and `node-admin`. Each deletion is a separate pull request that names what it supersedes and updates the documents that describe them (`docs/design/mac-lease-teardown.md`, `packaging/js-agent-macos/README.md`'s `as-user`).

**What the Macs in service need at the switch.** Nothing by hand:

- The update that brings step 5 drains each Mac, as every Mac update does today.
- Its cutover retires the helper's job and plist.
- The new agent opens the same ledger at the same path, and the pool users (600-699) and their homes stay.
- The group `_jsagent` and `/var/db/js-mac-session` stay. The second holds the ledger. `/var/run/js-mac-session` is gone at the next reboot.

The fixes of komira-ai/jobscheduler#988 (open), which make the first install and the launchd states work on a Mac, must be released before step 5 reaches them.

## 8. Questions for the owner

Each question has a recommendation and can be answered yes or no.

- The administrator lease is not a question here: native jobs never get administrator rights, and a job that needs them runs in a VM (the owner's ruling).
- **Q1.** One program on a Mac, as described: the agent does the helper's work in-process, and a root supervisor per run keeps the run alive across agent restarts and updates. **Recommended: yes.**
- **Q2.** Delete the helper verbs that no code in this repository calls (the GUI-session switch `session-*` with `as-user` and the auto-login writer, the VM user's `role-*`, and `image-*`) instead of moving them into the agent. The VM path already starts `js-vmm` itself (`crates/js-agent/src/vm/helper.rs`). **Recommended: yes.** They can come back from history when a caller arrives.
- **Q3.** Delete `node-admin` (account `_kbf`, `/usr/local/kbf`, run through `sudo`) instead of making it `js-agent node-admin`. It manages the earlier product's tree, and nothing in the repository's packaging, runbooks or workflows uses it. **Recommended: yes.**
- **Q4.** Keep the Mac rule that a run's deadline is held by its supervisor while no agent runs (the helper holds it today), and leave Linux as it is, where the limit holds only while an agent runs. **Recommended: yes.** Making Linux match would be a separate change.
- **Q5.** Accept that the switch release, and a rollback across it, drain each Mac once, and that hand-over starts with the following release. **Recommended: yes.** The only alternative is teaching the new agent to adopt the old helper's runs, which would mean keeping the helper's protocol for one release.

## 9. The case for keeping the helper, answered

The arguments for keeping the helper, strongest first, with the answers:

1. **The agent's restart would reset pool users.** Strong: the reset runs once per start of the helper today, and a naive merge would run it at every agent start. The answer is section 4.2's reset: `boot_reset` gains the adopted list, the agent adopts before it resets, and step 5 tests a restart, a lost journal line and a supervisor the journal never named.
2. **Many root supervisors against one ledger: a supervisor's deadline kill could hit a uid that was already reset and lent to another lease, or race the agent's own kill.** Strong. The answer has three parts. The agent's `remove` waits until the supervisor is gone before it resets the uid. A supervisor kills only under its lease's lock, which the agent also takes for any kill or reset of that uid. And a supervisor writes its own `supervisor` file before it can run a job, so the next agent finds every one (D7).
3. **The design rests on launchd behaviour proven only in a user domain.** Strong. Step 4 waits for the root probe of step 1, and section 4.3 says what happens if it fails.
4. **The switch would ship before the test that proves it.** Strong for the order. The real-macOS restart test now lands in step 5 and holds back `promote-macos`.
5. **The verb set is a validation layer against bugs.** Medium. The agent holds only `Pool`, whose destructive calls are keyed by a lease and resolve its uid from the ledger inside the pool's range (section 3). It never holds the `Host` itself.
6. **Keep the helper, but start it from the agent: one launchd job, one program, a `__helper` mode, a socket pair.** Medium. It removes the plists, the hook steps, the group and the cdhash pin. It loses on the point that matters:
   - The helper keeps its runs in memory, and its boot reset ends every lease. So the helper itself cannot be updated with jobs running: each agent update that replaces it either drains or ends the jobs.
   - A helper that is not replaced runs an older release beside a newer agent, and a new agent must find it again through a named socket and check who it is. That keeps the protocol, the client and their tests.
   - The supervisor removes both problems with less code, because Linux already has it. This option is the fallback only if the root probe fails.
7. **Blocking pool calls inside an async agent.** Medium. Pool calls run on blocking threads, and the ledger lock is not held across a supervisor's ready wait (section 3, section 4.2).
8. **Privacy grants.** Weak to medium. The agent holds Full Disk Access, and the supervisor and the job, started without the disclaim, hold what it holds. The slot user and the Seatbelt profile are what keep a job from the rest of the grant, and the prototype's profile, measured, leaves three places only the privacy controls protect readable to a job (section 4.2; not decided how that is closed). `spawn_sites` fails any start that keeps the responsibility from a file not in `KEEPERS` (section 4.2).
9. **Crash isolation.** Weak. Today a helper crash ends every job (its table is in memory and its reset ends every lease, `runs.rs:6-10`). After the change, an agent crash ends none. A crash mid-teardown leaves a `resetting` slot that the next start finishes.

## 10. The owner's answers

The owner answered yes to all five questions of section 8:

- **Q1.** Yes: one program on a Mac.
- **Q2.** Yes: delete the uncalled helper verbs.
- **Q3.** Yes: delete `node-admin`.
- **Q4.** Yes: keep the Mac rule that the supervisor holds a run's deadline while no agent runs, and leave Linux as it is.
- **Q5.** Yes: accept one drain per Mac at the switch and across a rollback.

## 11. Built: the slot accounts

`crates/js-agent/src/mac_accounts` is library code of `js-agent`: no helper program, no socket. It makes and deletes the accounts native jobs are to run as, one per job, on *slots*. A slot is a fixed uid and a fixed `GeneratedUID` derived from it, so the slot keeps one per-user folder in `/private/var/folders` for good. For each job the module makes a fresh record (a new name, `kbf-acct-<n>`, never used again) and a fresh home. After the job it kills everything of the uid, deletes the record and the home, empties the slot's per-user folder and keeps it, and shows the account gone before the slot is free again. An fsynced, append-only ledger in the agent's state records every account (`made`, `ready`, `lent`, `held`, `gone`). The accounts are hidden, have shell `/usr/bin/false`, no password and the group `staff`, and are never administrators.

- **Settings.** The slots (`Slots`, `<first>-<last>` or a first uid and a count) and the number of accounts made ahead of a job (`Settings::ready`) are settings of the agent and have no default. Each ready account holds one uid of the range while it waits. The range must not overlap the uids of the earlier release's reused accounts. Nothing of users or slots reaches the wire, the hello or the server.
- **A restart of the agent.** `Pool::open` takes the jobs the restarted agent adopted. The account lent to one of them is kept as it is, with its uid still held, and `Pool::retire` ends it when its job is over. Every other lent account belongs to a job that is over, and is deleted. An adopted job's account whose record or home is gone is deleted, not repaired.
- **A process nobody holds.** A slot that no ledger account holds and that no record has may still run a per-user daemon macOS started by itself (`distnoted`). The check that a slot is free ends such processes with the same kill rounds a retirement uses, without asking launchd. Only a process that survives them keeps the slot busy. A failed making runs the kill rounds before it deletes what was made. A slot the ledger holds is never killed by this check.
- **launchd.** Asking launchd is not reading it: `launchctl print user/<uid>` makes the user domain of any uid it names, and launchd then retries that domain's per-user services every few seconds, for ever for a uid with no account. So the module asks launchd only about the uid of a record it made and has not deleted, boots out `gui/<uid>` and `user/<uid>` while the record is there, prints only `gui/<uid>`, and asks nothing after the deletion. Kill rounds for a uid with no record judge by its processes alone.
- **A record that is gone.** The proof that a deleted record is gone, and the check that a slot is free, ask the directory node the record was deleted from. `getpw*`, which answers through the system's lookup cache, is asked as well. A lookup of its that fails still fails the proof, but one that still answers for the deleted record does not.
- **A development verb.** `js-agent __account-cycle` runs one account's life cycle on a Mac, to prove it with the signed agent that holds Full Disk Access. It runs no job, and it goes once the native runtime proves the cycle itself.

## 12. Built: the supervisor of one native job

`crates/js-agent/src/mac_supervisor` is the start of one native job in a process of its own: the hidden verb `js-agent __supervise-native --spec <file>`, answered before the agent's signal handlers and before any thread, with one thread for its whole life. The agent cannot apply a Seatbelt profile, or drop to a user, between `fork` and `exec` (the library allocates, and the agent has other threads), so it starts this verb instead; the verb is `js-agent` itself, not a helper.

- **Its steps,** in order: it reads its spec (a root-only file the agent writes in the run's record directory: the slot's uid and gid, the profile's text and parameters, the program and its arguments, the file-size and process limits) and removes it before parsing it; makes `ready.json` and `exit.json` there and keeps them open; opens a new audit session whose audit user is the slot uid; drops to the slot uid with its gid as its only group, and checks root cannot be regained; sets the two limits, soft and hard; applies the profile; starts the job without the disclaim, in a process group of its own, with its own environment, working directory and streams; writes the ready record (the job's pid and the audit session); waits for the job; writes the exit record. A step that fails ends the start there, with the step in the exit record and status 125; the job is never started after it.
- **It drops itself.** Unlike D5 below, the supervisor drops to the slot user before it starts the job, so it runs under the job's profile and as the job's user, and the uid's sweep can end it. It writes its records through the files it opened as root before the drop, never by path. It holds no deadline: the lifecycle's wall limit stops the job through the runtime.
- **The agent's side.** `SystemLauncher` writes the spec and starts the verb, keeping the responsibility, with the job's whole environment, working directory and streams; its `Supervised` waits for the ready record and reads how the run ended from the exit record, or from the supervisor's own status when it wrote none. The module's own documentation gives the records, the statuses and the interfaces.
- **Its caller** is the native runtime's start of a job (section 13).

## 13. Built: native runs over the slot accounts

`NativeRuntime` (`crates/js-agent/src/native`) runs each native job as an account of the pool of section 11 and starts it through the supervisor `js-agent __supervise-native`, with no helper program. Where this departs from sections 3 to 7, this section is what the code does.

- **Flags.** `--native-helper-socket` is gone (D6). `--native-accounts <dir>` (the pool's state directory) turns native jobs on and is required unless `--no-jobs`; `--native-account-range <first>-<last>` gives the slots and has no default; `--native-account-ready <n>` is the number of accounts made ahead (default 1, at most the number of slots). They belong in the host's flags file, not in the agent's plist. `--native-homes` takes only `/Users`, where the pool makes its homes.
- **Full Disk Access.** The pool is opened only when the agent's listing at start says the grant is held. Without it the agent opens no pool, reports `full-disk-access: not-granted` (or `undetermined`) in its hello, and refuses every native job `no-full-disk-access`.
- **A start**, in the order `start_steps` names the steps: `checked`, `bundle-pinned` (and the lease written to the journal), `account-lent` (`Pool::lend`, the job name being the lease with `.` replaced by `-`), `folder-made` (`<home>/exec` with `.home` and `.tmp`, the account's, made by root before any process of the account exists), `inputs-linked` (run files, mount inputs, job mounts, the working directory, a bundle's writable trees), `profile-built`, `supervisor-started` and `job-ready`. A failure at any step after the account is lent retires the account and marks the lease done; an account that cannot be retired is held by the pool, its lease stays in the journal, and the start's error says so.
- **The profile.** Writes are granted below `exec/` and to the account's own per-user temporary and cache folders (what macOS gives the account as `DARWIN_USER_TEMP_DIR` and `DARWIN_USER_CACHE_DIR`, asked of the system as the account at the start, each checked to be a directory below `/private/var/folders` that the account's uid owns, and granted by its own path, never `/var/folders` as a whole); they belong to the one slot, no other job reaches them, and the retirement empties them. Nothing else below `/var/folders` is writable. The agent's roots, its secret files, `--native-accounts`, `/etc/kcpassword` and the places only the privacy controls guard (`native/fence.rs`: the privacy database's directory, the database, Time Machine's preferences, each under `/Library` and `/System/Volumes/Data/Library`, measured in `spawn-disclaim.md`) are denied reads, and a mount in one of those places is refused.
- **The environment.** The supervisor sets nothing, so the runtime puts `HOME` and `CFFIXED_USER_HOME` (`exec/.home`), `USER` and `LOGNAME` (the account's name) and `xcrun_db` (`exec/.tmp/xcrun_db`, so `xcrun` keeps its cache in the run directory) last in the run's environment, and refuses a job or a body that sets one (`invalid-env`).
- **Limits.** The supervisor sets `RLIMIT_FSIZE` to `--native-max-file-mib` and `RLIMIT_NPROC` to `--native-max-processes` and one: the supervisor waits on the job as the account and counts among its processes. No memory limit is set.
- **Stop and removal.** A stop sends `SIGTERM` to the job's process group and notes when its grace ends (a second stop moves nothing). The first poll after the grace that finds the job running sweeps the run's processes (`mac_sweep`: in rounds, every process in the job's audit session whatever its uid, and every process whose effective or real uid is the slot's; never pid 0 or 1, the agent, or another uid outside the session), and every later poll sweeps again until a sweep leaves none; only then is the end read, so a job the sweep ended reads as its killed supervisor's 137. The lifecycle holds the wall limit; no other process holds a deadline. A removal sweeps first (so nothing writes into `exec/` while it is read), collects the declared outputs into the run's scratch directory, retires the account and marks the lease done; a sweep that is not clean does not stop it, and a retirement that fails is a failed removal.
- **A restart of the agent** ends every native run. `Pool::open` is always given an empty adopted set, so every account an earlier process lent is retired at the open; the journal's runs are listed ended (`Gone`) and the leases with no run are dropped. Section 4's adoption of running supervisors and section 5's hand-over are not built.
- **Checks.** A Mac agent runs no post-job leak scan (`NoChecks`): the helper's scan needs the helper's recorded baseline, which nothing records now; the retirement's proof covers everything of the run's uid.
- **The self-test.** With the pool open, the agent runs `/usr/bin/true` through every step, its end and its removal before it says hello, and does not start when that fails.

- **Trees.** A provider's tree reaches the job as a read-only link `exec/<rest>` to the directory the provider answered below `--native-trees-root` (tierstore's one NFS mount of the Mac). The profile denies reads, execution and executable mapping below that root and grants back the reads of the run's own trees only (js-mac-pool `MountTrees`), so the trees root is the one mount root that is not a read deny. A mount below the root must be the run's own tree, read-only and `no_exec`, or the start is refused (`native_fs::tree_rules`). The agent never unmounts the root; the lifecycle releases the trees after the run.


Not built yet: adoption across an agent update, the retirement of `js-mac-session`'s pool and the helper program.

## 14. Built: a run's output committed to the put ingest

After a native run, the agent itself puts the run's output in tierstore's store through the node's put ingest (`crates/js-agent/src/mac_put`, `native/put.rs`). The job never holds a writable put directory.

- **Flags.** `--native-put-root <path>` names the put ingest (tierstore's put root) and `--native-put-user <user>` the user the node runs as, by name or by number (a number is its uid and its group); they are given together or not at all. The root is no mount root: the lifecycle leaves a mount source beneath it to the native runtime (`Runtime::owns_host_path`) instead of holding it to the mount roots, and the runtime makes the root a read deny of every run's profile, so no run reads it. (A mount root on the state root's file system stops the agent at start, and no job writes to the put root.)
- **What is committed.** The build service sends a read-write mount of `<put root>/cas/b/<build>/<nonce>` at `/work/<rest>` (`/work/.kbf-out`) and declares no output. That mount is the run's put target, whether or not its nonce folder exists yet (the build service makes none; the tree check never looks a put target up): it is never linked into `exec/` and never granted in the profile, and any other mount beneath the put root, or one that holds it, is refused (`input-not-allowed`), its links followed. It gives the run one output, the regular file `<rest>/pack` in its folder, which the job writes itself. The removal copies it out with the declared outputs (no link followed, within the run's caps) and commits that copy as it is: no tar, no other wrapping. The key is the put directory's path below the root and `/pack`.
- **The three names.** In the put directory the agent writes `pack.part` (synced), renames it to `pack.sha256-<64 hex>.part`, which declares the digest, and renames that to `pack`, which the node takes: it stores the bytes under the key when they have the declared digest, then removes the file and the empty directory. `pack` is mode 0640.
- **Who owns what.** Before anything is written, `cas`, `cas/b` and `cas/b/<build>` are given to the node's user and group; the job's directory `cas/b/<build>/<nonce>` stays root's with the node's group, mode 0770; `pack` is root's, of that group. The node can read the file and remove it and the directory; no other user can enter. This hand-over is the one call that needs root; its kept test runs as root in the beta stage (`mac_put::root_tests`).
- **When.** In the removal, after the run's processes are ended and its outputs copied, and before its account is retired, once: a removal tried again (an account the pool held) does not commit again.
- **A commit that fails** (a put directory that already holds something, a failed write or rename) leaves no `pack` and removes the `.part` names it made. It fails the removal only as the run's outputs (the finding `outputs-failed`, `outputs not committed: <why>`): the run's exit is unchanged and its account is retired. A copy with failures commits nothing (a pack missing declared outputs under a key is worse than none), and so does a run with no put target or whose job wrote no pack, without failing.
- **The report.** What was committed is the finding `outputs-committed`, a JSON object with the fields `key`, `sha256` and `bytes`, made once the removal is done (`Runtime::outputs_report`). The run's result is built at its exit, before the removal, so the result carries no key; a result field comes with the build service's change for declared outputs.

## Decisions

- **D1.** No new platform trait: `Runtime` above and `Host` below already have two implementations each (R9).
- **D2.** The Mac arm gets its own mode word (`__supervise-native`) instead of growing Linux's argument list: the two arms take different arguments, and each parses only its own.
- **D3.** The agent locks the ledger for its whole life and keeps it at its current path, so the switch moves no file.
- **D4.** `AbandonProcessGroup` stays unset unless the root probe shows the system domain needs it. The supervisor's own process group is what the user-domain probe shows it needs, and launchd keeps cleaning up the agent's own leftovers.
- **D5.** (Superseded by section 12: the built supervisor drops itself.) The supervisor runs as root and only its child drops to the pool user, so that it can hold the deadline (killing a uid and booting out its domains needs root) and so that the pool user cannot signal it or write its record.
- **D6.** `--native-helper-socket` is removed in the switch release, not kept as an ignored flag. Only the package's own agent plist passes it, and that plist is replaced in the same cutover. A rollback restores the previous plist with the previous program.
- **D7.** A per-lease lock file in `<state root>/native/<lease>/` orders the supervisor's deadline kill against the agent's kill and reset of the same uid. The ledger keeps one writer, the agent.

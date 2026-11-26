# Starting programs on macOS: disclaimed by default, kept for a job

How `js-agent` and `js-mac-session` start every program on macOS, in two kinds of start, why each is built the way it is, and how the rule is kept. A start disclaims the privacy (TCC) responsibility by default. A native job and its supervisor are started without the disclaim: they keep the responsibility. The code is `crates/js-mac-session/src/disclaim.rs` and its modules (`plan.rs`, `darwin.rs`); the check is `crates/xtask/src/spawn_sites.rs`.

## The problem

macOS's privacy controls (TCC: Full Disk Access, the camera, other apps' data) decide what a process may do from its *responsible* process. A process started the usual way, by `fork` and `exec` or by `posix_spawn`, answers for the process that started it, and so on up to a process that answers for itself: one that launchd started as a job of its own, or one whose starter disclaimed the responsibility when it started it (`responsibility_spawnattrs_setdisclaim` on the `posix_spawn` attributes).

`js-agent` is a launchd daemon and answers for itself. A device-management privacy profile gives it Full Disk Access. Two rules follow, one for each kind of start.

**A job keeps the responsibility.** A native job and its supervisor are started without the disclaim, so they answer for `js-agent` and hold what it holds, Full Disk Access included. Every process between `js-agent` and the job is started so, and a job's own children (compilers, shells, buck2) follow their parent and answer for the agent too. The reason is measured. A job started disclaimed is judged by its own code signature, and it is refused every read of the network file system (NFS) loopback mount that tierstore serves its build trees from; Apple's own tools (`ls`, `cat`, `python3`, compilers) are refused too. On a Mac (macOS 27), disclaimed, the reads fail with "Operation not permitted" and the system log shows the network-volumes privacy service denying them. Started as ordinary children of the signed, profile-granted `js-agent`, the same reads as the same pool user succeed.

What keeps a job from using the rest of the grant is the slot user it runs as (Unix permissions) and the Seatbelt profile applied in the job's child before the program runs. Measured on a Mac, the prototype's profile did not keep a job out of the places only TCC protects (komira-ai/jobscheduler#1025); the profile now read-denies them, and denies links and clones from them; see "The fence, measured" and "The fence, as built" below.

**Every other program is disclaimed.** Host probes, `pfctl`, `launchctl`, `ioreg` and the rest answer for themselves, so a process the agent starts for its own use never carries the grant. Forgetting to choose gives the default, disclaimed: it can cost a program a read it needed, and never gives one a grant it should not have.

The table below says which start is which.

The native runtime (`crates/js-agent/src/native`) starts every native job through the supervisor (`js-agent __supervise-native`, `crates/js-agent/src/mac_supervisor`), which is the code that keeps the responsibility: the agent's launcher (`launch.rs`) starts it with `Kept::Supervisor`, and it starts the job with `Kept::Job` (`run.rs`). The root helper `js-mac-session` starts no native job of the agent's any more.

### Which program keeps its responsibility

| Program | Started by | Responsibility |
| --- | --- | --- |
| A native job | its supervisor | kept |
| The supervisor of a native job | `js-agent` | kept |
| A job's own children | the job | follow their parent (answer for the agent) |
| Host probes (`sw_vers`, `sysctl`), `pfctl`, `launchctl`, `ioreg` | `js-agent` | disclaimed |
| `getconf` as the lease user | `js-agent` | disclaimed, so a slot-user process the agent starts for itself never carries the grant |
| The `sandbox-exec` self-test (`native/profile.rs`) | `js-agent` | disclaimed; goes away with the helper path |
| The agent re-executing itself in place (`in_place.rs`) | `js-agent` | unchanged: the same process |
| The VM launcher (`vm/helper.rs`, the `js-vmm` lease) | `js-agent` | not decided; stays disclaimed. Disclaimed, it answers for itself by its own signature and entitlement and has no Full Disk Access; kept, it answers for the agent |
| The GUI-session path (`macos/gui.rs`: `launchctl asuser`, then `disclaim::exec`) | `js-agent` | not decided, not measured; stays disclaimed. launchd starts it in the user's session, so keeping it is not simply a flag. After an unattended restart a Mac has no logged-in user, so a start that needs a user's graphical session cannot happen until someone logs in. |

Not measured: what macOS does with the responsible process when the agent that started a supervisor exits (a restart or an update while a job runs). Nothing is claimed about it.

### The fence, measured

With the disclaim gone, a job holds the agent's Full Disk Access, so what Unix permissions leave readable to the slot user and only TCC protects is readable to the job unless the Seatbelt profile refuses it. This section is a measurement and is kept as history: it measured the prototype's profile, before the fence below was built, and not the profile `js-agent` applies now. A proof run on a Mac (macOS 27, Apple silicon, the prototype's `confined-writes` profile, which allows reads by default and denies writes) ran the same probe as four processes: root as the agent's ordinary child; a pool user as the agent's ordinary child with no profile; a pool-user job started disclaimed under the profile; and a pool-user job started keeping the responsibility under the profile, the shape this document describes. The processes' responsible pids were read back: the disclaimed job answered for itself, the other three for the agent.

| Place | Unix mode | Pool user, no profile, counted as the agent | Job started disclaimed | Job under the prototype's profile, counted as the agent |
| --- | --- | --- | --- | --- |
| `/Library/Application Support/com.apple.TCC` (list) | 0755 root | read | refused (`Operation not permitted`) | **read** |
| `/Library/Application Support/com.apple.TCC/TCC.db` (first bytes) | 0644 root | read | refused | **read** |
| `/Library/Preferences/com.apple.TimeMachine.plist` | 0644 root | read | refused | **read** |
| another user's home folders (`Documents`, `Desktop`, `Library/Mail`, `Library/Messages`, `Library/Safari`), root's home, another user's 0600 file | 0700, 0750, 0600 | refused (`Permission denied`) | refused | refused |

So the grant reached a job for three places, and the prototype's profile did not stop it (komira-ai/jobscheduler#1025). Unix permissions alone keep a job out of other users' files and root's home, as Full Disk Access never overrides them. The three places are the ones probed, not a complete list: Apple publishes no list of the places only TCC protects, and it changes between macOS releases.

What a job learns from each, from public descriptions of these files (not measured past the first bytes; Apple does not document the formats):

- the directory listing: the names, sizes and times of the system privacy control's files;
- `TCC.db`: the system privacy database, believed readable whole (mode 0644): which programs, by path or bundle identifier with their code requirements, hold or were refused machine-wide privacy services such as Full Disk Access, screen capture and accessibility, and when each row last changed; the agent's own row is among them. No credential is known to be in it, and reading it changes no grant (the profile denies every write);
- `com.apple.TimeMachine.plist`: whether backups are on, where they go (a disk, or a network server and share), the Mac's identifiers, the backup volume's name, and the paths excluded from backup. It is not known to hold a secret.

### The fence, as built

The decision is option (a) of the options below: the profile read-denies the places only TCC protects. `js-agent`'s native profile (`crates/js-agent/src/native/profile.rs`) does this for every run:

- It read-denies the privacy-only places (`PRIVACY_ONLY_PLACES` in `crates/js-agent/src/native/fence.rs`): the privacy database's directory, the database, and Time Machine's preferences, each under `/Library` and under `/System/Volumes/Data/Library`. `/Library` is a firmlink to the data volume and which spelling Seatbelt matches is not measured, so both are named. Each is a `(deny file-read* (subpath …))`, which covers every read operation, metadata included: the job can neither open, list nor `stat` the place (a `stat` answers `EPERM`). It still sees the place's name in a listing of its parent, which is not denied.
- It refuses a mount at or below those places.
- It read-denies the homes roots (every other account's home) and root's home (`/private/var/root`, both spellings), whatever their modes; the run's own account's home stays readable.
- It also denies hard links and clones from every read-denied place and homes root (`file-link` and `file-clone`), and grants both back below the run's own home and for what each mount grant grants. A read deny matches paths. A run that hard-links (`link`, `linkat`) or APFS-clones (`clonefile`) a denied file into its run directory would hold a name no deny matches, and could read the file there. The link and clone deny closes that; the own-home grant keeps links and clones inside the run working.

The list is the measured places, not a complete list: Apple publishes no list of the places only TCC protects, and it changes between macOS releases. Not built: the walk of the system volume per macOS release described in option (a) below (a walk with no profile by a pool user counted as the agent, again started disclaimed, and a third pass under the profile, with the list committed beside the profile and a check that fails on a listed place the profile does not deny). Until it is built, a place macOS adds stays readable to jobs until someone measures it and adds it to `fence.rs`. This is future work.

Kept tests that check the fence:

- The Linux text tests of the profile (`crates/js-mac-pool` and `crates/js-agent`) check the profile's text: the read denies, the link and clone denies, the own-home grant, the refusal of a mount at or below a place.
- `crates/js-mac-pool/tests/runner_links.rs` runs on the hosted macOS runner in the beta stage of the release pipeline. It checks that a read-denied file cannot be read through a symlink, a hard link, a clone, a `/.vol` path or another case spelling, each with a control that shows the read works without the deny.
- `crates/js-agent/tests/native_fence` is the lab-Mac root test, run by `cargo xtask mac-root-tests` (komira-ai/jobscheduler#1020 is its issue). It runs the probe as four processes: root as the test's ordinary child; the slot uid counted as the test; the slot uid disclaimed; and the job under the profile. It passes only when every place is refused to the job and every control holds. A failed control is INCONCLUSIVE and fails the test.

Options considered:

- **(a) The profile denies reads of the places only TCC protects. Chosen, as above.** The rule is the profile builder's, not the text first drafted here. Keeping the list complete, by a walk of the system volume per macOS release, was part of the option and is not built. Costs of the walk: the walker and its tests, a lab-Mac run per macOS release, and a place macOS adds stays readable to jobs until that release has been through the run.
- **(b) Accept the three as readable.** Not chosen. A job would learn what is listed above: which programs on the Mac hold which privacy grants, and how and where the Mac is backed up.
- **(c) Other.** Not chosen. For example: reads default-denied with an allow list of what a build needs (`/usr`, `/System`, `/Library/Developer`, the developer tools, the job's own folders), which the prototype has as `deny-default` and which needs every tool's reads found and kept up; or jobs started disclaimed again with the build trees reached another way than the network file system mount (a local copy costs the copy's time and disk).

## What a start does

`Program` is the standard library's `Command` with its program and arguments fixed when it is made. Off macOS it is that command, unchanged. On macOS, when it is started:

1. **In the parent, before the fork**, it makes everything the child will use, because the child may not allocate between `fork` and `exec` (another thread may hold the allocator's lock at the fork):
   - the argument vector;
   - the environment, rebuilt from the calls: the process's own, or none after `env_clear`, with each set and removal applied in order. The standard library sets the command's environment in the child only after the `pre_exec` closures have run, so the child cannot take it from `environ`;
   - the files to try: the program itself when it holds a `/`, otherwise the program joined to each directory of the environment's `PATH` (`/usr/bin:/bin` when there is none), in order, an empty directory being `.`. Nothing is looked at in the parent: a relative file is tried by the child, in the directory it has entered;
   - for each file, the vector that runs it through `/bin/sh`;
   - the `posix_spawn` attributes: `POSIX_SPAWN_SETEXEC` (execute in the caller's place) and the disclaim. A call that fails ends the start with its error, and nothing is started.
2. **In the child**, the standard library does its set-up (streams, ids, working directory, process group); the caller's steps run (the helper's `fchdir` into the lease directory, the close-on-exec pass, the limits); then, in place of `execvp`, the child tries the files with `posix_spawn` and those attributes. Each attempt that succeeds replaces the child with the program, disclaimed; each one that fails returns an error number and leaves the child as it was, so the next can be tried.
3. **The search decides as `execvp` does** (Apple's `execvP`):
   - `ENOEXEC` (an executable text file with no `#!` line) runs the file through `/bin/sh` with the same arguments, and that answer is final;
   - in a `PATH` search, `ENOENT`, `ENOTDIR`, `ELOOP` and `ENAMETOOLONG` pass to the next file, and `EACCES` does too but is remembered; any other error ends the search with it; when every file was passed over the answer is `EACCES` if one was refused so, `ENOENT` otherwise;
   - a program with a `/` is one file, and its error is the answer.

A `Program` is used once: a second start, after a first that succeeded or failed, is refused. What the first start made is the child's, and the parent never makes it twice for one command.

The helper's `as-user` mode (the last step of a run in a GUI session, executed by `launchctl asuser`) executes the program in its own place with `disclaim::exec`: the same plan and attributes, from the process's own environment, with no fork.

What the disclaim gives: the program is its own responsible process, and what it starts answers for it. What it does not do: it does not take away a grant that the program itself already has (a grant given to that app by someone); it only stops the daemon's grants from reaching it. A start that keeps the responsibility (`Program::keep_responsibility(Kept::Job)` or `Kept::Supervisor`) makes none of the above: the child does the standard library's set-up and the caller's steps, and the standard library executes the program, as it does off macOS. The default stays disclaimed.

## How the rule is kept

`cargo test -p xtask --test spawn_sites` parses, with `syn`, the production source of `js-agent`, `js-mac-session` and every workspace crate built into them for macOS (their path dependencies, not the Linux-only target tables), from every crate root the manifests and the source layout give. Outside `disclaim.rs` it reports an import that renames `libc` or a `process` module (under another name, `Command` and the shells would not be named), and any naming of the standard library's or tokio's `process::Command`, `CommandExt`, a `pre_exec` or `exec` call, the C library's `exec*`, `posix_spawn*`, `vfork`, `system` and `popen`, and glob imports of those modules, in code, imports and macro tokens. What only a test build compiles is left out. A module it cannot find, a module whose file a `cfg_attr` chooses by target, an `include!`, or a crate with no root stops it with an error, never a skip.

`cargo test -p xtask --test spawn_sites_keepers` holds the second rule: a call of `keep_responsibility` or a use of `disclaim::Kept` outside the files listed in the constant `KEEPERS` in `crates/xtask/src/spawn_sites.rs` is a site, and the test fails. `KEEPERS` lists the supervisor's two starts and nothing else: `crates/js-agent/src/mac_supervisor/launch.rs` (the agent's start of the supervisor) and `crates/js-agent/src/mac_supervisor/run.rs` (the supervisor's start of the job). A listed file that the walk does not reach is an error, as a missing module is, so a stale entry cannot stand.

`js-mac-provision`'s `pfctl` and `launchctl` runners go through `Program` too: the agent starts `pfctl` through them, disclaimed.

## Public interface

Added, in `js-mac-session` (a workspace crate, not published):

- `disclaim::Program`: `new(program, args)` and the unsafe `with_steps(program, args, steps)` (the caller promises the steps are async-signal-safe); the builder `keep_responsibility(why: Kept)` (the default is disclaimed); the builders `env_clear`, `env`, `envs`, `current_dir`, `uid`, `gid`, `process_group`, `stdin`, `stdout`, `stderr`; and `spawn` and `output`, which return `io::Result` (a NUL is `InvalidInput`, a program not found `NotFound`, one that may not be executed `PermissionDenied`, a second start an error of its own). The program owns its arguments, environment and streams from the moment they are given.
- `disclaim::Kept`, `Job` or `Supervisor`: which of the two starts that keep the responsibility a start is.
- `disclaim::exec(program, args) -> io::Error` and `disclaim::responsible_pid(pid) -> io::Result<u32>`, macOS only.

Changed: `macos::gui::command(uid, gid, program, args) -> io::Result<Command>` became `macos::gui::argv(uid, gid, program, args) -> io::Result<(&'static str, Vec<OsString>)>`, the vector `launchctl asuser` is started with, so the start goes through `Program`.

Added, in `xtask` (used by its `spawn_sites` test only): the module `spawn_sites`, with `check(&Path) -> Result<Vec<Site>, Error>`, `check_with_keepers(&Path, &[&str]) -> Result<Vec<Site>, Error>` (the same check with the given files as the keepers, for the tests) and `crates(&Path) -> Result<BTreeSet<String>, Error>` (both borrow the repository root and return owned results); `Site` (file, line, what); `Error`, whose variants each carry the file and the cause (`Read`, `Parse`, `Manifest`, `NoModule`, `PathAttribute`, `ConditionalPath`, `Include`, `NoRoot`, `NoSpawner`, `NoKeeper`); and the constants `START_CRATES`, `SPAWNER`, `EXECUTORS`, `KEEPERS`.

No CLI, flag, configuration key, wire message or file format changes.

## What is tested where

- The rules that need no system call (the vectors byte for byte, the search, the order and cleanup of the attribute calls, the pid conversions) and `Program` as the standard library's command run on Linux, in `js-mac-session`'s own tests.
- On the macOS CI runner, `crates/js-agent/tests/agent/native/disclaim.rs` starts real jobs through the helper's `spawn` and through `Program` and reads each process's responsible process back (`responsibility_get_pid_responsible_for_pid`): jobs, a job under `sandbox-exec`, a job's child and grandchild, the agent's `pfctl`, jobs started at the same moment, and a control started the usual way. It also checks the search and the environment on a real system: relative and empty `PATH` directories in the job's own directory, a non-executable and an unreadable candidate passed over, a script without `#!`, a `PATH` only the job has, and the environment byte for byte.
- Not proven on the hosted runner, which has no grant to give: a protected read refused while the helper holds a grant; nor the GUI-session path (`launchctl asuser`, the drop to the lease user and the in-process `disclaim::exec`), which needs root and a logged-in user. Its `exec` shares the plan, the search and the attributes with the `Program` start the runner does test; what only it does is build the plan from its own environment and call the search without a fork. The check by hand is in `packaging/js-agent-macos/README.md`.

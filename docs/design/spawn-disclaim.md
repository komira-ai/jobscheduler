# Starting programs on macOS: disclaimed by default, kept for a job

How `js-agent` and `js-mac-session` start every program on macOS, in two kinds of start, why each is built the way it is, and how the rule is kept. A start disclaims the privacy (TCC) responsibility by default. A native job and its supervisor are started without the disclaim: they keep the responsibility. The code is `crates/js-mac-session/src/disclaim.rs` and its modules (`plan.rs`, `darwin.rs`); the check is `crates/xtask/src/spawn_sites.rs`.

## The problem

macOS's privacy controls (TCC: Full Disk Access, the camera, other apps' data) decide what a process may do from its *responsible* process. A process started the usual way, by `fork` and `exec` or by `posix_spawn`, answers for the process that started it, and so on up to a process that answers for itself: one that launchd started as a job of its own, or one whose starter disclaimed the responsibility when it started it (`responsibility_spawnattrs_setdisclaim` on the `posix_spawn` attributes).

`js-agent` is a launchd daemon and answers for itself. A device-management privacy profile gives it Full Disk Access. Two rules follow, one for each kind of start.

**A job keeps the responsibility.** A native job and its supervisor are started without the disclaim, so they answer for `js-agent` and hold what it holds, Full Disk Access included. Every process between `js-agent` and the job is started so, and a job's own children (compilers, shells, buck2) follow their parent and answer for the agent too. The reason is measured. A job started disclaimed is judged by its own code signature, and it is refused every read of the network file system (NFS) loopback mount that tierstore serves its build trees from; Apple's own tools (`ls`, `cat`, `python3`, compilers) are refused too. On a Mac (macOS 27), disclaimed, the reads fail with "Operation not permitted" and the system log shows the network-volumes privacy service denying them. Started as ordinary children of the signed, profile-granted `js-agent`, the same reads as the same pool user succeed.

What keeps a job from using the rest of the grant is the slot user it runs as (Unix permissions) and the Seatbelt profile applied in the job's child before the program runs. The profile is what is relied on to keep a slot-user job out of the places only TCC protects. A proof run measures that; its result is pending.

**Every other program is disclaimed.** Host probes, `pfctl`, `launchctl`, `ioreg` and the rest answer for themselves, so a process the agent starts for its own use never carries the grant. Forgetting to choose gives the default, disclaimed: it can cost a program a read it needed, and never gives one a grant it should not have.

The table below says which start is which.

Today the root helper `js-mac-session` still starts every native job (`crates/js-mac-session/src/spawn.rs`), disclaimed, and the helper holds no grant (the agent holds it), so a job it starts holds none. No production code keeps the responsibility yet. The supervisor (`js-agent __supervise-native`) is the first caller: the agent starts it with `Kept::Supervisor`, and it starts the job with `Kept::Job`.

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

`cargo test -p xtask --test spawn_sites_keepers` holds the second rule: a call of `keep_responsibility` or a use of `disclaim::Kept` outside the files listed in the constant `KEEPERS` in `crates/xtask/src/spawn_sites.rs` is a site, and the test fails. `KEEPERS` is empty until the supervisor lands, because no production code keeps the responsibility yet. A listed file that the walk does not reach is an error, as a missing module is, so a stale entry cannot stand.

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

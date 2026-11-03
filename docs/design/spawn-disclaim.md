# Starting programs with the privacy responsibility disclaimed

How `js-agent` and `js-mac-session` start every program on macOS so that no job holds a privacy (TCC) grant of theirs, why the start is built the way it is, and how the rule is kept. The code is `crates/js-mac-session/src/disclaim.rs` and its modules (`plan.rs`, `darwin.rs`); the check is `crates/xtask/src/spawn_sites.rs`.

## The problem

macOS's privacy controls (TCC: Full Disk Access, the camera, other apps' data) decide what a process may do from its *responsible* process. A process started the usual way, by `fork` and `exec` or by `posix_spawn`, answers for the process that started it, and so on up to a process that answers for itself: one that launchd started as a job of its own, or one whose starter disclaimed the responsibility when it started it (`responsibility_spawnattrs_setdisclaim` on the `posix_spawn` attributes).

The agent and the helper are launchd daemons and answer for themselves. A job is started by the helper (the `run` verb), so a job started the usual way answers for the helper, and a grant ever given to the helper (by hand, or by a profile) would reach the code of every job. The agent's own probes would answer for the agent in the same way.

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

What the disclaim gives: the program is its own responsible process, and what it starts answers for it. What it does not do: it does not take away a grant that the job's own program already has (a grant given to that app by someone); it only stops the daemon's grants from reaching it.

## How the rule is kept

`cargo test -p xtask --test spawn_sites` parses, with `syn`, the production source of `js-agent`, `js-mac-session` and every workspace crate built into them for macOS (their path dependencies, not the Linux-only target tables), from every crate root the manifests and the source layout give. Outside `disclaim.rs` it reports any naming of the standard library's or tokio's `process::Command`, `CommandExt`, a `pre_exec` or `exec` call, the C library's `exec*`, `posix_spawn*`, `vfork`, `system` and `popen`, and glob imports of those modules, in code, imports and macro tokens. What only a test build compiles is left out. A module it cannot find, an `include!`, or a crate with no root stops it with an error, never a skip.

`js-mac-provision`'s `pfctl` and `launchctl` runners go through `Program` too: the agent starts `pfctl` through them.

## Public interface

Added, in `js-mac-session` (a workspace crate, not published):

- `disclaim::Program`: `new(program, args)` and the unsafe `with_steps(program, args, steps)` (the caller promises the steps are async-signal-safe); the builders `env_clear`, `env`, `envs`, `current_dir`, `uid`, `gid`, `process_group`, `stdin`, `stdout`, `stderr`; and `spawn` and `output`, which return `io::Result` (a NUL is `InvalidInput`, a program not found `NotFound`, one that may not be executed `PermissionDenied`, a second start an error of its own). The program owns its arguments, environment and streams from the moment they are given.
- `disclaim::exec(program, args) -> io::Error` and `disclaim::responsible_pid(pid) -> io::Result<u32>`, macOS only.

Changed: `macos::gui::command(uid, gid, program, args) -> io::Result<Command>` became `macos::gui::argv(uid, gid, program, args) -> io::Result<(&'static str, Vec<OsString>)>`, the vector `launchctl asuser` is started with, so the start goes through `Program`.

Added, in `xtask`: `spawn_sites` (`check`, `crates`, `Site`, `Error` and its constants), used by its test only.

No CLI, flag, configuration key, wire message or file format changes.

## What is tested where

- The rules that need no system call (the vectors byte for byte, the search, the order and cleanup of the attribute calls, the pid conversions) and `Program` as the standard library's command run on Linux, in `js-mac-session`'s own tests.
- On the macOS CI runner, `crates/js-agent/tests/agent/native/disclaim.rs` starts real jobs through the helper's `spawn` and through `Program` and reads each process's responsible process back (`responsibility_get_pid_responsible_for_pid`): jobs, a job under `sandbox-exec`, a job's child and grandchild, the agent's `pfctl`, jobs started at the same moment, and a control started the usual way. It also checks the search and the environment on a real system: relative and empty `PATH` directories in the job's own directory, a non-executable and an unreadable candidate passed over, a script without `#!`, a `PATH` only the job has, and the environment byte for byte.
- Not proven on the hosted runner, which has no grant to give: a protected read refused while the helper holds a grant; nor the GUI-session path (`launchctl asuser`, the drop to the lease user and the in-process `disclaim::exec`), which needs root and a logged-in user. Its `exec` shares the plan, the search and the attributes with the `Program` start the runner does test; what only it does is build the plan from its own environment and call the search without a fork. The check by hand is in `packaging/js-agent-macos/README.md`.

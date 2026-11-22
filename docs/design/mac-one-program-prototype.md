# A prototype of the one Mac program: one native job, start to end

Status: prototype, not for release. It runs one native macOS job with no helper daemon and no `sandbox-exec`, so the parts of [mac-one-program.md](mac-one-program.md) that decide whether native jobs keep the machine clean can be measured on a real Mac now. It is the mode `js-agent __mac-job` (`crates/js-agent/src/mac_job/`, with the account pool of `crates/js-agent/src/mac_accounts/`).

## What it does

As root, given a job (a file, or the job's JSON inline), a root folder and an output folder:

1. It reads the job (`spec.rs`). The job names its operating system (`os`, required) and may name its architecture (`arch`). The scheduler placed it, so the mode refuses only a job that cannot run on this host at all. An unknown field is an error.
2. It takes the job's user. With `--accounts <state folder> --account-range <first>-<last>` (the uids are a required setting, with no default), the account pool lends an account that has never run a job (`mac_accounts`). With `--user`, one existing account runs it, for measurement. Either way the user must not be root or an administrator and must run no process.
3. It makes the job folder `<root>/<id>`, mode 0700, owned by the user, with `work`, `home`, `tmp` and `cache` inside (`layout.rs`). It copies the inputs into `work`. `HOME`, `CFFIXED_USER_HOME`, `TMPDIR`, `XDG_CACHE_HOME` and `CLANG_MODULE_CACHE_PATH` point inside the folder, so the places build tools write by default are under it. A failure after the folder is made removes it.
4. It links the job's trees. With `--mount-provider` and `--mount-root`, each tree the job names is prepared read-only and mounted through the mount provider (tierstore), and `work/<at>` becomes a link to it. The mounted path must lie beneath the mount root. The provider's calls run on a runtime of one thread that is gone when they return (`provider/blocking.rs`).
5. It starts the job through the spawner (`js_mac_session::disclaim::Program`): its own program again, in the start mode `__mac-job-start` (`start.rs`), a new process with one thread, reading a start file only root can read. That process opens a new audit session (`setaudit_addr` with `AU_ASSIGN_ASID`). It then drops to the user (`setgroups` to its primary group, `setgid`, `setuid`, and checks that `setuid(0)` fails). Finally it applies the Seatbelt profile with `sandbox_init_with_parameters`, the job folder as the parameter `RUN`, the profile the one builder's (`confine.rs`, over `crates/js-mac-pool/src/seatbelt/`), through the agent's one call of it (`job_start`). Then it executes the job's program in its own place, undisclaimed (`exec_kept`): nothing between the agent and the job disclaims it. A profile the library refuses fails the start, its message goes to the job's standard error, and the program is not executed. Nothing runs between `fork` and `exec` in the agent itself, which matters because the agent has threads: OpenDirectory, which the account pool uses, runs threads of its own (measured: an agent that had opened the pool had 3).
6. It waits for the program or its wall limit. Then it ends every process of the job's audit session and of its user, round after round until a round finds none (`sweep.rs`). It then releases the trees.
7. With `--put-ingest`, it commits one tar of the declared outputs to tierstore's put ingest (`put.rs`). The job itself is never given a writable put directory. It copies the declared outputs to the output folder, clears every file flag under the job folder (a job can set the user-immutable flag, which stops even root's unlink) and removes the folder.
8. It checks what is left: processes of the user, files of the user outside the folder that are new or changed (in `/private/tmp`, `/private/var/tmp`, the user's home and the user's folders under `/private/var/folders`), and the folder itself. A lent account is then retired: the pool ends the uid, deletes the account, shows it gone, and refills. The mode writes a JSON report (`report.rs`, with the tails of the program's streams). Its exit status is 0 (the job succeeded and nothing is left), 1 (the job failed, nothing is left), 2 (usage), 3 (something was left, a held account included) or 4 (setup).

The measurement scripts around the mode do the machine-wide check, before and after each job and beside an idle window of the same length. It covers every file changed on the data volume, launchd entries, System V IPC objects and preferences.

## The profile

There is one profile, the builder's (`crates/js-mac-pool/src/seatbelt/`), the one every native run has: allow by default, then deny every file write but the job folder's (the parameter `RUN`) and the put directories of the job (read-write grants). It also denies preference writes, launchd jobs, Launch Services, Mach name registration, mounts, POSIX shared memory creation, the network unless the job names it, and what reaches an attached device. Real builds pass under it. Unix permissions keep another job's files from a job, each job running as its own user. A tree under `work` is read through its link and cannot be written, because it lies outside the job folder. On the mount the job reads only its own trees: the profile denies reads below the mount root and allows each of the job's tree paths, and the metadata of the folders on the way (`MountTrees`), each a parameter (`MOUNT`, `MT_0`, ...). A tree not strictly below the mount root refuses the job, naming the tree. There is no per-user folder grant, and no flag adds rules.

A job's network is none by default. `loopback` allows listening on and connecting to the loopback address. `outbound_ports` is the builder's `Remote` level: loopback, outbound TCP to those ports on any address (Seatbelt matches a remote address only as `*` or `localhost`) and the name lookups that need (the resolver's socket). A Buck2 build needs `loopback`, because its client reaches its own daemon over the loopback address.

## How the job is started

The job is started the usual way (`Program::keep_responsibility`), always: no flag disclaims it. The job then answers to the privacy controls as the agent and holds the agent's Full Disk Access. That is the one way measured to let a job read tierstore's network file system mount. The owner decided it: a job starts as an ordinary child of the agent, and its profile keeps it on the mount to its own trees (below).

## Why the audit session

A job can leave its process group, its session and its process tree (a double fork, `setsid`, a disclaimed spawn). When measured, a process group and a walk of parent links each missed some of a job's processes. Every process a job starts inherits the audit session its child opened, and opening another one needs root. So the sweep finds a job's processes by its audit session, and by its user too. The user rule is the one the cleanup relies on, because whether a job can leave its audit session on the current macOS is not yet measured. The sweep records which rule found each process, so a run shows whether the audit session alone would have been enough.

## Shortcuts, each to be undone

- The job comes from a file or from the command line, not from the lease server.
- The inputs are copied into the job folder from a local folder. Trees come from the mount provider.
- The start is a process of its own for one job, not yet the per-run supervisor of [mac-one-program.md](mac-one-program.md) section 4, which would also keep the job alive across an agent restart.

## What a Mac with only the Command Line Tools builds

komira's macOS example (`//tools/build/examples:hello[run_check]`, Mojo, compiled and linked for macOS arm64, then run) builds with every action local on a Mac that has only the Command Line Tools, with the pinned Buck2. The build downloads its pinned toolchain packages over HTTPS. No `xcode` property is involved, because komira's macOS actions name only the operating system and the architecture.

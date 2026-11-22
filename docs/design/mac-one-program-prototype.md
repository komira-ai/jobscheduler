# A prototype of the one Mac program: one native job, start to end

Status: prototype, not for release. It runs one native macOS job from a job file, with no helper daemon, no `sandbox-exec` and no pool, so the parts of [mac-one-program.md](mac-one-program.md) that decide whether native jobs keep the machine clean can be measured on a real Mac now. It is the mode `js-agent __mac-job` (`crates/js-agent/src/mac_job/`).

## What it does

As root, given a job file, a user, a root folder and an output folder:

1. It reads the job (`spec.rs`). The job names its operating system (`os`, required) and may name its architecture (`arch`). The scheduler placed it, so the mode refuses only a job that cannot run on this host at all. An unknown field is an error.
2. It checks the user: not root, not in the `admin` group, and running no process.
3. It makes the job folder `<root>/<id>`, mode 0700, owned by the user, with `work`, `home`, `tmp` and `cache` inside (`layout.rs`). The inputs are copied into `work`. `HOME`, `CFFIXED_USER_HOME`, `TMPDIR`, `XDG_CACHE_HOME` and `CLANG_MODULE_CACHE_PATH` point inside the folder, so the places build tools write by default are under it.
4. It starts the program through the disclaiming spawner (`js_mac_session::disclaim::Program`). In the child, before the program executes, it opens a new audit session (`setaudit_addr` with `AU_ASSIGN_ASID`), drops to the user (`setgroups` to its primary group, `setgid`, `setuid`, and checks `setuid(0)` fails), and applies the Seatbelt profile with `sandbox_init_with_parameters`, the job folder as the parameter `JOB` (`profile.rs`). A profile the library refuses fails the start and its message goes to the job's standard error; the program is not executed.
5. It waits for the program or its wall limit, then ends every process of the job's audit session and of its user, round after round until a round finds none (`sweep.rs`).
6. It copies the declared outputs, clears every file flag under the folder (a job can set the user-immutable flag, which stops even root's unlink) and removes the folder.
7. It checks what is left: processes of the user, files of the user outside the folder that are new or changed (in `/private/tmp`, `/private/var/tmp`, the user's home and the user's folders under `/private/var/folders`), and the folder itself. It writes a JSON report (`report.rs`) and exits 0 (the job succeeded and nothing is left), 1 (the job failed, nothing is left), 2 (usage), 3 (something was left) or 4 (setup).

The machine-wide check (every file changed on the data volume, launchd entries, System V IPC objects, preferences) is done by the measurement scripts around the mode, before and after each job, beside an idle window of the same length.

## The two profiles

- `confined-writes`, the default: allow by default, then deny every file write but the job folder's, preference writes, launchd jobs, Launch Services, Mach name registration, mounts, POSIX shared memory creation, the network unless the job names it, and what reaches an attached device. It carries the head and the floor of the present profile (`crates/js-mac-pool/src/seatbelt/profile.rs`). This is the kind real builds pass under; another job's files are kept from a job by Unix permissions, each job running as its own user.
- `deny-default`: deny by default and allow back what measured builds needed. Kept for measurement.

A job's network is none by default. `loopback` allows listening on and connecting to the loopback address; `outbound_ports` allows outbound TCP to those ports on any address (Seatbelt matches a remote address only as `*` or `localhost`) and the name lookups that need. A Buck2 build needs `loopback`: its client reaches its own daemon over the loopback address.

## Why the audit session

A job can leave its process group, its session and its process tree (a double fork, `setsid`, a disclaimed spawn). A process group and a walk of parent links each missed some of a job's processes when measured. Every process a job starts inherits the audit session its child opened, and opening another one needs root. So the sweep finds a job's processes by its audit session and, because this prototype runs one job per user at a time, by its user too. It records which rule found each process, so a run shows whether the audit session alone would have been enough. Whether a job can leave its audit session on the current macOS is not yet measured.

## Shortcuts, each to be undone

- One existing ordinary account runs the jobs (`--user`), one job at a time. The decided design is a new account per job, from a pool of ready accounts, deleted after the job and shown gone before its uid is used again. The account module takes the place of `--user`.
- The inputs are copied into the job folder from a local folder; no mount.
- The job comes from a file, not from the lease server.
- `sandbox_init_with_parameters` allocates, which is safe after `fork` only in a process with one thread. The mode checks it has one thread before it starts the job. In the agent daemon, which has many, the start belongs in a supervisor process of its own, as [mac-one-program.md](mac-one-program.md) section 4 plans.
- `--profile-add` appends rules to the profile, for measuring what a build needs.

## What a Mac with only the Command Line Tools builds

komira's macOS example (`//tools/build/examples:hello[run_check]`, Mojo, compiled and linked for macOS arm64, then run) builds with every action local on a Mac that has only the Command Line Tools, with the pinned Buck2. The build downloads its pinned toolchain packages over HTTPS. No `xcode` property is involved: komira's macOS actions name only the operating system and the architecture.
